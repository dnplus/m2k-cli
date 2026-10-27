use chrono::{Duration, Utc};
use m2k::{
    config::{self, Config},
    mail_workflow::{
        execute, normalize_thread_subject, read_cleanup_plan, validate_cleanup_plan,
        validate_cleanup_plan_account, validate_cleanup_snapshot, CleanupAction,
        MailWorkflowCommand,
    },
    model::{Collection, Item, Kind, Snapshot},
    store::Store,
};
use rusqlite::Connection;
use std::{fs, path::PathBuf};
use tempfile::tempdir;

fn config_for(username: &str) -> Config {
    Config::new(username.to_owned(), "mail.example.test").expect("valid test config")
}

fn mail_item(id: &str, uid: u32, title: &str) -> Item {
    Item {
        id: id.to_owned(),
        kind: Kind::Mail,
        collection: "INBOX".to_owned(),
        remote_id: uid.to_string(),
        etag: Some(format!("7:{uid}")),
        uid_validity: Some(7),
        title: title.to_owned(),
        subtitle: "sender@example.test".to_owned(),
        date: Some("2026-01-01T00:00:00Z".to_owned()),
        end_date: None,
        text: title.to_owned(),
        raw: format!("Subject: {title}\r\n\r\n{title}\r\n"),
        flags: Vec::new(),
        emails: vec!["sender@example.test".to_owned()],
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

fn seed_local(dir: &std::path::Path, items: Vec<Item>) {
    let config = config_for("person@example.com");
    config::save(dir, &config).expect("save config");
    let mut store = Store::open(dir).expect("open store");
    store.ensure_account(&config).expect("bind account");
    store
        .apply_snapshot(
            &Snapshot {
                kind: Kind::Mail,
                collections: vec![Collection {
                    kind: Kind::Mail,
                    id: "INBOX".to_owned(),
                    name: "INBOX".to_owned(),
                }],
                items,
            },
            Utc::now(),
        )
        .expect("seed mail");
}

fn preview_plan(dir: &std::path::Path, output: PathBuf) {
    execute(
        dir,
        MailWorkflowCommand::CleanupPreview {
            query: Some("needle".to_owned()),
            folder: Some("INBOX".to_owned()),
            before: None,
            action: CleanupAction::Delete,
            destination: None,
            output,
            limit: 0,
        },
        true,
    )
    .expect("preview should succeed");
}

#[test]
fn thread_normalizes_re_and_fwd_but_keeps_unrelated_subjects_separate() {
    assert_eq!(normalize_thread_subject(" Re: Fwd: 專案同步 "), "專案同步");
    assert_eq!(normalize_thread_subject("專案同步"), "專案同步");
    assert_ne!(normalize_thread_subject("專案同步會議"), "專案同步");
}

#[test]
fn tampered_signed_cleanup_plan_is_rejected() {
    let dir = tempdir().expect("tempdir");
    seed_local(dir.path(), vec![mail_item("one", 1, "needle")]);
    let output = dir.path().join("cleanup.json");
    preview_plan(dir.path(), output.clone());
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    let signature = value["signature"].as_str().unwrap().to_owned();
    let replacement = if signature.ends_with('0') { '1' } else { '0' };
    value["signature"] = serde_json::Value::String(format!(
        "{}{}",
        &signature[..signature.len() - 1],
        replacement
    ));
    fs::write(&output, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    let error = read_cleanup_plan(dir.path(), &output).expect_err("tampering must fail");
    assert!(error.to_string().contains("簽章"));
}

#[test]
fn expired_cleanup_plan_is_rejected() {
    let dir = tempdir().expect("tempdir");
    seed_local(dir.path(), vec![mail_item("one", 1, "needle")]);
    let output = dir.path().join("cleanup.json");
    preview_plan(dir.path(), output.clone());
    let mut plan = read_cleanup_plan(dir.path(), &output).expect("read plan");
    plan.expires_at = (Utc::now() - Duration::seconds(1)).to_rfc3339();
    let error = validate_cleanup_plan(&plan).expect_err("expired plan must fail");
    assert!(error.to_string().contains("過期"));
}

#[test]
fn cleanup_plan_account_mismatch_is_rejected() {
    let dir = tempdir().expect("tempdir");
    seed_local(dir.path(), vec![mail_item("one", 1, "needle")]);
    let output = dir.path().join("cleanup.json");
    preview_plan(dir.path(), output.clone());
    let plan = read_cleanup_plan(dir.path(), &output).expect("read plan");
    let other = config_for("other@example.com");
    let error = validate_cleanup_plan_account(&plan, &other).expect_err("account drift must fail");
    assert!(error.to_string().contains("帳號"));
}

#[test]
fn cleanup_count_change_is_rejected_before_remote_actions() {
    let dir = tempdir().expect("tempdir");
    seed_local(dir.path(), vec![mail_item("one", 1, "needle")]);
    let output = dir.path().join("cleanup.json");
    preview_plan(dir.path(), output.clone());
    let plan = read_cleanup_plan(dir.path(), &output).expect("read plan");
    let changed = Snapshot {
        kind: Kind::Mail,
        collections: vec![Collection {
            kind: Kind::Mail,
            id: "INBOX".to_owned(),
            name: "INBOX".to_owned(),
        }],
        items: vec![
            mail_item("one", 1, "needle"),
            mail_item("two", 2, "needle second"),
        ],
    };
    let error = validate_cleanup_snapshot(&plan, &changed).expect_err("count drift must fail");
    assert!(error.to_string().contains("數量"));
}

#[test]
fn preview_refuses_missing_uidvalidity_without_writing_plan() {
    let dir = tempdir().expect("tempdir");
    let mut item = mail_item("one", 1, "needle");
    item.uid_validity = None;
    seed_local(dir.path(), vec![item]);
    let output = dir.path().join("cleanup.json");
    let error = execute(
        dir.path(),
        MailWorkflowCommand::CleanupPreview {
            query: Some("needle".to_owned()),
            folder: Some("INBOX".to_owned()),
            before: None,
            action: CleanupAction::Delete,
            destination: None,
            output: output.clone(),
            limit: 0,
        },
        true,
    )
    .expect_err("missing UIDVALIDITY must fail before plan creation");
    assert!(error.to_string().contains("UIDVALIDITY"));
    assert!(!output.exists());
}

#[test]
fn preview_refuses_zero_uidvalidity_without_writing_plan() {
    let dir = tempdir().expect("tempdir");
    seed_local(dir.path(), vec![mail_item("one", 1, "needle")]);
    let database =
        Connection::open(dir.path().join("database.sqlite3")).expect("open test database");
    database
        .execute("UPDATE items SET uid_validity = 0 WHERE id = 'one'", [])
        .expect("corrupt test UIDVALIDITY");
    let output = dir.path().join("cleanup.json");
    let error = execute(
        dir.path(),
        MailWorkflowCommand::CleanupPreview {
            query: Some("needle".to_owned()),
            folder: Some("INBOX".to_owned()),
            before: None,
            action: CleanupAction::Delete,
            destination: None,
            output: output.clone(),
            limit: 0,
        },
        true,
    )
    .expect_err("zero UIDVALIDITY must fail before plan creation");
    assert!(error.to_string().contains("UIDVALIDITY"));
    assert!(!output.exists());
}

#[test]
fn preview_refuses_overwriting_an_existing_plan() {
    let dir = tempdir().expect("tempdir");
    seed_local(dir.path(), vec![mail_item("one", 1, "needle")]);
    let output = dir.path().join("cleanup.json");
    preview_plan(dir.path(), output.clone());
    let error = execute(
        dir.path(),
        MailWorkflowCommand::CleanupPreview {
            query: Some("needle".to_owned()),
            folder: Some("INBOX".to_owned()),
            before: None,
            action: CleanupAction::Delete,
            destination: None,
            output,
            limit: 0,
        },
        true,
    )
    .expect_err("preview must create a new plan");
    assert!(error.to_string().contains("不覆寫"));
}
