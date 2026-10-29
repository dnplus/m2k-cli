use assert_cmd::Command;
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use m2k::{
    meeting::{query_rooms, RoomsArgs},
    model::{Attachment, Collection, Item, Kind, Snapshot},
    store::Store,
};
use serde_json::Value;
use tempfile::TempDir;
use unicode_width::UnicodeWidthStr;

// A keychain file that does not exist: m2k must never read or write the
// login keychain from tests.
const NO_KEYCHAIN: &str = "/nonexistent/m2k-test.keychain-db";

fn command(dir: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_m2k"));
    command.arg("--data-dir").arg(dir.path());
    command
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN);
    command
}

fn output(dir: &TempDir, args: &[&str]) -> Vec<u8> {
    command(dir)
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone()
}

fn plain(dir: &TempDir, args: &[&str]) -> String {
    String::from_utf8(output(dir, args)).unwrap()
}

fn json(dir: &TempDir, args: &[&str]) -> Value {
    let mut arguments = vec!["--json"];
    arguments.extend_from_slice(args);
    serde_json::from_slice(&output(dir, &arguments)).unwrap()
}

fn item(kind: Kind, id: &str, title: &str, text: &str, date: &str) -> Item {
    Item {
        id: id.into(),
        kind,
        collection: match kind {
            Kind::Mail => "INBOX",
            Kind::Calendar => "calendar",
            Kind::Contacts => "contacts",
        }
        .into(),
        remote_id: format!("remote-{id}"),
        etag: Some("\"fixture-etag\"".into()),
        uid_validity: (kind == Kind::Mail).then_some(7),
        title: title.into(),
        subtitle: "王小明 <sender@example.test>".into(),
        date: Some(date.into()),
        end_date: (kind == Kind::Calendar).then(|| "2026-10-07T15:00:00+08:00".into()),
        text: text.into(),
        raw: format!(
            "From: sender@example.test\r\nSubject: {title}\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{text}"
        ),
        flags: vec!["\\Seen".into(), "\\Flagged".into()],
        emails: vec!["sender@example.test".into()],
        phones: vec!["+886-2-1234-5678".into()],
        attachments: vec![Attachment {
            index: 0,
            filename: "完整附件名稱.txt".into(),
            content_type: "text/plain".into(),
            size: 123,
        }],
    }
}

fn seed(items: &[Item]) -> TempDir {
    let dir = TempDir::new().unwrap();
    m2k::demo::seed_without_rooms(dir.path()).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    for kind in [Kind::Mail, Kind::Calendar, Kind::Contacts] {
        let selected = items
            .iter()
            .filter(|item| item.kind == kind)
            .cloned()
            .collect::<Vec<_>>();
        let collections = selected
            .first()
            .map(|item| {
                vec![Collection {
                    kind,
                    id: item.collection.clone(),
                    name: kind.to_string(),
                }]
            })
            .unwrap_or_default();
        store
            .apply_snapshot(
                &Snapshot {
                    kind,
                    collections,
                    items: selected,
                },
                Utc::now(),
            )
            .unwrap();
    }
    dir
}

fn assert_unchanged(dir: &TempDir, items: &[Item]) {
    let store = Store::open_reader(dir.path()).unwrap();
    for original in items {
        assert_eq!(
            serde_json::to_value(store.get(&original.id).unwrap()).unwrap(),
            serde_json::to_value(original).unwrap(),
            "read command changed stored item {}",
            original.id
        );
    }
}

fn long_items() -> Vec<Item> {
    [Kind::Mail, Kind::Calendar, Kind::Contacts]
        .into_iter()
        .flat_map(|kind| {
            let mut long = item(
                kind,
                &format!("{kind}-{}", "full-id-".repeat(12)),
                &format!("前綴\r\n\t{}", "專案👩‍💻".repeat(20)),
                "BODY_MUST_NOT_APPEAR_IN_LIST",
                "2026-10-07T12:00:00.123456+08:00",
            );
            long.subtitle = format!("來源\r\n\t\u{7}\u{1b}{}", "工程師🙂".repeat(20));
            let short = item(
                kind,
                &format!("{kind}-short"),
                "短主旨",
                "SECOND_BODY_MUST_NOT_APPEAR_IN_LIST",
                "2026-10-06T12:00:00Z",
            );
            [long, short]
        })
        .collect()
}

#[test]
fn list_plain_output_has_one_fixed_width_line_per_item_and_no_box_or_control_codes() {
    let items = long_items();
    let dir = seed(&items);
    for kind in ["mail", "calendar", "contacts"] {
        let printed = plain(&dir, &[kind, "list"]);
        let lines = printed.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3, "{kind} must emit a header and two rows");
        for line in &lines {
            assert_eq!(UnicodeWidthStr::width(*line), 118, "{kind}: {line}");
            assert!(!line.chars().any(char::is_control), "{kind}: {line:?}");
        }
        for header in ["ID", "時間", "主旨", "來源"] {
            assert!(lines[0].contains(header), "{kind} missing {header}");
        }
        assert!(!printed.chars().any(|character| matches!(
            character,
            '│' | '┃'
                | '┌'
                | '┐'
                | '└'
                | '┘'
                | '├'
                | '┤'
                | '┬'
                | '┴'
                | '┼'
                | '═'
                | '─'
        )));
        assert!(
            printed.contains('…'),
            "{kind} must visibly indicate truncation"
        );
        assert!(printed.contains("短主旨"));
        assert!(!printed.contains("BODY_MUST_NOT_APPEAR"));
        assert!(!printed.contains(&format!("{kind}-{}", "full-id-".repeat(12))));
    }
    assert_unchanged(&dir, &items);
}

#[test]
fn empty_plain_list_still_emits_a_single_header() {
    let dir = seed(&[]);
    for kind in ["mail", "calendar", "contacts"] {
        let printed = plain(&dir, &[kind, "list"]);
        let lines = printed.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("ID"));
        assert_eq!(UnicodeWidthStr::width(lines[0]), 118);
    }
}

#[test]
fn json_list_keeps_full_ids_bodies_and_other_fields_for_every_kind() {
    let items = long_items();
    let dir = seed(&items);
    for kind in [Kind::Mail, Kind::Calendar, Kind::Contacts] {
        let kind_name = kind.to_string();
        let mut args = vec![kind_name.as_str(), "list"];
        if kind == Kind::Mail {
            // Mail JSON drops raw unless --full; see the compact-list test.
            args.push("--full");
        }
        let listed = json(&dir, &args);
        let rows = listed.as_array().unwrap();
        assert_eq!(rows.len(), 2);
        for original in items.iter().filter(|item| item.kind == kind) {
            let actual = rows.iter().find(|row| row["id"] == original.id).unwrap();
            assert_eq!(actual, &serde_json::to_value(original).unwrap());
        }
    }
    let compact = json(&dir, &["mail", "list"]);
    for original in items.iter().filter(|item| item.kind == Kind::Mail) {
        let mut expected = original.clone();
        expected.raw.clear();
        let actual = compact
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == original.id)
            .unwrap();
        assert_eq!(actual, &serde_json::to_value(&expected).unwrap());
    }
    assert_unchanged(&dir, &items);
}

#[test]
fn mail_read_full_flags_are_discoverable_and_raw_conflicts_with_full() {
    let dir = seed(&[]);
    for args in [
        ["mail", "show", "--help"],
        ["mail", "thread", "--help"],
        ["mail", "list", "--help"],
        ["mail", "search", "--help"],
    ] {
        let help = plain(&dir, &args);
        assert!(help.contains("--full"));
    }
    command(&dir)
        .args(["mail", "show", "missing-id", "--raw", "--full"])
        .assert()
        .failure()
        .code(2);
}

fn thread_items() -> Vec<Item> {
    vec![
        item(
            Kind::Mail,
            "old-requested",
            "專案上線",
            "REQUESTED_REPLY\n\nOn Tue, 6 Oct 2026, Alice wrote:\n> REQUESTED_QUOTED_SECRET",
            "2026-10-07T09:00:00Z",
        ),
        item(
            Kind::Mail,
            "latest-utc",
            "Re: 專案上線",
            "LATEST_REPLY\n\nOn Tue, 6 Oct 2026, Alice wrote:\n> LATEST_QUOTED_SECRET",
            "2026-10-07T12:00:00Z",
        ),
        item(
            Kind::Mail,
            "older-offset",
            "Re: Fwd: 專案上線",
            "OLDER_OFFSET_REPLY\n\n> OLDER_QUOTED_SECRET",
            "2026-10-07T19:00:00+08:00",
        ),
        item(
            Kind::Mail,
            "unrelated",
            "專案上線會議",
            "UNRELATED_SECRET",
            "2026-10-08T12:00:00Z",
        ),
    ]
}

#[test]
fn show_reads_the_requested_message_and_full_opt_in_restores_quoted_history() {
    let items = thread_items();
    let dir = seed(&items);
    let latest_reply = plain(&dir, &["mail", "show", "old-requested"]);
    assert!(latest_reply.contains("REQUESTED_REPLY"));
    assert!(!latest_reply.contains("REQUESTED_QUOTED_SECRET"));
    assert!(!latest_reply.contains("LATEST_REPLY"));
    assert!(!latest_reply.contains("UNRELATED_SECRET"));
    let full = plain(&dir, &["mail", "show", "old-requested", "--full"]);
    assert!(full.contains(&items[0].text));
    assert!(!full.contains("LATEST_REPLY"));
    assert_unchanged(&dir, &items);
}

#[test]
fn json_show_filters_body_and_omits_raw_by_default_but_full_preserves_both() {
    let items = thread_items();
    let dir = seed(&items);
    let displayed = json(&dir, &["mail", "show", "old-requested"]);
    assert_eq!(displayed["id"], "old-requested");
    assert_eq!(
        displayed["text"].as_str().unwrap().trim(),
        "REQUESTED_REPLY"
    );
    assert_eq!(displayed["raw"], "");
    assert!(!displayed.to_string().contains("QUOTED_SECRET"));
    assert_eq!(
        displayed["flags"],
        serde_json::to_value(&items[0].flags).unwrap()
    );
    let full = json(&dir, &["mail", "show", "old-requested", "--full"]);
    assert_eq!(full["text"], items[0].text);
    assert_eq!(full["raw"], items[0].raw);
    assert_unchanged(&dir, &items);
}

// Agents list mail with --json on every turn; a single attachment-bearing
// message used to put megabytes of base64 into their context.
#[test]
fn json_list_and_search_omit_raw_by_default_but_full_restores_it() {
    let payload = STANDARD.encode(vec![0x5a_u8; 64 * 1024]);
    let mut original = item(
        Kind::Mail,
        "with-attachment",
        "季報附件",
        "ATTACHED_REPLY\n\nOn Tue, 6 Oct 2026, Alice wrote:\n> ATTACHED_QUOTED_SECRET",
        "2026-10-07T12:00:00Z",
    );
    original.raw = format!(
        "From: sender@example.test\r\nSubject: =?UTF-8?B?{}?=\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n--b\r\nContent-Type: application/octet-stream; name=report.bin\r\nContent-Transfer-Encoding: base64\r\n\r\n{payload}\r\n--b--\r\n",
        STANDARD.encode("季報附件"),
        original.text
    );
    let dir = seed(std::slice::from_ref(&original));
    for args in [
        vec!["mail", "list"],
        vec!["mail", "list", "--collection", "INBOX"],
        vec!["mail", "search", "季報附件"],
    ] {
        let compact = json(&dir, &args);
        let found = compact
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "with-attachment")
            .unwrap_or_else(|| panic!("{args:?} missed the fixture: {compact}"));
        assert_eq!(found["raw"], "", "{args:?}");
        assert_eq!(found["text"].as_str().unwrap().trim(), "ATTACHED_REPLY");
        assert_eq!(
            found["attachments"],
            serde_json::to_value(&original.attachments).unwrap()
        );
        assert!(!compact.to_string().contains(&payload[..64]), "{args:?}");
        assert!(!compact.to_string().contains("ATTACHED_QUOTED_SECRET"));

        let mut full_args = args.clone();
        full_args.push("--full");
        let full = json(&dir, &full_args);
        let found = full
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "with-attachment")
            .unwrap();
        assert_eq!(found["raw"], original.raw, "{full_args:?}");
        assert_eq!(found["text"], original.text);
    }
    // Plain-text rows never carried raw; --full is accepted there as a no-op.
    assert_eq!(
        plain(&dir, &["mail", "list"]),
        plain(&dir, &["mail", "list", "--full"])
    );
    assert_unchanged(&dir, &[original]);
}

#[test]
fn raw_mail_output_preserves_original_crlf_and_non_utf8_bytes() {
    let raw = b"From: sender@example.test\r\nSubject: Raw\r\nContent-Type: text/plain; charset=iso-8859-1\r\n\r\ncaf\xe9\r\n> historical\r\n";
    let mut original = item(
        Kind::Mail,
        "raw-binary",
        "Raw",
        "café\n> historical",
        "2026-10-07T12:00:00Z",
    );
    original.raw = format!("base64:{}", STANDARD.encode(raw));
    let dir = seed(std::slice::from_ref(&original));
    assert_eq!(
        output(&dir, &["mail", "show", "raw-binary", "--raw"]),
        raw.to_vec()
    );
    assert_unchanged(&dir, &[original]);
}

#[test]
fn empty_cached_text_is_recovered_from_raw_in_memory_before_reply_filtering() {
    let body =
        "RECOVERED_REPLY\r\n\r\nOn Tue, 6 Oct 2026, Alice wrote:\r\n> RECOVERED_QUOTED_SECRET\r\n";
    let mut original = item(
        Kind::Mail,
        "empty-cached-text",
        "Cached",
        "",
        "2026-10-07T12:00:00Z",
    );
    original.raw = format!("From: sender@example.test\r\nSubject: Cached\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}");
    let dir = seed(std::slice::from_ref(&original));
    let shown = json(&dir, &["mail", "show", "empty-cached-text"]);
    assert_eq!(shown["text"].as_str().unwrap().trim(), "RECOVERED_REPLY");
    assert_eq!(shown["raw"], "");
    assert!(!shown.to_string().contains("RECOVERED_QUOTED_SECRET"));
    let full = json(&dir, &["mail", "show", "empty-cached-text", "--full"]);
    assert!(full["text"]
        .as_str()
        .unwrap()
        .contains("RECOVERED_QUOTED_SECRET"));
    assert_eq!(full["raw"], original.raw);
    assert_unchanged(&dir, &[original]);
}

#[test]
fn thread_defaults_to_latest_matching_reply_and_full_history_uses_real_timestamp_order() {
    let items = thread_items();
    let dir = seed(&items);
    let expected_keys = [
        "count",
        "first_date",
        "items",
        "last_date",
        "normalized_subject",
        "subject",
    ];
    for selector in [["--id", "old-requested"], ["--subject", "專案上線"]] {
        let printed = plain(&dir, &["mail", "thread", selector[0], selector[1]]);
        assert!(printed.contains("LATEST_REPLY"));
        for excluded in [
            "REQUESTED_REPLY",
            "OLDER_OFFSET_REPLY",
            "QUOTED_SECRET",
            "UNRELATED_SECRET",
        ] {
            assert!(
                !printed.contains(excluded),
                "default thread leaked {excluded}"
            );
        }
        let displayed = json(&dir, &["mail", "thread", selector[0], selector[1]]);
        assert_eq!(
            displayed
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            expected_keys
        );
        assert_eq!(displayed["count"], 1);
        assert_eq!(displayed["items"][0]["id"], "latest-utc");
        assert_eq!(displayed["items"][0]["raw"], "");
        assert_eq!(
            displayed["items"][0]["text"].as_str().unwrap().trim(),
            "LATEST_REPLY"
        );
        assert!(!displayed.to_string().contains("QUOTED_SECRET"));
        assert_eq!(
            displayed["first_date"],
            items[1].date.as_ref().unwrap().as_str()
        );
        assert_eq!(
            displayed["last_date"],
            items[1].date.as_ref().unwrap().as_str()
        );
    }
    let full = json(&dir, &["mail", "thread", "--subject", "專案上線", "--full"]);
    assert_eq!(full["count"], 3);
    assert_eq!(
        full["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["latest-utc", "older-offset", "old-requested"]
    );
    assert_eq!(full["first_date"], items[0].date.as_ref().unwrap().as_str());
    assert_eq!(full["last_date"], items[1].date.as_ref().unwrap().as_str());
    assert!(full["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|item| !item["raw"].as_str().unwrap().is_empty()));
    assert!(full.to_string().contains("QUOTED_SECRET"));
    assert!(!full.to_string().contains("UNRELATED_SECRET"));
    let full_text = plain(&dir, &["mail", "thread", "--id", "old-requested", "--full"]);
    assert!(full_text.contains("REQUESTED_QUOTED_SECRET"));
    assert!(full_text.contains("LATEST_QUOTED_SECRET"));
    assert!(!full_text.contains("UNRELATED_SECRET"));
    let limited = json(
        &dir,
        &[
            "mail",
            "thread",
            "--subject",
            "專案上線",
            "--full",
            "--limit",
            "2",
        ],
    );
    assert_eq!(limited["count"], 2);
    assert_eq!(limited["items"][1]["id"], "older-offset");
    assert_unchanged(&dir, &items);
}

#[test]
fn same_timestamp_thread_selects_the_higher_numeric_uid() {
    let mut older = item(
        Kind::Mail,
        "mail:INBOX:7:9",
        "UID tie",
        "UID_NINE",
        "2026-10-07T12:00:00Z",
    );
    older.remote_id = "9".into();
    let mut newer = item(
        Kind::Mail,
        "mail:INBOX:7:10",
        "Re: UID tie",
        "UID_TEN",
        "2026-10-07T12:00:00Z",
    );
    newer.remote_id = "10".into();
    let items = [older, newer];
    let dir = seed(&items);
    let latest = json(&dir, &["mail", "thread", "--subject", "UID tie"]);
    assert_eq!(latest["count"], 1);
    assert_eq!(latest["items"][0]["id"], "mail:INBOX:7:10");
    let full = json(&dir, &["mail", "thread", "--subject", "UID tie", "--full"]);
    assert_eq!(full["items"][0]["id"], "mail:INBOX:7:10");
    assert_eq!(full["items"][1]["id"], "mail:INBOX:7:9");
    assert_unchanged(&dir, &items);
}

#[test]
fn full_thread_default_limit_keeps_the_newest_fifty_messages() {
    let base = DateTime::parse_from_rfc3339("2026-10-07T00:00:00Z").unwrap();
    let items = (0..55)
        .map(|index| {
            item(
                Kind::Mail,
                &format!("history-{index:02}"),
                "Re: 大型郵件串",
                &format!("REPLY_{index:02}\n\n> HISTORY_{index:02}"),
                &(base + Duration::minutes(index)).to_rfc3339_opts(SecondsFormat::Secs, true),
            )
        })
        .collect::<Vec<_>>();
    let dir = seed(&items);
    let full = json(
        &dir,
        &["mail", "thread", "--subject", "大型郵件串", "--full"],
    );
    assert_eq!(full["count"], 50);
    assert_eq!(full["items"][0]["id"], "history-54");
    assert_eq!(full["items"][49]["id"], "history-05");
    assert!(full["items"][0]["text"]
        .as_str()
        .unwrap()
        .contains("HISTORY_54"));
    assert_unchanged(&dir, &items);
}

#[test]
fn calendar_and_contacts_show_keep_their_complete_text_and_raw_content() {
    let items = [Kind::Calendar, Kind::Contacts]
        .into_iter()
        .map(|kind| {
            item(
                kind,
                &format!("{kind}-show"),
                "完整資料",
                "KEEP_THIS_TEXT\n\nOn Tue, Alice wrote:\n> KEEP_QUOTED_LOOKING_TEXT",
                "2026-10-07T12:00:00Z",
            )
        })
        .collect::<Vec<_>>();
    let dir = seed(&items);
    for original in &items {
        let kind = original.kind.to_string();
        assert!(plain(&dir, &[&kind, "show", &original.id]).contains(&original.text));
        assert_eq!(
            json(&dir, &[&kind, "show", &original.id]),
            serde_json::to_value(original).unwrap()
        );
        assert_eq!(
            output(&dir, &[&kind, "show", &original.id, "--raw"]),
            original.raw.as_bytes()
        );
    }
    assert_unchanged(&dir, &items);
}

fn assert_compact_meeting_text(printed: &str) {
    assert!(!printed.contains('{'));
    assert!(!printed.contains("\"rooms\":"));
    assert!(!printed.contains("calendar_url"));
    assert!(!printed
        .chars()
        .any(|character| matches!(character, '│' | '┌' | '┐' | '└' | '┘' | '─')));
    for line in printed.lines() {
        assert!(
            UnicodeWidthStr::width(line) <= 119,
            "meeting line exceeds 119 columns: {line}"
        );
        assert!(
            !line.chars().any(char::is_control),
            "meeting line contains controls: {line:?}"
        );
    }
}

#[test]
fn meeting_rooms_prints_compact_rows_and_errors_without_appending_json() {
    let dir = TempDir::new().unwrap();
    m2k::demo::seed_without_rooms(dir.path()).unwrap();
    let args = || RoomsArgs {
        refresh: false,
        site: Some("北區".into()),
        capacity: Some(16),
        start: "2026-10-07T10:00:00+08:00".into(),
        end: "2026-10-07T11:00:00+08:00".into(),
        minutes: None,
        working_hours: "09:30-17:30".into(),
    };
    let catalog = query_rooms(dir.path(), args()).unwrap();
    assert!(catalog.availability.len() > 1);
    let room_url = catalog.availability[0].calendar_url.clone();
    let mut store = Store::open(dir.path()).unwrap();
    let mut cached = store.get("demo-event-1").unwrap();
    cached.id = "plain-room-cache".into();
    cached.remote_id = "plain-room-cache.ics".into();
    cached.collection = room_url.clone();
    cached.raw = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:plain-room-cache\r\nDTSTART:20261007T000000Z\r\nDTEND:20261007T010000Z\r\nSUMMARY:Earlier meeting\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into();
    cached.date = Some("2026-10-07T00:00:00Z".into());
    cached.end_date = Some("2026-10-07T01:00:00Z".into());
    store.upsert(&cached).unwrap();
    drop(store);
    let expected = query_rooms(dir.path(), args()).unwrap();
    assert_eq!(expected.rooms.len(), 1);
    assert!(!expected.unknown.is_empty());
    let flags = [
        "meeting",
        "rooms",
        "--site",
        "北區",
        "--capacity",
        "16",
        "--start",
        "2026-10-07T10:00:00+08:00",
        "--end",
        "2026-10-07T11:00:00+08:00",
    ];
    let printed = plain(&dir, &flags);
    assert_compact_meeting_text(&printed);
    assert!(printed.contains(&expected.rooms[0].name));
    assert!(printed.contains("2026-10-07T10:00:00+08:00"));
    assert!(printed.contains("2026-10-07T11:00:00+08:00"));
    assert!(printed.contains("本機沒有此對象的行事曆快取"));
    assert!(!printed.contains(&room_url));
    assert_eq!(json(&dir, &flags), serde_json::to_value(expected).unwrap());
    assert_unchanged(&dir, &[cached]);
}

#[test]
fn meeting_resources_prints_compact_catalog_and_keeps_json_structure() {
    let dir = TempDir::new().unwrap();
    m2k::demo::seed_without_rooms(dir.path()).unwrap();
    let flags = ["meeting", "resources", "--site", "北區", "--capacity", "16"];
    let before = json(&dir, &flags);
    let rooms = before["resources"].as_array().unwrap();
    assert!(!rooms.is_empty());
    assert!(rooms[0]["calendar_url"].is_string());
    assert!(before["errors"].is_array());
    let printed = plain(&dir, &flags);
    assert_compact_meeting_text(&printed);
    assert!(printed.contains(rooms[0]["name"].as_str().unwrap()));
    assert_eq!(json(&dir, &flags), before);
}
