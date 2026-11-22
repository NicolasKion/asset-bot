//! SQLite storage: linked characters, cached assets, names and stockpiles.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use serenity::model::id::{GuildId, UserId};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};

use crate::assets::{self, Holding};
use crate::esi::Owner;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS characters (
    character_id    INTEGER PRIMARY KEY,
    name            TEXT    NOT NULL,
    discord_user_id INTEGER NOT NULL,
    refresh_token   TEXT    NOT NULL,
    corporation_id  INTEGER NOT NULL DEFAULT 0,
    corp_assets     INTEGER NOT NULL DEFAULT 0,
    login_notified  INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS asset_sync (
    owner_kind TEXT    NOT NULL,
    owner_id   INTEGER NOT NULL,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (owner_kind, owner_id)
);
CREATE TABLE IF NOT EXISTS holdings (
    owner_kind  TEXT    NOT NULL,
    owner_id    INTEGER NOT NULL,
    location_id INTEGER NOT NULL,
    division    INTEGER,
    containers  TEXT    NOT NULL,
    in_ship     INTEGER NOT NULL DEFAULT 0,
    type_id     INTEGER NOT NULL,
    quantity    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS holdings_by_place
    ON holdings (owner_kind, owner_id, location_id);
CREATE TABLE IF NOT EXISTS names (
    id         INTEGER PRIMARY KEY,
    name       TEXT,
    checked_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS types (
    type_id INTEGER PRIMARY KEY,
    name    TEXT NOT NULL COLLATE NOCASE
);
CREATE INDEX IF NOT EXISTS types_by_name ON types (name);
CREATE TABLE IF NOT EXISTS stockpiles (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL COLLATE NOCASE,
    created_by  INTEGER NOT NULL,
    guild_id    INTEGER,
    source_kind TEXT    NOT NULL,
    source_id   INTEGER NOT NULL DEFAULT 0,
    location_id INTEGER NOT NULL,
    container   TEXT,
    division    INTEGER,
    count_ships INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS personal_stockpile_names
    ON stockpiles (created_by, name) WHERE guild_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS shared_stockpile_names
    ON stockpiles (guild_id, name) WHERE guild_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS stockpile_items (
    stockpile_id INTEGER NOT NULL REFERENCES stockpiles (id) ON DELETE CASCADE,
    type_id      INTEGER NOT NULL,
    quantity     INTEGER NOT NULL,
    PRIMARY KEY (stockpile_id, type_id)
);
";

/// Columns that were added after the first version, as table, column and
/// the statement that adds it to a database from before.
const ADDED_COLUMNS: [(&str, &str, &str); 2] = [
    (
        "holdings",
        "in_ship",
        "ALTER TABLE holdings ADD COLUMN in_ship INTEGER NOT NULL DEFAULT 0;
         DELETE FROM asset_sync;",
    ),
    (
        "stockpiles",
        "count_ships",
        "ALTER TABLE stockpiles ADD COLUMN count_ships INTEGER NOT NULL DEFAULT 0",
    ),
];

pub struct Character {
    pub character_id: i64,
    pub name: String,
    pub discord_user_id: UserId,
    pub refresh_token: String,
    pub corporation_id: i64,
    /// The login granted the scope to read the corporation's assets.
    pub corp_assets: bool,
}

impl Character {
    /// The EVE login stopped working and the character has to be linked again.
    /// Stored as an empty refresh token.
    pub fn login_broken(&self) -> bool {
        self.refresh_token.is_empty()
    }

    fn from_row(row: &SqliteRow) -> Result<Self> {
        let discord_user_id: i64 = row.try_get("discord_user_id")?;
        Ok(Self {
            character_id: row.try_get("character_id")?,
            name: row.try_get("name")?,
            discord_user_id: UserId::new(discord_user_id.cast_unsigned()),
            refresh_token: row.try_get("refresh_token")?,
            corporation_id: row.try_get("corporation_id")?,
            corp_assets: row.try_get("corp_assets")?,
        })
    }
}

/// Whether a login lets the bot read the corporation's assets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorpAssets {
    Readable,
    Unreadable,
}

/// Whose assets a stockpile is compared with. They are always read through
/// the characters of the stockpile's creator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Every character of the creator.
    AllCharacters,
    Character(i64),
    Corporation(i64),
}

impl Source {
    fn kind(self) -> &'static str {
        match self {
            Self::AllCharacters => "all",
            Self::Character(_) => "character",
            Self::Corporation(_) => "corporation",
        }
    }

    fn id(self) -> i64 {
        match self {
            Self::AllCharacters => 0,
            Self::Character(id) | Self::Corporation(id) => id,
        }
    }

    fn from_parts(kind: &str, id: i64) -> Result<Self> {
        match kind {
            "all" => Ok(Self::AllCharacters),
            "character" => Ok(Self::Character(id)),
            "corporation" => Ok(Self::Corporation(id)),
            other => bail!("unknown stockpile source {other}"),
        }
    }
}

/// What someone wants to have at a location.
#[derive(Debug, Clone)]
pub struct Stockpile {
    pub id: i64,
    pub name: String,
    pub created_by: UserId,
    /// The server a shared stockpile belongs to. `None` for a personal one.
    pub guild_id: Option<GuildId>,
    pub source: Source,
    pub location_id: i64,
    /// Only count what is inside the container with this name.
    pub container: Option<String>,
    /// Only count what is in this corporation hangar, 1 to 7.
    pub division: Option<u8>,
    /// Also count what is fitted to or loaded in ships. Without it only the
    /// hulls and what lies in hangars and containers count.
    pub count_ships: bool,
}

impl Stockpile {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let created_by: i64 = row.try_get("created_by")?;
        let guild_id: Option<i64> = row.try_get("guild_id")?;
        let source_kind: String = row.try_get("source_kind")?;
        Ok(Self {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            created_by: UserId::new(created_by.cast_unsigned()),
            guild_id: guild_id.map(|id| GuildId::new(id.cast_unsigned())),
            source: Source::from_parts(&source_kind, row.try_get("source_id")?)?,
            location_id: row.try_get("location_id")?,
            container: row.try_get("container")?,
            division: row.try_get("division")?,
            count_ships: row.try_get("count_ships")?,
        })
    }
}

/// A stockpile that is about to be created.
pub struct NewStockpile<'a> {
    pub name: &'a str,
    pub created_by: UserId,
    pub guild_id: Option<GuildId>,
    pub source: Source,
    pub location_id: i64,
    pub container: Option<&'a str>,
    pub division: Option<u8>,
    pub count_ships: bool,
}

/// One wanted type of a stockpile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub type_id: i64,
    pub name: String,
    pub quantity: i64,
}

fn owner_parts(owner: Owner) -> (&'static str, i64) {
    match owner {
        Owner::Character(id) => ("character", id),
        Owner::Corporation(id) => ("corporation", id),
    }
}

fn user_key(user: UserId) -> i64 {
    user.get().cast_signed()
}

fn guild_key(guild: GuildId) -> i64 {
    guild.get().cast_signed()
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true);
        // One connection keeps writes from the bot and the web server from
        // running into SQLITE_BUSY.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        for (table, column, statement) in ADDED_COLUMNS {
            let exists = sqlx::query("SELECT 1 FROM pragma_table_info(?) WHERE name = ?")
                .bind(table)
                .bind(column)
                .fetch_optional(&pool)
                .await?
                .is_some();
            if !exists {
                sqlx::raw_sql(statement).execute(&pool).await?;
            }
        }
        Ok(Self { pool })
    }

    // Characters

    /// Links a character to a Discord user, replacing an earlier link.
    pub async fn upsert_character(
        &self,
        character_id: i64,
        name: &str,
        owner: UserId,
        refresh_token: &str,
        corp_assets: CorpAssets,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO characters
                 (character_id, name, discord_user_id, refresh_token, corp_assets)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (character_id) DO UPDATE SET
                 name = excluded.name,
                 discord_user_id = excluded.discord_user_id,
                 refresh_token = excluded.refresh_token,
                 corp_assets = excluded.corp_assets,
                 login_notified = 0",
        )
        .bind(character_id)
        .bind(name)
        .bind(user_key(owner))
        .bind(refresh_token)
        .bind(corp_assets == CorpAssets::Readable)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Every linked character whose EVE login still works, as far as is known.
    pub async fn characters_with_login(&self) -> Result<Vec<Character>> {
        let rows =
            sqlx::query("SELECT * FROM characters WHERE refresh_token <> '' ORDER BY character_id")
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(Character::from_row).collect()
    }

    /// The characters whose EVE login stopped working and whose owner has
    /// not been told yet.
    pub async fn broken_logins_to_report(&self) -> Result<Vec<Character>> {
        let rows = sqlx::query(
            "SELECT * FROM characters
             WHERE refresh_token = '' AND login_notified = 0
             ORDER BY discord_user_id, name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Character::from_row).collect()
    }

    /// Remembers that the owner was told about a broken login.
    pub async fn set_login_notified(&self, character_id: i64) -> Result<()> {
        sqlx::query("UPDATE characters SET login_notified = 1 WHERE character_id = ?")
            .bind(character_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn characters_of(&self, owner: UserId) -> Result<Vec<Character>> {
        let rows = sqlx::query("SELECT * FROM characters WHERE discord_user_id = ? ORDER BY name")
            .bind(user_key(owner))
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Character::from_row).collect()
    }

    /// Removes one of the owner's characters and its cached assets. Returns
    /// false if they have no character with that name.
    pub async fn delete_character(&self, owner: UserId, name: &str) -> Result<bool> {
        let row = sqlx::query(
            "DELETE FROM characters
             WHERE discord_user_id = ? AND name = ? COLLATE NOCASE
             RETURNING character_id",
        )
        .bind(user_key(owner))
        .bind(name.trim())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let character_id: i64 = row.try_get("character_id")?;
        for table in [
            "DELETE FROM holdings WHERE owner_kind = 'character' AND owner_id = ?",
            "DELETE FROM asset_sync WHERE owner_kind = 'character' AND owner_id = ?",
        ] {
            sqlx::query(table)
                .bind(character_id)
                .execute(&self.pool)
                .await?;
        }
        Ok(true)
    }

    pub async fn set_refresh_token(&self, character_id: i64, refresh_token: &str) -> Result<()> {
        sqlx::query("UPDATE characters SET refresh_token = ? WHERE character_id = ?")
            .bind(refresh_token)
            .bind(character_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_corporation(&self, character_id: i64, corporation_id: i64) -> Result<()> {
        sqlx::query("UPDATE characters SET corporation_id = ? WHERE character_id = ?")
            .bind(corporation_id)
            .bind(character_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // Cached assets

    /// When the owner's assets were last read from ESI, as a Unix time.
    pub async fn fetched_at(&self, owner: Owner) -> Result<Option<i64>> {
        let (kind, id) = owner_parts(owner);
        let row =
            sqlx::query("SELECT fetched_at FROM asset_sync WHERE owner_kind = ? AND owner_id = ?")
                .bind(kind)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|row| row.try_get("fetched_at")).transpose()?)
    }

    /// Replaces everything cached for an owner with a fresh snapshot.
    pub async fn replace_holdings(
        &self,
        owner: Owner,
        holdings: &[Holding],
        fetched_at: i64,
    ) -> Result<()> {
        let (kind, id) = owner_parts(owner);
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM holdings WHERE owner_kind = ? AND owner_id = ?")
            .bind(kind)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for holding in holdings {
            sqlx::query(
                "INSERT INTO holdings
                     (owner_kind, owner_id, location_id, division, containers, in_ship, type_id, quantity)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(kind)
            .bind(id)
            .bind(holding.location_id)
            .bind(holding.division)
            .bind(&holding.containers)
            .bind(holding.in_ship)
            .bind(holding.type_id)
            .bind(holding.quantity)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "INSERT INTO asset_sync (owner_kind, owner_id, fetched_at) VALUES (?, ?, ?)
             ON CONFLICT (owner_kind, owner_id) DO UPDATE SET fetched_at = excluded.fetched_at",
        )
        .bind(kind)
        .bind(id)
        .bind(fetched_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// How much of each type the owners have at the stockpile's location,
    /// keyed by type id.
    pub async fn stock(
        &self,
        owners: &[Owner],
        stockpile: &Stockpile,
    ) -> Result<HashMap<i64, i64>> {
        let container = stockpile.container.as_deref().map(assets::container_key);
        let mut stock: HashMap<i64, i64> = HashMap::new();
        for &owner in owners {
            let (kind, id) = owner_parts(owner);
            let rows = sqlx::query(
                "SELECT type_id, SUM(quantity) AS quantity FROM holdings
                 WHERE owner_kind = ?1 AND owner_id = ?2 AND location_id = ?3
                   AND (?4 IS NULL OR division = ?4)
                   AND (?5 IS NULL OR instr(containers, ?5) > 0)
                   AND (?6 OR in_ship = 0)
                 GROUP BY type_id",
            )
            .bind(kind)
            .bind(id)
            .bind(stockpile.location_id)
            .bind(stockpile.division)
            .bind(container.as_deref())
            .bind(stockpile.count_ships)
            .fetch_all(&self.pool)
            .await?;
            for row in rows {
                let type_id: i64 = row.try_get("type_id")?;
                let quantity: i64 = row.try_get("quantity")?;
                *stock.entry(type_id).or_default() += quantity;
            }
        }
        Ok(stock)
    }

    /// The locations the owners have assets at, with the name if it is known.
    pub async fn locations(&self, owners: &[Owner]) -> Result<Vec<(i64, Option<String>)>> {
        let mut locations: HashMap<i64, Option<String>> = HashMap::new();
        for &owner in owners {
            let (kind, id) = owner_parts(owner);
            let rows = sqlx::query(
                "SELECT DISTINCT h.location_id, n.name FROM holdings h
                 LEFT JOIN names n ON n.id = h.location_id
                 WHERE h.owner_kind = ? AND h.owner_id = ?",
            )
            .bind(kind)
            .bind(id)
            .fetch_all(&self.pool)
            .await?;
            for row in rows {
                locations.insert(row.try_get("location_id")?, row.try_get("name")?);
            }
        }
        Ok(locations.into_iter().collect())
    }

    /// The type ids among the owner's assets that have no name stored yet.
    pub async fn unnamed_types(&self, owner: Owner) -> Result<Vec<i64>> {
        let (kind, id) = owner_parts(owner);
        let rows = sqlx::query(
            "SELECT DISTINCT h.type_id FROM holdings h
             LEFT JOIN types t ON t.type_id = h.type_id
             WHERE h.owner_kind = ? AND h.owner_id = ? AND t.type_id IS NULL",
        )
        .bind(kind)
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|row| Ok(row.try_get("type_id")?)).collect()
    }

    // Names of locations and corporations

    /// The stored name of a station, structure, solar system or corporation.
    pub async fn name(&self, id: i64) -> Result<Option<String>> {
        let row = sqlx::query("SELECT name FROM names WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| row.try_get("name")).transpose()?.flatten())
    }

    /// Whether a name lookup is due: the id was never looked up, or the
    /// lookup failed before `retry_failed_before`.
    pub async fn needs_name(&self, id: i64, retry_failed_before: i64) -> Result<bool> {
        let row = sqlx::query("SELECT name IS NULL AS failed, checked_at FROM names WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(true);
        };
        let failed: bool = row.try_get("failed")?;
        let checked_at: i64 = row.try_get("checked_at")?;
        Ok(failed && checked_at < retry_failed_before)
    }

    /// Stores a name, or with `None` that the lookup failed.
    pub async fn set_name(&self, id: i64, name: Option<&str>, checked_at: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO names (id, name, checked_at) VALUES (?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 name = COALESCE(excluded.name, names.name),
                 checked_at = excluded.checked_at",
        )
        .bind(id)
        .bind(name)
        .bind(checked_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // Item types

    pub async fn add_types(&self, types: &[(i64, String)]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for (type_id, name) in types {
            sqlx::query(
                "INSERT INTO types (type_id, name) VALUES (?, ?)
                 ON CONFLICT (type_id) DO UPDATE SET name = excluded.name",
            )
            .bind(type_id)
            .bind(name)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The id of a type by its exact name, ignoring case.
    pub async fn type_id(&self, name: &str) -> Result<Option<i64>> {
        let row = sqlx::query("SELECT type_id FROM types WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| row.try_get("type_id")).transpose()?)
    }

    /// Names of known types that contain the text, best matches first.
    pub async fn search_types(&self, text: &str, limit: u32) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT name FROM types
             WHERE instr(lower(name), lower(?1)) > 0
             ORDER BY instr(lower(name), lower(?1)), length(name), name
             LIMIT ?2",
        )
        .bind(text)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(|row| Ok(row.try_get("name")?)).collect()
    }

    // Stockpiles

    /// Creates a stockpile. Returns `None` if the name is already taken, among
    /// the creator's personal ones or among the server's shared ones.
    pub async fn create_stockpile(&self, new: &NewStockpile<'_>) -> Result<Option<i64>> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO stockpiles
                 (name, created_by, guild_id, source_kind, source_id, location_id, container, division,
                  count_ships)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(new.name)
        .bind(user_key(new.created_by))
        .bind(new.guild_id.map(guild_key))
        .bind(new.source.kind())
        .bind(new.source.id())
        .bind(new.location_id)
        .bind(new.container)
        .bind(new.division)
        .bind(new.count_ships)
        .execute(&self.pool)
        .await?;
        Ok((result.rows_affected() > 0).then(|| result.last_insert_rowid()))
    }

    pub async fn stockpile(&self, id: i64) -> Result<Option<Stockpile>> {
        let row = sqlx::query("SELECT * FROM stockpiles WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(Stockpile::from_row).transpose()
    }

    /// The user's personal stockpiles, then the shared ones of the server.
    pub async fn stockpiles(&self, user: UserId, guild: Option<GuildId>) -> Result<Vec<Stockpile>> {
        let rows = sqlx::query(
            "SELECT * FROM stockpiles
             WHERE (guild_id IS NULL AND created_by = ?1) OR guild_id = ?2
             ORDER BY guild_id IS NOT NULL, name",
        )
        .bind(user_key(user))
        .bind(guild.map(guild_key))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Stockpile::from_row).collect()
    }

    /// Makes one stockpile want everything another one wants as well.
    pub async fn copy_items(&self, from: i64, to: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO stockpile_items (stockpile_id, type_id, quantity)
             SELECT ?, type_id, quantity FROM stockpile_items WHERE stockpile_id = ?",
        )
        .bind(to)
        .bind(from)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Sets whether a stockpile counts what is in ships.
    pub async fn set_count_ships(&self, id: i64, count_ships: bool) -> Result<()> {
        sqlx::query("UPDATE stockpiles SET count_ships = ? WHERE id = ?")
            .bind(count_ships)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_stockpile(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM stockpiles WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The wanted items of a stockpile, sorted by name.
    pub async fn items(&self, stockpile_id: i64) -> Result<Vec<Item>> {
        let rows = sqlx::query(
            "SELECT i.type_id, i.quantity, COALESCE(t.name, 'Type ' || i.type_id) AS name
             FROM stockpile_items i
             LEFT JOIN types t ON t.type_id = i.type_id
             WHERE i.stockpile_id = ?
             ORDER BY name",
        )
        .bind(stockpile_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(Item {
                    type_id: row.try_get("type_id")?,
                    name: row.try_get("name")?,
                    quantity: row.try_get("quantity")?,
                })
            })
            .collect()
    }

    /// Sets the wanted quantity of one type. Zero removes it.
    pub async fn set_item(&self, stockpile_id: i64, type_id: i64, quantity: i64) -> Result<()> {
        if quantity <= 0 {
            sqlx::query("DELETE FROM stockpile_items WHERE stockpile_id = ? AND type_id = ?")
                .bind(stockpile_id)
                .bind(type_id)
                .execute(&self.pool)
                .await?;
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO stockpile_items (stockpile_id, type_id, quantity) VALUES (?, ?, ?)
             ON CONFLICT (stockpile_id, type_id) DO UPDATE SET quantity = excluded.quantity",
        )
        .bind(stockpile_id)
        .bind(type_id)
        .bind(quantity)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Puts quantities into a stockpile. `import` says what happens to the
    /// quantities it wants already.
    pub async fn import_items(
        &self,
        stockpile_id: i64,
        items: &[(i64, i64)],
        import: Import,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        if import == Import::Replace {
            sqlx::query("DELETE FROM stockpile_items WHERE stockpile_id = ?")
                .bind(stockpile_id)
                .execute(&mut *tx)
                .await?;
        }
        let statement = match import {
            Import::Add | Import::Replace => {
                "INSERT INTO stockpile_items (stockpile_id, type_id, quantity) VALUES (?, ?, ?)
                 ON CONFLICT (stockpile_id, type_id) DO UPDATE SET
                     quantity = stockpile_items.quantity + excluded.quantity"
            }
            Import::KeepHigher => {
                "INSERT INTO stockpile_items (stockpile_id, type_id, quantity) VALUES (?, ?, ?)
                 ON CONFLICT (stockpile_id, type_id) DO UPDATE SET
                     quantity = MAX(stockpile_items.quantity, excluded.quantity)"
            }
        };
        for &(type_id, quantity) in items {
            sqlx::query(statement)
                .bind(stockpile_id)
                .bind(type_id)
                .bind(quantity)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// What a pasted list does with the items a stockpile already wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Import {
    /// Wanting 5000 and pasting 5000 wants 10000.
    Add,
    /// Wanting 5000 and pasting 3000 still wants 5000, pasting 8000 wants 8000.
    KeepHigher,
    /// Forgets the whole list first.
    Replace,
}

#[cfg(test)]
mod tests {
    use super::*;

    const JITA: i64 = 60_003_760;
    const USER: UserId = UserId::new(1);
    const GUILD: GuildId = GuildId::new(7);

    fn holding(division: Option<u8>, container: &str, type_id: i64, quantity: i64) -> Holding {
        Holding {
            location_id: JITA,
            division,
            containers: if container.is_empty() {
                String::new()
            } else {
                assets::container_key(container)
            },
            in_ship: false,
            type_id,
            quantity,
        }
    }

    fn new_stockpile(name: &str, guild_id: Option<GuildId>) -> NewStockpile<'_> {
        NewStockpile {
            name,
            created_by: USER,
            guild_id,
            source: Source::AllCharacters,
            location_id: JITA,
            container: None,
            division: None,
            count_ships: false,
        }
    }

    async fn store() -> Store {
        Store::open(Path::new(":memory:"))
            .await
            .expect("an in-memory database opens")
    }

    #[tokio::test]
    async fn stock_adds_up_owners_and_narrows() -> Result<()> {
        let store = store().await;
        let pilot = Owner::Character(10);
        let corporation = Owner::Corporation(20);
        store
            .replace_holdings(
                pilot,
                &[holding(None, "", 34, 100), holding(None, "Ammo Box", 34, 5)],
                1,
            )
            .await?;
        store
            .replace_holdings(
                corporation,
                &[holding(Some(2), "", 34, 1000), holding(Some(3), "", 34, 7)],
                1,
            )
            .await?;

        let id = store
            .create_stockpile(&new_stockpile("Everything", None))
            .await?
            .expect("the name is free");
        let mut stockpile = store.stockpile(id).await?.expect("it was just created");

        let all = store.stock(&[pilot, corporation], &stockpile).await?;
        assert_eq!(all.get(&34), Some(&1112));

        stockpile.container = Some("ammo box".to_owned());
        let boxed = store.stock(&[pilot, corporation], &stockpile).await?;
        assert_eq!(boxed.get(&34), Some(&5));

        stockpile.container = None;
        stockpile.division = Some(3);
        let hangar = store.stock(&[pilot, corporation], &stockpile).await?;
        assert_eq!(hangar.get(&34), Some(&7));

        // What is in ships only counts when the stockpile asks for it.
        let fitted = Holding {
            in_ship: true,
            ..holding(None, "", 34, 40)
        };
        store
            .replace_holdings(Owner::Character(11), &[fitted], 1)
            .await?;
        stockpile.division = None;
        let without = store.stock(&[Owner::Character(11)], &stockpile).await?;
        assert_eq!(without.get(&34), None);
        stockpile.count_ships = true;
        let with = store.stock(&[Owner::Character(11)], &stockpile).await?;
        assert_eq!(with.get(&34), Some(&40));

        // A new snapshot replaces the old one.
        store.replace_holdings(pilot, &[], 2).await?;
        stockpile.division = None;
        let later = store.stock(&[pilot], &stockpile).await?;
        assert_eq!(later.get(&34), None);
        assert_eq!(store.fetched_at(pilot).await?, Some(2));
        Ok(())
    }

    #[tokio::test]
    async fn stockpile_names_are_unique_per_owner() -> Result<()> {
        let store = store().await;
        assert!(
            store
                .create_stockpile(&new_stockpile("Staging", None))
                .await?
                .is_some()
        );
        assert!(
            store
                .create_stockpile(&new_stockpile("staging", None))
                .await?
                .is_none()
        );
        // The same name can exist once more as a shared one.
        assert!(
            store
                .create_stockpile(&new_stockpile("Staging", Some(GUILD)))
                .await?
                .is_some()
        );
        assert!(
            store
                .create_stockpile(&new_stockpile("Staging", Some(GUILD)))
                .await?
                .is_none()
        );

        assert_eq!(store.stockpiles(USER, None).await?.len(), 1);
        let here = store.stockpiles(USER, Some(GUILD)).await?;
        assert_eq!(here.len(), 2);
        assert!(here[0].guild_id.is_none(), "personal ones come first");
        assert_eq!(
            store.stockpiles(UserId::new(2), Some(GUILD)).await?.len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn items_can_be_imported_set_and_removed() -> Result<()> {
        let store = store().await;
        store
            .add_types(&[(34, "Tritanium".to_owned()), (16227, "Ferox".to_owned())])
            .await?;
        assert_eq!(store.type_id("ferox").await?, Some(16227));
        assert_eq!(store.search_types("ITAN", 5).await?, vec!["Tritanium"]);

        let id = store
            .create_stockpile(&new_stockpile("Staging", None))
            .await?
            .expect("the name is free");
        store
            .import_items(id, &[(34, 100), (16227, 2)], Import::Add)
            .await?;
        store.import_items(id, &[(34, 50)], Import::Add).await?;
        let wanted: Vec<(String, i64)> = store
            .items(id)
            .await?
            .into_iter()
            .map(|item| (item.name, item.quantity))
            .collect();
        assert_eq!(
            wanted,
            vec![("Ferox".to_owned(), 2), ("Tritanium".to_owned(), 150)]
        );

        store
            .import_items(id, &[(34, 100), (16227, 9)], Import::KeepHigher)
            .await?;
        let wanted: Vec<i64> = store.items(id).await?.iter().map(|i| i.quantity).collect();
        assert_eq!(wanted, vec![9, 150], "the higher of old and new");

        let copy = store
            .create_stockpile(&new_stockpile("Copy", None))
            .await?
            .expect("the name is free");
        store.copy_items(id, copy).await?;
        assert_eq!(store.items(copy).await?, store.items(id).await?);

        store.set_item(id, 16227, 0).await?;
        store.import_items(id, &[(34, 1)], Import::Replace).await?;
        let wanted = store.items(id).await?;
        assert_eq!(wanted.len(), 1);
        assert_eq!(wanted[0].quantity, 1);

        store.delete_stockpile(id).await?;
        assert!(store.items(id).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn characters_and_names() -> Result<()> {
        let store = store().await;
        store
            .upsert_character(10, "Pilot One", USER, "token", CorpAssets::Readable)
            .await?;
        store.set_corporation(10, 20).await?;
        store
            .replace_holdings(Owner::Character(10), &[holding(None, "", 34, 1)], 1)
            .await?;

        let characters = store.characters_of(USER).await?;
        assert_eq!(characters.len(), 1);
        assert!(characters[0].corp_assets);
        assert_eq!(characters[0].corporation_id, 20);

        assert!(store.needs_name(JITA, 0).await?);
        store.set_name(JITA, None, 100).await?;
        assert!(!store.needs_name(JITA, 50).await?, "a failed lookup waits");
        assert!(store.needs_name(JITA, 200).await?);
        store.set_name(JITA, Some("Jita 4-4"), 300).await?;
        assert_eq!(store.name(JITA).await?.as_deref(), Some("Jita 4-4"));
        assert_eq!(
            store.locations(&[Owner::Character(10)]).await?,
            vec![(JITA, Some("Jita 4-4".to_owned()))]
        );
        assert_eq!(store.unnamed_types(Owner::Character(10)).await?, vec![34]);

        // A login that broke is reported once, and linking again resets that.
        assert_eq!(store.characters_with_login().await?.len(), 1);
        store.set_refresh_token(10, "").await?;
        assert!(store.characters_with_login().await?.is_empty());
        let broken = store.broken_logins_to_report().await?;
        assert_eq!(broken.len(), 1);
        assert_eq!(broken[0].discord_user_id, USER);
        store.set_login_notified(10).await?;
        assert!(store.broken_logins_to_report().await?.is_empty());
        store
            .upsert_character(10, "Pilot One", USER, "", CorpAssets::Readable)
            .await?;
        assert_eq!(store.broken_logins_to_report().await?.len(), 1);

        assert!(store.delete_character(USER, "pilot one").await?);
        assert!(!store.delete_character(USER, "pilot one").await?);
        assert_eq!(store.fetched_at(Owner::Character(10)).await?, None);
        Ok(())
    }
}
