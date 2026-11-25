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
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn sqlite_blocks_stale_drafts_before_credentials_or_network_for_all_states_and_keys() {
    for state in ["sending", "sent", "unknown"] {
        for same_draft in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            m2k::demo::seed(dir.path()).unwrap();
            let body = dir.path().join("body.txt");
            std::fs::write(&body, "test").unwrap();
            let created = cli(
                dir.path(),
                &[
                    "mail",
                    "draft",
                    "create",
                    "--to",
                    "a@example.test",
                    "--subject",
                    "test",
                    "--body-file",
                    body.to_str().unwrap(),
                ],
                true,
            );
            let id = created["draft_id"].as_str().unwrap();
            let shown = cli(dir.path(), &["mail", "draft", "show", id], true);
            let message_id = shown["message_id"].as_str().unwrap();
            let conn = rusqlite::Connection::open(dir.path().join("database.sqlite3")).unwrap();
            conn.execute("INSERT INTO mail_submissions(draft_id,message_id,state,updated_at) VALUES(?1,?2,?3,'test')", rusqlite::params![if same_draft { id } else { "different-draft" }, if same_draft { "<different@test>" } else { message_id }, state]).unwrap();
            let manifest = dir
                .path()
                .join("outgoing-drafts")
                .join(id)
                .join("manifest.json");
            let before = std::fs::read(&manifest).unwrap();
            let result = cli(dir.path(), &["mail", "send", id], false);
            let error = result["error"]["message"].as_str().unwrap();
            assert!(error.contains("SQLite") && error.contains(state), "{error}");
            assert_eq!(std::fs::read(manifest).unwrap(), before);
        }
    }
}
