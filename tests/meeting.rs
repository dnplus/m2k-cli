use chrono::{DateTime, FixedOffset};
use m2k::meeting::{
    booking_resource_ics, build_invite_ics, build_invite_ics_with_uid, classify_availability,
    escape_ical_text, event_overlaps, intersect_available_slots, parse_meeting_ics, MeetingEvent,
    WorkingHoursInput,
};

fn at(value: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(value).expect("valid RFC3339")
}

fn event(start: &str, end: &str) -> MeetingEvent {
    MeetingEvent {
        id: "event-1".into(),
        uid: Some("event-1".into()),
        title: "既有行程".into(),
        start: Some(start.into()),
        end: Some(end.into()),
        location: None,
        description: None,
        attendees: Vec::new(),
        recurring: false,
        source: "test".into(),
    }
}

#[test]
fn common_slots_respect_overlap_and_working_hours() {
    let range = m2k::meeting::TimeRange {
        start: at("2026-10-06T09:30:00+08:00"),
        end: at("2026-10-06T12:30:00+08:00"),
    };
    let busy = [event(
        "2026-10-06T10:00:00+08:00",
        "2026-10-06T10:30:00+08:00",
    )];
    let slots = intersect_available_slots(
        &range,
        &busy,
        30,
        30,
        WorkingHoursInput {
            start: 9 * 60 + 30,
            end: 17 * 60 + 30,
        },
    )
    .expect("slots");
    assert_eq!(slots[0].start, "2026-10-06T09:30:00+08:00");
    assert!(slots
        .iter()
        .all(|slot| slot.start != "2026-10-06T10:00:00+08:00"));
    assert!(slots
        .iter()
        .any(|slot| slot.start == "2026-10-06T10:30:00+08:00"));
}

#[test]
fn overlap_uses_half_open_intervals() {
    let range = m2k::meeting::TimeRange {
        start: at("2026-10-06T10:00:00+08:00"),
        end: at("2026-10-06T11:00:00+08:00"),
    };
    assert!(!event_overlaps(
        &range,
        &event("2026-10-06T09:00:00+08:00", "2026-10-06T10:00:00+08:00")
    ));
    assert!(event_overlaps(
        &range,
        &event("2026-10-06T10:59:00+08:00", "2026-10-06T11:30:00+08:00")
    ));
}

#[test]
fn duration_slots_clip_each_workday_without_crossing_midnight() {
    let range = m2k::meeting::TimeRange {
        start: at("2026-10-07T16:30:00+08:00"),
        end: at("2026-10-08T10:30:00+08:00"),
    };
    let slots = intersect_available_slots(
        &range,
        &[],
        120,
        15,
        WorkingHoursInput {
            start: 570,
            end: 1050,
        },
    )
    .unwrap();
    assert!(slots.is_empty(), "neither day contains two working hours");

    let range = m2k::meeting::TimeRange {
        start: at("2026-10-07T17:15:00+08:00"),
        end: at("2026-10-08T10:45:00+08:00"),
    };
    let slots = intersect_available_slots(
        &range,
        &[],
        30,
        15,
        WorkingHoursInput {
            start: 570,
            end: 1050,
        },
    )
    .unwrap();
    assert_eq!(slots.len(), 4);
    assert_eq!(slots[0].start, "2026-10-08T09:30:00+08:00");
    assert_eq!(slots.last().unwrap().end, "2026-10-08T10:45:00+08:00");
}

#[test]
fn duration_slots_compare_instants_with_different_dst_offsets() {
    let range = m2k::meeting::TimeRange {
        start: at("2026-11-01T00:00:00-04:00"),
        end: at("2026-11-02T00:00:00-05:00"),
    };
    let slots = intersect_available_slots(
        &range,
        &[],
        60,
        60,
        WorkingHoursInput { start: 0, end: 180 },
    )
    .unwrap();
    assert_eq!(slots.len(), 4);
    assert_eq!(slots.last().unwrap().start, "2026-11-02T00:00:00-04:00");
    assert_eq!(at(&slots.last().unwrap().end), range.end);
    assert!(slots.iter().all(|slot| {
        at(&slot.start).date_naive() == at(&slot.end).date_naive()
            && at(&slot.end)
                .signed_duration_since(at(&slot.start))
                .num_minutes()
                == 60
    }));
}

#[test]
fn duration_slots_respect_overlapping_events_and_touching_boundaries() {
    let range = m2k::meeting::TimeRange {
        start: at("2026-10-07T09:30:00+08:00"),
        end: at("2026-10-07T12:00:00+08:00"),
    };
    let slots = intersect_available_slots(
        &range,
        &[
            event("2026-10-07T10:00:00+08:00", "2026-10-07T11:00:00+08:00"),
            event("2026-10-07T10:30:00+08:00", "2026-10-07T11:30:00+08:00"),
        ],
        30,
        15,
        WorkingHoursInput {
            start: 570,
            end: 1050,
        },
    )
    .unwrap();
    assert_eq!(slots.len(), 2);
    assert_eq!(slots[0].end, "2026-10-07T10:00:00+08:00");
    assert_eq!(slots[1].start, "2026-10-07T11:30:00+08:00");
}

#[test]
fn unknown_availability_is_never_free() {
    let range = m2k::meeting::TimeRange {
        start: at("2026-10-06T10:00:00+08:00"),
        end: at("2026-10-06T11:00:00+08:00"),
    };
    let result = classify_availability(
        "同事",
        "https://mail.example.test/calendar",
        &range,
        &[],
        Some("401 Unauthorized"),
    );
    assert_eq!(result.state, "unknown");
    assert!(!result.free);
    assert_eq!(result.reason.as_deref(), Some("401 Unauthorized"));
}

#[test]
fn recurring_event_does_not_create_free_slots_without_expansion() {
    let range = m2k::meeting::TimeRange {
        start: at("2026-10-06T10:00:00+08:00"),
        end: at("2026-10-06T11:00:00+08:00"),
    };
    let mut recurring = event("2026-10-01T10:00:00+08:00", "2026-10-01T11:00:00+08:00");
    recurring.recurring = true;
    let slots = intersect_available_slots(
        &range,
        &[recurring],
        30,
        30,
        WorkingHoursInput {
            start: 9 * 60,
            end: 18 * 60,
        },
    )
    .expect("conservative result");
    assert!(slots.is_empty());
}

#[test]
fn invite_escapes_text_and_uses_create_new_friendly_crlf() {
    assert_eq!(escape_ical_text("a\\b,c;d\ne"), "a\\\\b\\,c\\;d\\ne");
    let range = m2k::meeting::TimeRange {
        start: at("2026-10-06T10:00:00+08:00"),
        end: at("2026-10-06T11:00:00+08:00"),
    };
    let (ics, uid) = build_invite_ics(
        "設計,評審",
        &range,
        Some("北區 A; 5F"),
        Some("第一行\n第二行"),
        "測試主持人",
        "owner@example.test",
        &["a@example.test".into()],
    )
    .expect("ICS");
    assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"));
    assert!(ics.ends_with("END:VCALENDAR\r\n"));
    assert!(ics.contains("METHOD:REQUEST\r\n"));
    assert!(ics.contains("SUMMARY:設計\\,評審"));
    assert!(ics.contains("DESCRIPTION:第一行\\n第二行"));
    assert!(ics.contains(&format!("UID:{uid}\r\n")));
}

#[test]
fn parser_keeps_multiple_events_and_expands_all_day_boundaries() {
    let raw = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:all-day@example.com\r\nDTSTART;VALUE=DATE:20261006\r\nSUMMARY:全天\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:expanded@example.com\r\nDTSTART:20261006T020000Z\r\nDTEND:20261006T030000Z\r\nRECURRENCE-ID:20261006T020000Z\r\nSUMMARY:展開實例\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let events = parse_meeting_ics(raw).expect("ICS events");
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].start.as_deref(),
        Some("2026-10-06T00:00:00+08:00")
    );
    assert_eq!(events[0].end.as_deref(), Some("2026-10-07T00:00:00+08:00"));
    assert!(!events[1].recurring);
    assert!(event_overlaps(
        &m2k::meeting::TimeRange {
            start: at("2026-10-06T10:00:00+08:00"),
            end: at("2026-10-06T11:00:00+08:00"),
        },
        &events[0]
    ));
}

#[test]
fn parser_derives_end_from_duration_and_marks_bad_event_unknown() {
    let raw = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:duration@example.com\nDTSTART:20261006T020000Z\nDURATION:PT90M\nSUMMARY:有時長\nEND:VEVENT\nBEGIN:VEVENT\nUID:bad@example.com\nDTSTART:NOT-A-DATE\nDTEND:20261006T040000Z\nSUMMARY:壞事件\nEND:VEVENT\nEND:VCALENDAR\n";
    let events = parse_meeting_ics(raw).expect("ICS events");
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].start.as_deref(),
        Some("2026-10-06T02:00:00+00:00")
    );
    assert_eq!(events[0].end.as_deref(), Some("2026-10-06T03:30:00+00:00"));
    let range = m2k::meeting::TimeRange {
        start: at("2026-10-06T02:00:00+00:00"),
        end: at("2026-10-06T03:00:00+00:00"),
    };
    let availability = classify_availability("行事曆", "calendar", &range, &events, None);
    assert_eq!(availability.state, "unknown");
    assert!(!availability.free);
}

#[test]
fn parser_accepts_day_and_time_duration() {
    let raw = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:duration-2@example.com\nDTSTART:20261006T020000Z\nDURATION:P1DT2H\nSUMMARY:跨日\nEND:VEVENT\nEND:VCALENDAR\n";
    let events = parse_meeting_ics(raw).expect("ICS events");
    assert_eq!(events[0].end.as_deref(), Some("2026-10-07T04:00:00+00:00"));
}

#[test]
fn cancelled_and_transparent_events_do_not_block_availability() {
    let raw = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:cancelled@example.com\nDTSTART:20261006T020000Z\nDTEND:20261006T030000Z\nSTATUS:CANCELLED\nEND:VEVENT\nBEGIN:VEVENT\nUID:transparent@example.com\nDTSTART:20261006T020000Z\nDTEND:20261006T030000Z\nTRANSP:TRANSPARENT\nEND:VEVENT\nEND:VCALENDAR\n";
    let events = parse_meeting_ics(raw).expect("ICS events");
    assert!(events.is_empty());
    let range = m2k::meeting::TimeRange {
        start: at("2026-10-06T02:00:00+00:00"),
        end: at("2026-10-06T03:00:00+00:00"),
    };
    let availability = classify_availability("行事曆", "calendar", &range, &events, None);
    assert_eq!(availability.state, "free");
    assert!(availability.free);
}

#[test]
fn booking_payload_removes_method_and_keeps_stable_uid() {
    let range = m2k::meeting::TimeRange {
        start: at("2026-10-06T10:00:00+08:00"),
        end: at("2026-10-06T11:00:00+08:00"),
    };
    let (invite, _) = build_invite_ics_with_uid(
        "stable@example.com",
        "會議",
        &range,
        Some("北區會議室"),
        Some("保留描述"),
        ("主持人", "owner@example.test"),
        &[],
    )
    .expect("invite");
    assert!(invite.contains("METHOD:REQUEST\r\n"));
    let booking = booking_resource_ics(&invite, "stable@example.com").expect("booking payload");
    assert!(!booking.contains("METHOD:"));
    assert!(booking.contains("UID:stable@example.com\r\n"));
    assert!(booking.contains("DESCRIPTION:保留描述\r\n"));
    let same = booking_resource_ics(&booking, "stable@example.com").expect("repeat payload");
    assert_eq!(same, booking);
}
