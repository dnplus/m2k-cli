//! `meeting cancel` 與 `calendar delete <href>` 對 loopback fake Mail2000 的端對端測試。
//!
//! 背景（issue #9）：`meeting book` 在自己與會議室行事曆各寫一份同 UID 副本，
//! 舊的 `calendar delete` 只刪得到本機有的那份，會議室因此沒有釋放。

#[path = "../examples/m2k_fixture/fixture.rs"]
mod fixture;

use serde_json::Value;
use std::{path::Path, process::Command};

const PASSWORD: &str = "fixture-test-password";

fn m2k(data_dir: &Path, info: &fixture::Info, args: &[&str]) -> (bool, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--json")
        .arg("--data-dir")
        .arg(data_dir)
        .env("M2K_PASSWORD", PASSWORD)
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db")
        .env("M2K_TEST_ROOT_CERT", &info.ca_cert)
        .args(args)
        .output()
        .expect("run m2k");
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
    let (success, json) = m2k(data_dir, info, args);
    assert!(success, "m2k {args:?} failed: {json}");
    json
}

fn failed(data_dir: &Path, info: &fixture::Info, args: &[&str]) -> Value {
    let (success, json) = m2k(data_dir, info, args);
    assert!(!success, "m2k {args:?} should fail: {json}");
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

fn start() -> (tempfile::TempDir, fixture::Info) {
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
    (root, info)
}

fn client(root: &Path, info: &fixture::Info, name: &str) -> std::path::PathBuf {
    let data = root.join(name);
    ok(
        &data,
        info,
        &["init", "--username", &info.user, "--server", &info.server],
    );
    let smtp_port = info.smtp_port.to_string();
    ok(&data, info, &["config", "set", "--smtp-port", &smtp_port]);
    ok(&data, info, &["sync", "--kind", "calendar"]);
    data
}

fn book(data: &Path, info: &fixture::Info, file: &Path) -> String {
    let file = file.to_string_lossy().into_owned();
    let booked = ok(
        data,
        info,
        &[
            "meeting",
            "book",
            "--file",
            &file,
            "--url",
            &info.room_calendar_url,
        ],
    );
    assert_eq!(booked["state"], "complete", "{booked}");
    booked["uid"].as_str().expect("uid").to_owned()
}

fn copy_paths(uid: &str) -> Vec<String> {
    vec![
        format!("/cgi-bin/cal/caldav/calendars/fixture@example.test/default/{uid}.ics"),
        format!("/cgi-bin/cal/caldav/calendars/room-a@example.test/default/{uid}.ics"),
    ]
}

fn deleted_paths(info: &fixture::Info) -> Vec<String> {
    let mut paths = events(info, "dav.delete")
        .iter()
        .map(|event| event["path"].as_str().expect("path").to_owned())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

#[test]
fn cancel_finds_and_deletes_every_copy_live_without_sending_mail() {
    let (root, info) = start();
    let data = client(root.path(), &info, "a");
    let uid = book(&data, &info, &info.booking_ics);
    let room = info.room_calendar_url.as_str();

    // dry-run 只列出 live 找到的副本：自己 + 會議室，什麼都不刪、不寄。
    let dry = ok(
        &data,
        &info,
        &["meeting", "cancel", &uid, "--url", room, "--dry-run"],
    );
    assert_eq!(dry["state"], "dry-run", "{dry}");
    let mut hrefs = dry["copies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|copy| {
            assert_eq!(copy["result"], "would_delete");
            let href = copy["href"].as_str().unwrap();
            href[href.find("/cgi-bin").unwrap()..].to_owned()
        })
        .collect::<Vec<_>>();
    hrefs.sort();
    assert_eq!(hrefs, copy_paths(&uid));
    assert!(
        events(&info, "dav.delete").is_empty(),
        "dry-run must not DELETE"
    );

    let cancelled = ok(&data, &info, &["meeting", "cancel", &uid, "--url", room]);
    assert_eq!(cancelled["state"], "complete", "{cancelled}");
    assert_eq!(cancelled["notified"], false);
    assert!(cancelled["copies"]
        .as_array()
        .unwrap()
        .iter()
        .all(|copy| copy["result"] == "deleted"));
    assert_eq!(deleted_paths(&info), copy_paths(&uid));
    for delete in events(&info, "dav.delete") {
        assert!(delete["etag"].as_str().is_some_and(|etag| !etag.is_empty()));
    }
    // 預設絕不寄信（m2k 不寄信原則）。
    assert!(
        events(&info, "smtp.message").is_empty(),
        "cancel must not send mail by default"
    );

    // 再取消一次：兩邊都已不在，非零結束並回報 not_found，不能誤報成功。
    let again = failed(&data, &info, &["meeting", "cancel", &uid, "--url", room]);
    assert_eq!(again["error"]["code"], "not_found", "{again}");
    assert_eq!(again["copies"].as_array().unwrap().len(), 0);
}

#[test]
fn cancel_finds_the_room_copy_even_when_the_room_collection_is_not_cached() {
    let (root, info) = start();
    let booker = client(root.path(), &info, "booker");
    let uid = book(&booker, &info, &info.booking_ics);

    // 另一個資料目錄只同步過自己的行事曆，本機完全沒有會議室 collection。
    let fresh = client(root.path(), &info, "fresh");
    let collections = ok(&fresh, &info, &["calendar", "collections"]);
    assert!(
        !collections.to_string().contains("room-a@example.test"),
        "precondition: room collection is not cached: {collections}"
    );
    let cancelled = ok(
        &fresh,
        &info,
        &["meeting", "cancel", &uid, "--url", &info.room_calendar_url],
    );
    assert_eq!(cancelled["state"], "complete", "{cancelled}");
    assert_eq!(deleted_paths(&info), copy_paths(&uid));
}

#[test]
fn calendar_delete_falls_back_to_live_delete_for_an_uncached_room_href() {
    let (root, info) = start();
    let booker = client(root.path(), &info, "booker");
    let uid = book(&booker, &info, &info.booking_ics);
    let fresh = client(root.path(), &info, "fresh");
    let room_href = format!("{}{uid}.ics", info.room_calendar_url);

    // 以前這裡會回 not_found，會議室副本就一直留著。
    let deleted = ok(&fresh, &info, &["calendar", "delete", &room_href]);
    assert_eq!(deleted["live"], true, "{deleted}");
    let deletes = events(&info, "dav.delete");
    assert_eq!(deletes.len(), 1);
    assert_eq!(deletes[0]["path"], copy_paths(&uid)[1]);
    assert!(deletes[0]["etag"]
        .as_str()
        .is_some_and(|etag| !etag.is_empty()));

    // 已不存在的 href 仍回 not_found，不會假裝成功。
    let missing = failed(&fresh, &info, &["calendar", "delete", &room_href]);
    assert_eq!(missing["error"]["code"], "not_found", "{missing}");
    // 本機 id（非 URL）走原本的快取路徑，錯誤維持 not_found。
    let unknown = failed(&fresh, &info, &["calendar", "delete", "no-such-local-id"]);
    assert_eq!(unknown["error"]["code"], "not_found", "{unknown}");
}

#[test]
fn cancel_refuses_meetings_organized_by_someone_else() {
    let (root, info) = start();
    let data = client(root.path(), &info, "a");
    let foreign = std::fs::read_to_string(&info.booking_ics).unwrap().replace(
        "ORGANIZER:mailto:fixture@example.test",
        "ORGANIZER:mailto:carol@example.test",
    );
    let file = root.path().join("foreign.ics");
    std::fs::write(&file, foreign).unwrap();
    let uid = book(&data, &info, &file);

    // 不能替別人釋放會議室；整筆拒絕，一份都不刪。
    let refused = failed(
        &data,
        &info,
        &["meeting", "cancel", &uid, "--url", &info.room_calendar_url],
    );
    assert!(
        refused.to_string().contains("carol@example.test"),
        "{refused}"
    );
    assert!(events(&info, "dav.delete").is_empty());
}

#[test]
fn notify_attendees_is_opt_in_and_sends_method_cancel_to_people_only() {
    let (root, info) = start();
    let data = client(root.path(), &info, "a");
    let with_bob = std::fs::read_to_string(&info.booking_ics).unwrap().replace(
        "END:VEVENT",
        "ATTENDEE;CN=Bob:mailto:bob@example.test\r\nEND:VEVENT",
    );
    let file = root.path().join("with-bob.ics");
    std::fs::write(&file, with_bob).unwrap();
    let uid = book(&data, &info, &file);

    let dry = ok(
        &data,
        &info,
        &[
            "meeting",
            "cancel",
            &uid,
            "--url",
            &info.room_calendar_url,
            "--notify-attendees",
            "--dry-run",
        ],
    );
    assert_eq!(
        dry["notify_recipients"],
        serde_json::json!(["bob@example.test"])
    );
    assert!(
        events(&info, "smtp.message").is_empty(),
        "dry-run must not send"
    );

    let cancelled = ok(
        &data,
        &info,
        &[
            "meeting",
            "cancel",
            &uid,
            "--url",
            &info.room_calendar_url,
            "--notify-attendees",
        ],
    );
    assert_eq!(cancelled["state"], "complete", "{cancelled}");
    assert_eq!(cancelled["notified"], true);
    let sent = events(&info, "smtp.message");
    assert_eq!(sent.len(), 1);
    // 只寄給與會者，不寄給會議室（CUTYPE=ROOM）或自己。
    assert_eq!(sent[0]["to"], serde_json::json!(["bob@example.test"]));
    let raw = std::fs::read(info.data_dir.join(sent[0]["file"].as_str().unwrap())).unwrap();
    let mail = mailparse::parse_mail(&raw).unwrap();
    let calendar = mail
        .subparts
        .iter()
        .find(|part| part.ctype.mimetype == "text/calendar")
        .expect("text/calendar part");
    assert_eq!(
        calendar.ctype.params.get("method").map(String::as_str),
        Some("CANCEL")
    );
    let ics = calendar.get_body().unwrap().replace("\r\n ", "");
    for line in [
        "METHOD:CANCEL",
        &format!("UID:{uid}"),
        "STATUS:CANCELLED",
        "SEQUENCE:1",
    ] {
        assert!(ics.lines().any(|got| got == line), "missing {line}:\n{ics}");
    }
}

#[test]
fn cancel_deletes_nothing_when_any_calendar_cannot_be_checked() {
    let (root, info) = start();
    let data = client(root.path(), &info, "a");
    let uid = book(&data, &info, &info.booking_ics);

    // 不指定會議室時掃所有會議室；fixture 沒有會議室清單 collection，discovery 失敗。
    // 只刪掉自己那份會讓會議室副本失聯，所以整筆不刪、非零結束。
    let incomplete = failed(&data, &info, &["meeting", "cancel", &uid]);
    assert_eq!(incomplete["state"], "incomplete", "{incomplete}");
    assert_eq!(incomplete["error"]["code"], "cancel_incomplete");
    assert!(!incomplete["lookup_errors"].as_array().unwrap().is_empty());
    assert!(incomplete["copies"]
        .as_array()
        .unwrap()
        .iter()
        .all(|copy| copy["result"] == "skipped"));
    assert!(events(&info, "dav.delete").is_empty());
}
