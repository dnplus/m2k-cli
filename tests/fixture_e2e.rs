//! Drives the real m2k binary against the loopback fixture server in
//! examples/m2k_fixture, so the online paths (TLS IMAP/SMTP, DAV sync,
//! booking PUTs) are exercised end to end, not just their parsers.

#[path = "../examples/m2k_fixture/fixture.rs"]
mod fixture;

use serde_json::Value;
use std::{path::Path, process::Command};

const PASSWORD: &str = "fixture-test-password";

fn m2k(data_dir: &Path, info: &fixture::Info, trust_fixture: bool, args: &[&str]) -> (bool, Value) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_m2k"));
    command
        .arg("--json")
        .arg("--data-dir")
        .arg(data_dir)
        .env("M2K_PASSWORD", PASSWORD)
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db")
        .env_remove("M2K_TEST_ROOT_CERT")
        .args(args);
    if trust_fixture {
        command.env("M2K_TEST_ROOT_CERT", &info.ca_cert);
    }
    let output = command.output().expect("run m2k");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| {
        panic!(
            "m2k {args:?} printed non-JSON\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.success(), json)
}

fn ok(data_dir: &Path, info: &fixture::Info, args: &[&str]) -> Value {
    let (success, json) = m2k(data_dir, info, true, args);
    assert!(success, "m2k {args:?} failed: {json}");
    json
}

fn events(info: &fixture::Info, kind: &str) -> Vec<Value> {
    std::fs::read_to_string(&info.events)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event JSON"))
        .filter(|event| event["kind"] == kind)
        .collect()
}

#[test]
fn real_binary_syncs_sends_and_books_against_loopback_fixture() {
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
    let data = root.path().join("m2k");
    let imap_port = info.imap_port.to_string();
    let smtp_port = info.smtp_port.to_string();
    ok(
        &data,
        &info,
        &["init", "--username", &info.user, "--server", &info.server],
    );
    ok(
        &data,
        &info,
        &[
            "config",
            "set",
            "--imap-port",
            &imap_port,
            "--smtp-port",
            &smtp_port,
        ],
    );

    // The test root is an extra anchor, not a verification bypass: without it
    // the self-signed fixture must be rejected.
    let (success, _) = m2k(&data, &info, false, &["auth", "check"]);
    assert!(!success, "untrusted fixture certificate must fail TLS");

    let verified = ok(&data, &info, &["auth", "check"]);
    assert_eq!(
        verified["verified"],
        serde_json::json!(["imap", "smtp", "caldav", "carddav"])
    );

    let synced = ok(&data, &info, &["sync"]);
    let count = |kind: &str| {
        synced
            .as_array()
            .and_then(|results| results.iter().find(|r| r["kind"] == kind))
            .map(|r| r["count"].clone())
    };
    assert_eq!(count("mail"), Some(6.into()), "{synced}");
    assert_eq!(count("calendar"), Some(2.into()), "{synced}");

    let subject = "fixture e2e 寄送";
    let body = info.mail_body.to_string_lossy().into_owned();
    ok(
        &data,
        &info,
        &[
            "mail",
            "send",
            "--to",
            "bob@example.test",
            "--subject",
            subject,
            "--body-file",
            &body,
        ],
    );
    let sent = events(&info, "smtp.message");
    assert_eq!(sent.len(), 1, "exactly one SMTP message: {sent:?}");
    assert_eq!(sent[0]["subject"], subject);
    assert_eq!(sent[0]["to"], serde_json::json!(["bob@example.test"]));

    let found = ok(&data, &info, &["mail", "search", "季度回顧會議"]);
    let invite_id = found
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["id"].as_str())
        .unwrap_or_else(|| panic!("seeded invite not synced: {found}"))
        .to_owned();
    let preview = ok(
        &data,
        &info,
        &[
            "meeting",
            "rsvp",
            &invite_id,
            "--response",
            "accept",
            "--dry-run",
        ],
    );
    assert_eq!(preview["to"], "carol@example.test");
    assert_eq!(
        events(&info, "smtp.message").len(),
        1,
        "dry-run must not send"
    );
    let replied = ok(
        &data,
        &info,
        &[
            "meeting",
            "rsvp",
            &invite_id,
            "--response",
            "decline",
            "--comment",
            "那天出差",
        ],
    );
    assert_eq!(replied["sent"], true, "{replied}");
    let sent = events(&info, "smtp.message");
    assert_eq!(sent.len(), 2, "exactly one RSVP message: {sent:?}");
    assert_eq!(sent[1]["to"], serde_json::json!(["carol@example.test"]));
    let eml = std::fs::read_to_string(info.data_dir.join(sent[1]["file"].as_str().unwrap()))
        .expect("stored RSVP message");
    let reply = mailparse::parse_mail(eml.as_bytes()).expect("RSVP MIME");
    let calendar = reply
        .subparts
        .iter()
        .find(|part| part.ctype.mimetype == "text/calendar")
        .expect("text/calendar part");
    assert_eq!(
        calendar.ctype.params.get("method").map(String::as_str),
        Some("REPLY")
    );
    let ics = calendar.get_body().expect("ICS body").replace("\r\n ", "");
    for line in [
        "METHOD:REPLY",
        "UID:fixture-invite-1@example.test",
        "SEQUENCE:1",
        "ORGANIZER;CN=Carol:mailto:carol@example.test",
        "ATTENDEE;PARTSTAT=DECLINED:mailto:fixture@example.test",
        "COMMENT:那天出差",
    ] {
        assert!(ics.lines().any(|got| got == line), "missing {line}:\n{ics}");
    }
    assert!(!ics.contains("bob@example.test"), "only my ATTENDEE: {ics}");

    let booking = info.booking_ics.to_string_lossy().into_owned();
    let dry = ok(
        &data,
        &info,
        &[
            "meeting",
            "book",
            "--file",
            &booking,
            "--url",
            &info.room_calendar_url,
            "--dry-run",
        ],
    );
    assert_eq!(dry["state"], "dry-run");
    assert!(events(&info, "dav.put").is_empty(), "dry-run must not PUT");

    let booked = ok(
        &data,
        &info,
        &[
            "meeting",
            "book",
            "--file",
            &booking,
            "--url",
            &info.room_calendar_url,
        ],
    );
    assert_eq!(booked["state"], "complete", "{booked}");
    let puts = events(&info, "dav.put");
    let mut paths = puts
        .iter()
        .map(|put| put["path"].as_str().expect("path").to_owned())
        .collect::<Vec<_>>();
    paths.sort();
    let uid = booked["uid"].as_str().expect("uid");
    assert_eq!(
        paths,
        vec![
            format!("/cgi-bin/cal/caldav/calendars/fixture@example.test/default/{uid}.ics"),
            format!("/cgi-bin/cal/caldav/calendars/room-a@example.test/default/{uid}.ics"),
        ]
    );
    for put in &puts {
        let stored = std::fs::read_to_string(info.data_dir.join(put["file"].as_str().unwrap()))
            .expect("stored PUT body");
        assert!(stored.contains(&format!("UID:{uid}")));
        assert!(
            !stored.contains("METHOD:"),
            "calendar PUT must drop iTIP METHOD"
        );
    }
}

#[test]
fn mail_reply_keeps_error_codes_and_check_order_and_replies_in_thread() {
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
    let data = root.path().join("m2k");
    let (imap_port, smtp_port) = (info.imap_port.to_string(), info.smtp_port.to_string());
    ok(
        &data,
        &info,
        &["init", "--username", &info.user, "--server", &info.server],
    );
    ok(
        &data,
        &info,
        &[
            "config",
            "set",
            "--imap-port",
            &imap_port,
            "--smtp-port",
            &smtp_port,
        ],
    );
    ok(&data, &info, &["sync"]);
    let list = ok(&data, &info, &["mail", "list"]);
    let id = list[0]["id"].as_str().expect("mail id").to_owned();
    let body = info.mail_body.to_string_lossy().into_owned();

    // 附件不存在：錯誤型別（not_found）不可被重新包裝成 operation_failed。
    let (success, error) = m2k(
        &data,
        &info,
        true,
        &[
            "mail",
            "reply",
            &id,
            "--body-file",
            &body,
            "--attach",
            "/nonexistent/missing.pdf",
        ],
    );
    assert!(!success);
    assert_eq!(error["error"]["code"], "not_found", "{error}");
    assert!(events(&info, "smtp.message").is_empty());

    // 找不到郵件優先於讀正文（正文檔不存在也一樣回 not_found，且不讀 stdin）。
    let (success, error) = m2k(
        &data,
        &info,
        true,
        &[
            "mail",
            "reply",
            "missing-id",
            "--body-file",
            "/nonexistent/body.txt",
        ],
    );
    assert!(!success);
    assert_eq!(error["error"]["code"], "not_found", "{error}");

    let replied = ok(&data, &info, &["mail", "reply", &id, "--body-file", &body]);
    assert!(replied["message_id"].is_string());
    let sent = events(&info, "smtp.message");
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0]["subject"].as_str().unwrap().starts_with("Re: "));
}

fn index_segments(data_dir: &Path) -> Vec<(i64, Vec<u8>)> {
    let conn = rusqlite::Connection::open(data_dir.join("database.sqlite3")).expect("open db");
    let mut statement = conn
        .prepare("SELECT id, block FROM items_search_data ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn unchanged_mail_resync_leaves_the_search_index_byte_identical() {
    // IMAP SINCE matches whole days, so the server keeps returning mail on the
    // retention boundary day that pruning deletes right after the sync. On a
    // real mailbox that rewrote ~29 index rows on every unchanged sync.
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
    let data = root.path().join("m2k");
    let imap_port = info.imap_port.to_string();
    let smtp_port = info.smtp_port.to_string();
    ok(
        &data,
        &info,
        &["init", "--username", &info.user, "--server", &info.server],
    );
    // The fixture's "午餐訂購" is exactly two days old: inside SINCE's
    // day window, a few seconds past the two-day cutoff.
    ok(
        &data,
        &info,
        &[
            "config",
            "set",
            "--imap-port",
            &imap_port,
            "--smtp-port",
            &smtp_port,
            "--mail-retention-days",
            "2",
        ],
    );
    ok(&data, &info, &["sync", "--kind", "mail"]);
    let titles = |list: Value| -> Vec<String> {
        list.as_array()
            .expect("mail list")
            .iter()
            .map(|item| item["title"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    let first = titles(ok(&data, &info, &["mail", "list"]));
    assert!(!first.iter().any(|title| title == "午餐訂購"), "{first:?}");
    assert!(
        first.iter().any(|title| title == "Q4 報價確認"),
        "{first:?}"
    );
    let segments = index_segments(&data);

    ok(&data, &info, &["sync", "--kind", "mail"]);
    assert_eq!(titles(ok(&data, &info, &["mail", "list"])), first);
    assert_eq!(index_segments(&data), segments);
}
