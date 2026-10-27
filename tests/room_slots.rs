use assert_cmd::Command;
use m2k::{
    demo,
    meeting::{
        booking_range_in_gap, booking_window_in_gap, parse_time_range, query_rooms,
        room_available_intervals, MeetingEvent, RoomsArgs, Slot, WorkingHoursInput,
    },
    store::Store,
};
use serde_json::{json, Value};
use tempfile::TempDir;

// A keychain file that does not exist: m2k must never read or write the
// login keychain from tests.
const NO_KEYCHAIN: &str = "/nonexistent/m2k-test.keychain-db";

fn hours() -> WorkingHoursInput {
    WorkingHoursInput {
        start: 9 * 60 + 30,
        end: 17 * 60 + 30,
    }
}

fn event(start: &str, end: &str) -> MeetingEvent {
    MeetingEvent {
        id: "busy".into(),
        uid: Some("busy".into()),
        title: "Busy".into(),
        start: Some(start.into()),
        end: Some(end.into()),
        location: None,
        description: None,
        attendees: Vec::new(),
        recurring: false,
        source: "test".into(),
    }
}

fn local_event(start: &str, end: &str) -> MeetingEvent {
    event(
        &format!("2026-10-07T{start}+08:00"),
        &format!("2026-10-07T{end}+08:00"),
    )
}

fn day_range(start: &str, end: &str) -> m2k::meeting::TimeRange {
    parse_time_range(
        &format!("2026-10-07T{start}+08:00"),
        &format!("2026-10-07T{end}+08:00"),
    )
    .unwrap()
}

fn assert_intervals(slots: &[Slot], expected: &[(&str, &str)]) {
    let actual = slots
        .iter()
        .map(|slot| (slot.start.as_str(), slot.end.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(actual.as_slice(), expected);
}

#[test]
fn unobstructed_workday_is_one_eight_hour_interval_not_sliding_two_hour_slots() {
    let slots =
        room_available_intervals(&day_range("09:30:00", "17:30:00"), &[], 120, hours()).unwrap();
    assert_intervals(
        &slots,
        &[("2026-10-07T09:30:00+08:00", "2026-10-07T17:30:00+08:00")],
    );
}

#[test]
fn a_midday_booking_leaves_two_actual_maximal_gaps() {
    let slots = room_available_intervals(
        &day_range("09:30:00", "17:30:00"),
        &[local_event("11:30:00", "12:30:00")],
        120,
        hours(),
    )
    .unwrap();
    assert_intervals(
        &slots,
        &[
            ("2026-10-07T09:30:00+08:00", "2026-10-07T11:30:00+08:00"),
            ("2026-10-07T12:30:00+08:00", "2026-10-07T17:30:00+08:00"),
        ],
    );
}

#[test]
fn two_hour_search_lists_full_morning_and_afternoon_gaps_not_sliding_windows() {
    let slots = room_available_intervals(
        &day_range("09:30:00", "17:30:00"),
        &[local_event("12:00:00", "13:00:00")],
        120,
        hours(),
    )
    .unwrap();
    assert_intervals(
        &slots,
        &[
            ("2026-10-07T09:30:00+08:00", "2026-10-07T12:00:00+08:00"),
            ("2026-10-07T13:00:00+08:00", "2026-10-07T17:30:00+08:00"),
        ],
    );
}

#[test]
fn two_half_hour_bookings_split_the_workday_into_three_maximal_gaps() {
    let slots = room_available_intervals(
        &day_range("09:30:00", "17:30:00"),
        &[
            local_event("11:30:00", "12:00:00"),
            local_event("14:00:00", "14:30:00"),
        ],
        120,
        hours(),
    )
    .unwrap();
    assert_intervals(
        &slots,
        &[
            ("2026-10-07T09:30:00+08:00", "2026-10-07T11:30:00+08:00"),
            ("2026-10-07T12:00:00+08:00", "2026-10-07T14:00:00+08:00"),
            ("2026-10-07T14:30:00+08:00", "2026-10-07T17:30:00+08:00"),
        ],
    );
}

#[test]
fn booking_window_picks_actual_start_inside_a_listed_maximal_gap() {
    let gap = Slot {
        start: "2026-10-07T13:00:00+08:00".into(),
        end: "2026-10-07T17:30:00+08:00".into(),
    };
    let booked = booking_window_in_gap(&gap, "2026-10-07T14:00:00+08:00", 120).unwrap();
    assert_eq!(booked.start, "2026-10-07T14:00:00+08:00");
    assert_eq!(booked.end, "2026-10-07T16:00:00+08:00");
    assert!(booking_window_in_gap(&gap, "2026-10-07T16:00:00+08:00", 120).is_err());
    assert!(booking_window_in_gap(&gap, "2026-10-07T12:30:00+08:00", 120).is_err());
    let custom = booking_range_in_gap(
        &gap,
        "2026-10-07T13:30:00+08:00",
        "2026-10-07T15:00:00+08:00",
    )
    .unwrap();
    assert_eq!(custom.start, "2026-10-07T13:30:00+08:00");
    assert_eq!(custom.end, "2026-10-07T15:00:00+08:00");
    assert!(booking_range_in_gap(
        &gap,
        "2026-10-07T13:00:00+08:00",
        "2026-10-07T18:00:00+08:00",
    )
    .is_err());
}

#[test]
fn unsorted_overlapping_and_adjacent_bookings_merge_without_rounding_boundaries() {
    let slots = room_available_intervals(
        &day_range("09:30:00", "17:30:00"),
        &[
            local_event("12:00:00", "12:37:23.125"),
            local_event("11:00:00", "12:00:00"),
            local_event("10:00:00", "11:30:00"),
            local_event("10:15:00", "10:20:00"),
        ],
        30,
        hours(),
    )
    .unwrap();
    assert_intervals(
        &slots,
        &[
            ("2026-10-07T09:30:00+08:00", "2026-10-07T10:00:00+08:00"),
            ("2026-10-07T12:37:23.125+08:00", "2026-10-07T17:30:00+08:00"),
        ],
    );
}

#[test]
fn query_and_working_hours_clip_gaps_and_busy_blocks() {
    let slots = room_available_intervals(
        &day_range("11:17:00", "16:05:00"),
        &[
            local_event("09:00:00", "12:03:00"),
            local_event("15:02:00", "18:00:00"),
            local_event("07:00:00", "08:00:00"),
            local_event("19:00:00", "20:00:00"),
        ],
        120,
        hours(),
    )
    .unwrap();
    assert_intervals(
        &slots,
        &[("2026-10-07T12:03:00+08:00", "2026-10-07T15:02:00+08:00")],
    );
    let slots =
        room_available_intervals(&day_range("07:00:00", "20:00:00"), &[], 120, hours()).unwrap();
    assert_intervals(
        &slots,
        &[("2026-10-07T09:30:00+08:00", "2026-10-07T17:30:00+08:00")],
    );
}

#[test]
fn minimum_is_inclusive_and_gaps_shorter_by_even_a_millisecond_are_excluded() {
    let range = day_range("09:30:00", "11:30:00");
    assert_eq!(
        room_available_intervals(&range, &[], 120, hours())
            .unwrap()
            .len(),
        1
    );
    assert!(room_available_intervals(
        &range,
        &[local_event("11:29:59.999", "11:30:00")],
        120,
        hours(),
    )
    .unwrap()
    .is_empty());
    assert!(room_available_intervals(&range, &[], 121, hours())
        .unwrap()
        .is_empty());
}

#[test]
fn multiple_days_keep_separate_daily_maximal_gaps_and_exact_query_edges() {
    let range = parse_time_range("2026-10-07T16:00:00+08:00", "2026-10-09T10:15:00+08:00").unwrap();
    let slots = room_available_intervals(&range, &[], 30, hours()).unwrap();
    assert_intervals(
        &slots,
        &[
            ("2026-10-07T16:00:00+08:00", "2026-10-07T17:30:00+08:00"),
            ("2026-10-08T09:30:00+08:00", "2026-10-08T17:30:00+08:00"),
            ("2026-10-09T09:30:00+08:00", "2026-10-09T10:15:00+08:00"),
        ],
    );
    let slots = room_available_intervals(
        &range,
        &[event(
            "2026-10-08T00:00:00+08:00",
            "2026-10-09T00:00:00+08:00",
        )],
        30,
        hours(),
    )
    .unwrap();
    assert_intervals(
        &slots,
        &[
            ("2026-10-07T16:00:00+08:00", "2026-10-07T17:30:00+08:00"),
            ("2026-10-09T09:30:00+08:00", "2026-10-09T10:15:00+08:00"),
        ],
    );
}

#[test]
fn event_offsets_are_compared_as_instants_and_output_uses_query_offset() {
    let slots = room_available_intervals(
        &day_range("09:30:00", "17:30:00"),
        &[event("2026-10-07T03:37:00Z", "2026-10-07T04:08:00Z")],
        60,
        hours(),
    )
    .unwrap();
    assert_intervals(
        &slots,
        &[
            ("2026-10-07T09:30:00+08:00", "2026-10-07T11:37:00+08:00"),
            ("2026-10-07T12:08:00+08:00", "2026-10-07T17:30:00+08:00"),
        ],
    );
}

#[test]
fn recurring_or_unknown_event_times_never_create_free_intervals() {
    let range = day_range("09:30:00", "17:30:00");
    let mut recurring = local_event("11:30:00", "12:30:00");
    recurring.recurring = true;
    let mut missing_end = recurring.clone();
    missing_end.recurring = false;
    missing_end.end = None;
    for event in [
        recurring,
        missing_end,
        event("malformed", "2026-10-07T12:30:00+08:00"),
        local_event("12:30:00", "11:30:00"),
    ] {
        assert!(room_available_intervals(&range, &[event], 120, hours())
            .unwrap()
            .is_empty());
    }
}

#[test]
fn invalid_parameters_are_rejected_without_any_query() {
    let range = day_range("09:30:00", "17:30:00");
    assert!(room_available_intervals(&range, &[], 0, hours()).is_err());
    assert!(room_available_intervals(
        &range,
        &[],
        120,
        WorkingHoursInput {
            start: 17 * 60,
            end: 9 * 60,
        }
    )
    .is_err());
    let range = parse_time_range("2026-10-01T00:00:00Z", "2026-11-02T00:00:00Z").unwrap();
    assert!(room_available_intervals(&range, &[], 120, hours()).is_err());
}

fn args() -> RoomsArgs {
    RoomsArgs {
        refresh: false,
        site: Some("北區".into()),
        capacity: Some(16),
        start: "2026-10-07T09:30:00+08:00".into(),
        end: "2026-10-07T17:30:00+08:00".into(),
        minutes: Some(120),
        working_hours: "09:30-17:30".into(),
    }
}

#[test]
fn cached_room_query_and_cli_json_share_the_same_maximal_intervals() {
    let dir = TempDir::new().unwrap();
    demo::seed_without_rooms(dir.path()).unwrap();
    let unknown = query_rooms(dir.path(), args()).unwrap();
    assert!(unknown.available_slots.is_empty());
    let room = &unknown.availability[0];
    let mut store = Store::open(dir.path()).unwrap();
    let mut item = store.get("demo-event-1").unwrap();
    item.id = "maximal-room-event".into();
    item.remote_id = item.id.clone();
    item.collection = room.calendar_url.clone();
    item.raw = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:maximal-room-event\r\nDTSTART:20261007T033000Z\r\nDTEND:20261007T043000Z\r\nSUMMARY:Busy\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into();
    store.upsert(&item).unwrap();
    drop(store);
    let result = query_rooms(dir.path(), args()).unwrap();
    assert_eq!(result.rooms.len(), 1);
    assert_eq!(result.available_slots.len(), 1);
    assert_intervals(
        &result.available_slots[0].slots,
        &[
            ("2026-10-07T09:30:00+08:00", "2026-10-07T11:30:00+08:00"),
            ("2026-10-07T12:30:00+08:00", "2026-10-07T17:30:00+08:00"),
        ],
    );
    assert_eq!(result.unknown.len(), unknown.unknown.len() - 1);
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
            "2026-10-07T09:30:00+08:00",
            "--end",
            "2026-10-07T17:30:00+08:00",
            "--minutes",
            "120",
        ])
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let actual: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(actual, serde_json::to_value(&result).unwrap());
    assert_eq!(
        actual["available_slots"][0]["slots"],
        json!([
            {"start":"2026-10-07T09:30:00+08:00", "end":"2026-10-07T11:30:00+08:00"},
            {"start":"2026-10-07T12:30:00+08:00", "end":"2026-10-07T17:30:00+08:00"},
        ])
    );
}
