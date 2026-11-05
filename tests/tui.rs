use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use m2k::model::{Collection, Item, Kind, Snapshot};
use m2k::store::Store;
use m2k::tui::{render_snapshot, render_snapshot_after_keys};
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

const CARD_BASE: &str = "https://mail.example.test/carddav/";

fn company_store() -> TempDir {
    let temp = TempDir::new().unwrap();
    let collections = ["ACME", "ACME/ENG", "ACME/MKT"]
        .into_iter()
        .map(|path| Collection {
            kind: Kind::Contacts,
            id: format!("{CARD_BASE}{path}/"),
            name: format!("{}(/{path})", path.rsplit('/').next().unwrap()),
        })
        .collect();
    let person = |id: &str, path: &str, uid: &str, name: &str, extra: &str| {
        Item {
        id: id.into(),
        kind: Kind::Contacts,
        collection: format!("{CARD_BASE}{path}/"),
        remote_id: format!("{id}.vcf"),
        etag: None,
        uid_validity: None,
        title: name.into(),
        subtitle: String::new(),
        date: None,
        end_date: None,
        text: String::new(),
        raw: format!(
            "BEGIN:VCARD\nVERSION:3.0\nUID:{uid}\nFN:{name}\n{extra}EMAIL:{uid}@example.test\nEND:VCARD\n"
        ),
        flags: vec![],
        emails: vec![format!("{uid}@example.test")],
        phones: vec![],
        attachments: vec![],
    }
    };
    let items = vec![
        person("1", "ACME", "ceo", "王大同", ""),
        person("2", "ACME", "lin", "林工程", ""),
        person("3", "ACME/ENG", "lin", "林工程", "TITLE:協理\n"),
        person("4", "ACME", "mkt", "許行銷", ""),
        person("5", "ACME/MKT", "mkt", "許行銷", ""),
    ];
    Store::open(temp.path())
        .unwrap()
        .upsert_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections,
                items,
            },
            Utc::now(),
        )
        .unwrap();
    temp
}

fn press(chars: &str) -> Vec<KeyEvent> {
    chars
        .chars()
        .map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
        .collect()
}

fn with(mut keys: Vec<KeyEvent>, extra: &[KeyCode]) -> Vec<KeyEvent> {
    keys.extend(
        extra
            .iter()
            .map(|code| KeyEvent::new(*code, KeyModifiers::NONE)),
    );
    keys
}

#[test]
fn contacts_v_toggles_org_chart_built_from_the_same_tree_as_directory_chart() {
    let temp = company_store();
    let chart = render_snapshot_after_keys(temp.path(), 120, 32, &press("3v")).unwrap();
    // 根部門預設展開，子部門收合：使用者先看到公司層級，再逐層展開。
    assert!(chart.contains("聯絡人 · 組織圖 · 3 個部門"), "{chart}");
    assert!(chart.contains("[v] ACME（1 人）"), "{chart}");
    assert!(chart.contains("王大同"), "{chart}");
    assert!(chart.contains("[>] ENG（1 人）"), "{chart}");
    assert!(
        !chart.contains("林工程 · 協理"),
        "子部門未展開前不應列出成員\n{chart}"
    );
    assert!(chart.contains("組織圖 · 部門 3 個 · 成員 3 位"), "{chart}");

    // 再按一次 v 回到原本的扁平列表。
    let list = render_snapshot_after_keys(temp.path(), 120, 32, &press("3vv")).unwrap();
    assert!(!list.contains("組織圖 · 3 個部門"), "{list}");
    assert!(list.contains("主旨 / 姓名"), "{list}");
}

#[test]
fn org_chart_enter_expands_department_and_member_detail_matches_contact_fields() {
    let temp = company_store();
    // ACME, 王大同, ENG → Enter 展開 ENG，j 選到林工程。
    let keys = with(press("3vjj"), &[KeyCode::Enter, KeyCode::Char('j')]);
    let frame = render_snapshot_after_keys(temp.path(), 120, 32, &keys).unwrap();
    assert!(frame.contains("[v] ENG（1 人）"), "{frame}");
    assert!(frame.contains("林工程 · 協理"), "{frame}");
    // 詳情面板與聯絡人列表的詳情同一套欄位（format_contact_detail）。
    assert!(frame.contains("職稱：協理"), "{frame}");
    assert!(frame.contains("Email：lin@example.test"), "{frame}");
    assert!(frame.contains("資料夾／群組："), "{frame}");

    // 在部門上再按 Enter 收合。
    let keys = with(press("3vjj"), &[KeyCode::Enter, KeyCode::Enter]);
    let frame = render_snapshot_after_keys(temp.path(), 120, 32, &keys).unwrap();
    assert!(frame.contains("[>] ENG（1 人）"), "{frame}");
    assert!(!frame.contains("林工程 · 協理"), "{frame}");
}

#[test]
fn org_chart_on_empty_cache_explains_why_and_points_to_shift_r_without_syncing() {
    let temp = TempDir::new().unwrap();
    let frame = render_snapshot_after_keys(temp.path(), 120, 32, &press("3v")).unwrap();
    assert!(frame.contains("組織圖：本機沒有通訊錄快取"), "{frame}");
    assert!(frame.contains("Shift-R"), "{frame}");
    assert!(!frame.contains("正在背景同步"), "不可自動同步\n{frame}");
}

#[test]
fn org_chart_on_flat_address_book_warns_that_company_root_is_missing() {
    let temp = TempDir::new().unwrap();
    m2k::demo::seed(temp.path()).unwrap();
    let frame = render_snapshot_after_keys(temp.path(), 120, 32, &press("3v")).unwrap();
    assert!(frame.contains("快取沒有公司根與子部門"), "{frame}");
    assert!(frame.contains("Shift-R"), "{frame}");
    assert!(frame.contains("王小明"), "{frame}");
}
