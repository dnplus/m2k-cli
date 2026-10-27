use chrono::Utc;
use m2k::model::{Collection, Item, Kind, Snapshot};
use m2k::store::Store;
use m2k::tui::render_snapshot;
use std::fs;
use tempfile::TempDir;

fn mail_item() -> Item {
    Item {
        id: "mail-summary".into(),
        kind: Kind::Mail,
        collection: "INBOX".into(),
        remote_id: "7".into(),
        etag: None,
        uid_validity: Some(11),
        title: "摘要測試主旨".into(),
        subtitle: "sender@example.com".into(),
        date: Some("2026-10-07T00:00:00Z".into()),
        end_date: None,
        text: "這是只有詳細頁才應載入的郵件本文".into(),
        raw: "完整 RFC822 內容".into(),
        flags: vec!["\\Seen".into()],
        emails: vec!["sender@example.com".into()],
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

#[test]
fn snapshot_renders_offline_shell_without_a_tty() {
    let temp = TempDir::new().unwrap();
    let output = render_snapshot(temp.path(), 100, 24).unwrap();
    assert!(output.contains("m2k"));
    assert!(output.contains("郵件"));
    assert!(output.contains("行事曆"));
    assert!(output.contains("聯絡人"));
    assert!(output.contains("q 離開"));
    assert!(output.contains("離線模式") || output.contains("設定已載入"));
}

#[test]
fn snapshot_handles_narrow_and_zero_dimensions() {
    let temp = TempDir::new().unwrap();
    assert!(render_snapshot(temp.path(), 69, 12).is_ok());
    assert_eq!(render_snapshot(temp.path(), 0, 24).unwrap(), "");
    assert_eq!(render_snapshot(temp.path(), 70, 0).unwrap(), "");
}

#[test]
fn snapshot_does_not_require_a_config_file() {
    let temp = TempDir::new().unwrap();
    let output = render_snapshot(temp.path(), 80, 10).unwrap();
    assert!(output.contains("離線模式") || output.contains("設定已載入"));
    assert!(!temp.path().join("config.json").exists());
}

#[test]
fn snapshot_is_repeatable_for_an_unchanged_empty_store() {
    let temp = TempDir::new().unwrap();
    let first = render_snapshot(temp.path(), 90, 18).unwrap();
    let second = render_snapshot(temp.path(), 90, 18).unwrap();
    assert_eq!(first, second);
    assert!(fs::read_dir(temp.path()).unwrap().next().is_some());
}

#[test]
fn list_summaries_omits_mail_body_but_keeps_searchable_metadata() {
    let temp = TempDir::new().unwrap();
    let item = mail_item();
    let mut store = Store::open(temp.path()).unwrap();
    store
        .apply_snapshot(
            &Snapshot {
                kind: Kind::Mail,
                collections: vec![Collection {
                    kind: Kind::Mail,
                    id: "INBOX".into(),
                    name: "收件匣".into(),
                }],
                items: vec![item.clone()],
            },
            Utc::now(),
        )
        .unwrap();

    let summaries = store
        .list_summaries(Kind::Mail, Some("詳細頁才應載入"), None, 10)
        .unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].title, item.title);
    assert_eq!(summaries[0].flags, item.flags);
    assert!(summaries[0].text.is_empty());
    assert!(summaries[0].raw.is_empty());
    assert_eq!(store.get("mail-summary").unwrap().text, item.text);
}
