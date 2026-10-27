use m2k::{
    config::{self, Config},
    meeting::{MeetingRoom, TimeRange},
    meeting_draft,
};
use std::{fs, process::Command};

// A keychain file that does not exist: m2k must never read or write the
// login keychain from tests.
const NO_KEYCHAIN: &str = "/nonexistent/m2k-test.keychain-db";

fn fixture(dir: &std::path::Path, username: &str) -> (MeetingRoom, TimeRange) {
    config::save(
        dir,
        &Config::new(username.into(), "mail.example.test").unwrap(),
    )
    .unwrap();
    (
        MeetingRoom {
            name: "北區 O'Brien 室".into(),
            access_name: "test-room".into(),
            site: "北區".into(),
            capacity: Some(8),
            calendar_url: "https://mail.example.test/room/".into(),
            source: "test".into(),
        },
        TimeRange {
            start: chrono::DateTime::parse_from_rfc3339("2026-10-15T10:00:00+08:00").unwrap(),
            end: chrono::DateTime::parse_from_rfc3339("2026-10-15T11:00:00+08:00").unwrap(),
        },
    )
}

#[test]
fn persistent_draft_reuses_identity_and_runs_quoted_offline_handoff() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("account A'中文");
    let (room, range) = fixture(&dir, "test@example.test");
    let draft = meeting_draft::save_draft(
        &dir,
        &room,
        &range,
        "專案討論",
        &["colleague@example.test".into()],
    )
    .unwrap();
    let first = fs::read(&draft.invitation).unwrap();
    let repeated = meeting_draft::save_draft(
        &dir,
        &room,
        &range,
        "專案討論",
        &["colleague@example.test".into()],
    )
    .unwrap();
    assert_eq!(draft.uid, repeated.uid);
    assert_eq!(first, fs::read(&repeated.invitation).unwrap());
    assert!(draft.actions["check_booking"].contains("--data-dir"));
    assert!(draft.actions["check_booking"]
        .contains("--url 'https://mail.example.test/room/' --dry-run"));
    assert!(draft.actions["repair_room"].ends_with("--target room"));
    assert!(draft.actions["send_invitation"].ends_with("--send"));
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_m2k"));
    let path = std::env::join_paths(
        std::iter::once(executable.parent().unwrap().to_path_buf()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )),
    )
    .unwrap();
    let output = Command::new("sh")
        .arg("-c")
        .arg(&draft.actions["check_invitation"])
        .env("PATH", path)
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: meeting_draft::Draft =
        serde_json::from_slice(&fs::read(&draft.manifest).unwrap()).unwrap();
    assert_eq!(manifest.uid, draft.uid);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&draft.invitation)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn account_identity_changes_uid_and_manual_edits_are_preserved() {
    let temp = tempfile::tempdir().unwrap();
    let first_dir = temp.path().join("first");
    let second_dir = temp.path().join("second");
    let (room, range) = fixture(&first_dir, "first@example.test");
    fixture(&second_dir, "second@example.test");
    let first = meeting_draft::save_draft(&first_dir, &room, &range, "討論", &[]).unwrap();
    let second = meeting_draft::save_draft(&second_dir, &room, &range, "討論", &[]).unwrap();
    assert_ne!(first.uid, second.uid);
    fs::write(&first.invitation, "manual edit").unwrap();
    assert!(meeting_draft::save_draft(&first_dir, &room, &range, "討論", &[]).is_err());
    assert_eq!(fs::read_to_string(first.invitation).unwrap(), "manual edit");
}
