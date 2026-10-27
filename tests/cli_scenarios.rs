use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use serde_json::Value;
use tempfile::TempDir;

// A keychain file that does not exist: m2k must never read or write the
// login keychain from tests.
const NO_KEYCHAIN: &str = "/nonexistent/m2k-test.keychain-db";

fn command(dir: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_m2k"));
    command.args(["--json", "--data-dir"]).arg(dir.path());
    command
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN);
    command
}

fn json(dir: &TempDir, args: &[&str]) -> Value {
    let output = command(dir)
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

#[test]
fn demo_survives_process_restart_and_supports_offline_chinese_search() {
    let dir = TempDir::new().unwrap();
    let seed = json(&dir, &["demo", "--no-tui"]);
    assert_eq!(seed["counts"]["mail"], 2);
    let found = json(&dir, &["mail", "search", "原生"]);
    assert_eq!(found.as_array().unwrap().len(), 1);
    assert_eq!(found[0]["id"], "demo-mail-1");
    let contact = json(&dir, &["directory", "resolve", "王小明"]);
    assert_eq!(contact["emails"][0], "ming@example.test");
    assert_eq!(json(&dir, &["directory", "self"])["name"], "展示使用者");
    let reopened = json(&dir, &["status"]);
    assert_eq!(reopened["counts"], seed["counts"]);
    command(&dir)
        .args(["tui", "--snapshot"])
        .assert()
        .success()
        .stdout(contains("m2k").and(contains("郵件")));
}

#[test]
fn init_persists_no_password_and_endpoint_changes_cannot_cross_existing_data() {
    let dir = TempDir::new().unwrap();
    json(
        &dir,
        &[
            "init",
            "--username",
            "sample",
            "--server",
            "mail.example.test",
        ],
    );
    let config = std::fs::read_to_string(dir.path().join("config.json")).unwrap();
    assert!(!config.contains("password"));
    assert!(config.contains("sample@example.test"));
    let demo = TempDir::new().unwrap();
    json(&demo, &["demo", "--no-tui"]);
    command(&demo)
        .args(["config", "set", "--smtp-host", "other.example"])
        .assert()
        .failure()
        .stdout(contains("帳號身分"));
    assert_eq!(
        json(&demo, &["status"])["config"]["smtp_host"],
        "mail.example.test"
    );
}

#[test]
fn cleanup_preview_is_reviewable_and_demo_execute_cannot_reach_a_server() {
    let dir = TempDir::new().unwrap();
    json(&dir, &["demo", "--no-tui"]);
    let plan = dir.path().join("cleanup.json");
    let output = command(&dir)
        .args([
            "mail",
            "cleanup-preview",
            "--folder",
            "INBOX",
            "--action",
            "move",
            "--destination",
            "Archive",
            "--output",
        ])
        .arg(&plan)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let preview: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(preview["selected_count"], 2);
    assert_eq!(preview["folders"][0]["uids"], serde_json::json!([1, 2]));
    command(&dir)
        .args(["mail", "cleanup-execute", "--plan"])
        .arg(&plan)
        .arg("--apply")
        .assert()
        .failure()
        .stdout(contains("展示"));
    assert_eq!(json(&dir, &["status"])["counts"]["mail"], 2);
    command(&dir)
        .arg("watch")
        .timeout(std::time::Duration::from_secs(2))
        .assert()
        .failure()
        .stdout(contains("展示"));
}

#[test]
fn prepare_and_invite_dry_run_preserve_attendees_without_network() {
    let dir = TempDir::new().unwrap();
    json(&dir, &["demo", "--no-tui"]);
    let invite = dir.path().join("invite.ics");
    command(&dir)
        .args([
            "meeting",
            "prepare",
            "--title",
            "專案同步",
            "--start",
            "2026-10-07T14:00:00+08:00",
            "--end",
            "2026-10-07T15:00:00+08:00",
            "--attendee",
            "ming@example.test",
            "--output",
        ])
        .arg(&invite)
        .assert()
        .success();
    let output = command(&dir)
        .args(["meeting", "invite", "--file"])
        .arg(&invite)
        .arg("--dry-run")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let invitation: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(invitation["sent"], false);
    assert_eq!(invitation["attendees"][0], "ming@example.test");
    command(&dir)
        .args(["meeting", "invite", "--file"])
        .arg(&invite)
        .arg("--send")
        .assert()
        .failure()
        .stdout(contains("展示"));
}
