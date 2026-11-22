//! State and logic shared by the Discord bot and the web server.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serenity::model::id::UserId;
use tokio::sync::Notify;
use tracing::warn;

use crate::assets::{self, Holding};
use crate::config::Config;
use crate::esi::{Esi, EsiError, Owner};
use crate::parse::Line;
use crate::sso::{CORPORATION_SCOPE, RefreshError, Sso};
use crate::store::{Character, Source, Stockpile, Store};

/// How long a login link stays usable after it was last used.
const LOGIN_TTL: Duration = Duration::from_mins(15);

/// ESI caches asset lists for an hour, so asking sooner only returns the same.
const ASSET_CACHE_SECS: i64 = 3600;

/// How long to wait before asking again for a location name ESI refused.
const NAME_RETRY_SECS: i64 = 24 * 3600;

/// Structure lookups per asset refresh. Each structure the character may not
/// dock at costs one error of ESI's limit of 100 a minute.
const MAX_STRUCTURE_LOOKUPS: usize = 30;

/// What a login asks the player to grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginScopes {
    Assets,
    AssetsAndCorporation,
}

struct PendingLogin {
    user: UserId,
    scopes: LoginScopes,
    last_used: Instant,
}

/// The assets a stockpile is compared with, after making sure they are fresh.
pub struct Sources {
    pub owners: Vec<Owner>,
    /// What went wrong, in words for the user.
    pub problems: Vec<String>,
    /// Unix time of the oldest asset snapshot among the owners.
    pub oldest: Option<i64>,
}

/// A pasted list with its names turned into type ids.
pub struct Resolved {
    /// Type id and quantity, one entry per type.
    pub items: Vec<(i64, i64)>,
    /// Names EVE does not know.
    pub unknown: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
enum FetchError {
    #[error(transparent)]
    Login(#[from] RefreshError),
    #[error(transparent)]
    Esi(#[from] EsiError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct App {
    pub config: Config,
    pub store: Store,
    pub sso: Sso,
    pub esi: Esi,
    logins: Mutex<HashMap<String, PendingLogin>>,
    /// Held while assets are read, so two commands do not fetch the same list.
    refreshing: tokio::sync::Mutex<()>,
    /// Wakes the monitor when an EVE login turned out to be broken.
    pub login_broke: Notify,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed())
}

impl App {
    pub fn new(config: Config, store: Store, sso: Sso, esi: Esi) -> Self {
        Self {
            config,
            store,
            sso,
            esi,
            logins: Mutex::new(HashMap::new()),
            refreshing: tokio::sync::Mutex::new(()),
            login_broke: Notify::new(),
        }
    }

    /// Opens a login session for a Discord user and returns the OAuth `state`
    /// for it. The session can link any number of characters.
    pub fn begin_login(&self, user: UserId, scopes: LoginScopes) -> String {
        let state = format!("{:032x}", rand::random::<u128>());
        let mut logins = self.logins.lock().expect("login map lock poisoned");
        logins.retain(|_, login| login.last_used.elapsed() < LOGIN_TTL);
        logins.insert(
            state.clone(),
            PendingLogin {
                user,
                scopes,
                last_used: Instant::now(),
            },
        );
        state
    }

    /// Who a login session belongs to and which scopes it asks for, if the
    /// state is known and fresh. Using a session keeps it alive.
    pub fn login(&self, state: &str) -> Option<(UserId, LoginScopes)> {
        let mut logins = self.logins.lock().expect("login map lock poisoned");
        let login = logins.get_mut(state)?;
        if login.last_used.elapsed() >= LOGIN_TTL {
            logins.remove(state);
            return None;
        }
        login.last_used = Instant::now();
        Some((login.user, login.scopes))
    }

    /// The EVE login URL of a session.
    pub fn login_url(&self, state: &str, scopes: LoginScopes) -> Result<reqwest::Url> {
        let extra: &[&str] = match scopes {
            LoginScopes::Assets => &[],
            LoginScopes::AssetsAndCorporation => &[CORPORATION_SCOPE],
        };
        self.sso.authorize_url(state, extra)
    }

    /// Gets an ESI access token for a character and keeps a rotated refresh token.
    pub async fn access_token(&self, character: &Character) -> Result<String, RefreshError> {
        if character.login_broken() {
            return Err(RefreshError::Revoked);
        }
        let tokens = match self.sso.refresh(&character.refresh_token).await {
            Ok(tokens) => tokens,
            Err(RefreshError::Revoked) => {
                self.store
                    .set_refresh_token(character.character_id, "")
                    .await?;
                self.login_broke.notify_one();
                return Err(RefreshError::Revoked);
            }
            Err(other) => return Err(other),
        };
        if tokens.refresh_token != character.refresh_token {
            self.store
                .set_refresh_token(character.character_id, &tokens.refresh_token)
                .await?;
        }
        Ok(tokens.access_token)
    }

    /// The name of a station, structure or corporation, as far as it is known.
    pub async fn display_name(&self, id: i64) -> Result<String> {
        Ok(self
            .store
            .name(id)
            .await?
            .unwrap_or_else(|| format!("Unknown location {id}")))
    }

    /// Looks up which corporation each character is in and stores it, with
    /// the corporation's name. Failures only get logged.
    pub async fn update_corporations(&self, characters: &mut [Character]) {
        let ids: Vec<i64> = characters.iter().map(|c| c.character_id).collect();
        if ids.is_empty() {
            return;
        }
        let corporations = match self.esi.corporations_of(&ids).await {
            Ok(corporations) => corporations,
            Err(error) => {
                warn!("could not look up corporations: {error}");
                return;
            }
        };
        for character in characters.iter_mut() {
            let Some(&corporation_id) = corporations.get(&character.character_id) else {
                continue;
            };
            character.corporation_id = corporation_id;
            if let Err(error) = self
                .store
                .set_corporation(character.character_id, corporation_id)
                .await
            {
                warn!(
                    "could not store the corporation of {}: {error:#}",
                    character.name
                );
            }
        }

        let unique: HashSet<i64> = corporations.into_values().collect();
        let mut unnamed = Vec::new();
        for id in unique {
            if self.store.needs_name(id, 0).await.unwrap_or(false) {
                unnamed.push(id);
            }
        }
        if unnamed.is_empty() {
            return;
        }
        match self.esi.names(&unnamed).await {
            Ok(names) => {
                for (id, name) in names {
                    if let Err(error) = self.store.set_name(id, Some(&name), now()).await {
                        warn!("could not store the name of corporation {id}: {error:#}");
                    }
                }
            }
            Err(error) => warn!("could not look up corporation names: {error}"),
        }
    }

    /// Reads the assets of all of a user's characters, and of their
    /// corporations where a login allows it, unless they are fresh.
    pub async fn refresh_user(&self, user: UserId) -> Result<Sources> {
        let characters = self.store.characters_of(user).await?;
        let mut sources = Sources {
            owners: Vec::new(),
            problems: Vec::new(),
            oldest: None,
        };
        for character in &characters {
            self.add_source(
                &mut sources,
                Owner::Character(character.character_id),
                character,
            )
            .await?;
            if character.corp_assets && character.corporation_id != 0 {
                let owner = Owner::Corporation(character.corporation_id);
                if !sources.owners.contains(&owner) {
                    self.add_source(&mut sources, owner, character).await?;
                }
            }
        }
        Ok(sources)
    }

    /// The assets a stockpile is compared with, read again from ESI where the
    /// cached copy is older than ESI's own cache.
    pub async fn sources(&self, stockpile: &Stockpile) -> Result<Sources> {
        let mut characters = self.store.characters_of(stockpile.created_by).await?;
        let mut sources = Sources {
            owners: Vec::new(),
            problems: Vec::new(),
            oldest: None,
        };

        match stockpile.source {
            Source::AllCharacters => {
                if characters.is_empty() {
                    sources.problems.push(format!(
                        "<@{}> has no characters linked, so there are no assets to compare with.",
                        stockpile.created_by
                    ));
                }
                for character in &characters {
                    self.add_source(
                        &mut sources,
                        Owner::Character(character.character_id),
                        character,
                    )
                    .await?;
                }
            }
            Source::Character(id) => match characters.iter().find(|c| c.character_id == id) {
                Some(character) => {
                    self.add_source(&mut sources, Owner::Character(id), character)
                        .await?;
                }
                None => sources.problems.push(format!(
                    "The character this stockpile reads is no longer linked by <@{}>.",
                    stockpile.created_by
                )),
            },
            Source::Corporation(id) => {
                let owner = Owner::Corporation(id);
                if !self.is_fresh(owner).await? {
                    // Someone may have left the corporation since the last read.
                    self.update_corporations(&mut characters).await;
                }
                let reader = characters
                    .iter()
                    .filter(|c| c.corp_assets && c.corporation_id == id)
                    .min_by_key(|c| c.login_broken());
                match reader {
                    Some(reader) => self.add_source(&mut sources, owner, reader).await?,
                    None => sources.problems.push(format!(
                        "<@{}> has no character in that corporation that lets me read its assets. Link one with `/assets auth corporation:True`.",
                        stockpile.created_by
                    )),
                }
            }
        }
        Ok(sources)
    }

    async fn is_fresh(&self, owner: Owner) -> Result<bool> {
        let fetched_at = self.store.fetched_at(owner).await?;
        Ok(fetched_at.is_some_and(|at| now() - at < ASSET_CACHE_SECS))
    }

    /// Makes sure one owner's assets are fresh and adds it to the sources.
    /// If ESI does not hand them out, an older snapshot is used if there is one.
    async fn add_source(
        &self,
        sources: &mut Sources,
        owner: Owner,
        reader: &Character,
    ) -> Result<()> {
        let _guard = self.refreshing.lock().await;

        let mut fetched_at = self.store.fetched_at(owner).await?;
        if fetched_at.is_none_or(|at| now() - at >= ASSET_CACHE_SECS) {
            match self.fetch(owner, reader).await {
                Ok(at) => fetched_at = Some(at),
                Err(error) => {
                    let problem = describe(&error, owner, reader);
                    if !sources.problems.contains(&problem) {
                        sources.problems.push(problem);
                    }
                    if matches!(
                        error,
                        FetchError::Other(_) | FetchError::Esi(EsiError::Other(_))
                    ) {
                        warn!("reading assets through {} failed: {error:#}", reader.name);
                    }
                }
            }
        }

        if let Some(at) = fetched_at {
            sources.owners.push(owner);
            sources.oldest = Some(sources.oldest.map_or(at, |oldest| oldest.min(at)));
        }
        Ok(())
    }

    /// Reads an owner's assets from ESI into the store and returns the time
    /// of the snapshot.
    async fn fetch(&self, owner: Owner, reader: &Character) -> Result<i64, FetchError> {
        let token = self.access_token(reader).await?;
        let listed = self.esi.assets(owner, &token).await?;

        let nameable = assets::nameable(&listed);
        let names = match self.esi.asset_names(owner, &token, &nameable).await {
            Ok(names) => names,
            Err(EsiError::Throttled { retry_after }) => {
                return Err(EsiError::Throttled { retry_after }.into());
            }
            // Without names, narrowing to a container finds nothing, but
            // everything else still works.
            Err(error) => {
                warn!("could not read item names through {}: {error}", reader.name);
                HashMap::new()
            }
        };

        let holdings = assets::flatten(&listed, &names);
        let fetched_at = now();
        self.store
            .replace_holdings(owner, &holdings, fetched_at)
            .await?;

        if let Err(error) = self.name_locations(&holdings, &names, &token).await {
            warn!("could not name the locations of {}: {error:#}", reader.name);
        }
        if let Err(error) = self.name_types(owner).await {
            warn!(
                "could not name the item types of {}: {error:#}",
                reader.name
            );
        }
        Ok(fetched_at)
    }

    /// Stores names for the locations of some holdings that have none yet.
    async fn name_locations(
        &self,
        holdings: &[Holding],
        item_names: &HashMap<i64, String>,
        token: &str,
    ) -> Result<()> {
        let now = now();
        let locations: HashSet<i64> = holdings.iter().map(|h| h.location_id).collect();
        let mut public = Vec::new();
        let mut structures = Vec::new();
        for id in locations {
            if !self.store.needs_name(id, now - NAME_RETRY_SECS).await? {
                continue;
            }
            if assets::has_public_name(id) {
                public.push(id);
            } else if let Some(name) = item_names.get(&id) {
                // A structure or ship of the owner itself.
                self.store.set_name(id, Some(name), now).await?;
            } else {
                structures.push(id);
            }
        }

        if !public.is_empty() {
            match self.esi.names(&public).await {
                Ok(names) => {
                    for (id, name) in names {
                        self.store.set_name(id, Some(&name), now).await?;
                    }
                }
                Err(error) => warn!("could not look up station names: {error}"),
            }
        }

        for id in structures.into_iter().take(MAX_STRUCTURE_LOOKUPS) {
            match self.esi.structure_name(id, token).await {
                Ok(name) => self.store.set_name(id, Some(&name), now).await?,
                Err(EsiError::Throttled { .. }) => break,
                // Not allowed to dock there any more, or not a structure.
                Err(_) => self.store.set_name(id, None, now).await?,
            }
        }
        Ok(())
    }

    /// Stores names for the owner's item types that have none yet, so they
    /// can be suggested while typing.
    async fn name_types(&self, owner: Owner) -> Result<()> {
        let unnamed = self.store.unnamed_types(owner).await?;
        if unnamed.is_empty() {
            return Ok(());
        }
        let names = self
            .esi
            .names(&unnamed)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let types: Vec<(i64, String)> = names.into_iter().collect();
        self.store.add_types(&types).await
    }

    /// Turns the names of a pasted list into type ids and adds up the
    /// quantities per type, each multiplied by `times`.
    pub async fn resolve(&self, lines: &[Line], times: i64) -> Result<Resolved, EsiError> {
        // Lowercase name to type id, `None` while unknown.
        let mut ids: HashMap<String, Option<i64>> = HashMap::new();
        for line in lines {
            ids.insert(line.name.to_lowercase(), None);
            if let Some(whole) = &line.whole {
                ids.insert(whole.to_lowercase(), None);
            }
        }

        let mut unknown = Vec::new();
        for (name, id) in &mut ids {
            *id = self.store.type_id(name).await?;
            // ESI refuses names longer than 100 characters.
            if id.is_none() && name.chars().count() <= 100 {
                unknown.push(name.clone());
            }
        }
        if !unknown.is_empty() {
            let found = self.esi.types_by_name(&unknown).await?;
            self.store.add_types(&found).await?;
            for (type_id, name) in found {
                ids.insert(name.to_lowercase(), Some(type_id));
            }
        }

        let known = |name: &str| ids.get(&name.to_lowercase()).copied().flatten();
        // Sorted so the answer lists things in a stable order.
        let mut totals: BTreeMap<i64, i64> = BTreeMap::new();
        let mut unknown = Vec::new();
        for line in lines {
            let (type_id, quantity) = if let Some(type_id) = known(&line.name) {
                (type_id, line.quantity)
            } else if let Some(type_id) = line.whole.as_deref().and_then(known) {
                (type_id, 1)
            } else {
                let name = line.whole.as_ref().unwrap_or(&line.name);
                if !unknown.contains(name) {
                    unknown.push(name.clone());
                }
                continue;
            };
            let total = totals.entry(type_id).or_default();
            *total = total.saturating_add(quantity.saturating_mul(times));
        }
        Ok(Resolved {
            items: totals.into_iter().collect(),
            unknown,
        })
    }
}

/// Says in the user's words why assets could not be read.
fn describe(error: &FetchError, owner: Owner, reader: &Character) -> String {
    let name = &reader.name;
    match (error, owner) {
        (FetchError::Login(RefreshError::Revoked), _) => {
            format!("The EVE login of {name} no longer works. Link it again with `/assets auth`.")
        }
        (FetchError::Esi(EsiError::Unauthorized), Owner::Corporation(_)) => format!(
            "{name} may not read the assets of its corporation. That needs the Director role in game."
        ),
        (FetchError::Esi(EsiError::Unauthorized), Owner::Character(_)) => {
            format!("ESI refused the assets of {name}. Link it again with `/assets auth`.")
        }
        (FetchError::Esi(EsiError::Throttled { .. }), _) => {
            "ESI is rate limiting me right now. Try again in a few minutes.".to_owned()
        }
        _ => format!("I could not read the assets through {name} just now."),
    }
}
