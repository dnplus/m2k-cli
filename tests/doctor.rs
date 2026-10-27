use m2k::{config, demo, store::Store, sync};
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{Command, Output},
    sync::{Arc, Mutex},
    thread,
};

// A keychain file that does not exist: m2k must never read or write the
// login keychain from tests.
const NO_KEYCHAIN: &str = "/nonexistent/m2k-test.keychain-db";

const SECRET: &str = "doctor-secret-pw-9731";

fn run(dir: &Path, password: Option<&str>, json: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_m2k"));
    command.arg("--data-dir").arg(dir);
    if json {
        command.arg("--json");
    }
    command
        .arg("doctor")
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN);
    if let Some(password) = password {
        command.env("M2K_PASSWORD", password);
    }
    command.output().unwrap()
}

fn status_of(report: &Value, name: &str) -> String {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == name)
        .unwrap_or_else(|| panic!("missing check {name}: {report}"))["status"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn listing(dir: &Path) -> Vec<(String, u64)> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.metadata().unwrap().len(),
            )
        })
        .collect();
    entries.sort();
    entries
}

fn closed_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Loopback DAV fixture: answers every request with one calendar and one
/// address book, and records request methods so tests can prove it stayed read-only.
fn dav_fixture() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let methods = Arc::new(Mutex::new(Vec::new()));
    let seen = methods.clone();
    let body = "<?xml version=\"1.0\"?><D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\"><D:response><D:href>/cal/</D:href><D:propstat><D:prop><D:displayname>個人</D:displayname><D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/card/</D:href><D:propstat><D:prop><D:displayname>聯絡人</D:displayname><D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut bytes = Vec::new();
            let mut one = [0_u8; 1];
            while !bytes.ends_with(b"\r\n\r\n") {
                if stream.read_exact(&mut one).is_err() {
                    break;
                }
                bytes.push(one[0]);
            }
            let head = String::from_utf8_lossy(&bytes).into_owned();
            seen.lock()
                .unwrap()
                .push(head.split_whitespace().next().unwrap_or("").to_owned());
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap_or(0))
                })
                .unwrap_or(0);
            let mut request_body = vec![0_u8; length];
            let _ = stream.read_exact(&mut request_body);
            let _ = write!(
                stream,
                "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, methods)
}

fn init_account(dir: &Path, dav_port: u16) -> config::Config {
    let mut cfg = config::Config::new(
        "doctor@example.test".into(),
        &format!("http://127.0.0.1:{dav_port}"),
    )
    .unwrap();
    cfg.imap_port = closed_port();
    cfg.smtp_port = closed_port();
    cfg.caldav_url = format!("http://127.0.0.1:{dav_port}/cal/");
    cfg.carddav_url = format!("http://127.0.0.1:{dav_port}/card/");
    config::save(dir, &cfg).unwrap();
    Store::open(dir).unwrap().ensure_account(&cfg).unwrap();
    cfg
}

#[test]
fn demo_directory_skips_every_online_check_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    demo::seed(dir.path()).unwrap();
    let before = listing(dir.path());
    let output = run(dir.path(), Some(SECRET), true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["state"], "ok");
    assert_eq!(report["demo"], true);
    for name in ["credentials", "imap", "smtp", "caldav", "carddav"] {
        assert_eq!(status_of(&report, name), "skip", "{name}");
    }
    for name in ["build", "config", "database", "sync_lock"] {
        assert_eq!(status_of(&report, name), "pass", "{name}");
    }
    assert!(report["checks"].as_array().unwrap().iter().any(
        |check| check["status"] == "skip" && check["detail"].as_str().unwrap().contains("展示")
    ));
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    assert!(report["build_commit"].as_str().unwrap().len() >= 7);
    assert_eq!(listing(dir.path()), before, "doctor must not write");
    let text = run(dir.path(), None, false);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("展示資料目錄"));
}

#[cfg(unix)]
#[test]
fn loose_permissions_fail_with_a_fix_and_json_keeps_the_error_contract() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    demo::seed(dir.path()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(
        dir.path().join("config.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let output = run(dir.path(), None, true);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["state"], "error");
    assert_eq!(value["error"]["code"], "doctor_failed");
    assert_eq!(value["error"]["retryable"], false);
    let report = &value;
    assert_eq!(status_of(report, "data_dir"), "fail");
    assert_eq!(status_of(report, "config"), "fail");
    let detail = report["checks"][1]["detail"].as_str().unwrap();
    assert!(
        detail.contains("0755") && detail.contains("0700"),
        "{detail}"
    );
    let text = run(dir.path(), None, false);
    assert_eq!(text.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&text.stderr).contains("[FAIL]"));
}

#[test]
fn missing_credentials_fail_and_skip_the_logins() {
    let dir = tempfile::tempdir().unwrap();
    let (port, methods) = dav_fixture();
    init_account(dir.path(), port);
    let output = run(dir.path(), None, true);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let report = &value;
    assert_eq!(status_of(report, "credentials"), "fail");
    for name in ["imap", "smtp", "caldav", "carddav"] {
        assert_eq!(status_of(report, name), "skip", "{name}");
    }
    assert_eq!(status_of(report, "database"), "pass");
    assert!(
        methods.lock().unwrap().is_empty(),
        "no network without credentials"
    );
}

#[test]
fn missing_config_fails_without_creating_anything() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(dir.path(), Some(SECRET), true);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status_of(&value, "config"), "fail");
    assert_eq!(status_of(&value, "database"), "fail");
    assert!(listing(dir.path()).is_empty());
}

#[test]
fn online_checks_report_each_service_read_only_and_never_print_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let (port, methods) = dav_fixture();
    init_account(dir.path(), port);
    let before = listing(dir.path());
    let output = run(dir.path(), Some(SECRET), true);
    assert_eq!(
        output.status.code(),
        Some(1),
        "closed IMAP/SMTP ports must fail"
    );
    let everything = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!everything.contains(SECRET));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let report = &value;
    assert_eq!(status_of(report, "credentials"), "pass");
    assert_eq!(status_of(report, "imap"), "fail");
    assert_eq!(status_of(report, "smtp"), "fail");
    assert_eq!(status_of(report, "caldav"), "pass");
    assert_eq!(status_of(report, "carddav"), "pass");
    let methods = methods.lock().unwrap();
    assert!(!methods.is_empty());
    assert!(
        methods.iter().all(|method| matches!(
            method.as_str(),
            "PROPFIND" | "REPORT" | "OPTIONS" | "GET" | "HEAD"
        )),
        "doctor sent a non-read request: {methods:?}"
    );
    assert_eq!(listing(dir.path()), before, "doctor must not write");
}

#[test]
fn a_held_sync_lock_is_reported_without_blocking_or_stealing_it() {
    let dir = tempfile::tempdir().unwrap();
    demo::seed(dir.path()).unwrap();
    let lock = sync::lock(dir.path()).unwrap();
    let output = run(dir.path(), None, true);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status_of(&value, "sync_lock"), "fail");
    drop(lock);
    assert!(run(dir.path(), None, true).status.success());
}

#[test]
fn an_old_schema_is_reported_not_silently_migrated() {
    let dir = tempfile::tempdir().unwrap();
    demo::seed(dir.path()).unwrap();
    let path = dir.path().join("database.sqlite3");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    drop(conn);
    let output = run(dir.path(), None, true);
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status_of(&value, "database"), "fail");
    let conn = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1, "doctor must not migrate");
}

#[test]
fn success_and_failure_expose_checks_at_the_same_top_level_path() {
    let ok_dir = tempfile::tempdir().unwrap();
    demo::seed(ok_dir.path()).unwrap();
    let ok: Value = serde_json::from_slice(&run(ok_dir.path(), None, true).stdout).unwrap();
    assert_eq!(ok["state"], "ok");

    let bad_dir = tempfile::tempdir().unwrap();
    demo::seed(bad_dir.path()).unwrap();
    std::fs::remove_file(bad_dir.path().join("config.json")).unwrap();
    let bad: Value = serde_json::from_slice(&run(bad_dir.path(), None, true).stdout).unwrap();
    assert_eq!(bad["state"], "error");
    assert_eq!(bad["error"]["code"], "doctor_failed");

    for value in [&ok, &bad] {
        let names: Vec<_> = value["checks"]
            .as_array()
            .expect("checks at top level")
            .iter()
            .map(|check| check["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.first(), Some(&"build"));
        assert!(names.contains(&"sync_lock"));
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        assert!(value["build_commit"].as_str().unwrap().len() >= 7);
        assert!(value["data_dir"].is_string());
        assert!(value["demo"].is_boolean());
    }
    // 舊的 `report` 別名仍在，內容與頂層一致。
    assert_eq!(bad["report"]["checks"], bad["checks"]);
}
