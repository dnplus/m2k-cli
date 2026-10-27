use crate::config::Config;
use crate::model::{Collection, Item, Kind, Snapshot, SyncResult};
use crate::sqlite;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use rusqlite::functions::{Context as SqlFunctionContext, FunctionFlags};
use rusqlite::types::Value as SqlValue;
use rusqlite::{
    params, params_from_iter, Connection, OptionalExtension, Row, Transaction, TransactionBehavior,
};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

const DATABASE_FILE_NAME: &str = "database.sqlite3";
pub const SCHEMA_VERSION: i64 = 4;
const CALENDAR_SYNC_TOKEN_PREFIX: &str = "sync.calendar.token.";
pub(crate) const DATABASE_FILE: &str = DATABASE_FILE_NAME;
mod search;
mod webhook;
const PAGE_ORDER: &str = "COALESCE(end_date, date, '') DESC, title COLLATE NOCASE ASC, id ASC";

#[derive(Debug)]
pub struct ItemPage {
    pub items: Vec<Item>,
    pub total: usize,
}

#[derive(Clone, Copy)]
enum ItemProjection {
    Full,
    Summary,
}

impl ItemProjection {
    fn select_fields(self) -> &'static str {
        match self {
            Self::Full => {
                "id, kind, collection, remote_id, etag, uid_validity, title, subtitle,
                 date, end_date, text, raw, flags_json, emails_json, phones_json,
                 attachments_json, last_seen"
            }
            Self::Summary => {
                "id, kind, collection, remote_id, etag, uid_validity, title, subtitle,
                 date, end_date, '' AS text, '' AS raw, flags_json,
                 '[]' AS emails_json, '[]' AS phones_json, '[]' AS attachments_json, last_seen"
            }
        }
    }
}

pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    pub fn open(data_dir: &Path) -> Result<Self> {
        ensure_data_dir(data_dir)?;
        let path = data_dir.join(DATABASE_FILE_NAME);
        let (conn, _initialization) =
            sqlite::open(&path).with_context(|| format!("無法開啟資料庫：{}", path.display()))?;
        register_query_functions(&conn)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(anyhow!("資料庫 schema 版本較新，請更新 m2k"));
        }
        if version < SCHEMA_VERSION {
            if search::backfill_is_slow(&conn, version)? {
                // Runs before the TUI enters the alternate screen, so both CLI
                // and TUI users see why the first open is slow.
                eprintln!("正在建立搜尋索引（第一次升級，可能需要數十秒）…");
            }
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY NOT NULL,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS collections (
                 kind TEXT NOT NULL,
                 id TEXT NOT NULL,
                 name TEXT NOT NULL,
                 PRIMARY KEY (kind, id)
             );
             CREATE TABLE IF NOT EXISTS items (
                 id TEXT PRIMARY KEY NOT NULL,
                 kind TEXT NOT NULL,
                 collection TEXT NOT NULL,
                 remote_id TEXT NOT NULL,
                 etag TEXT,
                 uid_validity INTEGER,
                 title TEXT NOT NULL,
                 subtitle TEXT NOT NULL,
                 date TEXT,
                 end_date TEXT,
                 text TEXT NOT NULL,
                 raw TEXT NOT NULL,
                 flags_json TEXT NOT NULL,
                 emails_json TEXT NOT NULL,
                 phones_json TEXT NOT NULL,
                 attachments_json TEXT NOT NULL,
                 last_seen INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS items_kind_idx ON items(kind);
             CREATE INDEX IF NOT EXISTS items_collection_idx ON items(kind, collection);
             CREATE INDEX IF NOT EXISTS items_last_seen_idx ON items(kind, last_seen);
             CREATE INDEX IF NOT EXISTS items_order_idx
                 ON items(kind, COALESCE(end_date, date, '') DESC, id ASC);
             CREATE INDEX IF NOT EXISTS items_page_order_idx
                 ON items(kind, COALESCE(end_date, date, '') DESC, title COLLATE NOCASE ASC, id ASC);
             CREATE TABLE IF NOT EXISTS sync_state (
                 kind TEXT PRIMARY KEY NOT NULL,
                 count INTEGER NOT NULL,
                 error TEXT,
                 synced_at INTEGER NOT NULL,
                 attempted_at INTEGER,
                 last_success_at INTEGER,
                 last_success_count INTEGER
             );",
            )?;
            migrate_sync_state(&tx)?;
            if version < 2 {
                crate::contact_index::initialize(&tx)?;
            }
            webhook::initialize(&tx)?;
            search::initialize(&tx, version < 4)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            // The backfill can leave a WAL as large as the index; fold it back.
            // The migration is already committed, so a failed or busy checkpoint
            // only delays shrinking the WAL and must not fail this open.
            let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
        Ok(Self { conn, path })
    }

    pub fn open_reader(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(DATABASE_FILE_NAME);
        if let Ok(conn) = sqlite::open_reader(&path) {
            let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
            if version == SCHEMA_VERSION {
                register_query_functions(&conn)?;
                return Ok(Self { conn, path });
            }
        }
        drop(Self::open(data_dir)?);
        let conn = sqlite::open_reader(&path)?;
        register_query_functions(&conn)?;
        Ok(Self { conn, path })
    }

    pub fn verify_account(&self, config: &Config) -> Result<()> {
        config.validate()?;
        let expected = identity_values(config);
        let tx = self.conn.unchecked_transaction()?;
        let actual = account_values(&tx)?;
        if !actual.is_empty() && !account_matches(&actual, &expected) {
            return Err(anyhow!("資料庫帳號身分不符，請使用對應的資料目錄"));
        }
        if actual.is_empty() && has_account_data(&tx)? {
            return Err(anyhow!(
                "資料庫已有資料但沒有帳號身分資訊，請使用新的資料目錄"
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn ensure_account(&mut self, config: &Config) -> Result<()> {
        config.validate()?;
        let expected = identity_values(config);
        if account_matches(&account_values(&self.conn)?, &expected) {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actual = account_values(&tx)?;
        if actual.is_empty() {
            if has_account_data(&tx)? {
                return Err(anyhow!(
                    "資料庫已有資料但沒有帳號身分資訊，請使用新的資料目錄"
                ));
            }
            tx.execute("DELETE FROM metadata WHERE key LIKE 'sync.%'", [])?;
            for (key, value) in &expected {
                tx.execute(
                    "INSERT INTO metadata(key, value) VALUES (?1, ?2)",
                    params![key, value],
                )?;
            }
        } else if !account_matches(&actual, &expected) {
            if has_account_data(&tx)? {
                return Err(anyhow!("資料庫帳號身分不符，請使用對應的資料目錄"));
            }
            tx.execute("DELETE FROM sync_state", [])?;
            tx.execute("DELETE FROM metadata WHERE key LIKE 'sync.%'", [])?;
            tx.execute("DELETE FROM metadata WHERE key LIKE 'account.%'", [])?;
            for (key, value) in &expected {
                tx.execute(
                    "INSERT INTO metadata(key, value) VALUES (?1, ?2)",
                    params![key, value],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn apply_snapshot(&mut self, snapshot: &Snapshot, now: DateTime<Utc>) -> Result<()> {
        validate_snapshot(snapshot)?;
        let encoded_collections = snapshot
            .collections
            .iter()
            .map(encode_collection)
            .collect::<Result<Vec<_>>>()?;
        let encoded_items = snapshot
            .items
            .iter()
            .map(encode_item)
            .collect::<Result<Vec<_>>>()?;
        let kind = kind_string(snapshot.kind);
        let seen_at = now.timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM collections WHERE kind = ?1", params![kind])?;
        for collection in &encoded_collections {
            tx.execute(
                "INSERT INTO collections(kind, id, name) VALUES (?1, ?2, ?3)",
                params![kind, collection.0, collection.1],
            )?;
        }
        replace_items(&tx, kind, None, &encoded_items, seen_at)?;
        if snapshot.kind == Kind::Contacts {
            crate::contact_index::rebuild(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn apply_collection_snapshot(
        &mut self,
        snapshot: &Snapshot,
        now: DateTime<Utc>,
    ) -> Result<()> {
        validate_snapshot(snapshot)?;
        if snapshot.collections.is_empty() {
            return Ok(());
        }
        let collection_ids = snapshot
            .collections
            .iter()
            .map(|collection| collection.id.clone())
            .collect::<HashSet<_>>();
        if snapshot
            .items
            .iter()
            .any(|item| !collection_ids.contains(&item.collection))
        {
            return Err(anyhow!("集合快照包含不屬於快取集合的項目"));
        }
        let encoded_collections = snapshot
            .collections
            .iter()
            .map(encode_collection)
            .collect::<Result<Vec<_>>>()?;
        let encoded_items = snapshot
            .items
            .iter()
            .map(encode_item)
            .collect::<Result<Vec<_>>>()?;
        let kind = kind_string(snapshot.kind);
        let seen_at = now.timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for collection in &encoded_collections {
            tx.execute(
                "INSERT INTO collections(kind, id, name) VALUES (?1, ?2, ?3)
                 ON CONFLICT(kind, id) DO UPDATE SET name = excluded.name",
                params![kind, collection.0, collection.1],
            )?;
        }
        replace_items(&tx, kind, Some(&collection_ids), &encoded_items, seen_at)?;
        if snapshot.kind == Kind::Contacts {
            crate::contact_index::rebuild(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_snapshot(&mut self, snapshot: &Snapshot, now: DateTime<Utc>) -> Result<()> {
        validate_snapshot(snapshot)?;
        let encoded_collections = snapshot
            .collections
            .iter()
            .map(encode_collection)
            .collect::<Result<Vec<_>>>()?;
        let encoded_items = snapshot
            .items
            .iter()
            .map(encode_item)
            .collect::<Result<Vec<_>>>()?;
        let kind = kind_string(snapshot.kind);
        let seen_at = now.timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for collection in &encoded_collections {
            tx.execute(
                "INSERT INTO collections(kind, id, name) VALUES (?1, ?2, ?3)
                 ON CONFLICT(kind, id) DO UPDATE SET name = excluded.name",
                params![kind, collection.0, collection.1],
            )?;
        }
        upsert_items(&tx, &encoded_items, seen_at)?;
        if snapshot.kind == Kind::Contacts {
            crate::contact_index::rebuild(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list(
        &self,
        kind: Kind,
        query: Option<&str>,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Item>> {
        self.list_with_projection(kind, query, collection, limit, ItemProjection::Full)
    }

    pub fn list_summaries(
        &self,
        kind: Kind,
        query: Option<&str>,
        collection: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Item>> {
        self.list_with_projection(kind, query, collection, limit, ItemProjection::Summary)
    }

    pub fn contact_page(
        &self,
        query: Option<&str>,
        collection: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<ItemPage> {
        let filtered = query.is_some_and(|q| !q.trim().is_empty())
            || collection.is_some_and(|c| !c.trim().is_empty());
        let filter = ItemFilter::new(Kind::Contacts, query, collection);
        let (source, bound) = if filtered {
            (format!("contact_people WHERE id IN (SELECT person_id FROM contact_members WHERE item_id IN (SELECT id FROM items WHERE {}))", filter.predicate()), filter.bound)
        } else {
            ("contact_people".to_owned(), Vec::new())
        };
        let tx = self.conn.unchecked_transaction()?;
        let total: usize = tx.query_row(
            &format!("SELECT COUNT(*) FROM {source}"),
            params_from_iter(bound.iter()),
            |row| row.get(0),
        )?;
        let mut items = Vec::new();
        if limit != 0 && offset < total {
            let mut values = bound;
            values.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
            values.push(SqlValue::Integer(i64::try_from(offset).unwrap_or(i64::MAX)));
            let sql = format!(
                "SELECT payload FROM {source} ORDER BY name_key,id LIMIT ?{} OFFSET ?{}",
                values.len() - 1,
                values.len()
            );
            let mut statement = tx.prepare(&sql)?;
            let payloads = statement.query_map(params_from_iter(values.iter()), |row| {
                row.get::<_, String>(0)
            })?;
            for payload in payloads {
                items.push(crate::contact_index::decode(&payload?)?);
            }
        }
        tx.commit()?;
        Ok(ItemPage { items, total })
    }

    pub fn list_summaries_page(
        &self,
        kind: Kind,
        query: Option<&str>,
        collection: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<ItemPage> {
        let filter = ItemFilter::new(kind, query, collection);
        let source = format!("main.items WHERE {}", filter.predicate());
        let tx = self.conn.unchecked_transaction()?;
        let page = query_summaries_page(&tx, "", &source, &filter.bound, limit, offset)?;
        tx.commit()?;
        Ok(page)
    }

    pub fn hybrid_summaries_page(
        &self,
        kind: Kind,
        query: Option<&str>,
        collection: Option<&str>,
        remote: &[Item],
        limit: usize,
        offset: usize,
    ) -> Result<ItemPage> {
        if remote.is_empty() {
            return self.list_summaries_page(kind, query, collection, limit, offset);
        }
        let filter = ItemFilter::new(kind, query, collection);
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS m2k_remote_summaries (
                 id TEXT PRIMARY KEY NOT NULL,
                 kind TEXT NOT NULL,
                 collection TEXT NOT NULL,
                 remote_id TEXT NOT NULL,
                 etag TEXT,
                 uid_validity INTEGER,
                 title TEXT NOT NULL,
                 subtitle TEXT NOT NULL,
                 date TEXT,
                 end_date TEXT,
                 flags_json TEXT NOT NULL,
                 last_seen INTEGER NOT NULL
             );
             DELETE FROM temp.m2k_remote_summaries;",
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO temp.m2k_remote_summaries (
                     id, kind, collection, remote_id, etag, uid_validity,
                     title, subtitle, date, end_date, flags_json, last_seen
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0)",
            )?;
            for item in remote.iter().filter(|item| item.kind == kind) {
                validate_item(item)?;
                insert.execute(params![
                    item.id,
                    kind_string(item.kind),
                    item.collection,
                    item.remote_id,
                    item.etag,
                    item.uid_validity.map(i64::from),
                    item.title,
                    item.subtitle,
                    item.date,
                    item.end_date,
                    serde_json::to_string(&item.flags).context("無法序列化郵件旗標")?,
                ])?;
            }
        }
        let local_predicate = match &filter.search {
            Some(search) => format!(
                "{} AND (id IN (SELECT id FROM remote_matches) OR {search})",
                filter.scope
            ),
            None => filter.scope.clone(),
        };
        let fields = ItemProjection::Summary.select_fields();
        let prefix = format!(
            "WITH remote_matches AS (
                 SELECT {fields} FROM temp.m2k_remote_summaries WHERE {}
             ), matches AS (
                 SELECT {fields} FROM main.items WHERE {local_predicate}
                 UNION ALL
                 SELECT * FROM remote_matches
                 WHERE NOT EXISTS (
                     SELECT 1 FROM main.items WHERE items.id = remote_matches.id
                 )
             )",
            filter.scope
        );
        let page = query_summaries_page(&tx, &prefix, "matches", &filter.bound, limit, offset)?;
        tx.execute("DELETE FROM temp.m2k_remote_summaries", [])?;
        tx.commit()?;
        Ok(page)
    }

    fn list_with_projection(
        &self,
        kind: Kind,
        query: Option<&str>,
        collection: Option<&str>,
        limit: usize,
        projection: ItemProjection,
    ) -> Result<Vec<Item>> {
        let filter = ItemFilter::new(kind, query, collection);
        let mut sql = format!(
            "SELECT {} FROM items WHERE {}",
            projection.select_fields(),
            filter.predicate()
        );
        sql.push_str(" ORDER BY COALESCE(end_date, date, '') DESC, id ASC");
        let bounded_limit = (limit > 0 && limit != usize::MAX)
            .then(|| i64::try_from(limit).context("項目數量超過資料庫可表示範圍"))
            .transpose()?;
        if bounded_limit.is_some() {
            sql.push_str(&format!(" LIMIT ?{}", filter.bound.len() + 1));
        }
        let mut bound = filter.bound;
        if let Some(limit) = bounded_limit {
            bound.push(SqlValue::Integer(limit));
        }
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(bound.iter()))?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(decode_item(row)?);
        }
        Ok(items)
    }

    pub fn get(&self, id: &str) -> Result<Item> {
        let mut statement = self.conn.prepare(
            "SELECT id, kind, collection, remote_id, etag, uid_validity, title, subtitle,
                    date, end_date, text, raw, flags_json, emails_json, phones_json,
                    attachments_json, last_seen
             FROM items WHERE id = ?1",
        )?;
        statement
            .query_row(params![id], decode_item)
            .optional()?
            .ok_or_else(|| {
                crate::error::Failure {
                    code: "not_found",
                    message: format!("找不到項目：{id}"),
                    retryable: false,
                }
                .into()
            })
    }

    pub fn collections(&self, kind: Kind) -> Result<Vec<Collection>> {
        let mut statement = self
            .conn
            .prepare("SELECT kind, id, name FROM collections WHERE kind = ?1 ORDER BY id")?;
        let mut rows = statement.query(params![kind_string(kind)])?;
        let mut collections = Vec::new();
        while let Some(row) = rows.next()? {
            let row_kind: String = row.get(0)?;
            collections.push(Collection {
                kind: parse_kind(&row_kind)?,
                id: row.get(1)?,
                name: row.get(2)?,
            });
        }
        Ok(collections)
    }

    pub fn remove(&mut self, id: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let contact: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM items WHERE id=?1 AND kind='contacts')",
            [id],
            |row| row.get(0),
        )?;
        tx.execute("DELETE FROM items WHERE id = ?1", params![id])?;
        if contact {
            crate::contact_index::rebuild(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert(&mut self, item: &Item) -> Result<()> {
        validate_item(item)?;
        let encoded = encode_item(item)?;
        let now = Utc::now().timestamp();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_items(&tx, std::slice::from_ref(&encoded), now)?;
        if item.kind == Kind::Contacts {
            crate::contact_index::rebuild(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn prune(&mut self, config: &Config, now: DateTime<Utc>) -> Result<usize> {
        self.prune_kinds(config, now, &[Kind::Mail, Kind::Calendar, Kind::Contacts])
    }

    pub fn prune_kind(&mut self, config: &Config, now: DateTime<Utc>, kind: Kind) -> Result<usize> {
        self.prune_kinds(config, now, &[kind])
    }

    pub fn prune_kinds(
        &mut self,
        config: &Config,
        now: DateTime<Utc>,
        kinds: &[Kind],
    ) -> Result<usize> {
        config.validate()?;
        let mut ids = Vec::new();
        let mut contacts_changed = false;
        let mut seen_kinds = HashSet::new();
        for kind in kinds {
            if !seen_kinds.insert(*kind) {
                continue;
            }
            let (retention_days, preserve_recurrence) = match kind {
                Kind::Mail => (config.mail_retention_days, false),
                Kind::Calendar => (config.calendar_retention_days, true),
                Kind::Contacts => (config.contacts_retention_days, false),
            };
            let expired = self.expired_ids(*kind, retention_days, now, preserve_recurrence)?;
            contacts_changed |= *kind == Kind::Contacts && !expired.is_empty();
            ids.extend(expired);
        }
        if ids.is_empty() {
            return Ok(0);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for id in &ids {
            tx.prepare_cached("DELETE FROM items WHERE id = ?1")?
                .execute(params![id])?;
        }
        if contacts_changed {
            crate::contact_index::rebuild(&tx)?;
        }
        tx.commit()?;
        Ok(ids.len())
    }

    pub fn last_full_calendar_sync(&self) -> Result<Option<DateTime<Utc>>> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'sync.calendar.full_success_at'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|time| time.with_timezone(&Utc))
            .filter(|time| *time <= Utc::now()))
    }

    pub fn mark_full_calendar_sync(&self, now: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO metadata(key, value) VALUES ('sync.calendar.full_success_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![now.to_rfc3339()],
        )?;
        Ok(())
    }

    /// Last successful near-window ETag check (window or full calendar sync).
    pub fn last_calendar_window_sync(&self) -> Result<Option<DateTime<Utc>>> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'sync.calendar.window_success_at'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|time| time.with_timezone(&Utc)))
    }

    pub fn mark_calendar_window_sync(&self, now: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO metadata(key, value) VALUES ('sync.calendar.window_success_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![now.to_rfc3339()],
        )?;
        Ok(())
    }

    /// RFC 6578 sync-tokens keyed by calendar collection id, stored in
    /// `metadata` so no schema change is needed.
    pub fn calendar_sync_tokens(&self) -> Result<std::collections::BTreeMap<String, String>> {
        let mut statement = self
            .conn
            .prepare("SELECT substr(key, ?1), value FROM metadata WHERE substr(key, 1, ?2) = ?3")?;
        let prefix_len = i64::try_from(CALENDAR_SYNC_TOKEN_PREFIX.len())?;
        let rows = statement.query_map(
            params![prefix_len + 1, prefix_len, CALENDAR_SYNC_TOKEN_PREFIX],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces every stored calendar sync-token; an empty map clears them.
    pub fn set_calendar_sync_tokens(
        &mut self,
        tokens: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        let prefix_len = i64::try_from(CALENDAR_SYNC_TOKEN_PREFIX.len())?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM metadata WHERE substr(key, 1, ?1) = ?2",
            params![prefix_len, CALENDAR_SYNC_TOKEN_PREFIX],
        )?;
        for (collection, token) in tokens {
            tx.execute(
                "INSERT INTO metadata(key, value) VALUES (?1, ?2)",
                params![format!("{CALENDAR_SYNC_TOKEN_PREFIX}{collection}"), token],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record_sync(&mut self, result: &SyncResult, now: DateTime<Utc>) -> Result<()> {
        let count = i64::try_from(result.count).context("同步筆數超過資料庫可表示範圍")?;
        self.conn.execute(
            "INSERT INTO sync_state(
                 kind, count, error, synced_at, attempted_at, last_success_at, last_success_count
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?4,
                 CASE WHEN ?3 IS NULL THEN ?4 ELSE NULL END,
                 CASE WHEN ?3 IS NULL THEN ?2 ELSE NULL END
             )
             ON CONFLICT(kind) DO UPDATE SET
                 count = CASE WHEN excluded.error IS NULL THEN excluded.count ELSE sync_state.count END,
                 error = excluded.error,
                 synced_at = excluded.synced_at,
                 attempted_at = excluded.attempted_at,
                 last_success_at = CASE
                     WHEN excluded.error IS NULL THEN excluded.last_success_at
                     ELSE sync_state.last_success_at
                 END,
                 last_success_count = CASE
                     WHEN excluded.error IS NULL THEN excluded.last_success_count
                     ELSE sync_state.last_success_count
                 END",
            params![
                kind_string(result.kind),
                count,
                result.error,
                now.timestamp()
            ],
        )?;
        Ok(())
    }

    pub fn sync_status(&self) -> Result<Value> {
        let mut statement = self.conn.prepare(
            "SELECT kind, count, error, synced_at, attempted_at,
                        last_success_at, last_success_count
                 FROM sync_state ORDER BY kind",
        )?;
        let mut rows = statement.query([])?;
        let mut status = Map::new();
        while let Some(row) = rows.next()? {
            let kind: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            let error: Option<String> = row.get(2)?;
            let synced_at: i64 = row.get(3)?;
            let attempted_at: Option<i64> = row.get(4)?;
            let last_success_at: Option<i64> = row.get(5)?;
            let last_success_count: Option<i64> = row.get(6)?;
            let attempted_at = attempted_at.unwrap_or(synced_at);
            status.insert(
                kind,
                json!({
                    "count": last_success_count.unwrap_or(count),
                    "last_success_count": last_success_count,
                    "error": error,
                    "attempted_at": format_timestamp(attempted_at),
                    "last_success_at": last_success_at.map(format_timestamp),
                    "synced_at": format_timestamp(attempted_at),
                }),
            );
        }
        Ok(Value::Object(status))
    }

    pub fn counts(&self) -> Result<Value> {
        let mut counts = Map::new();
        for kind in [Kind::Mail, Kind::Calendar, Kind::Contacts] {
            let count: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM items WHERE kind = ?1",
                params![kind_string(kind)],
                |row| row.get(0),
            )?;
            counts.insert(kind_string(kind).to_owned(), json!(count));
        }
        Ok(Value::Object(counts))
    }

    fn expired_ids(
        &self,
        kind: Kind,
        retention_days: u32,
        now: DateTime<Utc>,
        preserve_recurrence: bool,
    ) -> Result<Vec<String>> {
        if retention_days == 0 {
            return Ok(Vec::new());
        }
        let cutoff = now - Duration::days(i64::from(retention_days));
        match kind {
            Kind::Mail => {
                let mut statement = self
                    .conn
                    .prepare("SELECT id, date FROM items WHERE kind = ?1")?;
                let mut rows = statement.query(params![kind_string(kind)])?;
                let mut ids = Vec::new();
                while let Some(row) = rows.next()? {
                    let date: Option<String> = row.get(1)?;
                    if mail_expired(date.as_deref(), cutoff) {
                        ids.push(row.get(0)?);
                    }
                }
                Ok(ids)
            }
            Kind::Calendar => {
                let mut statement = self.conn.prepare(
                    "SELECT id, date, end_date, raw, flags_json
                     FROM items WHERE kind = ?1",
                )?;
                let mut rows = statement.query(params![kind_string(kind)])?;
                let mut ids = Vec::new();
                while let Some(row) = rows.next()? {
                    let date: Option<String> = row.get(1)?;
                    let end_date: Option<String> = row.get(2)?;
                    if !end_date
                        .as_deref()
                        .or(date.as_deref())
                        .and_then(parse_timestamp)
                        .is_some_and(|value| value < cutoff)
                    {
                        continue;
                    }
                    let raw: String = row.get(3)?;
                    let flags_json: String = row.get(4)?;
                    if preserve_recurrence
                        && calendar_resource_needs_preservation(&raw, &flags_json)
                    {
                        continue;
                    }
                    ids.push(row.get(0)?);
                }
                Ok(ids)
            }
            Kind::Contacts => {
                let mut statement = self
                    .conn
                    .prepare("SELECT id, last_seen FROM items WHERE kind = ?1")?;
                let mut rows = statement.query(params![kind_string(kind)])?;
                let mut ids = Vec::new();
                while let Some(row) = rows.next()? {
                    let last_seen: i64 = row.get(1)?;
                    if DateTime::from_timestamp(last_seen, 0).is_some_and(|value| value < cutoff) {
                        ids.push(row.get(0)?);
                    }
                }
                Ok(ids)
            }
        }
    }

    pub fn database_path(&self) -> &Path {
        &self.path
    }
}

struct ItemFilter {
    scope: String,
    search: Option<String>,
    bound: Vec<SqlValue>,
}

impl ItemFilter {
    fn new(kind: Kind, query: Option<&str>, collection: Option<&str>) -> Self {
        let query = query.map(str::trim).filter(|value| !value.is_empty());
        let collection = collection.map(str::trim).filter(|value| !value.is_empty());
        let mut scope = "kind = ?1".to_owned();
        let mut bound = vec![SqlValue::Text(kind_string(kind).to_owned())];
        if let Some(collection) = collection {
            scope.push_str(&format!(
                " AND m2k_casefold(collection) = m2k_casefold(?{})",
                bound.len() + 1
            ));
            bound.push(SqlValue::Text(collection.to_owned()));
        }
        let search = query.map(|query| {
            let index = bound.len() + 1;
            bound.push(SqlValue::Text(query.to_owned()));
            let exact = format!(
                "(
                    m2k_contains(id, ?{index})
                    OR m2k_contains(collection, ?{index})
                    OR m2k_contains(remote_id, ?{index})
                    OR m2k_contains(title, ?{index})
                    OR m2k_contains(subtitle, ?{index})
                    OR m2k_contains(date, ?{index})
                    OR m2k_contains(end_date, ?{index})
                    OR m2k_contains(text, ?{index})
                    OR (kind <> 'mail' AND m2k_contains(raw, ?{index}))
                    OR m2k_json_values_contains(emails_json, ?{index}, NULL)
                    OR m2k_json_values_contains(phones_json, ?{index}, NULL)
                    OR m2k_json_values_contains(attachments_json, ?{index}, 'filename')
                )"
            );
            match search::indexed_predicate(query, &exact, bound.len() + 1) {
                Some(indexed) => {
                    bound.push(SqlValue::Text(search::phrase(query)));
                    indexed
                }
                None => exact,
            }
        });
        Self {
            scope,
            search,
            bound,
        }
    }

    fn predicate(&self) -> String {
        match &self.search {
            Some(search) => format!("{} AND {search}", self.scope),
            None => self.scope.clone(),
        }
    }
}

fn query_summaries_page(
    conn: &Connection,
    prefix: &str,
    source: &str,
    bound: &[SqlValue],
    limit: usize,
    offset: usize,
) -> Result<ItemPage> {
    let sql_limit = i64::try_from(limit).context("分頁大小超過資料庫可表示範圍")?;
    let sql_offset = i64::try_from(offset).context("分頁位置超過資料庫可表示範圍")?;
    let total: i64 = conn.query_row(
        &format!("{prefix} SELECT COUNT(*) FROM {source}"),
        params_from_iter(bound.iter()),
        |row| row.get(0),
    )?;
    let total = usize::try_from(total).context("項目總數超過可表示範圍")?;
    if limit == 0 || offset >= total {
        return Ok(ItemPage {
            items: Vec::new(),
            total,
        });
    }
    let sql = format!(
        "{prefix} SELECT {} FROM {source} ORDER BY {PAGE_ORDER} LIMIT ?{} OFFSET ?{}",
        ItemProjection::Summary.select_fields(),
        bound.len() + 1,
        bound.len() + 2
    );
    let mut page_bound = bound.to_vec();
    page_bound.push(SqlValue::Integer(sql_limit));
    page_bound.push(SqlValue::Integer(sql_offset));
    let mut statement = conn.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(page_bound.iter()))?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        items.push(decode_item(row)?);
    }
    Ok(ItemPage { items, total })
}

fn account_values(conn: &Connection) -> Result<std::collections::BTreeMap<String, String>> {
    let mut statement =
        conn.prepare("SELECT key, value FROM metadata WHERE key LIKE 'account.%'")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn account_matches(
    actual: &std::collections::BTreeMap<String, String>,
    expected: &[(&str, String)],
) -> bool {
    actual.len() == expected.len()
        && expected.iter().all(|(key, value)| {
            actual.get(*key).is_some_and(|stored| {
                if matches!(*key, "account.caldav_url" | "account.carddav_url") {
                    crate::dav_url::identity(stored) == crate::dav_url::identity(value)
                } else {
                    stored == value
                }
            })
        })
}

fn has_account_data(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM items LIMIT 1) OR EXISTS(SELECT 1 FROM collections LIMIT 1)
         OR EXISTS(SELECT 1 FROM mail_webhook_subscriptions LIMIT 1)",
        [],
        |row| row.get(0),
    )?)
}

fn identity_values(config: &Config) -> Vec<(&'static str, String)> {
    vec![
        ("account.username", config.username.trim().to_owned()),
        ("account.email", config.email.trim().to_lowercase()),
        (
            "account.imap_host",
            config.imap_host.trim().to_ascii_lowercase(),
        ),
        ("account.imap_port", config.imap_port.to_string()),
        (
            "account.smtp_host",
            config.smtp_host.trim().to_ascii_lowercase(),
        ),
        ("account.smtp_port", config.smtp_port.to_string()),
        (
            "account.caldav_url",
            crate::dav_url::identity(&config.caldav_url),
        ),
        (
            "account.carddav_url",
            crate::dav_url::identity(&config.carddav_url),
        ),
    ]
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<()> {
    let mut collection_ids = HashSet::new();
    for collection in &snapshot.collections {
        if collection.kind != snapshot.kind {
            return Err(anyhow!("快照集合類型與快照不一致"));
        }
        if collection.id.trim().is_empty() {
            return Err(anyhow!("快照集合識別碼不可為空白"));
        }
        if !collection_ids.insert(collection.id.clone()) {
            return Err(anyhow!("快照包含重複集合：{}", collection.id));
        }
    }
    let mut item_ids = HashSet::new();
    for item in &snapshot.items {
        validate_item(item)?;
        if item.kind != snapshot.kind {
            return Err(anyhow!("快照項目類型與快照不一致"));
        }
        if !item_ids.insert(item.id.clone()) {
            return Err(anyhow!("快照包含重複項目：{}", item.id));
        }
    }
    Ok(())
}

fn validate_item(item: &Item) -> Result<()> {
    if item.id.trim().is_empty() {
        return Err(anyhow!("項目識別碼不可為空白"));
    }
    if item.collection.trim().is_empty() {
        return Err(anyhow!("項目集合不可為空白"));
    }
    if item.remote_id.trim().is_empty() {
        return Err(anyhow!("項目遠端識別碼不可為空白"));
    }
    if item.uid_validity.is_some_and(|value| value == 0) {
        return Err(anyhow!("UIDVALIDITY 必須大於 0"));
    }
    Ok(())
}

fn encode_collection(collection: &Collection) -> Result<(String, String)> {
    Ok((collection.id.clone(), collection.name.clone()))
}

#[derive(Debug)]
struct EncodedItem {
    id: String,
    kind: String,
    collection: String,
    remote_id: String,
    etag: Option<String>,
    uid_validity: Option<u32>,
    title: String,
    subtitle: String,
    date: Option<String>,
    end_date: Option<String>,
    text: String,
    raw: String,
    flags_json: String,
    emails_json: String,
    phones_json: String,
    attachments_json: String,
}

fn encode_item(item: &Item) -> Result<EncodedItem> {
    Ok(EncodedItem {
        id: item.id.clone(),
        kind: kind_string(item.kind).to_owned(),
        collection: item.collection.clone(),
        remote_id: item.remote_id.clone(),
        etag: item.etag.clone(),
        uid_validity: item.uid_validity,
        title: item.title.clone(),
        subtitle: item.subtitle.clone(),
        date: item.date.clone(),
        end_date: item.end_date.clone(),
        text: item.text.clone(),
        raw: item.raw.clone(),
        flags_json: serde_json::to_string(&item.flags).context("無法序列化郵件旗標")?,
        emails_json: serde_json::to_string(&item.emails).context("無法序列化電子郵件")?,
        phones_json: serde_json::to_string(&item.phones).context("無法序列化電話")?,
        attachments_json: serde_json::to_string(&item.attachments).context("無法序列化附件")?,
    })
}

fn upsert_items(tx: &Transaction<'_>, items: &[EncodedItem], last_seen: i64) -> Result<()> {
    for item in items {
        let existing_kind: Option<String> = tx
            .prepare_cached("SELECT kind FROM items WHERE id = ?1")?
            .query_row(params![item.id], |row| row.get(0))
            .optional()?;
        if existing_kind
            .as_deref()
            .is_some_and(|value| value != item.kind)
        {
            return Err(anyhow!("項目識別碼已被其他資料類型使用：{}", item.id));
        }
        tx.prepare_cached(
            "INSERT INTO collections(kind, id, name) VALUES (?1, ?2, ?3)
             ON CONFLICT(kind, id) DO NOTHING",
        )?
        .execute(params![item.kind, item.collection, item.collection])?;
        insert_or_update_item(tx, item, last_seen)?;
    }
    Ok(())
}

/// Makes the items of `kind` (limited to `collections` when given) equal to
/// `items`. Rows that stay keep their rowid and are updated in place, so the
/// search index trigger only rewrites rows whose searchable text changed.
/// An id held outside the replaced scope is rejected, as a plain INSERT
/// after deleting the scope would have been.
fn replace_items(
    tx: &Transaction<'_>,
    kind: &str,
    collections: Option<&HashSet<String>>,
    items: &[EncodedItem],
    last_seen: i64,
) -> Result<()> {
    let in_scope = |collection: &str| collections.is_none_or(|ids| ids.contains(collection));
    let keep = items
        .iter()
        .map(|item| item.id.as_str())
        .collect::<HashSet<_>>();
    let stale = {
        let mut statement = tx.prepare("SELECT id, collection FROM items WHERE kind = ?1")?;
        let rows = statement.query_map(params![kind], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut stale = Vec::new();
        for row in rows {
            let (id, collection) = row?;
            if in_scope(&collection) && !keep.contains(id.as_str()) {
                stale.push(id);
            }
        }
        stale
    };
    for id in &stale {
        tx.prepare_cached("DELETE FROM items WHERE id = ?1")?
            .execute(params![id])?;
    }
    for item in items {
        let existing: Option<(String, String)> = tx
            .prepare_cached("SELECT kind, collection FROM items WHERE id = ?1")?
            .query_row(params![item.id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        if existing.is_some_and(|(row_kind, collection)| row_kind != kind || !in_scope(&collection))
        {
            return Err(anyhow!("項目識別碼已被快照範圍外的資料使用：{}", item.id));
        }
        insert_or_update_item(tx, item, last_seen)?;
    }
    Ok(())
}

fn insert_or_update_item(tx: &Transaction<'_>, item: &EncodedItem, last_seen: i64) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO items(
             id, kind, collection, remote_id, etag, uid_validity, title, subtitle,
             date, end_date, text, raw, flags_json, emails_json, phones_json,
             attachments_json, last_seen
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         ON CONFLICT(id) DO UPDATE SET
             kind = excluded.kind,
             collection = excluded.collection,
             remote_id = excluded.remote_id,
             etag = excluded.etag,
             uid_validity = excluded.uid_validity,
             title = excluded.title,
             subtitle = excluded.subtitle,
             date = excluded.date,
             end_date = excluded.end_date,
             text = excluded.text,
             raw = excluded.raw,
             flags_json = excluded.flags_json,
             emails_json = excluded.emails_json,
             phones_json = excluded.phones_json,
             attachments_json = excluded.attachments_json,
             last_seen = excluded.last_seen",
    )?
    .execute(params![
        item.id,
        item.kind,
        item.collection,
        item.remote_id,
        item.etag,
        item.uid_validity.map(i64::from),
        item.title,
        item.subtitle,
        item.date,
        item.end_date,
        item.text,
        item.raw,
        item.flags_json,
        item.emails_json,
        item.phones_json,
        item.attachments_json,
        last_seen,
    ])?;
    Ok(())
}

pub(crate) fn decode_item(row: &Row<'_>) -> rusqlite::Result<Item> {
    let kind_value: String = row.get(1)?;
    let uid_validity: Option<i64> = row.get(5)?;
    let flags_json: String = row.get(12)?;
    let emails_json: String = row.get(13)?;
    let phones_json: String = row.get(14)?;
    let attachments_json: String = row.get(15)?;
    Ok(Item {
        id: row.get(0)?,
        kind: parse_kind_sql(&kind_value)?,
        collection: row.get(2)?,
        remote_id: row.get(3)?,
        etag: row.get(4)?,
        uid_validity: uid_validity.map(|value| value as u32),
        title: row.get(6)?,
        subtitle: row.get(7)?,
        date: row.get(8)?,
        end_date: row.get(9)?,
        text: row.get(10)?,
        raw: row.get(11)?,
        flags: serde_json::from_str(&flags_json).map_err(serde_to_sql_error)?,
        emails: serde_json::from_str(&emails_json).map_err(serde_to_sql_error)?,
        phones: serde_json::from_str(&phones_json).map_err(serde_to_sql_error)?,
        attachments: serde_json::from_str(&attachments_json).map_err(serde_to_sql_error)?,
    })
}

fn serde_to_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn parse_kind_sql(value: &str) -> rusqlite::Result<Kind> {
    match value {
        "mail" => Ok(Kind::Mail),
        "calendar" => Ok(Kind::Calendar),
        "contacts" => Ok(Kind::Contacts),
        _ => Err(rusqlite::Error::InvalidColumnType(
            1,
            "kind".to_owned(),
            rusqlite::types::Type::Text,
        )),
    }
}

fn register_query_functions(conn: &Connection) -> Result<()> {
    conn.create_scalar_function(
        "m2k_casefold",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context: &SqlFunctionContext<'_>| {
            let value: Option<String> = context.get(0)?;
            Ok(value.map(|value| value.to_lowercase()))
        },
    )?;
    conn.create_scalar_function(
        "m2k_contains",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context: &SqlFunctionContext<'_>| {
            let value: Option<String> = context.get(0)?;
            let needle: String = context.get(1)?;
            let needle = needle.to_lowercase();
            Ok(value.is_some_and(|value| value.to_lowercase().contains(needle.as_str())))
        },
    )?;
    conn.create_scalar_function(
        "m2k_json_values_contains",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context: &SqlFunctionContext<'_>| {
            let value: String = context.get(0)?;
            let needle: String = context.get(1)?;
            let key: Option<String> = context.get(2)?;
            Ok(json_values_contain(&value, &needle, key.as_deref()))
        },
    )?;
    Ok(())
}

fn json_values_contain(value: &str, needle: &str, key: Option<&str>) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(value) else {
        return false;
    };
    let Some(values) = value.as_array() else {
        return false;
    };
    let needle = needle.to_lowercase();
    values.iter().any(|value| {
        let value = match key {
            Some(key) => value.get(key).and_then(Value::as_str),
            None => value.as_str(),
        };
        value.is_some_and(|value| value.to_lowercase().contains(needle.as_str()))
    })
}

fn calendar_resource_needs_preservation(raw: &str, flags_json: &str) -> bool {
    const RECURRENCE_PROPERTIES: [&str; 4] = ["RRULE", "RDATE", "EXDATE", "RECURRENCE-ID"];
    let Ok(flags) = serde_json::from_str::<Vec<String>>(flags_json) else {
        return true;
    };
    if flags.iter().any(|flag| {
        let flag = flag.to_ascii_uppercase();
        RECURRENCE_PROPERTIES
            .iter()
            .any(|property| flag.contains(property))
    }) {
        return true;
    }
    if raw.trim().is_empty() {
        return false;
    }
    let mut event_count = 0;
    for calendar in ical::IcalParser::new(Cursor::new(raw.as_bytes())) {
        let Ok(calendar) = calendar else {
            return true;
        };
        for event in calendar.events {
            event_count += 1;
            if event_count > 1
                || event.properties.iter().any(|property| {
                    RECURRENCE_PROPERTIES
                        .iter()
                        .any(|name| property.name.eq_ignore_ascii_case(name))
                })
            {
                return true;
            }
        }
    }
    event_count != 1
}

fn kind_string(kind: Kind) -> &'static str {
    match kind {
        Kind::Mail => "mail",
        Kind::Calendar => "calendar",
        Kind::Contacts => "contacts",
    }
}

fn parse_kind(value: &str) -> Result<Kind> {
    match value {
        "mail" => Ok(Kind::Mail),
        "calendar" => Ok(Kind::Calendar),
        "contacts" => Ok(Kind::Contacts),
        _ => Err(anyhow!("資料庫資料類型錯誤：{value}")),
    }
}

fn mail_expired(date: Option<&str>, cutoff: DateTime<Utc>) -> bool {
    date.and_then(parse_timestamp)
        .is_some_and(|value| value < cutoff)
}

/// Drops mail that retention pruning would delete right after the sync.
/// IMAP `SINCE` matches whole days of INTERNALDATE, so the server keeps
/// returning messages on the boundary day that are already past the exact
/// cutoff; inserting and then pruning them rewrote the search index on
/// every unchanged sync.
pub(crate) fn without_expired_mail(
    mut snapshot: Snapshot,
    config: &Config,
    now: DateTime<Utc>,
) -> Snapshot {
    if snapshot.kind == Kind::Mail && config.mail_retention_days > 0 {
        let cutoff = now - Duration::days(i64::from(config.mail_retention_days));
        snapshot
            .items
            .retain(|item| !mail_expired(item.date.as_deref(), cutoff));
    }
    snapshot
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(timestamp) = trimmed.parse::<i64>() {
        return DateTime::from_timestamp(timestamp, 0);
    }
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(timestamp.with_timezone(&Utc));
    }
    for format in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y%m%dT%H%M%SZ",
        "%Y%m%dT%H%M%S",
    ] {
        if let Ok(timestamp) = NaiveDateTime::parse_from_str(trimmed, format) {
            return Some(DateTime::from_naive_utc_and_offset(timestamp, Utc));
        }
    }
    for format in ["%Y-%m-%d", "%Y%m%d"] {
        if let Ok(date) = NaiveDate::parse_from_str(trimmed, format) {
            return date
                .and_hms_opt(0, 0, 0)
                .map(|timestamp| DateTime::from_naive_utc_and_offset(timestamp, Utc));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(trimmed.get(..10).unwrap_or(trimmed), "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|timestamp| DateTime::from_naive_utc_and_offset(timestamp, Utc));
    }
    None
}

fn format_timestamp(timestamp: i64) -> Value {
    DateTime::from_timestamp(timestamp, 0)
        .map(|value| Value::String(value.to_rfc3339()))
        .unwrap_or(Value::Null)
}

fn migrate_sync_state(conn: &Connection) -> Result<()> {
    let columns = {
        let mut statement = conn.prepare("PRAGMA table_info(sync_state)")?;
        let mut rows = statement.query([])?;
        let mut columns = HashSet::new();
        while let Some(row) = rows.next()? {
            columns.insert(row.get::<_, String>(1)?);
        }
        columns
    };
    if !columns.contains("attempted_at") {
        conn.execute("ALTER TABLE sync_state ADD COLUMN attempted_at INTEGER", [])?;
    }
    if !columns.contains("last_success_at") {
        conn.execute(
            "ALTER TABLE sync_state ADD COLUMN last_success_at INTEGER",
            [],
        )?;
    }
    if !columns.contains("last_success_count") {
        conn.execute(
            "ALTER TABLE sync_state ADD COLUMN last_success_count INTEGER",
            [],
        )?;
    }
    conn.execute(
        "UPDATE sync_state SET attempted_at = synced_at WHERE attempted_at IS NULL",
        [],
    )?;
    conn.execute(
        "UPDATE sync_state
         SET last_success_at = synced_at,
             last_success_count = count
         WHERE error IS NULL
           AND (last_success_at IS NULL OR last_success_count IS NULL)",
        [],
    )?;
    Ok(())
}

fn ensure_data_dir(data_dir: &Path) -> Result<()> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("無法建立資料目錄：{}", data_dir.display()))?;
    set_path_mode(data_dir, 0o700)?;
    Ok(())
}

#[cfg(unix)]
fn set_path_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)
        .with_context(|| format!("無法讀取檔案權限：{}", path.display()))?
        .permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("無法設定檔案權限：{}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_path_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}
