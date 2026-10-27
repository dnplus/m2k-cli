use m2k::{demo, sync};
use serde_json::Value;
use std::process::Command;

// A keychain file that does not exist: m2k must never read or write the
// login keychain from tests.
const NO_KEYCHAIN: &str = "/nonexistent/m2k-test.keychain-db";

fn failure(dir: &std::path::Path, args: &[&str], status: i32) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--data-dir")
        .arg(dir)
        .args(args)
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(status));
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "error");
    assert!(value["error"]["message"].is_string());
    value
}

#[test]
fn json_errors_cover_parser_missing_resource_and_account_lock() {
    let temp = tempfile::tempdir().unwrap();
    demo::seed(temp.path()).unwrap();
    let parser = failure(temp.path(), &["mail", "--json", "unknown-command"], 2);
    assert_eq!(parser["error"]["code"], "invalid_arguments");
    assert_eq!(parser["error"]["retryable"], false);
    let missing = failure(temp.path(), &["--json", "mail", "show", "missing-id"], 1);
    assert_eq!(missing["error"]["code"], "not_found");
    let lock = sync::lock(temp.path()).unwrap();
    let busy = failure(temp.path(), &["--json", "prune"], 1);
    assert_eq!(busy["error"]["code"], "account_busy");
    assert_eq!(busy["error"]["retryable"], true);
    drop(lock);
    let status = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--data-dir")
        .arg(temp.path())
        .args(["--json", "status"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
    assert!(status["build_commit"]
        .as_str()
        .is_some_and(|value| value.len() >= 12));
    assert!(status["progress"].is_null());
}

#[test]
fn unknown_failures_never_advise_automatic_retry() {
    let error = anyhow::anyhow!("SMTP 結果不確定");
    let output = m2k::error::ErrorOutput::from_error(&error);
    assert_eq!(output.error.code, "operation_failed");
    assert!(!output.error.retryable);
}

#[test]
fn mail_reply_reads_password_before_body_file_on_non_demo_dir() {
    let temp = tempfile::tempdir().unwrap();
    demo::seed(temp.path()).unwrap();
    std::fs::remove_file(temp.path().join(".demo")).unwrap();
    let error = failure(
        temp.path(),
        &[
            "--json",
            "mail",
            "reply",
            "demo-mail-2",
            "--body-file",
            "/nonexistent/body.txt",
        ],
        1,
    );
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        !message.contains("/nonexistent/body.txt"),
        "密碼庫要先於讀正文：{message}"
    );
}

#[test]
fn mail_reply_cli_keeps_error_code_and_text_with_a_short_password() {
    // A one-letter password used to be masked out of CLI errors, turning
    // not_found into operation_failed and "/nonexistent" into "/n[已隱藏]nexistent".
    let temp = tempfile::tempdir().unwrap();
    demo::seed(temp.path()).unwrap();
    std::fs::remove_file(temp.path().join(".demo")).unwrap();
    let body = temp.path().join("body.txt");
    std::fs::write(&body, "ok").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--data-dir")
        .arg(temp.path())
        .args(["--json", "mail", "reply", "demo-mail-2", "--body-file"])
        .arg(&body)
        .args(["--attach", "/nonexistent/missing.pdf"])
        .env("M2K_PASSWORD", "o")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "not_found", "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("/nonexistent/missing.pdf"), "{message}");
}

#[test]
fn calendar_and_contacts_writes_check_lock_and_account_before_reading_the_file() {
    let temp = tempfile::tempdir().unwrap();
    demo::seed(temp.path()).unwrap();
    for args in [
        &["--json", "calendar", "create", "--file", "/nonexistent.ics"][..],
        &[
            "--json",
            "calendar",
            "update",
            "demo-event-1",
            "--file",
            "/nonexistent.ics",
        ][..],
        &["--json", "contacts", "create", "--file", "/nonexistent.vcf"][..],
        &[
            "--json",
            "contacts",
            "update",
            "demo-contact-1",
            "--file",
            "/nonexistent.vcf",
        ][..],
    ] {
        let demo = failure(temp.path(), args, 1);
        assert_eq!(demo["error"]["code"], "operation_failed", "{args:?}");
        assert!(demo["error"]["message"]
            .as_str()
            .unwrap()
            .contains("展示資料"));
        let lock = sync::lock(temp.path()).unwrap();
        let busy = failure(temp.path(), args, 1);
        assert_eq!(busy["error"]["code"], "account_busy", "{args:?}");
        drop(lock);
    }
}
