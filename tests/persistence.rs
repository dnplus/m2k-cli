use chrono::{Duration, Utc};
use m2k::config::{self, Config};
use m2k::model::{Attachment, Collection, Item, Kind, Snapshot, SyncResult};
use m2k::store::Store;
use rusqlite::Connection;
use std::fs;
use tempfile::tempdir;

fn item(kind: Kind, id: &str, title: &str) -> Item {
    Item {
        id: id.to_owned(),
        kind,
        collection: "default".to_owned(),
        remote_id: id.to_owned(),
        etag: None,
        uid_validity: (kind == Kind::Mail).then_some(7),
        title: title.to_owned(),
        subtitle: String::new(),
        date: Some("2026-10-01T00:00:00Z".to_owned()),
        end_date: None,
        text: title.to_owned(),
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
            id: "default".to_owned(),
            name: "預設".to_owned(),
        }],
        items,
    }
}

fn config() -> Config {
    Config::new("person@example.com".to_owned(), "mail.example.test").unwrap()
}

#[test]
fn prefix_username_derives_server_domain_email_but_keeps_login_name() {
    let cfg = Config::new("Person".to_owned(), "mail.example.test").unwrap();
    assert_eq!(cfg.username, "Person");
    assert_eq!(cfg.email, "person@example.test");
    assert!(cfg
        .caldav_url
        .contains("/calendars/person%40example.test/default/"));
}

#[test]
fn credential_key_is_structured_and_tracks_service_endpoints() {
    let cfg = config();
    let key = config::credential_key(&cfg).unwrap();
    let mut canonical_case = cfg.clone();
    canonical_case.email = cfg.email.to_uppercase();
    assert_eq!(key, config::credential_key(&canonical_case).unwrap());

    let mut changed_port = cfg.clone();
    changed_port.smtp_port = 465;
    assert_ne!(key, config::credential_key(&changed_port).unwrap());

    let mut changed_host = cfg.clone();
    changed_host.imap_host = "imap.example.com".to_owned();
    assert_ne!(key, config::credential_key(&changed_host).unwrap());

    let mut changed_carddav = cfg.clone();
    changed_carddav.carddav_url.push_str("/addressbook");
    assert_ne!(key, config::credential_key(&changed_carddav).unwrap());
}

#[test]
fn identity_can_rebind_empty_store_but_not_nonempty_store() {
    let cfg = config();
    let mut changed = cfg.clone();
    changed.smtp_host = "smtp.example.com".to_owned();

    let empty_dir = tempdir().unwrap();
    let mut empty_store = Store::open(empty_dir.path()).unwrap();
    empty_store.ensure_account(&cfg).unwrap();
    empty_store.ensure_account(&changed).unwrap();

    let nonempty_dir = tempdir().unwrap();
    let mut nonempty_store = Store::open(nonempty_dir.path()).unwrap();
    nonempty_store.ensure_account(&cfg).unwrap();
    nonempty_store
        .apply_snapshot(
            &snapshot(Kind::Mail, vec![item(Kind::Mail, "identity-mail", "資料")]),
            Utc::now(),
        )
        .unwrap();
    let error = nonempty_store.ensure_account(&changed).unwrap_err();
    assert!(error.to_string().contains("帳號身分不符"));
}

#[test]
fn config_and_store_reopen_persist_with_permissions() {
    let dir = tempdir().unwrap();
    let cfg = config();
    config::save(dir.path(), &cfg).unwrap();
    assert_eq!(config::load(dir.path()).unwrap(), cfg);
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    store
        .apply_snapshot(
            &snapshot(Kind::Mail, vec![item(Kind::Mail, "mail-1", "保存")]),
            Utc::now(),
        )
        .unwrap();
    drop(store);
    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.get("mail-1").unwrap().title, "保存");
    assert!(dir.path().join("config.json").exists());
    assert!(dir.path().join("database.sqlite3").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        let config_mode = fs::metadata(dir.path().join("config.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let database_mode = fs::metadata(dir.path().join("database.sqlite3"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(config_mode, 0o600);
        assert_eq!(database_mode, 0o600);
    }
}

#[test]
fn snapshot_replacement_is_atomic_and_removes_remote_deletes() {
    let dir = tempdir().unwrap();
    let cfg = config();
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    let now = Utc::now();
    store
        .apply_snapshot(
            &snapshot(
                Kind::Mail,
                vec![
                    item(Kind::Mail, "mail-1", "第一封"),
                    item(Kind::Mail, "mail-2", "第二封"),
                ],
            ),
            now,
        )
        .unwrap();
    let invalid = Snapshot {
        kind: Kind::Mail,
        collections: vec![Collection {
            kind: Kind::Mail,
            id: "default".to_owned(),
            name: "預設".to_owned(),
        }],
        items: vec![
            item(Kind::Mail, "mail-3", "新封"),
            item(Kind::Mail, "mail-3", "重複"),
        ],
    };
    assert!(store.apply_snapshot(&invalid, now).is_err());
    assert_eq!(store.list(Kind::Mail, None, None, 0).unwrap().len(), 2);
    store
        .apply_snapshot(
            &snapshot(Kind::Mail, vec![item(Kind::Mail, "mail-3", "更新後")]),
            now,
        )
        .unwrap();
    assert!(store.get("mail-1").is_err());
    assert_eq!(store.get("mail-3").unwrap().title, "更新後");
}

#[test]
fn collection_snapshot_replaces_only_the_declared_scope() {
    let dir = tempdir().unwrap();
    let cfg = config();
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    let now = Utc::now();
    let mut branch_a = item(Kind::Contacts, "branch-a", "甲");
    branch_a.collection = "a".to_owned();
    let mut branch_b = item(Kind::Contacts, "branch-b", "乙");
    branch_b.collection = "b".to_owned();
    store
        .apply_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: vec![
                    Collection {
                        kind: Kind::Contacts,
                        id: "a".to_owned(),
                        name: "甲組".to_owned(),
                    },
                    Collection {
                        kind: Kind::Contacts,
                        id: "b".to_owned(),
                        name: "乙組".to_owned(),
                    },
                ],
                items: vec![branch_a.clone(), branch_b.clone()],
            },
            now,
        )
        .unwrap();
    branch_a.title = "甲組更新".to_owned();
    store
        .apply_collection_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: vec![Collection {
                    kind: Kind::Contacts,
                    id: "a".to_owned(),
                    name: "甲組".to_owned(),
                }],
                items: vec![branch_a],
            },
            now,
        )
        .unwrap();
    assert_eq!(store.get("branch-a").unwrap().title, "甲組更新");
    assert_eq!(store.get("branch-b").unwrap().title, "乙");
}

#[test]
fn upsert_snapshot_preserves_items_outside_the_partial_snapshot() {
    let dir = tempdir().unwrap();
    let cfg = config();
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    let now = Utc::now();
    store
        .apply_snapshot(
            &snapshot(
                Kind::Contacts,
                vec![
                    item(Kind::Contacts, "contact-1", "原有"),
                    item(Kind::Contacts, "contact-2", "保留"),
                ],
            ),
            now,
        )
        .unwrap();
    let updated = item(Kind::Contacts, "contact-1", "更新");
    store
        .upsert_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: vec![Collection {
                    kind: Kind::Contacts,
                    id: "default".to_owned(),
                    name: "預設".to_owned(),
                }],
                items: vec![updated.clone()],
            },
            now,
        )
        .unwrap();
    assert_eq!(store.get("contact-1").unwrap().title, "更新");
    assert_eq!(store.get("contact-2").unwrap().title, "保留");
}

#[test]
fn retention_prunes_old_items_but_preserves_rrule_and_permanent_contacts() {
    let dir = tempdir().unwrap();
    let mut cfg = config();
    cfg.mail_retention_days = 30;
    cfg.calendar_retention_days = 30;
    cfg.contacts_retention_days = 30;
    let now = Utc::now();
    let old = now - Duration::days(60);
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    let mut old_mail = item(Kind::Mail, "old-mail", "舊郵件");
    old_mail.date = Some(old.to_rfc3339());
    let mut old_event = item(Kind::Calendar, "old-event", "舊事件");
    old_event.date = Some(old.to_rfc3339());
    old_event.end_date = Some((old + Duration::hours(1)).to_rfc3339());
    let mut repeating = old_event.clone();
    repeating.id = "repeating-event".to_owned();
    repeating.remote_id = repeating.id.clone();
    repeating.raw = "BEGIN:VEVENT\nRRULE:FREQ=DAILY\nEND:VEVENT".to_owned();
    let mut old_contact = item(Kind::Contacts, "old-contact", "舊聯絡人");
    old_contact.date = None;
    store
        .apply_snapshot(&snapshot(Kind::Mail, vec![old_mail]), old)
        .unwrap();
    store
        .apply_snapshot(&snapshot(Kind::Calendar, vec![old_event, repeating]), old)
        .unwrap();
    store
        .apply_snapshot(&snapshot(Kind::Contacts, vec![old_contact]), old)
        .unwrap();
    assert_eq!(store.prune(&cfg, now).unwrap(), 3);
    assert!(store.get("old-mail").is_err());
    assert!(store.get("old-event").is_err());
    assert!(store.get("repeating-event").is_ok());
    assert!(store.get("old-contact").is_err());
    cfg.contacts_retention_days = 0;
    let mut newer_contact = item(Kind::Contacts, "new-contact", "活躍聯絡人");
    newer_contact.date = None;
    store.upsert(&newer_contact).unwrap();
    assert_eq!(store.prune(&cfg, now + Duration::days(365)).unwrap(), 0);
    assert!(store.get("new-contact").is_ok());
}

fn calendar_retention_fixture(id: &str, old: chrono::DateTime<Utc>) -> Item {
    let mut event = item(Kind::Calendar, id, id);
    event.date = Some(old.to_rfc3339());
    event.end_date = Some((old + Duration::hours(1)).to_rfc3339());
    event.raw = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{id}\r\nDTSTART:{}\r\nDTEND:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        old.format("%Y%m%dT%H%M%SZ"),
        (old + Duration::hours(1)).format("%Y%m%dT%H%M%SZ"),
    );
    event
}

#[test]
fn retention_preserves_rdate_only_resource_with_an_old_master_and_future_occurrence() {
    let dir = tempdir().unwrap();
    let mut cfg = config();
    cfg.calendar_retention_days = 30;
    let now = Utc::now();
    let old = now - Duration::days(60);
    let mut recurring = calendar_retention_fixture("rdate-only", old);
    recurring.raw = recurring.raw.replace(
        "END:VEVENT",
        &format!(
            "RDATE:\r\n {}\r\nEND:VEVENT",
            (now + Duration::days(10)).format("%Y%m%dT%H%M%SZ")
        ),
    );
    let raw = recurring.raw.clone();
    let plain = calendar_retention_fixture("old-plain", old);
    let mut store = Store::open(dir.path()).unwrap();
    store
        .apply_collection_snapshot(&snapshot(Kind::Calendar, vec![recurring, plain]), now)
        .unwrap();
    assert_eq!(store.prune_kind(&cfg, now, Kind::Calendar).unwrap(), 1);
    assert_eq!(store.get("rdate-only").unwrap().raw, raw);
    assert!(store.get("old-plain").is_err());
}

#[test]
fn retention_preserves_multi_event_resource_when_first_event_is_old() {
    let dir = tempdir().unwrap();
    let mut cfg = config();
    cfg.calendar_retention_days = 30;
    let now = Utc::now();
    let old = now - Duration::days(60);
    let mut multiple = calendar_retention_fixture("multiple", old);
    let future = now + Duration::days(10);
    multiple.raw = multiple.raw.replace(
        "END:VCALENDAR",
        &format!(
            "BEGIN:VEVENT\r\nUID:multiple\r\nDTSTART:{}\r\nDTEND:{}\r\nEND:VEVENT\r\nEND:VCALENDAR",
            future.format("%Y%m%dT%H%M%SZ"),
            (future + Duration::hours(1)).format("%Y%m%dT%H%M%SZ"),
        ),
    );
    let raw = multiple.raw.clone();
    let plain = calendar_retention_fixture("old-plain", old);
    let mut store = Store::open(dir.path()).unwrap();
    store
        .apply_collection_snapshot(&snapshot(Kind::Calendar, vec![multiple, plain]), now)
        .unwrap();
    assert_eq!(store.prune_kind(&cfg, now, Kind::Calendar).unwrap(), 1);
    assert_eq!(store.get("multiple").unwrap().raw, raw);
    assert!(store.get("old-plain").is_err());
}

#[test]
fn retention_checks_recurrence_properties_instead_of_description_words() {
    let dir = tempdir().unwrap();
    let mut cfg = config();
    cfg.calendar_retention_days = 30;
    let now = Utc::now();
    let old = now - Duration::days(60);
    let mut plain = calendar_retention_fixture("plain-description", old);
    plain.raw = plain.raw.replace(
        "END:VEVENT",
        "DESCRIPTION:Examples of RRULE and RDATE are plain text\r\nEND:VEVENT",
    );
    let mut store = Store::open(dir.path()).unwrap();
    store
        .apply_collection_snapshot(&snapshot(Kind::Calendar, vec![plain]), now)
        .unwrap();
    assert_eq!(store.prune_kind(&cfg, now, Kind::Calendar).unwrap(), 1);
    assert!(store.get("plain-description").is_err());
}

#[test]
fn prune_kinds_only_prunes_requested_kind() {
    let dir = tempdir().unwrap();
    let mut cfg = config();
    cfg.mail_retention_days = 30;
    cfg.calendar_retention_days = 30;
    let now = Utc::now();
    let old = now - Duration::days(60);
    let mut old_mail = item(Kind::Mail, "scoped-old-mail", "舊郵件");
    old_mail.date = Some(old.to_rfc3339());
    let mut old_event = item(Kind::Calendar, "scoped-old-event", "舊事件");
    old_event.date = Some(old.to_rfc3339());
    old_event.end_date = Some((old + Duration::hours(1)).to_rfc3339());

    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    store
        .apply_snapshot(&snapshot(Kind::Mail, vec![old_mail]), old)
        .unwrap();
    store
        .apply_snapshot(&snapshot(Kind::Calendar, vec![old_event]), old)
        .unwrap();

    assert_eq!(store.prune_kinds(&cfg, now, &[Kind::Mail]).unwrap(), 1);
    assert!(store.get("scoped-old-mail").is_err());
    assert!(store.get("scoped-old-event").is_ok());
}

#[test]
fn unicode_case_insensitive_search_and_sync_status_survive_reopen() {
    let dir = tempdir().unwrap();
    let cfg = config();
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    store
        .apply_snapshot(
            &snapshot(
                Kind::Contacts,
                vec![item(Kind::Contacts, "contact-1", "Équipe 測試")],
            ),
            Utc::now(),
        )
        .unwrap();
    let matches = store
        .list(Kind::Contacts, Some("éQUIPE"), None, 10)
        .unwrap();
    assert_eq!(matches.len(), 1);
    store
        .record_sync(
            &SyncResult {
                kind: Kind::Contacts,
                count: 1,
                error: None,
            },
            Utc::now(),
        )
        .unwrap();
    let status = store.sync_status().unwrap();
    assert_eq!(status["contacts"]["count"], 1);
    assert_eq!(store.counts().unwrap()["contacts"], 1);
}

#[test]
fn list_pushes_collection_order_and_limit_into_sqlite() {
    let dir = tempdir().unwrap();
    let cfg = config();
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    let mut newest = item(Kind::Contacts, "contact-03", "最新");
    newest.collection = "Équipe".to_owned();
    newest.date = Some("2026-10-03T00:00:00Z".to_owned());
    let mut middle = item(Kind::Contacts, "contact-01", "中間");
    middle.collection = "Équipe".to_owned();
    middle.date = Some("2026-10-02T00:00:00Z".to_owned());
    let mut same_date = item(Kind::Contacts, "contact-02", "同日");
    same_date.collection = "Équipe".to_owned();
    same_date.date = Some("2026-10-02T00:00:00Z".to_owned());
    let mut outside = item(Kind::Contacts, "contact-04", "不應出現");
    outside.collection = "other".to_owned();
    outside.date = Some("2026-10-04T00:00:00Z".to_owned());
    store
        .apply_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: vec![
                    Collection {
                        kind: Kind::Contacts,
                        id: "Équipe".to_owned(),
                        name: "團隊".to_owned(),
                    },
                    Collection {
                        kind: Kind::Contacts,
                        id: "other".to_owned(),
                        name: "其他".to_owned(),
                    },
                ],
                items: vec![newest, middle, same_date, outside],
            },
            Utc::now(),
        )
        .unwrap();
    let listed = store.list(Kind::Contacts, None, Some("éQUIPE"), 2).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["contact-03", "contact-01"]
    );
}

#[test]
fn list_searches_useful_fields_without_decoding_irrelevant_mail_raw() {
    let dir = tempdir().unwrap();
    let cfg = config();
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    let mut attachment_mail = item(Kind::Mail, "mail-attachment", "一般主旨");
    attachment_mail.attachments = vec![Attachment {
        index: 0,
        filename: "報表-2026.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        size: 12,
    }];
    let mut encoded_mail = item(Kind::Mail, "mail-encoded", "另一封");
    encoded_mail.raw = "base64:c2VjcmV0LWJvZHk=".to_owned();
    let mut contact = item(Kind::Contacts, "contact-vcard", "聯絡人");
    contact.raw = "BEGIN:VCARD\nFN:原始卡片\nEND:VCARD".to_owned();
    store
        .apply_snapshot(
            &snapshot(Kind::Mail, vec![attachment_mail, encoded_mail]),
            Utc::now(),
        )
        .unwrap();
    store
        .apply_snapshot(&snapshot(Kind::Contacts, vec![contact]), Utc::now())
        .unwrap();

    let attachment_match = store.list(Kind::Mail, Some("報表-2026"), None, 10).unwrap();
    assert_eq!(
        attachment_match
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["mail-attachment"]
    );
    let raw_contact_match = store
        .list(Kind::Contacts, Some("原始卡片"), None, 10)
        .unwrap();
    assert_eq!(raw_contact_match.len(), 1);
    assert!(store
        .list(Kind::Mail, Some("secret-body"), None, 10)
        .unwrap()
        .is_empty());

    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .execute(
            "UPDATE items
             SET emails_json = 'not-json', raw = ?1
             WHERE id = 'mail-encoded'",
            ["base64:very-large-irrelevant-body".repeat(10_000)],
        )
        .unwrap();
    drop(connection);
    let target = store.list(Kind::Mail, Some("一般主旨"), None, 10).unwrap();
    assert_eq!(target.len(), 1);
    assert_eq!(target[0].id, "mail-attachment");
}

#[test]
fn failed_sync_attempt_keeps_last_success_status() {
    let dir = tempdir().unwrap();
    let cfg = config();
    let first = Utc::now();
    let second = first + Duration::minutes(5);
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    store
        .record_sync(
            &SyncResult {
                kind: Kind::Mail,
                count: 4,
                error: None,
            },
            first,
        )
        .unwrap();
    let first_status = store.sync_status();
    let first_status = first_status.unwrap();
    let first_success_at = first_status["mail"]["last_success_at"].clone();
    assert_eq!(first_status["mail"]["count"], 4);
    assert_eq!(first_status["mail"]["last_success_count"], 4);

    store
        .record_sync(
            &SyncResult {
                kind: Kind::Mail,
                count: 0,
                error: Some("連線失敗".to_owned()),
            },
            second,
        )
        .unwrap();
    let status = store.sync_status().unwrap();
    assert_eq!(status["mail"]["count"], 4);
    assert_eq!(status["mail"]["last_success_count"], 4);
    assert_eq!(status["mail"]["last_success_at"], first_success_at);
    assert_eq!(status["mail"]["error"], "連線失敗");
    assert_ne!(
        status["mail"]["attempted_at"],
        status["mail"]["last_success_at"]
    );
}

#[test]
fn legacy_sync_state_schema_migrates_last_success_fields() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("database.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE sync_state (
                 kind TEXT PRIMARY KEY NOT NULL,
                 count INTEGER NOT NULL,
                 error TEXT,
                 synced_at INTEGER NOT NULL
             );
             INSERT INTO sync_state(kind, count, error, synced_at)
             VALUES ('contacts', 3, NULL, 1791321600);",
        )
        .unwrap();
    drop(connection);

    let store = Store::open(dir.path()).unwrap();
    let status = store.sync_status().unwrap();
    assert_eq!(status["contacts"]["count"], 3);
    assert_eq!(status["contacts"]["last_success_count"], 3);
    assert_eq!(
        status["contacts"]["attempted_at"],
        status["contacts"]["last_success_at"]
    );
}

#[test]
fn legacy_dav_identity_accepts_equivalent_urls_but_rejects_other_changes() {
    let dir = tempdir().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let old = config();
    store.ensure_account(&old).unwrap();
    store
        .upsert(&item(Kind::Calendar, "identity-fixture", "fixture"))
        .unwrap();
    // Simulate metadata written by the old version, before normalization existed.
    let conn = Connection::open(store.database_path()).unwrap();
    for (key, value) in [
        ("account.caldav_url", &old.caldav_url),
        ("account.carddav_url", &old.carddav_url),
    ] {
        conn.execute(
            "UPDATE metadata SET value = ?1 WHERE key = ?2",
            [value.replace('@', "%40").trim_end_matches('/'), key],
        )
        .unwrap();
    }
    let mut equivalent = old.clone();
    equivalent.caldav_url = format!(
        "{}/",
        old.caldav_url.replace("%40", "@").trim_end_matches('/')
    );
    equivalent.carddav_url = format!(
        "{}/",
        old.carddav_url.replace("%40", "@").trim_end_matches('/')
    );
    store.verify_account(&equivalent).unwrap();
    store.ensure_account(&equivalent).unwrap();
    for field in [
        "username",
        "email",
        "imap_host",
        "smtp_host",
        "caldav_url",
        "carddav_url",
    ] {
        let mut changed = equivalent.clone();
        match field {
            "username" => changed.username.push('x'),
            "email" => changed.email.insert(0, 'x'),
            "imap_host" => changed.imap_host.insert_str(0, "other."),
            "smtp_host" => changed.smtp_host.insert_str(0, "other."),
            "caldav_url" => changed.caldav_url.push_str("different/"),
            "carddav_url" => changed.carddav_url.push_str("different/"),
            _ => unreachable!(),
        }
        assert!(store.verify_account(&changed).is_err(), "{field}");
        assert!(store.ensure_account(&changed).is_err(), "{field}");
    }
}
