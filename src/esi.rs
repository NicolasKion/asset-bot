//! The ESI routes the bot uses: assets, names, structures and search.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::anyhow;
use reqwest::header::{LAST_MODIFIED, RETRY_AFTER};
use reqwest::{Client, Method, RequestBuilder, Response};
use serde::Deserialize;

const BASE_URL: &str = "https://esi.evetech.net";

/// Pins ESI's behavior. Check what changed before moving it forward.
const COMPATIBILITY_DATE: &str = "2026-11-19";

/// ESI takes at most this many ids in one name lookup.
const NAMES_CHUNK: usize = 1000;

/// ESI takes at most this many names in one id lookup.
const IDS_CHUNK: usize = 500;

/// ESI takes at most this many characters in one affiliation call.
const AFFILIATION_CHUNK: usize = 1000;

/// ESI refuses searches shorter than this.
pub const MIN_SEARCH_LEN: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum EsiError {
    /// The token was refused: a scope is missing or the character has no
    /// right to the resource.
    #[error("ESI refused the access token")]
    Unauthorized,
    /// A rate or error limit was hit. Stop calling ESI for now.
    #[error("ESI rate limit hit")]
    Throttled { retry_after: Option<Duration> },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<reqwest::Error> for EsiError {
    fn from(error: reqwest::Error) -> Self {
        Self::Other(error.into())
    }
}

/// Whose assets are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Owner {
    Character(i64),
    Corporation(i64),
}

impl Owner {
    fn assets_path(self) -> String {
        match self {
            Self::Character(id) => format!("/characters/{id}/assets"),
            Self::Corporation(id) => format!("/corporations/{id}/assets"),
        }
    }
}

/// What kind of thing an asset's `location_id` points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationType {
    Station,
    SolarSystem,
    /// Another asset, or a structure.
    Item,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub item_id: i64,
    pub type_id: i64,
    pub quantity: i64,
    pub location_id: i64,
    pub location_flag: String,
    pub location_type: LocationType,
    pub is_singleton: bool,
}

#[derive(Deserialize)]
struct Name {
    id: i64,
    name: String,
}

#[derive(Deserialize)]
struct ItemName {
    item_id: i64,
    name: String,
}

#[derive(Deserialize)]
struct Ids {
    #[serde(default)]
    inventory_types: Vec<Name>,
}

#[derive(Deserialize)]
struct Structure {
    name: String,
}

#[derive(Deserialize)]
struct SearchResult {
    #[serde(default)]
    station: Vec<i64>,
    #[serde(default)]
    structure: Vec<i64>,
}

#[derive(Deserialize)]
struct Affiliation {
    character_id: i64,
    corporation_id: i64,
}

pub struct Esi {
    http: Client,
}

impl Esi {
    pub fn new(http: Client) -> Self {
        Self { http }
    }

    /// Every asset of a character or corporation. A corporation needs a token
    /// of a character with the Director role.
    pub async fn assets(&self, owner: Owner, token: &str) -> Result<Vec<Asset>, EsiError> {
        // The pages of one snapshot share a Last-Modified. If ESI rebuilt its
        // cache between two pages they differ, and the set has to be read again.
        for _ in 0..2 {
            if let Some(assets) = self.asset_pages(owner, token).await? {
                return Ok(assets);
            }
        }
        Err(anyhow!("the asset list kept changing while it was read").into())
    }

    /// One pass over all asset pages, or `None` if they are not one snapshot.
    async fn asset_pages(&self, owner: Owner, token: &str) -> Result<Option<Vec<Asset>>, EsiError> {
        let path = owner.assets_path();
        let mut assets = Vec::new();
        let mut snapshot: Option<Vec<u8>> = None;
        let mut page = 1_u32;
        loop {
            let request = self
                .request(Method::GET, &path)
                .bearer_auth(token)
                .query(&[("page", page)]);
            let response = send(request).await?;
            let pages: u32 = response
                .headers()
                .get("x-pages")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
                .unwrap_or(1);
            let modified = response
                .headers()
                .get(LAST_MODIFIED)
                .map(|value| value.as_bytes().to_vec());
            if page > 1 && modified != snapshot {
                return Ok(None);
            }
            snapshot = modified;

            let batch: Vec<Asset> = response.json().await?;
            assets.extend(batch);

            if page >= pages {
                return Ok(Some(assets));
            }
            page += 1;
        }
    }

    /// The names players gave to their own items, such as containers and
    /// ships. Items without a name of their own are left out.
    pub async fn asset_names(
        &self,
        owner: Owner,
        token: &str,
        item_ids: &[i64],
    ) -> Result<HashMap<i64, String>, EsiError> {
        let path = format!("{}/names", owner.assets_path());
        let mut names = HashMap::new();
        for chunk in item_ids.chunks(NAMES_CHUNK) {
            let request = self
                .request(Method::POST, &path)
                .bearer_auth(token)
                .json(chunk);
            let batch: Vec<ItemName> = send(request).await?.json().await?;
            names.extend(
                batch
                    .into_iter()
                    // ESI answers "None" for an item that was never renamed.
                    .filter(|item| !item.name.is_empty() && item.name != "None")
                    .map(|item| (item.item_id, item.name)),
            );
        }
        Ok(names)
    }

    /// Names of types, stations, solar systems or corporations. One unknown id
    /// fails the whole call. Public, needs no token.
    pub async fn names(&self, ids: &[i64]) -> Result<HashMap<i64, String>, EsiError> {
        let mut names = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(NAMES_CHUNK) {
            let request = self.request(Method::POST, "/universe/names").json(chunk);
            let batch: Vec<Name> = send(request).await?.json().await?;
            names.extend(batch.into_iter().map(|entry| (entry.id, entry.name)));
        }
        Ok(names)
    }

    /// Finds item types by their exact names and returns id and name as EVE
    /// spells it. Names EVE does not know are left out. Public, needs no token.
    pub async fn types_by_name(&self, names: &[String]) -> Result<Vec<(i64, String)>, EsiError> {
        let mut types = Vec::new();
        for chunk in names.chunks(IDS_CHUNK) {
            let request = self.request(Method::POST, "/universe/ids").json(chunk);
            let ids: Ids = send(request).await?.json().await?;
            types.extend(
                ids.inventory_types
                    .into_iter()
                    .map(|found| (found.id, found.name)),
            );
        }
        Ok(types)
    }

    /// Name of a player structure. Only works for a character that is allowed
    /// to dock there.
    pub async fn structure_name(&self, structure_id: i64, token: &str) -> Result<String, EsiError> {
        let path = format!("/universe/structures/{structure_id}");
        let request = self.request(Method::GET, &path).bearer_auth(token);
        let structure: Structure = send(request).await?.json().await?;
        Ok(structure.name)
    }

    /// Ids of the stations and structures whose name contains the text. The
    /// structures are the ones the character may dock at.
    pub async fn search_locations(
        &self,
        character_id: i64,
        token: &str,
        text: &str,
    ) -> Result<Vec<i64>, EsiError> {
        let path = format!("/characters/{character_id}/search");
        let request = self
            .request(Method::GET, &path)
            .bearer_auth(token)
            .query(&[("categories", "station,structure"), ("search", text)]);
        let found: SearchResult = send(request).await?.json().await?;
        Ok([found.station, found.structure].concat())
    }

    /// The corporation of each character. Public, needs no token.
    pub async fn corporations_of(
        &self,
        character_ids: &[i64],
    ) -> Result<HashMap<i64, i64>, EsiError> {
        let mut corporations = HashMap::with_capacity(character_ids.len());
        for chunk in character_ids.chunks(AFFILIATION_CHUNK) {
            let request = self
                .request(Method::POST, "/characters/affiliation")
                .json(chunk);
            let batch: Vec<Affiliation> = send(request).await?.json().await?;
            corporations.extend(batch.iter().map(|a| (a.character_id, a.corporation_id)));
        }
        Ok(corporations)
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{BASE_URL}{path}"))
            .header("X-Compatibility-Date", COMPATIBILITY_DATE)
    }
}

async fn send(request: RequestBuilder) -> Result<Response, EsiError> {
    let response = request.send().await?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    match status.as_u16() {
        401 | 403 => Err(EsiError::Unauthorized),
        // 420 is the legacy error limit, 429 the bucket rate limit.
        420 | 429 => {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
                .map(Duration::from_secs);
            Err(EsiError::Throttled { retry_after })
        }
        _ => {
            let body = response.text().await.unwrap_or_default();
            Err(anyhow!("ESI answered {status}: {body}").into())
        }
    }
}
