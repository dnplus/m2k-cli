use m2k::config::{self, Config};
use m2k::model::{Item, Kind};
use m2k::room_cache::RoomCalendarCache;
use m2k::store::Store;
use m2k::tui::render_snapshot;
use rusqlite::Connection;
use tempfile::TempDir;

fn mail() -> Item {
    Item {
        id: "mail-concurrency".into(),
        kind: Kind::Mail,
        collection: "INBOX".into(),
        remote_id: "1".into(),
        etag: None,
        uid_validity: Some(1),
        title: "Committed subject".into(),
        subtitle: "sender@example.com".into(),
        date: Some("2026-10-07T00:00:00Z".into()),
        end_date: None,
        text: "Committed body".into(),
        raw: "Committed RFC822".into(),
        flags: Vec::new(),
        emails: Vec::new(),
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

#[test]
fn existing_store_and_tui_snapshot_read_while_another_connection_is_writing() {
    let dir = TempDir::new().unwrap();
    let cfg = Config::new("person@example.com".into(), "mail.example.test").unwrap();
    config::save(dir.path(), &cfg).unwrap();
    let mut initial = Store::open(dir.path()).unwrap();
    initial.ensure_account(&cfg).unwrap();
    initial.upsert(&mail()).unwrap();
    let path = initial.database_path().to_owned();
    drop(initial);

    let writer = Connection::open(path).unwrap();
    writer
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;
        UPDATE items SET title = 'Uncommitted subject' WHERE id = 'mail-concurrency';",
        )
        .unwrap();
    let opened = Store::open(dir.path());
    if let Err(error) = &opened {
        eprintln!("reader open during active WAL writer: {error:#}");
    }
    let mut reader = opened.expect("a reader must not request the writer lock on Store::open");
    reader.ensure_account(&cfg).unwrap();
    assert_eq!(
        reader.get("mail-concurrency").unwrap().title,
        "Committed subject"
    );
    let snapshot = render_snapshot(dir.path(), 120, 32).unwrap();
    assert!(snapshot.contains("Committed subject"));
    assert!(!snapshot.contains("Uncommitted subject"));
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn readonly_store_allows_hybrid_temp_merge_and_rejects_main_writes() {
    let dir = TempDir::new().unwrap();
    let cfg = Config::new("person@example.com".into(), "mail.example.test").unwrap();
    let mut initial = Store::open(dir.path()).unwrap();
    initial.ensure_account(&cfg).unwrap();
    initial.upsert(&mail()).unwrap();
    let writer = Connection::open(initial.database_path()).unwrap();
    drop(initial);
    writer
        .execute_batch(
            "BEGIN IMMEDIATE; UPDATE items SET title = 'Uncommitted' WHERE id = 'mail-concurrency'",
        )
        .unwrap();
    let mut reader = Store::open_reader(dir.path()).unwrap();
    reader.verify_account(&cfg).unwrap();
    reader.ensure_account(&cfg).unwrap();
    let remote = Item {
        id: "mail-remote-only".into(),
        title: "Server summary".into(),
        remote_id: "2".into(),
        text: String::new(),
        raw: String::new(),
        ..mail()
    };
    let page = reader
        .hybrid_summaries_page(Kind::Mail, None, None, &[remote], 10, 0)
        .unwrap();
    assert_eq!(page.total, 2);
    assert!(page
        .items
        .iter()
        .any(|item| item.title == "Committed subject"));
    assert!(page.items.iter().any(|item| item.title == "Server summary"));
    writer.execute_batch("ROLLBACK").unwrap();
    let error = reader.upsert(&mail()).unwrap_err();
    assert!(error.to_string().contains("readonly"), "{error:#}");
    assert_eq!(reader.counts().unwrap()["mail"], 1);
}

#[test]
fn reader_bootstraps_a_new_empty_store_then_verifies_account_without_writes() {
    let dir = TempDir::new().unwrap();
    let fresh = dir.path().join("new-data-directory");
    let cfg = Config::new("person@example.com".into(), "mail.example.test").unwrap();
    let reader = Store::open_reader(&fresh).unwrap();
    reader.verify_account(&cfg).unwrap();
    assert_eq!(
        reader
            .list_summaries_page(Kind::Mail, None, None, 10, 0)
            .unwrap()
            .total,
        0
    );
    let connection = Connection::open(reader.database_path()).unwrap();
    let identities: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM metadata WHERE key LIKE 'account.%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(identities, 0);
    drop(reader);
    let mut writable = Store::open(&fresh).unwrap();
    writable.ensure_account(&cfg).unwrap();
    writable.upsert(&mail()).unwrap();
    drop(writable);
    let reader = Store::open_reader(&fresh).unwrap();
    reader.verify_account(&cfg).unwrap();
    let other = Config::new("other@example.com".into(), "mail.example.test").unwrap();
    assert!(reader.verify_account(&other).is_err());
}

#[test]
fn reader_upgrades_a_legacy_delete_journal_database_once() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("database.sqlite3");
    let legacy = Connection::open(&path).unwrap();
    legacy.execute_batch("CREATE TABLE sync_state (
        kind TEXT PRIMARY KEY NOT NULL, count INTEGER NOT NULL, error TEXT, synced_at INTEGER NOT NULL
    ); INSERT INTO sync_state VALUES ('contacts', 3, NULL, 1791321600);").unwrap();
    drop(legacy);
    let reader = Store::open_reader(dir.path()).unwrap();
    assert_eq!(
        reader.sync_status().unwrap()["contacts"]["last_success_count"],
        3
    );
    let check = Connection::open(reader.database_path()).unwrap();
    let mode: String = check
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let writer = check;
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE sync_state SET count=4 WHERE kind='contacts'")
        .unwrap();
    let reopened = Store::open_reader(dir.path()).unwrap();
    assert_eq!(reopened.sync_status().unwrap()["contacts"]["count"], 3);
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn room_cache_opens_and_reads_catalog_during_an_active_cache_writer() {
    let dir = TempDir::new().unwrap();
    let cfg = Config::new("person@example.com".into(), "mail.example.test").unwrap();
    let cache = RoomCalendarCache::open(dir.path()).unwrap();
    cache.store_catalog(&cfg, chrono::Utc::now(), &[]).unwrap();
    let writer = Connection::open(cache.database_path()).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE room_catalogs SET rooms_json = '[{}]'")
        .unwrap();
    let reader = RoomCalendarCache::open(dir.path()).unwrap();
    assert_eq!(
        reader
            .lookup_catalog(&cfg, chrono::Utc::now())
            .unwrap()
            .unwrap()
            .len(),
        0
    );
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn full_calendar_stamp_is_durable_and_cleared_on_account_rebind() {
    let dir = TempDir::new().unwrap();
    let first = Config::new("person@example.com".into(), "mail.example.test").unwrap();
    let second = Config::new("other@example.com".into(), "mail.example.test").unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&first).unwrap();
    assert!(store.last_full_calendar_sync().unwrap().is_none());
    let stamp = chrono::Utc::now() - chrono::Duration::minutes(1);
    store.mark_full_calendar_sync(stamp).unwrap();
    assert_eq!(store.last_full_calendar_sync().unwrap(), Some(stamp));
    let reader = Store::open_reader(dir.path()).unwrap();
    assert_eq!(reader.last_full_calendar_sync().unwrap(), Some(stamp));
    store.ensure_account(&second).unwrap();
    assert!(store.last_full_calendar_sync().unwrap().is_none());
    assert!(reader.verify_account(&first).is_err());
    store
        .mark_full_calendar_sync(chrono::Utc::now() + chrono::Duration::days(1))
        .unwrap();
    assert!(store.last_full_calendar_sync().unwrap().is_none());
    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .execute(
            "UPDATE metadata SET value = 'bad-time' WHERE key='sync.calendar.full_success_at'",
            [],
        )
        .unwrap();
    assert!(store.last_full_calendar_sync().unwrap().is_none());
}
