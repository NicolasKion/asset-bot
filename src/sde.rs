//! Keeps the names of all item types current from CCP's static data API, so
//! every item can be suggested and found by name without asking ESI.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use tracing::{info, warn};

use crate::app::App;

/// Redirects to the current release and lists its tables.
const LATEST_URL: &str = "https://sde.evetech.net/tranquility/latest/";

/// How long to wait between two looks at the static data. It changes with
/// game patches, and a look that finds nothing new is one small request.
const CHECK_INTERVAL: Duration = Duration::from_hours(12);

/// How many types go into the database in one transaction.
const BATCH: usize = 5000;

/// A line of the types table can hold descriptions in eight languages. This
/// only stops a broken download from eating all memory.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Setting that remembers which version of the types table is stored.
const VERSION_KEY: &str = "sde_types_version";

#[derive(Deserialize)]
struct Release {
    tables: Tables,
}

#[derive(Deserialize)]
struct Tables {
    types: Table,
}

#[derive(Deserialize)]
struct Table {
    /// Changes only when the data of the table changes.
    version: String,
    jsonl: Jsonl,
}

#[derive(Deserialize)]
struct Jsonl {
    url: String,
}

#[derive(Deserialize)]
struct Type {
    #[serde(rename = "typeID")]
    type_id: i64,
    name: Name,
    #[serde(default)]
    published: bool,
}

#[derive(Deserialize)]
struct Name {
    en: Option<String>,
}

/// Loads the type names now and again whenever they change. Runs until the
/// process ends.
pub async fn run(app: Arc<App>, http: Client) {
    loop {
        if let Err(error) = sync_types(&app, &http).await {
            warn!("could not update item types from the static data: {error:#}");
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

/// Stores the names of all published types, unless the stored ones are current.
async fn sync_types(app: &App, http: &Client) -> Result<()> {
    let release: Release = http
        .get(LATEST_URL)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("asking for the current static data release")?
        .json()
        .await
        .context("reading the static data release")?;
    let table = release.tables.types;
    if app.store.setting(VERSION_KEY).await?.as_deref() == Some(table.version.as_str()) {
        return Ok(());
    }

    let mut response = http
        .get(&table.jsonl.url)
        // The table is big, and the client's usual timeout covers the whole body.
        .timeout(Duration::from_mins(10))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("downloading the types table")?;

    // The table is read line by line as it arrives instead of held in memory.
    let mut pending: Vec<u8> = Vec::new();
    let mut batch: Vec<(i64, String)> = Vec::with_capacity(BATCH);
    let mut stored = 0_usize;
    while let Some(chunk) = response.chunk().await.context("reading the types table")? {
        pending.extend_from_slice(&chunk);
        let mut start = 0;
        while let Some(end) = pending[start..].iter().position(|&byte| byte == b'\n') {
            batch.extend(published_type(&pending[start..start + end])?);
            start += end + 1;
            if batch.len() >= BATCH {
                app.store.add_types(&batch).await?;
                stored += batch.len();
                batch.clear();
            }
        }
        pending.drain(..start);
        if pending.len() > MAX_LINE_BYTES {
            bail!("a line of the types table is longer than {MAX_LINE_BYTES} bytes");
        }
    }
    batch.extend(published_type(&pending)?);
    app.store.add_types(&batch).await?;
    stored += batch.len();

    app.store.set_setting(VERSION_KEY, &table.version).await?;
    info!("stored the names of {stored} item types from the static data");
    Ok(())
}

/// Id and English name of the type on one line, if it is one players can have.
fn published_type(line: &[u8]) -> Result<Option<(i64, String)>> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let record: Type = serde_json::from_slice(line).context("reading a line of the types table")?;
    Ok(record
        .name
        .en
        .filter(|name| record.published && !name.trim().is_empty())
        .map(|name| (record.type_id, name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_published_types_with_a_name_are_kept() -> Result<()> {
        let rifter =
            br#"{"typeID":587,"groupID":25,"name":{"de":"Rifter","en":"Rifter"},"published":true}"#;
        assert_eq!(published_type(rifter)?, Some((587, "Rifter".to_owned())));

        let hidden = br#"{"typeID":1,"groupID":1,"name":{"en":"Old Thing"},"published":false}"#;
        assert_eq!(published_type(hidden)?, None);
        let unnamed = br#"{"typeID":2,"groupID":1,"name":{"de":"Ding"},"published":true}"#;
        assert_eq!(published_type(unnamed)?, None);
        assert_eq!(published_type(b"  ")?, None);
        assert!(published_type(b"{not json").is_err());
        Ok(())
    }
}
