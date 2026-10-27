use assert_cmd::Command;
use m2k::{
    demo,
    meeting::{query_rooms, RoomsArgs},
    store::Store,
};
use serde_json::Value;
use tempfile::TempDir;

// A keychain file that does not exist: m2k must never read or write the
// login keychain from tests.
const NO_KEYCHAIN: &str = "/nonexistent/m2k-test.keychain-db";

const START: &str = "2026-10-07T10:00:00+08:00";
const END: &str = "2026-10-07T11:00:00+08:00";

fn args() -> RoomsArgs {
    RoomsArgs {
        refresh: false,
        site: Some("北區".into()),
        capacity: Some(16),
        start: START.into(),
        end: END.into(),
        minutes: None,
        working_hours: "09:30-17:30".into(),
    }
}

#[test]
fn room_query_validates_site_and_time_before_loading_credentials() {
    let dir = TempDir::new().unwrap();
    let mut input = args();
    input.site = Some("不存在的據點".into());
    assert!(query_rooms(dir.path(), input)
        .unwrap_err()
        .to_string()
        .contains("site 必須"));

    let mut input = args();
    input.start = "2026-10-07T10:00:00".into();
    assert!(query_rooms(dir.path(), input)
        .unwrap_err()
        .to_string()
        .contains("帶時區"));
    let mut input = args();
    input.end = START.into();
    assert!(query_rooms(dir.path(), input)
        .unwrap_err()
        .to_string()
        .contains("結束時間必須晚於開始時間"));
    let mut input = args();
    input.minutes = Some(0);
    assert!(query_rooms(dir.path(), input)
        .unwrap_err()
        .to_string()
        .contains("minutes 必須大於 0"));
    let mut input = args();
    input.working_hours = "17:30-09:30".into();
    assert!(query_rooms(dir.path(), input)
        .unwrap_err()
        .to_string()
        .contains("工作時間結束必須晚於開始"));
    let mut input = args();
    input.minutes = Some(30);
    input.end = "2026-12-07T11:00:00+08:00".into();
    assert!(query_rooms(dir.path(), input)
        .unwrap_err()
        .to_string()
        .contains("最多 31 天"));
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
}

#[test]
fn demo_room_query_keeps_missing_cache_unknown_and_exposes_reasons() {
    let dir = TempDir::new().unwrap();
    demo::seed_without_rooms(dir.path()).unwrap();

    let result = query_rooms(dir.path(), args()).unwrap();
    assert_eq!(result.start, START);
    assert_eq!(result.end, END);
    assert!(result.rooms.is_empty());
    assert!(!result.availability.is_empty());
    assert_eq!(result.unknown.len(), result.availability.len());
    assert_eq!(result.errors.len(), result.unknown.len());
    for check in &result.availability {
        assert_eq!(check.state, "unknown");
        assert!(!check.free);
        assert!(check.target.starts_with("北區"));
        assert!(check
            .reason
            .as_deref()
            .unwrap()
            .contains("本機沒有此對象的行事曆快取"));
        assert!(result
            .errors
            .iter()
            .any(|error| error.contains(&check.target)));
    }
}

#[test]
fn reusable_room_query_distinguishes_free_busy_and_unknown_rooms() {
    let dir = TempDir::new().unwrap();
    demo::seed_without_rooms(dir.path()).unwrap();
    let catalog = query_rooms(dir.path(), args()).unwrap();
    assert!(catalog.availability.len() >= 3);
    let busy = &catalog.availability[0];
    let free = &catalog.availability[1];
    let mut store = Store::open(dir.path()).unwrap();
    let base = store.get("demo-event-1").unwrap();
    for (id, room, start, end) in [
        ("room-busy", busy, "20261007T020000Z", "20261007T030000Z"),
        ("room-free", free, "20261007T010000Z", "20261007T020000Z"),
    ] {
        let mut item = base.clone();
        item.id = id.into();
        item.remote_id = id.into();
        item.collection = room.calendar_url.clone();
        item.raw = format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{id}\r\nDTSTART:{start}\r\nDTEND:{end}\r\nSUMMARY:會議室行程\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        );
        store.upsert(&item).unwrap();
    }
    drop(store);

    let result = query_rooms(dir.path(), args()).unwrap();
    assert_eq!(result.rooms.len(), 1);
    assert_eq!(result.rooms[0].calendar_url, free.calendar_url);
    assert_eq!(result.rooms[0].site, "北區");
    assert!(result.rooms[0].capacity.unwrap() >= 16);
    let busy_result = result
        .availability
        .iter()
        .find(|check| check.calendar_url == busy.calendar_url)
        .unwrap();
    assert_eq!(busy_result.state, "busy");
    assert!(!busy_result.free);
    assert_eq!(busy_result.events.len(), 1);
    let free_result = result
        .availability
        .iter()
        .find(|check| check.calendar_url == free.calendar_url)
        .unwrap();
    assert_eq!(free_result.state, "free");
    assert!(free_result.free);
    assert!(free_result.events.is_empty());
    assert_eq!(result.unknown.len(), catalog.availability.len() - 2);
    assert!(result
        .unknown
        .iter()
        .all(|check| check.calendar_url != free.calendar_url
            && check.calendar_url != busy.calendar_url));
}

#[test]
fn room_query_applies_capacity_filter_before_probing() {
    let dir = TempDir::new().unwrap();
    demo::seed_without_rooms(dir.path()).unwrap();
    let mut input = args();
    input.site = Some(" 中區 ".into());
    input.capacity = Some(u32::MAX);
    let result = query_rooms(dir.path(), input).unwrap();
    assert!(result.rooms.is_empty());
    assert!(result.availability.is_empty());
    assert!(result.unknown.is_empty());
    assert!(result.errors.is_empty());
}

#[test]
fn meeting_rooms_cli_json_matches_reusable_query_without_credentials() {
    let dir = TempDir::new().unwrap();
    demo::seed_without_rooms(dir.path()).unwrap();
    let expected = serde_json::to_value(query_rooms(dir.path(), args()).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args([
            "meeting",
            "rooms",
            "--site",
            "北區",
            "--capacity",
            "16",
            "--start",
            START,
            "--end",
            END,
        ])
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let actual: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn duration_query_returns_real_slots_and_cli_prints_their_times() {
    let dir = TempDir::new().unwrap();
    demo::seed_without_rooms(dir.path()).unwrap();
    let catalog = query_rooms(dir.path(), args()).unwrap();
    let room = &catalog.availability[0];
    let mut store = Store::open(dir.path()).unwrap();
    let mut item = store.get("demo-event-1").unwrap();
    item.id = "duration-room-event".into();
    item.remote_id = item.id.clone();
    item.collection = room.calendar_url.clone();
    item.raw = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:duration-room-event\r\nDTSTART:20261007T020000Z\r\nDTEND:20261007T030000Z\r\nSUMMARY:既有會議\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into();
    store.upsert(&item).unwrap();
    drop(store);
    let mut input = args();
    input.start = "2026-10-07T09:30:00+08:00".into();
    input.end = "2026-10-07T12:00:00+08:00".into();
    input.minutes = Some(30);
    let result = query_rooms(dir.path(), input).unwrap();
    assert_eq!(result.rooms.len(), 1);
    assert_eq!(result.available_slots.len(), 1);
    let slots = &result.available_slots[0].slots;
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0].start, "2026-10-07T09:30:00+08:00");
    assert_eq!(slots[0].end, "2026-10-07T10:00:00+08:00");
    assert_eq!(slots[1].start, "2026-10-07T11:00:00+08:00");
    assert_eq!(slots.last().unwrap().end, "2026-10-07T12:00:00+08:00");
    assert_eq!(result.availability[0].state, "busy");
    assert_eq!(result.unknown.len(), catalog.unknown.len() - 1);
    let flags = [
        "meeting",
        "rooms",
        "--site",
        "北區",
        "--capacity",
        "16",
        "--start",
        "2026-10-07T09:30:00+08:00",
        "--end",
        "2026-10-07T12:00:00+08:00",
        "--minutes",
        "30",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(flags)
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains(&room.target));
    assert!(text.contains("2026-10-07T09:30:00+08:00"));
    assert!(text.contains("2026-10-07T10:00:00+08:00"));
    assert!(text.contains("2026-10-07T11:00:00+08:00"));
    assert!(text.contains("2026-10-07T12:00:00+08:00"));
    assert_eq!(
        text.lines()
            .filter(|line| line.contains(&room.target))
            .count(),
        2
    );
    assert!(!text.contains("\"available_slots\""));
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args(flags)
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&output).unwrap(),
        serde_json::to_value(result).unwrap()
    );
}

#[test]
fn duration_query_excludes_unexpanded_recurring_room_calendars() {
    let dir = TempDir::new().unwrap();
    demo::seed_without_rooms(dir.path()).unwrap();
    let catalog = query_rooms(dir.path(), args()).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut item = store.get("demo-event-1").unwrap();
    item.collection = catalog.availability[0].calendar_url.clone();
    item.raw = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:recurring-room\r\nDTSTART:20261007T020000Z\r\nDTEND:20261007T030000Z\r\nRRULE:FREQ=DAILY\r\nSUMMARY:重複會議\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into();
    store.upsert(&item).unwrap();
    drop(store);
    let mut input = args();
    input.minutes = Some(30);
    let result = query_rooms(dir.path(), input).unwrap();
    assert!(result.rooms.is_empty());
    assert!(result.available_slots.is_empty());
    assert!(result.availability.iter().all(|check| !check.free));
    assert!(result.unknown[0]
        .reason
        .as_deref()
        .unwrap()
        .contains("重複"));
}

#[test]
fn malformed_room_ics_stays_unknown_in_exact_and_duration_queries() {
    let dir = TempDir::new().unwrap();
    demo::seed_without_rooms(dir.path()).unwrap();
    let catalog = query_rooms(dir.path(), args()).unwrap();
    let room_url = catalog.availability[0].calendar_url.clone();
    for raw in [
        "nonempty but not an iCalendar resource",
        "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:unfinished-room\r\nDTSTART:20261007T020000Z\r\nDTEND:20261007T030000Z\r\n",
    ] {
        let mut store = Store::open(dir.path()).unwrap();
        let mut item = store.get("demo-event-1").unwrap();
        item.collection = room_url.clone();
        item.raw = raw.into();
        store.upsert(&item).unwrap();
        drop(store);
        for minutes in [None, Some(30)] {
            let mut input = args();
            input.minutes = minutes;
            let result = query_rooms(dir.path(), input).unwrap();
            assert!(result.rooms.is_empty());
            assert!(result.available_slots.is_empty());
            let check = result.availability.iter().find(|check| check.calendar_url == room_url).unwrap();
            assert_eq!(check.state, "unknown");
            assert!(!check.free);
            assert!(check.reason.as_deref().unwrap().contains("ICS"));
        }
    }
}

#[test]
fn ignored_room_events_remain_free_in_exact_and_duration_queries() {
    let dir = TempDir::new().unwrap();
    demo::seed_without_rooms(dir.path()).unwrap();
    let catalog = query_rooms(dir.path(), args()).unwrap();
    let room_url = catalog.availability[0].calendar_url.clone();
    let mut store = Store::open(dir.path()).unwrap();
    let mut item = store.get("demo-event-1").unwrap();
    item.collection = room_url.clone();
    item.raw = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:cancelled-room\r\nDTSTART:20261007T020000Z\r\nDTEND:20261007T030000Z\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:transparent-room\r\nDTSTART:20261007T020000Z\r\nDTEND:20261007T030000Z\r\nTRANSP:TRANSPARENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into();
    store.upsert(&item).unwrap();
    drop(store);
    for minutes in [None, Some(30)] {
        let mut input = args();
        input.minutes = minutes;
        let result = query_rooms(dir.path(), input).unwrap();
        assert_eq!(result.rooms.len(), 1);
        assert_eq!(result.rooms[0].calendar_url, room_url);
        let check = result
            .availability
            .iter()
            .find(|check| check.calendar_url == room_url)
            .unwrap();
        assert_eq!(check.state, "free");
        assert!(check.free);
        assert!(check.events.is_empty());
        if minutes.is_some() {
            assert_eq!(result.available_slots.len(), 1);
            assert_eq!(result.available_slots[0].slots.len(), 1);
            assert_eq!(result.available_slots[0].slots[0].start, START);
            assert_eq!(result.available_slots[0].slots[0].end, END);
        }
    }
}

#[test]
fn room_c_oct20_taipei_fixture_blocks_the_requested_room_calendar() {
    let dir = TempDir::new().unwrap();
    demo::seed_without_rooms(dir.path()).unwrap();
    let input = || RoomsArgs {
        capacity: None,
        start: "2026-10-20T16:00:00+08:00".into(),
        end: "2026-10-20T17:30:00+08:00".into(),
        ..args()
    };
    let catalog = query_rooms(dir.path(), input()).unwrap();
    let room = catalog
        .availability
        .iter()
        .find(|room| room.target.starts_with("北區丙室_"))
        .unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut item = store.get("demo-event-1").unwrap();
    item.id = "room-c-fixture".into();
    item.remote_id = item.id.clone();
    item.collection = room.calendar_url.clone();
    item.raw = include_str!("fixtures/room-c-oct20.ics").into();
    store.upsert(&item).unwrap();
    drop(store);
    let result = query_rooms(dir.path(), input()).unwrap();
    let room_result = result
        .availability
        .iter()
        .find(|entry| entry.calendar_url == room.calendar_url)
        .unwrap();
    assert_eq!(room_result.state, "busy");
    assert!(!result
        .rooms
        .iter()
        .any(|entry| entry.calendar_url == room.calendar_url));
    let mut input = input();
    input.start = "2026-10-20T08:00:00Z".into();
    input.end = "2026-10-20T09:30:00Z".into();
    let utc = query_rooms(dir.path(), input).unwrap();
    assert_eq!(
        utc.availability
            .iter()
            .find(|entry| entry.calendar_url == room.calendar_url)
            .unwrap()
            .state,
        "busy"
    );
}
