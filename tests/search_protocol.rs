use m2k::{
    config::Config,
    model::{Item, Kind},
    provider::mail::{fetch_item, search},
};

fn config() -> Config {
    let mut config = Config::new("user@example.test".into(), "mail.example.test").unwrap();
    config.imap_port = 0; // Fail immediately if a test unexpectedly opens a connection.
    config
}

fn summary() -> Item {
    Item {
        id: "mail:INBOX:7:12".into(),
        kind: Kind::Mail,
        collection: "INBOX".into(),
        remote_id: "12".into(),
        etag: Some("7:12".into()),
        uid_validity: Some(7),
        title: "summary".into(),
        subtitle: "sender@example.test".into(),
        date: None,
        end_date: None,
        text: String::new(),
        raw: String::new(),
        flags: Vec::new(),
        emails: Vec::new(),
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

#[test]
fn search_rejects_command_injection_before_network() {
    for query in [
        "match\r\nA1 UID STORE 1 +FLAGS (\\Seen)",
        "match\n",
        "match\t",
        "\0",
        "\u{7f}",
    ] {
        let error = search(&config(), "", query, None, 500).unwrap_err();
        assert!(error.to_string().contains("控制字元"));
    }
}

#[test]
fn search_rejects_empty_and_oversized_queries_before_network() {
    for query in ["", "   "] {
        assert!(search(&config(), "", query, None, 500)
            .unwrap_err()
            .to_string()
            .contains("空白"));
    }
    assert!(search(&config(), "", &"x".repeat(4097), None, 500)
        .unwrap_err()
        .to_string()
        .contains("4096"));
}

#[test]
fn search_rejects_invalid_folder_before_network() {
    assert!(search(&config(), "", "match", Some("INBOX\r\n"), 500)
        .unwrap_err()
        .to_string()
        .contains("控制字元"));
}

#[test]
fn zero_capacity_unicode_search_returns_bounded_empty_result_without_network() {
    let result = search(&config(), "", "中文 \"quoted\" \\ value", None, 0).unwrap();
    assert!(result.items.is_empty());
    assert!(result.truncated);
}

#[test]
fn cached_body_can_be_loaded_offline_without_credentials() {
    let mut item = summary();
    item.raw = "base64:U3ViamVjdDogY2FjaGVkDQoNCmJvZHk=".into();
    item.text = "cached body".into();
    item.flags = vec!["\\Flagged".into()];
    let loaded = fetch_item(&config(), "", &item).unwrap();
    assert_eq!(loaded.id, item.id);
    assert_eq!(loaded.raw, item.raw);
    assert_eq!(loaded.text, item.text);
    assert_eq!(loaded.flags, item.flags);
}

#[test]
fn lazy_body_rejects_missing_identity_before_network() {
    let mut item = summary();
    item.uid_validity = None;
    assert!(fetch_item(&config(), "", &item)
        .unwrap_err()
        .to_string()
        .contains("UIDVALIDITY"));
    item.uid_validity = Some(7);
    item.remote_id = "0".into();
    assert!(fetch_item(&config(), "", &item)
        .unwrap_err()
        .to_string()
        .contains("UID"));
}

#[test]
fn lazy_body_rejects_calendar_item_before_network() {
    let mut item = summary();
    item.kind = Kind::Calendar;
    assert!(fetch_item(&config(), "", &item)
        .unwrap_err()
        .to_string()
        .contains("不是郵件"));
}
