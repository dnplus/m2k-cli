use chrono::{DateTime, Utc};
use m2k::{
    config::Config,
    model::{Item, Kind},
    provider::dav,
};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

struct Reply {
    status: &'static str,
    body: String,
    etag: Option<String>,
}

fn xml(body: String) -> Reply {
    Reply {
        status: "207 Multi-Status",
        body,
        etag: None,
    }
}

fn calendar_reply(title: &str, uid: Option<&str>, etag: Option<&str>) -> Reply {
    Reply {
        status: "200 OK",
        body: ics(title, uid),
        etag: etag.map(str::to_owned),
    }
}

fn server(replies: Vec<Reply>) -> (SocketAddr, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "mock request did not arrive");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("mock accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            requests.push(request(&mut stream));
            let mut wire = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                reply.status,
                reply.body.len()
            );
            if let Some(etag) = reply.etag {
                wire.push_str(&format!("ETag: {etag}\r\n"));
            }
            wire.push_str("\r\n");
            wire.push_str(&reply.body);
            stream.write_all(wire.as_bytes()).unwrap();
        }
        requests
    });
    (address, worker)
}

fn request(stream: &mut TcpStream) -> String {
    let mut headers = Vec::new();
    let mut byte = [0];
    while !headers.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        headers.push(byte[0]);
    }
    let mut text = String::from_utf8(headers).unwrap();
    let length = text
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    text.push_str(&String::from_utf8(body).unwrap());
    text
}

fn config(address: SocketAddr) -> Config {
    let mut cfg = Config::new("user@example.test".into(), &format!("http://{address}")).unwrap();
    cfg.caldav_url = format!("http://{address}/cal/");
    cfg.carddav_url = format!("http://{address}/card/");
    cfg
}

fn ics(title: &str, uid: Option<&str>) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n{}DTSTART:20261007T010000Z\r\nDTEND:20261007T020000Z\r\nSUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", uid.map(|uid| format!("UID:{uid}\r\n")).unwrap_or_default())
}

fn multi(resources: Vec<String>) -> String {
    format!("<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">{}</D:multistatus>", resources.concat())
}

fn discovery() -> String {
    multi(vec!["<D:response><D:href>/cal/</D:href><D:propstat><D:prop><D:displayname>Calendar</D:displayname><D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>".into()])
}

fn metadata(href: &str, etag: Option<&str>, inline: Option<&str>) -> String {
    let etag = etag
        .map(|tag| format!("<D:getetag>{tag}</D:getetag>"))
        .unwrap_or_default();
    let inline = inline
        .map(|raw| format!("<C:calendar-data><![CDATA[{raw}]]></C:calendar-data>"))
        .unwrap_or_default();
    format!("<D:response><D:href>{href}</D:href><D:propstat><D:prop>{etag}{inline}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>")
}

fn cached(cfg: &Config, name: &str, etag: Option<&str>) -> Item {
    Item {
        id: format!("legacy-{name}"),
        kind: Kind::Calendar,
        collection: cfg.caldav_url.clone(),
        remote_id: format!("{}{name}.ics", cfg.caldav_url),
        etag: etag.map(str::to_owned),
        uid_validity: None,
        title: format!("Cached {name}"),
        subtitle: String::new(),
        date: Some("2026-10-07T01:00:00Z".into()),
        end_date: Some("2026-10-07T02:00:00Z".into()),
        text: "Cached description".into(),
        raw: ics(&format!("Cached {name}"), Some(name)),
        flags: Vec::new(),
        emails: Vec::new(),
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

fn window() -> (DateTime<Utc>, DateTime<Utc>) {
    (
        DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    )
}

#[test]
fn event_moved_outside_window_is_refetched_with_same_identity() {
    let moved = ics("Moved event", Some("moved")).replace("20261007", "20261215");
    let (address, worker) = server(vec![
        xml(discovery()),
        xml(multi(Vec::new())),
        Reply {
            status: "200 OK",
            body: moved,
            etag: Some("new-tag".into()),
        },
    ]);
    let cfg = config(address);
    let original = cached(&cfg, "moved", Some("old-tag"));
    let (start, end) = window();
    let snapshot = dav::sync_calendar_window(
        &cfg,
        "test-only",
        std::slice::from_ref(&original),
        start,
        end,
    )
    .unwrap();
    assert_eq!(snapshot.items.len(), 1);
    assert_eq!(snapshot.items[0].id, original.id);
    assert_eq!(snapshot.items[0].remote_id, original.remote_id);
    assert!(snapshot.items[0]
        .date
        .as_ref()
        .unwrap()
        .starts_with("2026-12-15"));
    assert_eq!(snapshot.items[0].etag.as_deref(), Some("new-tag"));
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].starts_with("GET /cal/moved.ics HTTP/1.1"));
}

#[test]
fn absent_event_is_removed_only_after_remote_confirms_not_found() {
    for status in ["404 Not Found", "410 Gone"] {
        let (address, worker) = server(vec![
            xml(discovery()),
            xml(multi(Vec::new())),
            Reply {
                status,
                body: String::new(),
                etag: None,
            },
        ]);
        let cfg = config(address);
        let (start, end) = window();
        let snapshot = dav::sync_calendar_window(
            &cfg,
            "test-only",
            &[cached(&cfg, "deleted", Some("old"))],
            start,
            end,
        )
        .unwrap();
        assert!(snapshot.items.is_empty());
        assert_eq!(worker.join().unwrap().len(), 3);
    }
}

#[test]
fn all_day_and_duration_events_moved_outside_window_are_refetched() {
    for (properties, date, end_date) in [
        (
            "DTSTART;VALUE=DATE:20261007\r\nDTEND;VALUE=DATE:20261008",
            "2026-10-07",
            Some("2026-10-08"),
        ),
        (
            "DTSTART:20261007T010000Z\r\nDURATION:PT1H",
            "2026-10-07T01:00:00Z",
            None,
        ),
        ("DTSTART;VALUE=DATE:20261007", "2026-10-07", None),
    ] {
        let raw = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:moved-special\r\n{properties}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
        let (address, worker) = server(vec![
            xml(discovery()),
            xml(multi(Vec::new())),
            Reply {
                status: "200 OK",
                body: raw
                    .replace("20261007", "20261215")
                    .replace("20261008", "20261216"),
                etag: Some("new-tag".into()),
            },
        ]);
        let cfg = config(address);
        let mut original = cached(&cfg, "moved-special", Some("old-tag"));
        original.raw = raw;
        original.date = Some(date.into());
        original.end_date = end_date.map(str::to_owned);
        let (start, end) = window();
        let snapshot = dav::sync_calendar_window(
            &cfg,
            "test-only",
            std::slice::from_ref(&original),
            start,
            end,
        )
        .unwrap();
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].id, original.id);
        assert!(snapshot.items[0]
            .date
            .as_deref()
            .unwrap()
            .starts_with("2026-12-15"));
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].starts_with("GET /cal/moved-special.ics"));
    }
}

#[test]
fn absent_event_with_revoked_permission_fails_instead_of_becoming_deleted() {
    let (address, worker) = server(vec![
        xml(discovery()),
        xml(multi(Vec::new())),
        Reply {
            status: "403 Forbidden",
            body: String::new(),
            etag: None,
        },
    ]);
    let cfg = config(address);
    let (start, end) = window();
    assert!(dav::sync_calendar_window(
        &cfg,
        "test-only",
        &[cached(&cfg, "private", Some("old"))],
        start,
        end
    )
    .is_err());
    assert_eq!(worker.join().unwrap().len(), 3);
}

#[test]
fn near_refresh_does_not_refetch_distant_cached_events() {
    let (address, worker) = server(vec![xml(discovery()), xml(multi(Vec::new()))]);
    let cfg = config(address);
    let mut distant = cached(&cfg, "distant", Some("old"));
    distant.date = Some("2026-12-15T01:00:00Z".into());
    distant.end_date = Some("2026-12-15T02:00:00Z".into());
    distant.raw = distant.raw.replace("20261007", "20261215");
    let (start, end) = window();
    assert!(
        dav::sync_calendar_window(&cfg, "test-only", &[distant], start, end)
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(worker.join().unwrap().len(), 2);
}

#[test]
fn unchanged_etag_uses_cached_body_and_metadata_only_window_report() {
    let (address, worker) = server(vec![
        xml(discovery()),
        xml(multi(vec![metadata(
            "/cal/unchanged.ics",
            Some("&quot;v1&quot;"),
            Some("ignored malformed inline content"),
        )])),
        Reply {
            status: "404 Not Found",
            body: String::new(),
            etag: None,
        },
    ]);
    let cfg = config(address);
    let unchanged = cached(&cfg, "unchanged", Some("\"v1\""));
    let deleted = cached(&cfg, "deleted", Some("\"old\""));
    let mut outside = cached(&cfg, "outside", Some("\"outside\""));
    outside.date = Some("2025-01-01T00:00:00Z".into());
    outside.end_date = Some("2025-01-01T01:00:00Z".into());
    outside.raw = outside.raw.replace("20261007", "20250101");
    let (start, end) = window();
    let snapshot = dav::sync_calendar_window(
        &cfg,
        "test-password",
        &[unchanged.clone(), deleted, outside],
        start,
        end,
    )
    .unwrap();
    assert_eq!(snapshot.items.len(), 1);
    assert_eq!(snapshot.items[0].id, unchanged.id);
    assert_eq!(snapshot.items[0].raw, unchanged.raw);
    assert_eq!(snapshot.items[0].text, unchanged.text);
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].starts_with("GET /cal/deleted.ics HTTP/1.1"));
    assert!(requests[1].starts_with("REPORT /cal/ HTTP/1.1"));
    assert!(requests[1].contains("<D:getetag/>"));
    assert!(!requests[1].contains("calendar-data"));
    assert!(requests[1].contains("start=\"20261001T000000Z\" end=\"20261101T000000Z\""));
}

#[test]
fn only_new_and_changed_resources_are_fetched_and_existing_ids_survive() {
    let (address, worker) = server(vec![
        xml(discovery()),
        xml(multi(vec![
            metadata("/cal/changed.ics", Some("&quot;v2&quot;"), None),
            metadata("/cal/new.ics", Some("&quot;new-v1&quot;"), None),
        ])),
        calendar_reply("Changed event", Some("changed"), Some("\"v2\"")),
        calendar_reply("New event without UID", None, None),
        Reply {
            status: "404 Not Found",
            body: String::new(),
            etag: None,
        },
    ]);
    let cfg = config(address);
    let changed = cached(&cfg, "changed", Some("\"v1\""));
    let deleted = cached(&cfg, "deleted", Some("\"old\""));
    let (start, end) = window();
    let snapshot = dav::sync_calendar_window(
        &cfg,
        "test-password",
        &[changed.clone(), deleted],
        start,
        end,
    )
    .unwrap();
    assert_eq!(snapshot.items.len(), 2);
    assert_eq!(snapshot.items[0].id, changed.id);
    assert_eq!(snapshot.items[0].title, "Changed event");
    assert_eq!(snapshot.items[0].etag.as_deref(), Some("\"v2\""));
    assert_eq!(snapshot.items[1].title, "New event without UID");
    assert_eq!(snapshot.items[1].etag.as_deref(), Some("\"new-v1\""));
    assert!(snapshot.items[1].remote_id.ends_with("/cal/new.ics"));
    assert!(!snapshot
        .items
        .iter()
        .any(|item| item.id == "legacy-deleted"));
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests[2].starts_with("GET /cal/changed.ics HTTP/1.1"));
    assert!(requests[3].starts_with("GET /cal/new.ics HTTP/1.1"));
    assert!(requests[2].to_lowercase().contains("accept: text/calendar"));
}

#[test]
fn inline_changed_calendar_data_is_tolerated_without_an_extra_get() {
    let inline = ics("Changed inline", Some("changed"));
    let (address, worker) = server(vec![
        xml(discovery()),
        xml(multi(vec![metadata(
            "/cal/changed.ics",
            Some("&quot;v2&quot;"),
            Some(&inline),
        )])),
    ]);
    let cfg = config(address);
    let previous = cached(&cfg, "changed", Some("\"v1\""));
    let (start, end) = window();
    let snapshot = dav::sync_calendar_window(
        &cfg,
        "test-password",
        std::slice::from_ref(&previous),
        start,
        end,
    )
    .unwrap();
    assert_eq!(snapshot.items[0].id, previous.id);
    assert_eq!(snapshot.items[0].title, "Changed inline");
    assert_eq!(worker.join().unwrap().len(), 2);
}

#[test]
fn missing_or_wildcard_etag_and_incomplete_cached_body_require_fetch() {
    for (listed, cached_etag, incomplete) in [
        (None, Some("\"v1\""), false),
        (Some("*"), Some("*"), false),
        (Some("&quot;v1&quot;"), Some("\"v1\""), true),
    ] {
        let (address, worker) = server(vec![
            xml(discovery()),
            xml(multi(vec![metadata("/cal/event.ics", listed, None)])),
            calendar_reply("Fetched complete", Some("event"), None),
        ]);
        let cfg = config(address);
        let mut previous = cached(&cfg, "event", cached_etag);
        if incomplete {
            previous.raw.clear();
        }
        let (start, end) = window();
        let snapshot =
            dav::sync_calendar_window(&cfg, "test-password", &[previous.clone()], start, end)
                .unwrap();
        assert_eq!(snapshot.items[0].id, previous.id);
        assert_eq!(snapshot.items[0].title, "Fetched complete");
        assert!(!snapshot.items[0].raw.is_empty());
        if listed.is_none() || listed == Some("*") {
            assert!(snapshot.items[0].etag.is_none());
        }
        assert_eq!(worker.join().unwrap().len(), 3);
    }
}

#[test]
fn full_retention_and_unlimited_sync_also_use_etag_metadata() {
    for retention in [0, 180] {
        let (address, worker) = server(vec![
            xml(discovery()),
            xml(multi(vec![metadata(
                "/cal/event.ics",
                Some("&quot;v1&quot;"),
                None,
            )])),
        ]);
        let mut cfg = config(address);
        cfg.calendar_retention_days = retention;
        let previous = cached(&cfg, "event", Some("\"v1\""));
        let snapshot = dav::sync(
            &cfg,
            "test-password",
            Kind::Calendar,
            std::slice::from_ref(&previous),
        )
        .unwrap();
        assert_eq!(snapshot.items[0].id, previous.id);
        assert_eq!(snapshot.items[0].raw, previous.raw);
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[1].contains("calendar-data"));
        assert_eq!(requests[1].contains("<C:time-range start="), retention > 0);
        assert!(!requests[1].contains(" end="));
    }
}

#[test]
fn invalid_window_is_rejected_before_network_and_cross_collection_href_is_rejected() {
    let cfg = config("127.0.0.1:9".parse().unwrap());
    let (start, end) = window();
    assert!(
        dav::sync_calendar_window(&cfg, "test-password", &[], end, start)
            .unwrap_err()
            .to_string()
            .contains("早到晚")
    );
    let (address, worker) = server(vec![
        xml(discovery()),
        xml(multi(vec![metadata(
            "/another/event.ics",
            Some("&quot;v1&quot;"),
            None,
        )])),
    ]);
    let cfg = config(address);
    let error = dav::sync_calendar_window(&cfg, "test-password", &[], start, end).unwrap_err();
    assert!(error.to_string().contains("不屬於指定 collection"));
    assert_eq!(worker.join().unwrap().len(), 2);
}
