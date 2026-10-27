use chrono::{DateTime, Duration, Utc};
use m2k::config::Config;
use m2k::meeting::MeetingRoom;
use m2k::model::{Item, Kind};
use m2k::room_cache::{RoomCalendarCache, LOOKAHEAD_DAYS, TTL_SECONDS};
use rusqlite::{params, Connection};
use tempfile::TempDir;

const ROOM_URL: &str = "https://mail.example.test/calendars/room@example.com/default/";

fn config() -> Config {
    Config::new("person@example.com".into(), "mail.example.test").unwrap()
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn calendar() -> Item {
    Item {
        id: "calendar-event".into(),
        kind: Kind::Calendar,
        collection: ROOM_URL.into(),
        remote_id: "event.ics".into(),
        etag: Some("v1".into()),
        uid_validity: None,
        title: "會議".into(),
        subtitle: "會議室".into(),
        date: Some("2026-10-07T01:00:00Z".into()),
        end_date: Some("2026-10-07T02:00:00Z".into()),
        text: "Description".into(),
        raw: "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:event\r\nDTSTART:20261007T010000Z\r\nDTEND:20261007T020000Z\r\nRRULE:FREQ=DAILY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into(),
        flags: Vec::new(),
        emails: Vec::new(),
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

fn room() -> MeetingRoom {
    MeetingRoom {
        name: "北區測試室".into(),
        access_name: "北區測試室".into(),
        site: "北區".into(),
        capacity: Some(8),
        calendar_url: ROOM_URL.into(),
        source: "dav".into(),
    }
}

fn seed(cache: &RoomCalendarCache) {
    cache
        .store_success(
            &config(),
            ROOM_URL,
            now(),
            now() + Duration::days(LOOKAHEAD_DAYS),
            now(),
            &[calendar()],
        )
        .unwrap();
}

fn hit(cache: &RoomCalendarCache, at: DateTime<Utc>) -> bool {
    cache
        .lookup(
            &config(),
            ROOM_URL,
            now() + Duration::hours(1),
            now() + Duration::hours(2),
            at,
        )
        .unwrap()
        .is_some()
}

#[test]
fn fresh_calendar_cache_survives_reopen_and_preserves_raw_ics() {
    let dir = TempDir::new().unwrap();
    let cache = RoomCalendarCache::open(dir.path()).unwrap();
    seed(&cache);
    assert!(!dir.path().join("database.sqlite3").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(cache.database_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    drop(cache);
    let reopened = RoomCalendarCache::open(dir.path()).unwrap();
    let cached = reopened
        .lookup(
            &config(),
            ROOM_URL,
            now(),
            now() + Duration::days(LOOKAHEAD_DAYS),
            now() + Duration::minutes(1),
        )
        .unwrap()
        .unwrap();
    assert_eq!(cached.items[0].raw, calendar().raw);
    assert_eq!(cached.items[0].etag, calendar().etag);
    assert_eq!(cached.coverage_start, now());
    assert_eq!(cached.coverage_end, now() + Duration::days(30));
    assert_eq!(cached.fetched_at, now());
}

#[test]
fn ttl_boundary_and_future_fetch_time_are_misses() {
    let dir = TempDir::new().unwrap();
    let cache = RoomCalendarCache::open(dir.path()).unwrap();
    seed(&cache);
    assert!(hit(&cache, now()));
    assert!(hit(
        &cache,
        now() + Duration::seconds(TTL_SECONDS) - Duration::nanoseconds(1)
    ));
    assert!(!hit(&cache, now() + Duration::seconds(TTL_SECONDS)));
    assert!(!hit(&cache, now() + Duration::hours(12)));
    assert!(!hit(&cache, now() - Duration::nanoseconds(1)));
}

#[test]
fn query_coverage_uses_utc_and_requires_both_boundaries() {
    let dir = TempDir::new().unwrap();
    let cache = RoomCalendarCache::open(dir.path()).unwrap();
    seed(&cache);
    let taipei_start = DateTime::parse_from_rfc3339("2026-10-07T09:00:00+08:00")
        .unwrap()
        .with_timezone(&Utc);
    let taipei_end = DateTime::parse_from_rfc3339("2026-10-07T10:00:00+08:00")
        .unwrap()
        .with_timezone(&Utc);
    assert!(cache
        .lookup(&config(), ROOM_URL, taipei_start, taipei_end, now())
        .unwrap()
        .is_some());
    for (start, end) in [
        (now() - Duration::nanoseconds(1), now() + Duration::hours(1)),
        (now(), now() + Duration::days(30) + Duration::nanoseconds(1)),
        (now(), now()),
        (now() + Duration::hours(1), now()),
    ] {
        assert!(cache
            .lookup(&config(), ROOM_URL, start, end, now())
            .unwrap()
            .is_none());
    }
}

#[test]
fn cache_isolated_by_account_service_endpoints_and_room_url() {
    let dir = TempDir::new().unwrap();
    let cache = RoomCalendarCache::open(dir.path()).unwrap();
    seed(&cache);
    let mut changed_endpoint = config();
    changed_endpoint.caldav_url.push_str("other/");
    let mut changed_imap = config();
    changed_imap.imap_port = 994;
    let another_account = Config::new("other@example.com".into(), "mail.example.test").unwrap();
    for cfg in [changed_endpoint, changed_imap, another_account] {
        assert!(cache
            .lookup(&cfg, ROOM_URL, now(), now() + Duration::hours(1), now(),)
            .unwrap()
            .is_none());
    }
    assert!(cache
        .lookup(
            &config(),
            "https://mail.example.test/calendars/another-room/default/",
            now(),
            now() + Duration::hours(1),
            now(),
        )
        .unwrap()
        .is_none());
    assert!(hit(&cache, now()));
}

#[test]
fn malformed_metadata_payload_and_scope_are_treated_as_misses() {
    let dir = TempDir::new().unwrap();
    let cache = RoomCalendarCache::open(dir.path()).unwrap();
    let connection = Connection::open(cache.database_path()).unwrap();
    for statement in [
        "UPDATE room_calendars SET items_json = 'not-json'",
        "UPDATE room_calendars SET coverage_start = 'not-a-date'",
        "UPDATE room_calendars SET coverage_end = '2020-01-01T00:00:00Z'",
        "UPDATE room_calendars SET fetched_at = x'0001'",
        "UPDATE room_calendars SET account_key = 'another-account'",
        "UPDATE room_calendars SET calendar_url = 'another-calendar'",
    ] {
        seed(&cache);
        connection.execute(statement, []).unwrap();
        assert!(!hit(&cache, now()), "must miss: {statement}");
    }
    for invalid in [
        Item {
            kind: Kind::Mail,
            ..calendar()
        },
        Item {
            collection: "another-room".into(),
            ..calendar()
        },
        Item {
            raw: String::new(),
            ..calendar()
        },
    ] {
        seed(&cache);
        connection
            .execute(
                "UPDATE room_calendars SET items_json = ?1",
                params![serde_json::to_string(&vec![invalid]).unwrap()],
            )
            .unwrap();
        assert!(!hit(&cache, now()));
    }
}

#[test]
fn only_complete_successful_calendar_payloads_can_replace_cache() {
    let dir = TempDir::new().unwrap();
    let cache = RoomCalendarCache::open(dir.path()).unwrap();
    seed(&cache);
    let partial = Item {
        raw: String::new(),
        ..calendar()
    };
    assert!(cache
        .store_success(
            &config(),
            ROOM_URL,
            now(),
            now() + Duration::days(30),
            now(),
            &[partial],
        )
        .is_err());
    assert!(cache
        .store_success(&config(), ROOM_URL, now(), now(), now(), &[])
        .is_err());
    assert!(hit(&cache, now()));
    cache
        .store_success(
            &config(),
            ROOM_URL,
            now(),
            now() + Duration::days(30),
            now(),
            &[],
        )
        .unwrap();
    let verified_empty = cache
        .lookup(
            &config(),
            ROOM_URL,
            now(),
            now() + Duration::hours(1),
            now(),
        )
        .unwrap()
        .unwrap();
    assert!(verified_empty.items.is_empty());
}

#[test]
fn catalog_cache_is_fresh_account_scoped_and_rejects_malformed_payloads() {
    let dir = TempDir::new().unwrap();
    let cache = RoomCalendarCache::open(dir.path()).unwrap();
    cache.store_catalog(&config(), now(), &[room()]).unwrap();
    let rooms = cache.lookup_catalog(&config(), now()).unwrap().unwrap();
    assert_eq!(rooms[0].calendar_url, ROOM_URL);
    assert_eq!(rooms[0].capacity, Some(8));
    assert!(cache
        .lookup_catalog(&config(), now() + Duration::seconds(TTL_SECONDS))
        .unwrap()
        .is_none());
    assert!(cache
        .lookup_catalog(&config(), now() - Duration::nanoseconds(1))
        .unwrap()
        .is_none());
    let other = Config::new("another@example.com".into(), "mail.example.test").unwrap();
    assert!(cache.lookup_catalog(&other, now()).unwrap().is_none());
    let connection = Connection::open(cache.database_path()).unwrap();
    connection
        .execute("UPDATE room_catalogs SET rooms_json = '[{}]'", [])
        .unwrap();
    assert!(cache.lookup_catalog(&config(), now()).unwrap().is_none());
    cache.store_catalog(&config(), now(), &[room()]).unwrap();
    connection
        .execute("UPDATE room_catalogs SET fetched_at = 'bad-date'", [])
        .unwrap();
    assert!(cache.lookup_catalog(&config(), now()).unwrap().is_none());
}

#[test]
fn concurrent_connections_keep_room_calendar_payloads_isolated() {
    let dir = TempDir::new().unwrap();
    let workers = (0..4)
        .map(|index| {
            let path = dir.path().to_owned();
            std::thread::spawn(move || {
                let cache = RoomCalendarCache::open(&path).unwrap();
                let url = format!("https://mail.example.test/calendars/room-{index}/default/");
                let item = Item {
                    collection: url.clone(),
                    title: format!("Room {index} event"),
                    ..calendar()
                };
                cache
                    .store_success(
                        &config(),
                        &url,
                        now(),
                        now() + Duration::days(30),
                        now(),
                        &[item],
                    )
                    .unwrap();
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap();
    }
    let cache = RoomCalendarCache::open(dir.path()).unwrap();
    for index in 0..4 {
        let url = format!("https://mail.example.test/calendars/room-{index}/default/");
        let entry = cache
            .lookup(&config(), &url, now(), now() + Duration::hours(1), now())
            .unwrap()
            .unwrap();
        assert_eq!(entry.items.len(), 1);
        assert_eq!(entry.items[0].title, format!("Room {index} event"));
        assert_eq!(entry.items[0].collection, url);
    }
}
