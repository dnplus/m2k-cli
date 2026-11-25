use serde_json::Value;
use std::{path::Path, process::Command};

fn cli(dir: &Path, args: &[&str], success: bool) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--data-dir")
        .arg(dir)
        .arg("--json")
        .args(args)
        .env_remove("M2K_PASSWORD")
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/tmp/m2k-fix071-no-keyring",
        )
        .output()
        .unwrap();
    assert_eq!(
        output.status.success(),
        success,
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).expect("one complete JSON result")
}

#[test]
fn edit_and_delete_failures_emit_one_json_result_and_nonzero_status() {
    let dir = tempfile::tempdir().unwrap();
    m2k::demo::seed(dir.path()).unwrap();
    let body = dir.path().join("body.md");
    std::fs::write(&body, "# draft body").unwrap();
    let create = || {
        cli(
            dir.path(),
            &[
                "mail",
                "draft",
                "create",
                "--to",
                "to@example.test",
                "--bcc",
                "hidden@example.test",
                "--subject",
                "old",
                "--body-file",
                body.to_str().unwrap(),
            ],
            true,
        )["draft_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let id = create();
    cli(
        dir.path(),
        &["mail", "send", &id, "--at", "2099-01-01 09:00"],
        true,
    );
    let edited = cli(
        dir.path(),
        &[
            "mail",
            "draft",
            "edit",
            &id,
            "--subject",
            "new",
            "--cc",
            "--bcc",
        ],
        false,
    );
    assert_eq!(edited["draft_id"], id);
    assert_eq!(edited["subject"], "new");
    assert_eq!(edited["state"], "scheduled");
    assert_eq!(edited["remote_sync"], "failed");
    assert_eq!(edited["edit_pending"], true);
    assert_eq!(edited["to"], serde_json::json!(["to@example.test"]));
    let saved = cli(dir.path(), &["mail", "draft", "show", &id], true);
    assert_eq!(saved["subject"], "new");
    let other = create();
    let deleted = cli(dir.path(), &["mail", "draft", "delete", &other], false);
    assert_eq!(deleted["deleted"], false);
    assert_eq!(deleted["remote_sync"], "failed");
}
