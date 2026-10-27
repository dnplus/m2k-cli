use chrono::{Duration, Utc};
use m2k::config::Config;
use m2k::model::{Attachment, Collection, Item, Kind, Snapshot};
use m2k::store::{Store, SCHEMA_VERSION};
use rusqlite::Connection;
use tempfile::TempDir;

fn item(kind: Kind, id: &str, title: &str, text: &str) -> Item {
    Item {
        id: id.into(),
        kind,
        collection: "default".into(),
        remote_id: id.into(),
        etag: None,
        uid_validity: (kind == Kind::Mail).then_some(7),
        title: title.into(),
        subtitle: String::new(),
        date: Some("2026-10-01T00:00:00Z".into()),
        end_date: None,
        text: text.into(),
        raw: String::new(),
        flags: Vec::new(),
        emails: Vec::new(),
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

fn snapshot(kind: Kind, items: Vec<Item>) -> Snapshot {
    Snapshot {
        kind,
        collections: vec![Collection {
            kind,
            id: "default".into(),
            name: "預設".into(),
        }],
        items,
    }
}

fn found(store: &Store, kind: Kind, query: &str) -> Vec<String> {
    // Summaries skip decoding emails/phones/attachments, like the TUI list.
    let mut ids: Vec<_> = store
        .list_summaries(kind, Some(query), None, 0)
        .unwrap()
        .into_iter()
        .map(|item| item.id)
        .collect();
    ids.sort();
    ids
}

fn database(dir: &TempDir) -> Connection {
    Connection::open(dir.path().join("database.sqlite3")).unwrap()
}

/// Rowids the index itself returns, without the exact re-check. A stale row
/// here means prune/delete left index garbage behind.
fn indexed_rowids(conn: &Connection, phrase: &str) -> Vec<i64> {
    conn.prepare("SELECT rowid FROM items_search WHERE items_search MATCH ?1 ORDER BY rowid")
        .unwrap()
        .query_map([format!("\"{phrase}\"")], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn mail_fixture() -> Vec<Item> {
    let mut budget = item(
        Kind::Mail,
        "budget",
        "第四季預算審核會議",
        "請於週五前回覆。",
    );
    budget.subtitle = "王小明 <ming@example.test>".into();
    budget.emails = vec!["ming@example.test".into()];
    budget.attachments = vec![Attachment {
        index: 0,
        filename: "二〇二六年度預算表.xlsx".into(),
        content_type: "application/vnd.ms-excel".into(),
        size: 10,
    }];
    let mut mixed = item(
        Kind::Mail,
        "mixed",
        "Q4 Roadmap 規劃",
        "ExampleSoft 平台升級時程與 Beta 模組導入",
    );
    mixed.raw = "base64:5pyD6K2w5a6k".into();
    let syntax = item(
        Kind::Mail,
        "syntax",
        "報價 \"final\" (v2) AND OR NOT NEAR*",
        "path C:\\tmp\\a-b^c:d 100% done; it's {ok}",
    );
    let meeting = item(Kind::Mail, "meeting", "週會", "會議室已預約");
    vec![budget, mixed, syntax, meeting]
}

#[test]
fn chinese_long_and_short_queries_match_every_searchable_mail_field() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .apply_snapshot(&snapshot(Kind::Mail, mail_fixture()), Utc::now())
        .unwrap();

    // Three or more characters go through the trigram index.
    assert_eq!(found(&store, Kind::Mail, "預算審核"), ["budget"]);
    assert_eq!(found(&store, Kind::Mail, "王小明"), ["budget"]);
    assert_eq!(found(&store, Kind::Mail, "週五前"), ["budget"]);
    assert_eq!(found(&store, Kind::Mail, "年度預算表"), ["budget"]);
    assert_eq!(found(&store, Kind::Mail, "MING@EXAMPLE"), ["budget"]);
    // One and two characters cannot form a trigram and must still match.
    assert_eq!(found(&store, Kind::Mail, "預算"), ["budget"]);
    assert_eq!(found(&store, Kind::Mail, "會議"), ["budget", "meeting"]);
    assert_eq!(found(&store, Kind::Mail, "週"), ["budget", "meeting"]);
    assert_eq!(found(&store, Kind::Mail, "q4"), ["mixed"]);
    // Mail raw is base64 RFC822 and is deliberately not searched.
    assert!(found(&store, Kind::Mail, "5pyD6K2w").is_empty());
    assert!(found(&store, Kind::Mail, "沒有這個詞").is_empty());
}

#[test]
fn mixed_language_queries_match_as_case_insensitive_substrings() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .apply_snapshot(&snapshot(Kind::Mail, mail_fixture()), Utc::now())
        .unwrap();

    assert_eq!(found(&store, Kind::Mail, "q4 roadmap 規"), ["mixed"]);
    assert_eq!(found(&store, Kind::Mail, "examplesoft 平台"), ["mixed"]);
    assert_eq!(found(&store, Kind::Mail, "beta 模組導入"), ["mixed"]);
    assert_eq!(found(&store, Kind::Mail, "amplesoft"), ["mixed"]);
    // Word order matters: this is a substring search, not a bag of words.
    assert!(found(&store, Kind::Mail, "平台 examplesoft").is_empty());
}

#[test]
fn fts_syntax_characters_are_literal_text_not_query_operators() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .apply_snapshot(&snapshot(Kind::Mail, mail_fixture()), Utc::now())
        .unwrap();

    for query in [
        "\"final\"",
        "(v2)",
        "AND OR NOT",
        "NEAR*",
        "C:\\tmp\\a-b^c:d",
        "100% done",
        "it's {ok}",
    ] {
        assert_eq!(found(&store, Kind::Mail, query), ["syntax"], "{query}");
    }
    // Operators alone must not turn into a match-all or a syntax error.
    for query in ["\"\"\"", "***", "a OR b", "x AND", "NEAR(a b)", "^-:"] {
        assert!(found(&store, Kind::Mail, query).is_empty(), "{query}");
    }
}

#[test]
fn contact_and_calendar_fields_are_indexed() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut contact = item(Kind::Contacts, "contact", "王大明", "");
    contact.subtitle = "範例研發部".into();
    contact.emails = vec!["daming.wang@example.test".into()];
    contact.phones = vec!["+886-2-2345-6789".into()];
    contact.raw = "BEGIN:VCARD\nNOTE:負責雲端客戶\nEND:VCARD".into();
    store
        .apply_snapshot(&snapshot(Kind::Contacts, vec![contact]), Utc::now())
        .unwrap();
    let mut event = item(Kind::Calendar, "event", "季度檢討", "議程");
    event.raw = "BEGIN:VEVENT\nLOCATION:頂樓大會議室\nEND:VEVENT".into();
    store
        .apply_snapshot(&snapshot(Kind::Calendar, vec![event]), Utc::now())
        .unwrap();

    assert_eq!(found(&store, Kind::Contacts, "研發"), ["contact"]);
    assert_eq!(found(&store, Kind::Contacts, "DAMING.WANG"), ["contact"]);
    assert_eq!(found(&store, Kind::Contacts, "2345-6789"), ["contact"]);
    assert_eq!(found(&store, Kind::Contacts, "雲端客戶"), ["contact"]);
    assert_eq!(found(&store, Kind::Calendar, "大會議室"), ["event"]);
    assert_eq!(found(&store, Kind::Calendar, "頂樓"), ["event"]);
    let page = store.contact_page(Some("雲端客戶"), None, 10, 0).unwrap();
    assert_eq!(page.total, 1);
}

#[test]
fn long_queries_use_the_index_and_short_queries_fall_back_to_scanning() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .apply_snapshot(&snapshot(Kind::Mail, mail_fixture()), Utc::now())
        .unwrap();
    // Remove only the index row: the trigram path must lose the item while the
    // two-character scan path still sees it. This proves which path ran.
    database(&dir)
        .execute(
            "DELETE FROM items_search WHERE rowid = (SELECT rowid FROM items WHERE id='meeting')",
            [],
        )
        .unwrap();
    assert!(found(&store, Kind::Mail, "會議室").is_empty());
    assert_eq!(found(&store, Kind::Mail, "會議"), ["budget", "meeting"]);
}

#[test]
fn index_follows_updates_deletes_snapshots_and_prune() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let now = Utc::now();
    let mut old = item(Kind::Mail, "old", "過期的專案通知", "");
    old.date = Some((now - Duration::days(400)).to_rfc3339());
    let keep = item(Kind::Mail, "keep", "保留的專案通知", "");
    let gone = item(Kind::Mail, "gone", "會被刪除的專案通知", "");
    store
        .apply_snapshot(&snapshot(Kind::Mail, vec![old, keep.clone(), gone]), now)
        .unwrap();
    let conn = database(&dir);
    assert_eq!(indexed_rowids(&conn, "專案通知").len(), 3);

    let mut renamed = keep.clone();
    renamed.title = "改名後的公告".into();
    store.upsert(&renamed).unwrap();
    assert_eq!(found(&store, Kind::Mail, "改名後的公告"), ["keep"]);
    assert!(found(&store, Kind::Mail, "保留的專案").is_empty());

    store.remove("gone").unwrap();
    let mut config = Config::new("person@example.com".into(), "mail.example.test").unwrap();
    config.mail_retention_days = 30;
    assert_eq!(store.prune(&config, now).unwrap(), 1);
    assert!(indexed_rowids(&conn, "專案通知").is_empty());

    store
        .apply_snapshot(&snapshot(Kind::Mail, Vec::new()), now)
        .unwrap();
    assert!(indexed_rowids(&conn, "改名後").is_empty());
    let leftover: i64 = conn
        .query_row("SELECT COUNT(*) FROM items_search", [], |row| row.get(0))
        .unwrap();
    assert_eq!(leftover, 0);
}

/// The FTS5 segment blobs. Identical bytes mean nothing was re-tokenized.
fn index_segments(conn: &Connection) -> Vec<(i64, Vec<u8>)> {
    conn.prepare("SELECT id, block FROM items_search_data ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn resyncing_an_unchanged_snapshot_does_not_rewrite_the_index() {
    // Every mail sync calls apply_snapshot with the whole mailbox. Re-indexing
    // all of it each time cost tens of seconds under the write lock.
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mail = snapshot(Kind::Mail, mail_fixture());
    store.apply_snapshot(&mail, Utc::now()).unwrap();
    let mut event = item(Kind::Calendar, "event", "季度檢討會議", "");
    event.raw = "BEGIN:VEVENT\nLOCATION:頂樓\nEND:VEVENT".into();
    let calendar = snapshot(Kind::Calendar, vec![event]);
    store
        .apply_collection_snapshot(&calendar, Utc::now())
        .unwrap();
    let conn = database(&dir);
    let rowid = |id: &str| -> i64 {
        conn.query_row("SELECT rowid FROM items WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap()
    };
    let rowids = (rowid("budget"), rowid("event"));
    let segments = index_segments(&conn);

    let later = Utc::now() + Duration::minutes(5);
    store.apply_snapshot(&mail, later).unwrap();
    store.apply_collection_snapshot(&calendar, later).unwrap();
    assert_eq!((rowid("budget"), rowid("event")), rowids);
    assert_eq!(index_segments(&conn), segments);
    let last_seen: i64 = conn
        .query_row(
            "SELECT last_seen FROM items WHERE id = 'budget'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(last_seen, later.timestamp());

    // A changed subject is re-indexed; a mail missing from the snapshot leaves.
    let mut changed = mail_fixture();
    changed[0].title = "改過的預算主旨".into();
    changed.retain(|item| item.id != "meeting");
    store
        .apply_snapshot(&snapshot(Kind::Mail, changed), later)
        .unwrap();
    assert_ne!(index_segments(&conn), segments);
    assert_eq!(found(&store, Kind::Mail, "改過的預算"), ["budget"]);
    assert!(found(&store, Kind::Mail, "預算審核").is_empty());
    assert!(found(&store, Kind::Mail, "會議室").is_empty());
    assert!(indexed_rowids(&conn, "會議室").is_empty());
}

#[test]
fn collection_snapshot_still_rejects_ids_owned_outside_its_scope() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut other = item(Kind::Calendar, "shared-id", "別的行事曆", "");
    other.collection = "other".into();
    store.upsert(&other).unwrap();
    let error = store
        .apply_collection_snapshot(
            &snapshot(
                Kind::Calendar,
                vec![item(Kind::Calendar, "shared-id", "搶走", "")],
            ),
            Utc::now(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("shared-id"), "{error}");
    assert_eq!(found(&store, Kind::Calendar, "別的行事曆"), ["shared-id"]);
}

#[test]
fn plain_sqlite_writers_without_m2k_functions_keep_the_index_in_sync() {
    let dir = TempDir::new().unwrap();
    drop(Store::open(dir.path()).unwrap());
    let conn = database(&dir);
    conn.execute(
        "INSERT INTO items VALUES ('ext','mail','INBOX','1',NULL,1,'外部寫入的主旨','',NULL,NULL,
         '','', '[]','[\"not-an-object\"]','{bad json','[1,{\"filename\":\"外部附件.pdf\"}]',0)",
        [],
    )
    .unwrap();
    let store = Store::open(dir.path()).unwrap();
    assert_eq!(found(&store, Kind::Mail, "外部寫入"), ["ext"]);
    assert_eq!(found(&store, Kind::Mail, "外部附件"), ["ext"]);
    conn.execute("UPDATE items SET title = '已改的主旨' WHERE id = 'ext'", [])
        .unwrap();
    assert_eq!(found(&store, Kind::Mail, "已改的主旨"), ["ext"]);
    assert!(found(&store, Kind::Mail, "外部寫入").is_empty());
}

#[test]
fn schema_three_database_migrates_and_backfills_the_index() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .apply_snapshot(&snapshot(Kind::Mail, mail_fixture()), Utc::now())
        .unwrap();
    drop(store);
    database(&dir)
        .execute_batch(
            "DROP TRIGGER items_search_insert;
             DROP TRIGGER items_search_delete;
             DROP TRIGGER items_search_update;
             DROP TABLE items_search;
             PRAGMA user_version = 3;",
        )
        .unwrap();

    let store = Store::open_reader(dir.path()).unwrap();
    let conn = database(&dir);
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(indexed_rowids(&conn, "預算審核").len(), 1);
    assert_eq!(found(&store, Kind::Mail, "預算審核"), ["budget"]);
    assert_eq!(found(&store, Kind::Mail, "examplesoft 平台"), ["mixed"]);
}

#[test]
fn indexed_search_pages_in_the_same_order_as_the_scan() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let items = (0..25)
        .map(|index| {
            let mut mail = item(
                Kind::Mail,
                &format!("m{index:02}"),
                &format!("{} 號專案進度", ["甲", "乙", "丙"][index % 3]),
                "",
            );
            mail.date = Some(format!("2026-10-{:02}T00:00:00Z", 1 + index % 5));
            mail
        })
        .collect();
    store
        .apply_snapshot(&snapshot(Kind::Mail, items), Utc::now())
        .unwrap();
    let pages = |query: &str| {
        (0..3)
            .flat_map(|page| {
                store
                    .list_summaries_page(Kind::Mail, Some(query), None, 10, page * 10)
                    .unwrap()
                    .items
            })
            .map(|item| item.id)
            .collect::<Vec<_>>()
    };
    // "專案進度" uses the index, "專案" scans; both cover every item.
    let indexed = pages("專案進度");
    assert_eq!(indexed.len(), 25);
    assert_eq!(indexed, pages("專案"));
}

#[test]
fn resync_overwrites_every_field_including_ones_the_server_cleared() {
    // Snapshots used to be delete+insert, so nothing could linger. With the
    // differential upsert, a column missing from its SET would keep a stale
    // value (e.g. an etag the server dropped) and break If-Match checks.
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut full = item(Kind::Mail, "m1", "完整主旨", "完整正文");
    full.remote_id = "100".into();
    full.etag = Some("\"v1\"".into());
    full.subtitle = "王小明 <ming@example.test>".into();
    full.end_date = Some("2026-10-02T00:00:00Z".into());
    full.raw = "Subject: x\r\n\r\nbody".into();
    full.flags = vec!["\\Seen".into()];
    full.emails = vec!["ming@example.test".into()];
    full.phones = vec!["+886-2-0000-0000".into()];
    full.attachments = vec![Attachment {
        index: 0,
        filename: "附件.pdf".into(),
        content_type: "application/pdf".into(),
        size: 3,
    }];
    store
        .apply_snapshot(&snapshot(Kind::Mail, vec![full]), Utc::now())
        .unwrap();

    let mut cleared = item(Kind::Mail, "m1", "新主旨", "");
    cleared.remote_id = "200".into();
    cleared.uid_validity = Some(9);
    cleared.date = None;
    store
        .apply_snapshot(&snapshot(Kind::Mail, vec![cleared.clone()]), Utc::now())
        .unwrap();

    let stored = serde_json::to_value(store.get("m1").unwrap()).unwrap();
    assert_eq!(stored, serde_json::to_value(&cleared).unwrap());
}

#[test]
fn mail_snapshot_refuses_an_id_owned_by_another_kind() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .upsert(&item(Kind::Calendar, "shared-id", "行事曆事件", ""))
        .unwrap();
    let error = store
        .apply_snapshot(
            &snapshot(Kind::Mail, vec![item(Kind::Mail, "shared-id", "搶走", "")]),
            Utc::now(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("shared-id"), "{error}");
    assert_eq!(found(&store, Kind::Calendar, "行事曆事件"), ["shared-id"]);
    assert!(found(&store, Kind::Mail, "搶走").is_empty());
}
