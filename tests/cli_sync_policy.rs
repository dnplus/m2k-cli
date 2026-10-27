use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn contacts_cannot_be_scheduled_by_watch_even_without_an_account() {
    let dir = TempDir::new().unwrap();
    Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["watch", "--kind", "contacts"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("聯絡人採手動全量"))
        .stderr(predicate::str::contains("directory sync --scope company"));
    assert!(dir.path().read_dir().unwrap().next().is_none());
}

#[test]
fn sync_and_watch_help_expose_the_manual_contacts_policy() {
    Command::new(env!("CARGO_BIN_EXE_m2k"))
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("手動全公司同步"));
    Command::new(env!("CARGO_BIN_EXE_m2k"))
        .args(["watch", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("聯絡人用手動全量"));
}
