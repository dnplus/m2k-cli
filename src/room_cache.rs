use crate::config::{self, Config};
use crate::meeting::MeetingRoom;
use crate::model::{Item, Kind};
use crate::sqlite;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

pub const TTL_SECONDS: i64 = 600;
pub const LOOKAHEAD_DAYS: i64 = 30;
const CACHE_FILE_NAME: &str = "room_cache.sqlite3";
const SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
pub struct CachedRoomCalendar {
    pub items: Vec<Item>,
    pub coverage_start: DateTime<Utc>,
    pub coverage_end: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
}

pub struct RoomCalendarCache {
    conn: Connection,
    path: PathBuf,
}

impl RoomCalendarCache {
    pub fn open(data_dir: &Path) -> Result<Self> {
        fs::create_dir_all(data_dir).context("無法建立會議室快取目錄")?;
        private_permissions(data_dir, 0o700)?;
        let path = data_dir.join(CACHE_FILE_NAME);
        let (conn, _initialization) = sqlite::open(&path).context("無法開啟會議室快取資料庫")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("會議室快取 schema 版本較新，請更新 m2k");
        }
        if version < SCHEMA_VERSION {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS room_calendars (
                 cache_key TEXT PRIMARY KEY NOT NULL,
                 account_key TEXT NOT NULL,
                 calendar_url TEXT NOT NULL,
                 coverage_start TEXT NOT NULL,
                 coverage_end TEXT NOT NULL,
                 fetched_at TEXT NOT NULL,
                 items_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS room_catalogs (
                 account_key TEXT PRIMARY KEY NOT NULL,
                 fetched_at TEXT NOT NULL,
                 rooms_json TEXT NOT NULL
             );",
            )?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
        }
        Ok(Self { conn, path })
    }

    pub fn database_path(&self) -> &Path {
        &self.path
    }

    pub fn lookup(
        &self,
        config: &Config,
        calendar_url: &str,
        query_start: DateTime<Utc>,
        query_end: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Option<CachedRoomCalendar>> {
        if query_start >= query_end {
            return Ok(None);
        }
        let account_key = config::credential_key(config)?;
        let cache_key = calendar_key(&account_key, calendar_url)?;
        let stored: Option<[Value; 6]> = self
            .conn
            .query_row(
                "SELECT account_key, calendar_url, coverage_start, coverage_end, fetched_at, items_json
                 FROM room_calendars WHERE cache_key = ?1",
                params![cache_key],
                |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?]),
            )
            .optional()?;
        let Some(
            [Value::Text(stored_account), Value::Text(stored_url), Value::Text(start), Value::Text(end), Value::Text(fetched), Value::Text(payload)],
        ) = stored
        else {
            return Ok(None);
        };
        if stored_account != account_key || stored_url != calendar_url {
            return Ok(None);
        }
        let Some(coverage_start) = parse_time(&start) else {
            return Ok(None);
        };
        let Some(coverage_end) = parse_time(&end) else {
            return Ok(None);
        };
        let Some(fetched_at) = parse_time(&fetched) else {
            return Ok(None);
        };
        if !is_fresh(fetched_at, now)
            || coverage_start >= coverage_end
            || coverage_start > query_start
            || coverage_end < query_end
        {
            return Ok(None);
        }
        let Ok(items) = serde_json::from_str::<Vec<Item>>(&payload) else {
            return Ok(None);
        };
        if !valid_items(&items, calendar_url) {
            return Ok(None);
        }
        Ok(Some(CachedRoomCalendar {
            items,
            coverage_start,
            coverage_end,
            fetched_at,
        }))
    }

    pub fn store_success(
        &self,
        config: &Config,
        calendar_url: &str,
        coverage_start: DateTime<Utc>,
        coverage_end: DateTime<Utc>,
        fetched_at: DateTime<Utc>,
        items: &[Item],
    ) -> Result<()> {
        if coverage_start >= coverage_end {
            bail!("會議室快取範圍必須由早到晚");
        }
        if calendar_url.trim().is_empty() || !valid_items(items, calendar_url) {
            bail!("會議室快取必須包含對應行事曆的完整資料");
        }
        let account_key = config::credential_key(config)?;
        let cache_key = calendar_key(&account_key, calendar_url)?;
        let payload = serde_json::to_string(items).context("無法序列化會議室行事曆快取")?;
        self.conn.execute(
            "INSERT INTO room_calendars (
                 cache_key, account_key, calendar_url, coverage_start, coverage_end, fetched_at, items_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(cache_key) DO UPDATE SET
                 account_key = excluded.account_key,
                 calendar_url = excluded.calendar_url,
                 coverage_start = excluded.coverage_start,
                 coverage_end = excluded.coverage_end,
                 fetched_at = excluded.fetched_at,
                 items_json = excluded.items_json",
            params![
                cache_key,
                account_key,
                calendar_url,
                coverage_start.to_rfc3339(),
                coverage_end.to_rfc3339(),
                fetched_at.to_rfc3339(),
                payload,
            ],
        )?;
        Ok(())
    }

    pub fn lookup_catalog(
        &self,
        config: &Config,
        now: DateTime<Utc>,
    ) -> Result<Option<Vec<MeetingRoom>>> {
        let account_key = config::credential_key(config)?;
        let stored: Option<[Value; 2]> = self
            .conn
            .query_row(
                "SELECT fetched_at, rooms_json FROM room_catalogs WHERE account_key = ?1",
                params![account_key],
                |row| Ok([row.get(0)?, row.get(1)?]),
            )
            .optional()?;
        let Some([Value::Text(fetched), Value::Text(payload)]) = stored else {
            return Ok(None);
        };
        let Some(fetched_at) = parse_time(&fetched) else {
            return Ok(None);
        };
        if !is_fresh(fetched_at, now) {
            return Ok(None);
        }
        let Ok(rooms) = serde_json::from_str::<Vec<MeetingRoom>>(&payload) else {
            return Ok(None);
        };
        if !valid_rooms(&rooms) {
            return Ok(None);
        }
        Ok(Some(rooms))
    }

    pub fn store_catalog(
        &self,
        config: &Config,
        fetched_at: DateTime<Utc>,
        rooms: &[MeetingRoom],
    ) -> Result<()> {
        if !valid_rooms(rooms) {
            bail!("會議室資源目錄快取不完整");
        }
        let account_key = config::credential_key(config)?;
        self.conn.execute(
            "INSERT INTO room_catalogs(account_key, fetched_at, rooms_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_key) DO UPDATE SET
                 fetched_at = excluded.fetched_at,
                 rooms_json = excluded.rooms_json",
            params![
                account_key,
                fetched_at.to_rfc3339(),
                serde_json::to_string(rooms)?
            ],
        )?;
        Ok(())
    }
}

fn calendar_key(account_key: &str, calendar_url: &str) -> Result<String> {
    let encoded = serde_json::to_vec(&(account_key, calendar_url))?;
    Ok(Sha256::digest(encoded)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn is_fresh(fetched_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let age = now.signed_duration_since(fetched_at);
    age >= Duration::zero() && age < Duration::seconds(TTL_SECONDS)
}

fn valid_items(items: &[Item], calendar_url: &str) -> bool {
    items.iter().all(|item| {
        item.kind == Kind::Calendar
            && !item.id.trim().is_empty()
            && !item.remote_id.trim().is_empty()
            && !item.raw.trim().is_empty()
            && item.collection.trim_end_matches('/') == calendar_url.trim_end_matches('/')
    })
}

fn valid_rooms(rooms: &[MeetingRoom]) -> bool {
    rooms
        .iter()
        .all(|room| !room.name.trim().is_empty() && !room.calendar_url.trim().is_empty())
}

#[cfg(unix)]
fn private_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn private_permissions(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}
