//! SQLite storage for linked characters and the contacts the bot set on them.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serenity::model::id::{ChannelId, GuildId, UserId};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS characters (
    character_id     INTEGER PRIMARY KEY,
    name             TEXT    NOT NULL,
    discord_user_id  INTEGER NOT NULL,
    refresh_token    TEXT    NOT NULL,
    unlink_requested INTEGER NOT NULL DEFAULT 0,
    reads_access_lists INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS managed_contacts (
    character_id      INTEGER NOT NULL REFERENCES characters (character_id) ON DELETE CASCADE,
    contact_id        INTEGER NOT NULL,
    previous_standing REAL,
    PRIMARY KEY (character_id, contact_id)
);
CREATE TABLE IF NOT EXISTS blue_contacts (
    contact_id INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    kind       TEXT    NOT NULL
);
";

pub struct Character {
    pub character_id: i64,
    pub name: String,
    pub discord_user_id: UserId,
    pub refresh_token: String,
    /// The owner asked to unlink; the row goes away once its contacts are cleaned up.
    pub unlink_requested: bool,
    /// The login granted the scope to read access lists.
    pub reads_access_lists: bool,
    /// Added by an admin without an EVE login. The others set it blue, but
    /// the bot cannot touch its contacts. Stored as an empty refresh token.
    pub manual: bool,
}

/// Where the bot was set up with `/setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Home {
    pub guild_id: GuildId,
    /// Members who can see this channel form the standings network.
    pub channel_id: ChannelId,
}

/// The access list the network is checked against.
pub struct TrackedList {
    pub id: i64,
    pub name: String,
}

impl Character {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let discord_user_id: i64 = row.try_get("discord_user_id")?;
        let refresh_token: String = row.try_get("refresh_token")?;
        Ok(Self {
            character_id: row.try_get("character_id")?,
            name: row.try_get("name")?,
            discord_user_id: UserId::new(discord_user_id.cast_unsigned()),
            manual: refresh_token.is_empty(),
            refresh_token,
            unlink_requested: row.try_get("unlink_requested")?,
            reads_access_lists: row.try_get("reads_access_lists")?,
        })
    }
}

/// A contact the bot put on a character, with the standing it had before.
///
/// `previous_standing` is `None` when the bot created the contact, so removing
/// it means deleting the contact instead of restoring a standing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ManagedContact {
    pub contact_id: i64,
    pub previous_standing: Option<f64>,
}

/// What kind of entity a blue contact is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlueKind {
    Corporation,
    Alliance,
}

impl BlueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Corporation => "corporation",
            Self::Alliance => "alliance",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "corporation" => Ok(Self::Corporation),
            "alliance" => Ok(Self::Alliance),
            other => anyhow::bail!("unknown blue contact kind {other}"),
        }
    }
}

/// A corporation or alliance every network character sets blue, added by an admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlueContact {
    pub contact_id: i64,
    pub name: String,
    pub kind: BlueKind,
}

/// Whether a login lets the bot read the character's access lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessLists {
    Readable,
    Unreadable,
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
        // One connection keeps writes from the bot, the web server and the sync
        // from running into SQLITE_BUSY.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }

    /// Links a character to a Discord user, replacing an earlier link.
    pub async fn upsert_character(
        &self,
        character_id: i64,
        name: &str,
        owner: UserId,
        refresh_token: &str,
        access_lists: AccessLists,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO characters
                 (character_id, name, discord_user_id, refresh_token, reads_access_lists)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (character_id) DO UPDATE SET
                 name = excluded.name,
                 discord_user_id = excluded.discord_user_id,
                 refresh_token = excluded.refresh_token,
                 reads_access_lists = excluded.reads_access_lists,
                 unlink_requested = 0",
        )
        .bind(character_id)
        .bind(name)
        .bind(owner.get().cast_signed())
        .bind(refresh_token)
        .bind(access_lists == AccessLists::Readable)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn home(&self) -> Result<Option<Home>> {
        let guild = self.setting("guild_id").await?;
        let channel = self.setting("channel_id").await?;
        let ids = guild
            .and_then(|id| id.parse::<u64>().ok())
            .zip(channel.and_then(|id| id.parse::<u64>().ok()))
            .filter(|&(guild, channel)| guild != 0 && channel != 0);
        Ok(ids.map(|(guild, channel)| Home {
            guild_id: GuildId::new(guild),
            channel_id: ChannelId::new(channel),
        }))
    }

    pub async fn set_home(&self, home: Home) -> Result<()> {
        self.set_setting("guild_id", &home.guild_id.to_string())
            .await?;
        self.set_setting("channel_id", &home.channel_id.to_string())
            .await
    }

    /// Id of the pinned status message in the home channel.
    pub async fn status_message(&self) -> Result<Option<u64>> {
        let id = self.setting("status_message_id").await?;
        Ok(id.and_then(|id| id.parse().ok()).filter(|&id| id != 0))
    }

    pub async fn set_status_message(&self, id: u64) -> Result<()> {
        self.set_setting("status_message_id", &id.to_string()).await
    }

    /// Forgets the status message, so the next update posts a new one.
    pub async fn clear_status_message(&self) -> Result<()> {
        self.set_setting("status_message_id", "0").await
    }

    /// Adds a character without an EVE login. Returns false if the character
    /// is already linked.
    pub async fn add_manual_character(
        &self,
        character_id: i64,
        name: &str,
        owner: UserId,
    ) -> Result<bool> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO characters
                 (character_id, name, discord_user_id, refresh_token)
             VALUES (?, ?, ?, '')",
        )
        .bind(character_id)
        .bind(name)
        .bind(owner.get().cast_signed())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn tracked_list(&self) -> Result<Option<TrackedList>> {
        let id = self.setting("access_list_id").await?;
        let name = self.setting("access_list_name").await?;
        Ok(id
            .and_then(|id| id.parse().ok())
            .zip(name)
            .map(|(id, name)| TrackedList { id, name }))
    }

    pub async fn set_tracked_list(&self, list: &TrackedList) -> Result<()> {
        self.set_setting("access_list_id", &list.id.to_string())
            .await?;
        self.set_setting("access_list_name", &list.name).await
    }

    async fn setting(&self, key: &'static str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| row.try_get("value")).transpose()?)
    }

    async fn set_setting(&self, key: &'static str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn characters(&self) -> Result<Vec<Character>> {
        let rows = sqlx::query("SELECT * FROM characters ORDER BY character_id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(Character::from_row).collect()
    }

    pub async fn characters_of(&self, owner: UserId) -> Result<Vec<Character>> {
        let rows = sqlx::query(
            "SELECT * FROM characters
             WHERE discord_user_id = ? AND unlink_requested = 0
             ORDER BY name",
        )
        .bind(owner.get().cast_signed())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(Character::from_row).collect()
    }

    /// Marks one of the owner's characters for removal. Returns false if they
    /// have no character with that name.
    pub async fn request_unlink(&self, owner: UserId, name: &str) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE characters SET unlink_requested = 1
             WHERE discord_user_id = ? AND name = ? COLLATE NOCASE",
        )
        .bind(owner.get().cast_signed())
        .bind(name.trim())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The Discord user a linked character belongs to.
    pub async fn owner_of(&self, name: &str) -> Result<Option<UserId>> {
        let row = sqlx::query(
            "SELECT discord_user_id FROM characters
             WHERE name = ? COLLATE NOCASE AND unlink_requested = 0",
        )
        .bind(name.trim())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id: i64 = row.try_get("discord_user_id")?;
        Ok(Some(UserId::new(id.cast_unsigned())))
    }

    /// Marks every character of a user for removal and returns how many.
    pub async fn request_unlink_all(&self, owner: UserId) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE characters SET unlink_requested = 1
             WHERE discord_user_id = ? AND unlink_requested = 0",
        )
        .bind(owner.get().cast_signed())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn set_refresh_token(&self, character_id: i64, refresh_token: &str) -> Result<()> {
        sqlx::query("UPDATE characters SET refresh_token = ? WHERE character_id = ?")
            .bind(refresh_token)
            .bind(character_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_character(&self, character_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM characters WHERE character_id = ?")
            .bind(character_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Contacts the bot manages on a character, keyed by contact id.
    pub async fn managed_contacts(&self, character_id: i64) -> Result<HashMap<i64, Option<f64>>> {
        let rows = sqlx::query(
            "SELECT contact_id, previous_standing FROM managed_contacts WHERE character_id = ?",
        )
        .bind(character_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get("contact_id")?,
                    row.try_get("previous_standing")?,
                ))
            })
            .collect()
    }

    pub async fn add_managed(&self, character_id: i64, contacts: &[ManagedContact]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for contact in contacts {
            sqlx::query(
                "INSERT OR IGNORE INTO managed_contacts (character_id, contact_id, previous_standing)
                 VALUES (?, ?, ?)",
            )
            .bind(character_id)
            .bind(contact.contact_id)
            .bind(contact.previous_standing)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn remove_managed(&self, character_id: i64, contact_ids: &[i64]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for contact_id in contact_ids {
            sqlx::query("DELETE FROM managed_contacts WHERE character_id = ? AND contact_id = ?")
                .bind(character_id)
                .bind(contact_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn blue_contacts(&self) -> Result<Vec<BlueContact>> {
        let rows = sqlx::query("SELECT * FROM blue_contacts ORDER BY name")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                let kind: String = row.try_get("kind")?;
                Ok(BlueContact {
                    contact_id: row.try_get("contact_id")?,
                    name: row.try_get("name")?,
                    kind: BlueKind::parse(&kind)?,
                })
            })
            .collect()
    }

    /// Returns false if the contact is already on the list.
    pub async fn add_blue(&self, contact: &BlueContact) -> Result<bool> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO blue_contacts (contact_id, name, kind) VALUES (?, ?, ?)",
        )
        .bind(contact.contact_id)
        .bind(&contact.name)
        .bind(contact.kind.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Returns false if no blue contact has that name.
    pub async fn remove_blue(&self, name: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM blue_contacts WHERE name = ? COLLATE NOCASE")
            .bind(name.trim())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}
