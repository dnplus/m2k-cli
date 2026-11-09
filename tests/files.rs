use serde_json::Value;
use smb_server::{Access, LocalFsBackend, Share, SmbServer};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const USER: &str = "alice";
const PASSWORD: &str = "fixture-smb-password";

struct Setup {
    root: tempfile::TempDir,
    data: PathBuf,
}

/// Serves `share` as `Shared` to `alice`. Two failed logons lock the
/// name out, so a client that retries a rejected password cannot log in
/// afterwards.
fn serve(share: &Path) -> SocketAddr {
    let share = share.to_owned();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let server = SmbServer::builder()
                    .listen("127.0.0.1:0".parse().unwrap())
                    .user(USER, PASSWORD)
                    .logon_throttle(2, Duration::from_secs(60))
                    .share(
                        Share::new("Shared", LocalFsBackend::new(&share).unwrap().read_only())
                            .user(USER, Access::Read),
                    )
                    .build()
                    .unwrap();
                sender.send(server.bind().await.unwrap()).unwrap();
                server.serve().await.unwrap();
            });
    });
    receiver.recv().unwrap()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// `Team/Plans/Deep/plan-deep.txt` sits at depth 4 below `Shared`.
fn setup(file_server: bool) -> Setup {
    let root = tempfile::tempdir().unwrap();
    let share = root.path().join("share");
    write(&share.join("readme.txt"), "hello");
    write(&share.join("Team/notes.txt"), "notes");
    write(&share.join("Team/Plan-A.txt"), "a");
    write(&share.join("Team/Plans/plan-b.txt"), "bb");
    write(&share.join("Team/Plans/Deep/plan-deep.txt"), "deep");
    let addr = serve(&share);
    let mut profile = serde_json::json!({"name": "files-test"});
    if file_server {
        profile["file_server"] = serde_json::json!({
            "server": addr.to_string(),
            "shares": ["Shared"],
            "domain": null,
        });
    }
    let profile_path = root.path().join("files-test.json");
    std::fs::write(&profile_path, profile.to_string()).unwrap();
    let setup = Setup {
        data: root.path().join("m2k"),
        root,
    };
    let (success, json) = run(
        &setup,
        PASSWORD,
        &[
            "init",
            "--username",
            "alice@example.test",
            "--server",
            "mail.example.test",
            "--site-profile",
            profile_path.to_str().unwrap(),
        ],
    );
    assert!(success, "init failed: {json}");
    setup
}

fn run(setup: &Setup, password: &str, args: &[&str]) -> (bool, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--json")
        .arg("--data-dir")
        .arg(&setup.data)
        .env("M2K_PASSWORD", password)
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db")
        .env_remove("M2K_SITE_PROFILE_DIR")
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "m2k {args:?} printed non-JSON\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        !stdout.contains(PASSWORD),
        "password leaked by m2k {args:?}"
    );
    (output.status.success(), json)
}

fn names(entries: &Value) -> Vec<&str> {
    entries
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect()
}

#[test]
fn ls_lists_a_nested_directory_with_reusable_paths() {
    let setup = setup(true);
    let (success, json) = run(&setup, PASSWORD, &["files", "ls", "Shared/Team"]);
    assert!(success, "{json}");
    assert_eq!(names(&json), ["Plan-A.txt", "Plans", "notes.txt"]);
    let plans = &json[1];
    assert_eq!(plans["kind"], "dir");
    let notes = &json[2];
    assert_eq!(notes["kind"], "file");
    assert_eq!(notes["size"], 5);
    assert!(notes["modified"].as_str().unwrap().ends_with('Z'));

    // A listed path is accepted back as input, so agents can walk the tree.
    let (success, nested) = run(
        &setup,
        PASSWORD,
        &["files", "ls", plans["path"].as_str().unwrap()],
    );
    assert!(success, "{nested}");
    assert_eq!(names(&nested), ["Deep", "plan-b.txt"]);
}

#[test]
fn find_respects_depth_and_limit_and_reports_truncation() {
    use std::os::unix::fs::PermissionsExt;
    let setup = setup(true);
    let locked = setup.root.path().join("share/Team/Locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let find = |extra: &[&str]| {
        let mut args = vec!["files", "find", "Shared", "--name", "PLAN"];
        args.extend(extra);
        let (success, json) = run(&setup, PASSWORD, &args);
        assert!(success, "{json}");
        json
    };
    let all = find(&["--depth", "4"]);
    assert_eq!(
        names(&all["matches"]),
        ["Plan-A.txt", "Plans", "plan-b.txt", "plan-deep.txt"]
    );
    assert_eq!(all["truncated"], false);
    // An unreadable folder is counted and skipped, not fatal.
    assert_eq!(all["skipped_dirs"], 1, "{all}");

    // Default depth 3 stops above Team/Plans/Deep.
    let shallow = find(&[]);
    assert_eq!(
        names(&shallow["matches"]),
        ["Plan-A.txt", "Plans", "plan-b.txt"]
    );
    assert_eq!(shallow["truncated"], false);

    let limited = find(&["--depth", "4", "--limit", "2"]);
    assert_eq!(names(&limited["matches"]), ["Plan-A.txt", "Plans"]);
    assert_eq!(limited["truncated"], true);
    assert_eq!(limited["truncated_reason"], "limit");

    // Exactly `limit` matches is a complete answer, not a truncated one.
    let exact = find(&["--depth", "4", "--limit", "4"]);
    assert_eq!(exact["truncated"], false);
}

#[test]
fn get_downloads_once_and_never_overwrites() {
    let setup = setup(true);
    let target = setup.root.path().join("out.txt");
    let target_arg = target.to_str().unwrap();
    let args = [
        "files",
        "get",
        r"Shared\Team\Plans\plan-b.txt",
        "--output",
        target_arg,
    ];
    let (success, json) = run(&setup, PASSWORD, &args);
    assert!(success, "{json}");
    assert_eq!(json["bytes"], 2);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "bb");

    std::fs::write(&target, "mine").unwrap();
    let (success, json) = run(&setup, PASSWORD, &args);
    assert!(!success);
    assert_eq!(json["state"], "error");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine");
    let leftovers: Vec<_> = std::fs::read_dir(setup.root.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".m2k-download")
        })
        .collect();
    assert!(leftovers.is_empty(), "temp files left behind");
}

#[test]
fn rejected_password_is_auth_failed_after_a_single_logon() {
    let setup = setup(true);
    let (success, json) = run(&setup, "stale-password", &["files", "ls", "Shared"]);
    assert!(!success);
    assert_eq!(json["error"]["code"], "auth_failed", "{json}");
    assert_eq!(json["error"]["retryable"], false);

    // The server locks the name after two failures; a retry would have done that.
    let (success, json) = run(&setup, PASSWORD, &["files", "ls", "Shared"]);
    assert!(success, "second logon was locked out: {json}");
}

#[test]
fn missing_path_and_share_are_not_found() {
    let setup = setup(true);
    for path in ["Shared/Team/missing", "Missing/Team"] {
        let (success, json) = run(&setup, PASSWORD, &["files", "ls", path]);
        assert!(!success);
        assert_eq!(json["error"]["code"], "not_found", "{path}: {json}");
    }
}

#[test]
fn shares_reads_the_profile_and_explains_a_missing_file_server() {
    let setup = setup(true);
    let (success, json) = run(&setup, PASSWORD, &["files", "shares"]);
    assert!(success, "{json}");
    assert_eq!(json["shares"], serde_json::json!(["Shared"]));

    let setup = self::setup(false);
    for args in [&["files", "shares"][..], &["files", "ls", "Shared/Team"]] {
        let (success, json) = run(&setup, PASSWORD, args);
        assert!(!success);
        let message = json["error"]["message"].as_str().unwrap();
        assert!(message.contains("file_server"), "{args:?}: {message}");
    }
}

#[test]
fn demo_data_dir_refuses_file_server_access() {
    let root = tempfile::tempdir().unwrap();
    let setup = Setup {
        data: root.path().join("demo"),
        root,
    };
    let (success, json) = run(&setup, PASSWORD, &["demo", "--no-tui"]);
    assert!(success, "{json}");
    let (success, json) = run(
        &setup,
        PASSWORD,
        &["files", "ls", "smb://127.0.0.1:9/Shared"],
    );
    assert!(!success);
    assert_eq!(
        json["error"]["message"],
        m2k::mail_action::DEMO_OFFLINE,
        "{json}"
    );
}
