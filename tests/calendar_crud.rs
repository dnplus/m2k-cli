//! `calendar create|update|delete` 的欄位寫法，用實際 binary 對 loopback fixture 驗證：
//! 伺服器收到的 ICS、UID 與其他屬性保留、ETag 衝突拒絕，以及不能改的事件不會發出 PUT。

#[path = "../examples/m2k_fixture/fixture.rs"]
mod fixture;

use serde_json::Value;
use std::{path::Path, process::Command};

const PASSWORD: &str = "fixture-test-password";

fn m2k(data_dir: &Path, args: &[&str]) -> (bool, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--json")
        .arg("--data-dir")
        .arg(data_dir)
        .env("M2K_PASSWORD", PASSWORD)
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db")
        .env_remove("M2K_TEST_ROOT_CERT")
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

fn ok(data_dir: &Path, args: &[&str]) -> Value {
    let (success, json) = m2k(data_dir, args);
    assert!(success, "m2k {args:?} failed: {json}");
    json
}

fn failed(data_dir: &Path, args: &[&str]) -> String {
    let (success, json) = m2k(data_dir, args);
    assert!(!success, "m2k {args:?} should fail: {json}");
    json.to_string()
}

fn events(info: &fixture::Info, kind: &str) -> Vec<Value> {
    std::fs::read_to_string(&info.events)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event JSON"))
        .filter(|event| event["kind"] == kind)
        .collect()
}

fn body(info: &fixture::Info, event: &Value) -> String {
    std::fs::read_to_string(info.data_dir.join(event["file"].as_str().expect("file"))).unwrap()
}

/// 只設定行事曆需要的帳號並同步；IMAP／SMTP 不在這裡驗證。
fn client(root: &Path, info: &fixture::Info, name: &str) -> std::path::PathBuf {
    let data = root.join(name);
    ok(
        &data,
        &["init", "--username", &info.user, "--server", &info.server],
    );
    ok(&data, &["sync", "--kind", "calendar"]);
    data
}

#[test]
fn field_based_crud_writes_correct_ics_and_honours_etag() {
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
    let a = client(root.path(), &info, "a");

    // 必填欄位不足時在連線前就失敗，不會 PUT。
    failed(
        &a,
        &[
            "calendar",
            "create",
            "--title",
            "缺結束",
            "--start",
            "明天 14:00",
        ],
    );
    failed(
        &a,
        &[
            "calendar",
            "create",
            "--start",
            "明天 14:00",
            "--end",
            "15:00",
        ],
    );
    assert!(events(&info, "dav.put").is_empty());

    let created = ok(
        &a,
        &[
            "calendar",
            "create",
            "--title",
            "欄位建立",
            "--start",
            "2026-11-03 14:00",
            "--end",
            "15:30",
            "--location",
            "北區, 3F",
            "--description",
            "第一行\n第二行",
        ],
    );
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["title"], "欄位建立");
    let puts = events(&info, "dav.put");
    assert_eq!(puts.len(), 1);
    assert_eq!(puts[0]["created"], true);
    let uid = puts[0]["uid"].as_str().expect("uid").to_owned();
    let raw = body(&info, &puts[0]);
    for expected in [
        "DTSTART:20261103T060000Z\r\n",
        "DTEND:20261103T073000Z\r\n",
        "LOCATION:北區\\, 3F\r\n",
        "DESCRIPTION:第一行\\n第二行\r\n",
    ] {
        assert!(raw.contains(expected), "{expected:?} missing in\n{raw}");
    }
    assert!(!raw.contains("METHOD") && !raw.contains("ORGANIZER"));

    // 只改標題與開始：UID、地點、說明保留，長度沿用 90 分鐘。
    let updated = ok(
        &a,
        &[
            "calendar",
            "update",
            &id,
            "--title",
            "欄位更新",
            "--start",
            "2026-11-04 09:00",
        ],
    );
    assert_eq!(updated["title"], "欄位更新");
    let puts = events(&info, "dav.put");
    assert_eq!(puts.len(), 2);
    assert_eq!(puts[1]["created"], false);
    assert_eq!(puts[1]["uid"], uid.as_str());
    let raw = body(&info, &puts[1]);
    for expected in [
        "SUMMARY:欄位更新\r\n",
        "DTSTART:20261104T010000Z\r\n",
        "DTEND:20261104T023000Z\r\n",
        "LOCATION:北區\\, 3F\r\n",
        "DESCRIPTION:第一行\\n第二行\r\n",
    ] {
        assert!(raw.contains(expected), "{expected:?} missing in\n{raw}");
    }

    // 全天：--end 是最後一天（含），ICS DTEND 為隔天。
    ok(
        &a,
        &[
            "calendar",
            "update",
            &id,
            "--all-day",
            "--end",
            "2026-11-05",
        ],
    );
    let raw = body(&info, events(&info, "dav.put").last().unwrap());
    assert!(
        raw.contains("DTSTART;VALUE=DATE:20261104\r\nDTEND;VALUE=DATE:20261106\r\n"),
        "{raw}"
    );

    // 另一個客戶端先更新：a 手上的 ETag 過期，更新與刪除都被伺服器拒絕。
    let b = client(root.path(), &info, "b");
    ok(&b, &["calendar", "update", &id, "--location", "B 端"]);
    let before = events(&info, "dav.put").len();
    let conflict = failed(&a, &["calendar", "update", &id, "--title", "會蓋掉 B"]);
    assert!(conflict.contains("412"), "{conflict}");
    let conflict = failed(&a, &["calendar", "delete", &id]);
    assert!(conflict.contains("412"), "{conflict}");
    assert_eq!(
        events(&info, "dav.put").len(),
        before,
        "no write after a conflict"
    );
    assert!(events(&info, "dav.delete").is_empty());

    // 同步取得新 ETag 後刪除成功，本機快取也移除。
    ok(&a, &["sync", "--kind", "calendar"]);
    assert_eq!(ok(&a, &["calendar", "delete", &id])["deleted"], id.as_str());
    let deletes = events(&info, "dav.delete");
    assert_eq!(deletes.len(), 1);
    assert!(deletes[0]["path"].as_str().unwrap().ends_with(".ics"));
    failed(&a, &["calendar", "show", &id]);
}

#[test]
fn recurring_and_invited_events_are_refused_without_a_put() {
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
    let a = client(root.path(), &info, "a");
    for (name, extra, reason) in [
        ("recurring", "RRULE:FREQ=WEEKLY;COUNT=3", "重複事件"),
        (
            "invited",
            "ORGANIZER;CN=Carol:mailto:carol@example.test",
            "carol@example.test 邀請",
        ),
        (
            "booked",
            "ORGANIZER:mailto:fixture@example.test\r\nATTENDEE;CN=Room:mailto:room-a@example.test",
            "meeting prepare／book／invite",
        ),
    ] {
        let file = root.path().join(format!("{name}.ics"));
        std::fs::write(
            &file,
            format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//EN\r\nBEGIN:VEVENT\r\nUID:{name}@example.test\r\nDTSTAMP:20261001T000000Z\r\nDTSTART:20261110T060000Z\r\nDTEND:20261110T070000Z\r\nSUMMARY:{name}\r\n{extra}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"),
        )
        .unwrap();
        let created = ok(
            &a,
            &["calendar", "create", "--file", file.to_str().unwrap()],
        );
        let id = created["id"].as_str().unwrap();
        let before = events(&info, "dav.put").len();
        let error = failed(&a, &["calendar", "update", id, "--title", "改不了"]);
        assert!(error.contains(reason), "{error}");
        assert_eq!(events(&info, "dav.put").len(), before);
    }
    // --file 與欄位不能混用；update 至少要一個欄位，兩者都是參數錯誤（exit 2）。
    for args in [
        &["calendar", "create", "--file", "x.ics", "--title", "混用"][..],
        &["calendar", "update", "some-id"][..],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
            .arg("--json")
            .arg("--data-dir")
            .arg(&a)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let json: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(json["error"]["code"], "invalid_arguments");
    }

    // 欄位建立不能指定會議室行事曆；--file 維持原本行為不檢查。
    let before = events(&info, "dav.put").len();
    let error = failed(
        &a,
        &[
            "calendar",
            "create",
            "--title",
            "寫到會議室",
            "--start",
            "2026-11-11 10:00",
            "--end",
            "11:00",
            "--collection",
            &info.room_calendar_url,
        ],
    );
    assert!(error.contains("會議室"), "{error}");
    assert_eq!(events(&info, "dav.put").len(), before);
}
