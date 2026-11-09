#[path = "../examples/m2k_fixture/fixture.rs"]
mod fixture;

use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const PASSWORD: &str = "fixture-test-password";
const WRONG: &str = "stale-ad-password";

struct Setup {
    _root: tempfile::TempDir,
    info: fixture::Info,
    data: PathBuf,
}

fn command(setup: &Setup, password: &str, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_m2k"));
    command
        .arg("--json")
        .arg("--data-dir")
        .arg(&setup.data)
        .env("M2K_PASSWORD", password)
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db")
        .env("M2K_TEST_ROOT_CERT", &setup.info.ca_cert)
        .env_remove("M2K_WEBHOOK_TOKEN")
        .env_remove("M2K_WEBHOOK_SECRET")
        .args(args);
    command
}

fn run(setup: &Setup, password: &str, args: &[&str]) -> (bool, Value) {
    let output = command(setup, password, args).output().expect("run m2k");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "m2k {args:?} printed non-JSON\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.success(), json)
}

fn setup(imap_port: Option<u16>) -> Setup {
    let root = tempfile::tempdir().expect("tempdir");
    let info = fixture::start(fixture::Options {
        data_dir: root.path().join("fixture"),
        user: "fixture@example.test".into(),
        password: PASSWORD.into(),
        imap_port: 0,
        smtp_port: 0,
        http_port: 0,
        sync_collection: false,
    })
    .expect("start fixture");
    let setup = Setup {
        data: root.path().join("m2k"),
        _root: root,
        info,
    };
    let imap = imap_port.unwrap_or(setup.info.imap_port).to_string();
    let smtp = setup.info.smtp_port.to_string();
    for args in [
        vec![
            "init",
            "--username",
            &setup.info.user,
            "--server",
            &setup.info.server,
        ],
        vec!["config", "set", "--imap-port", &imap, "--smtp-port", &smtp],
    ] {
        let (success, json) = run(&setup, PASSWORD, &args);
        assert!(success, "m2k {args:?} failed: {json}");
    }
    setup
}

fn rejected_logins(info: &fixture::Info) -> usize {
    std::fs::read_to_string(&info.events)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("\"imap.login_rejected\""))
        .count()
}

fn assert_auth_failed(json: &Value) {
    assert_eq!(json["error"]["code"], "auth_failed", "{json}");
    assert_eq!(json["error"]["retryable"], false, "{json}");
    let message = json["error"]["message"].as_str().unwrap();
    assert!(message.contains("m2k auth login"), "{json}");
    assert!(!json.to_string().contains(WRONG), "password leaked: {json}");
}

fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_exit(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < limit {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

#[test]
fn rejected_password_reports_auth_failed_for_imap_smtp_and_dav() {
    let setup = setup(None);

    let (success, json) = run(&setup, WRONG, &["sync", "--kind", "mail"]);
    assert!(!success);
    assert_auth_failed(&json);
    assert_eq!(json["results"][0]["code"], "auth_failed", "{json}");
    assert_eq!(
        rejected_logins(&setup.info),
        1,
        "one-shot sync logs in once"
    );

    let (success, json) = run(&setup, WRONG, &["sync", "--kind", "calendar"]);
    assert!(!success);
    assert_auth_failed(&json);
    assert_eq!(json["results"][0]["code"], "auth_failed", "{json}");
    assert!(json["results"][0]["error"]
        .as_str()
        .unwrap()
        .contains("401"));

    let (success, json) = run(&setup, WRONG, &["auth", "check"]);
    assert!(!success);
    assert_auth_failed(&json);
    assert!(json["error"]["message"].as_str().unwrap().contains("IMAP"));

    let body = setup.info.mail_body.to_string_lossy().into_owned();
    let (success, json) = run(
        &setup,
        WRONG,
        &[
            "mail",
            "send",
            "--to",
            "bob@example.test",
            "--subject",
            "x",
            "--body-file",
            &body,
        ],
    );
    assert!(!success);
    assert_auth_failed(&json);
    assert!(json["error"]["message"].as_str().unwrap().contains("SMTP"));
}

#[test]
fn network_failure_keeps_its_code() {
    let setup = setup(Some(closed_port()));
    let (success, json) = run(&setup, PASSWORD, &["sync", "--kind", "mail"]);
    assert!(!success);
    assert_eq!(json["error"]["code"], "partial_sync", "{json}");
    assert_ne!(json["results"][0]["code"], "auth_failed", "{json}");
}

#[test]
fn watch_stops_after_one_rejected_login() {
    let setup = setup(None);
    let mut child = command(&setup, WRONG, &["watch", "--kind", "mail"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let status = wait_exit(&mut child, Duration::from_secs(30));
    let Some(status) = status else {
        child.kill().unwrap();
        panic!("watch kept running after auth_failed");
    };
    assert!(!status.success());
    let output = child.wait_with_output().unwrap();
    let json: Value = serde_json::from_slice(&output.stdout).expect("error JSON");
    assert_auth_failed(&json);
    assert_eq!(rejected_logins(&setup.info), 1);
}

#[test]
fn mail_watch_stops_after_one_rejected_login() {
    let setup = setup(None);
    let mut child = command(
        &setup,
        WRONG,
        &["mail", "watch", "--webhook-url", &setup.info.webhook_url],
    )
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
    let Some(status) = wait_exit(&mut child, Duration::from_secs(30)) else {
        child.kill().unwrap();
        panic!("mail watch kept reconnecting after auth_failed");
    };
    assert!(!status.success());
    let output = child.wait_with_output().unwrap();
    let json: Value = serde_json::from_slice(&output.stdout).expect("error JSON");
    assert_eq!(json["state"], "error", "{json}");
    assert_auth_failed(&json);
    assert_eq!(rejected_logins(&setup.info), 1);
}

#[test]
fn mail_watch_still_reconnects_after_network_failure() {
    let setup = setup(Some(closed_port()));
    let mut child = command(
        &setup,
        PASSWORD,
        &["mail", "watch", "--webhook-url", &setup.info.webhook_url],
    )
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let json: Value = serde_json::from_str(&line).expect("reconnecting JSON");
    assert_eq!(json["state"], "reconnecting", "{json}");
    assert_ne!(json["error"]["code"], "auth_failed", "{json}");
    let exited = wait_exit(&mut child, Duration::from_secs(2));
    child.kill().ok();
    child.wait().ok();
    assert!(
        exited.is_none(),
        "network failure must keep the watcher alive"
    );
}
