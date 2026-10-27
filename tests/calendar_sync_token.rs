//! RFC 6578 sync-token support: what the DAV layer sends and how it turns a
//! sync-collection delta into a whole-collection snapshot. The policy that
//! picks token vs. ETag sync is covered end to end in `fixture_sync_token.rs`.

use m2k::{
    config::Config,
    model::{Item, Kind},
    provider::dav::{self, CalendarChanges},
};
use std::{
    collections::BTreeMap,
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

fn calendar_reply(title: &str, etag: &str) -> Reply {
    Reply {
        status: "200 OK",
        body: ics(title),
        etag: Some(etag.to_owned()),
    }
}

fn status(status: &'static str, body: &str) -> Reply {
    Reply {
        status,
        body: body.to_owned(),
        etag: None,
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

const OLD_TOKEN: &str = "urn:test:sync:1";
const NEW_TOKEN: &str = "urn:test:sync:2";

fn config(address: SocketAddr) -> Config {
    let mut cfg = Config::new("user@example.test".into(), &format!("http://{address}")).unwrap();
    cfg.caldav_url = format!("http://{address}/cal/");
    cfg.carddav_url = format!("http://{address}/card/");
    cfg
}

fn ics(title: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{title}\r\nDTSTART:20261007T010000Z\r\nDTEND:20261007T020000Z\r\nSUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n")
}

fn multi(resources: &[String], token: Option<&str>) -> String {
    let token = token
        .map(|token| format!("<D:sync-token>{token}</D:sync-token>"))
        .unwrap_or_default();
    format!(
        "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">{}{token}</D:multistatus>",
        resources.concat()
    )
}

fn discovery() -> Reply {
    xml(multi(&["<D:response><D:href>/cal/</D:href><D:propstat><D:prop><D:displayname>Calendar</D:displayname><D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>".into()], None))
}

fn changed(name: &str, etag: &str) -> String {
    format!("<D:response><D:href>/cal/{name}.ics</D:href><D:propstat><D:prop><D:getetag>{etag}</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>")
}

fn gone(name: &str) -> String {
    format!("<D:response><D:href>/cal/{name}.ics</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>")
}

fn probe(props: &str) -> Reply {
    xml(multi(&[format!("<D:response><D:href>/cal/</D:href><D:propstat><D:prop>{props}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>")], None))
}

const SUPPORTED: &str = "<D:supported-report-set><D:supported-report><D:report><D:sync-collection/></D:report></D:supported-report></D:supported-report-set>";

fn cached(cfg: &Config, name: &str, etag: &str) -> Item {
    Item {
        id: format!("cached-{name}"),
        kind: Kind::Calendar,
        collection: cfg.caldav_url.clone(),
        remote_id: format!("{}{name}.ics", cfg.caldav_url),
        etag: Some(etag.to_owned()),
        uid_validity: None,
        title: name.to_owned(),
        subtitle: String::new(),
        date: Some("2026-10-07T01:00:00Z".into()),
        end_date: Some("2026-10-07T02:00:00Z".into()),
        text: String::new(),
        raw: ics(name),
        flags: Vec::new(),
        emails: Vec::new(),
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

fn tokens(cfg: &Config) -> BTreeMap<String, String> {
    BTreeMap::from([(cfg.caldav_url.clone(), OLD_TOKEN.to_owned())])
}

fn changes(cfg: &Config, existing: &[Item]) -> CalendarChanges {
    dav::sync_calendar_changes(cfg, "test-password", existing, &tokens(cfg)).unwrap()
}

fn titles(sync: &dav::CalendarSync) -> Vec<String> {
    let mut titles = sync
        .snapshot
        .items
        .iter()
        .map(|item| item.title.clone())
        .collect::<Vec<_>>();
    titles.sort();
    titles
}

#[test]
fn delta_adds_changes_and_deletes_while_untouched_cache_survives_without_requests() {
    let (address, worker) = server(vec![
        discovery(),
        xml(multi(
            &[
                changed("edited", "\"e2\""),
                changed("same", "\"s1\""),
                gone("removed"),
                changed("added", "\"a1\""),
            ],
            Some(NEW_TOKEN),
        )),
        calendar_reply("edited v2", "\"e2\""),
        calendar_reply("added", "\"a1\""),
    ]);
    let cfg = config(address);
    let existing = [
        cached(&cfg, "edited", "\"e1\""),
        cached(&cfg, "same", "\"s1\""),
        cached(&cfg, "removed", "\"r1\""),
        // Far from any near window and not in the delta: must stay cached.
        cached(&cfg, "distant", "\"d1\""),
    ];
    let CalendarChanges::Synced(sync) = changes(&cfg, &existing) else {
        panic!("expected a sync-collection delta");
    };
    assert_eq!(titles(&sync), ["added", "distant", "edited v2", "same"]);
    let edited = sync
        .snapshot
        .items
        .iter()
        .find(|item| item.title == "edited v2")
        .unwrap();
    assert_eq!(edited.id, "cached-edited", "identity survives an edit");
    assert_eq!(sync.sync_tokens[&cfg.caldav_url], NEW_TOKEN);
    let requests = worker.join().unwrap();
    assert_eq!(
        requests.len(),
        4,
        "unchanged ETag and deletions need no GET"
    );
    assert!(requests[1].starts_with("REPORT /cal/ "));
    assert!(requests[1].contains("Depth: 0") || requests[1].contains("depth: 0"));
    assert!(requests[1].contains(&format!("<D:sync-token>{OLD_TOKEN}</D:sync-token>")));
    assert!(!requests[1].contains("time-range"));
    assert!(requests[2].starts_with("GET /cal/edited.ics "));
    assert!(requests[3].starts_with("GET /cal/added.ics "));
}

#[test]
fn truncated_delta_follows_the_next_page_token() {
    let (address, worker) = server(vec![
        discovery(),
        xml(multi(
            &[
                gone("first"),
                "<D:response><D:href>/cal/</D:href><D:status>HTTP/1.1 507 Insufficient Storage</D:status></D:response>".into(),
            ],
            Some("urn:test:sync:page"),
        )),
        xml(multi(&[gone("second")], Some(NEW_TOKEN))),
    ]);
    let cfg = config(address);
    let existing = [
        cached(&cfg, "first", "\"1\""),
        cached(&cfg, "second", "\"2\""),
        cached(&cfg, "kept", "\"3\""),
    ];
    let CalendarChanges::Synced(sync) = changes(&cfg, &existing) else {
        panic!("expected a sync-collection delta");
    };
    assert_eq!(titles(&sync), ["kept"]);
    assert_eq!(sync.sync_tokens[&cfg.caldav_url], NEW_TOKEN);
    let requests = worker.join().unwrap();
    assert!(requests[2].contains("<D:sync-token>urn:test:sync:page</D:sync-token>"));
}

#[test]
fn only_valid_sync_token_errors_ask_for_a_full_sync() {
    for reply in [
        status(
            "403 Forbidden",
            "<D:error xmlns:D=\"DAV:\"><D:valid-sync-token/></D:error>",
        ),
        status(
            "400 Bad Request",
            "<D:error xmlns:D=\"DAV:\"><D:valid-sync-token/></D:error>",
        ),
    ] {
        let (address, worker) = server(vec![discovery(), reply]);
        let cfg = config(address);
        let existing = [cached(&cfg, "kept", "\"1\"")];
        assert!(matches!(
            changes(&cfg, &existing),
            CalendarChanges::TokenInvalid
        ));
        assert_eq!(worker.join().unwrap().len(), 2);
    }
}

#[test]
fn untrustworthy_sync_collection_answers_fall_back_instead_of_failing_every_tick() {
    let denied = "<D:response><D:href>/cal/secret.ics</D:href><D:status>HTTP/1.1 403 Forbidden</D:status></D:response>".to_owned();
    let nonstandard =
        "<D:response><D:href>/cal/x.ics</D:href><D:status>HTTP/1.1 200 OK</D:status></D:response>"
            .to_owned();
    let outside = "<D:response><D:href>/other/x.ics</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>".to_owned();
    for reply in [
        // A plain 403/409 without valid-sync-token is not proof the token expired.
        status("403 Forbidden", ""),
        status("409 Conflict", ""),
        status("500 Internal Server Error", ""),
        status("501 Not Implemented", ""),
        status("405 Method Not Allowed", ""),
        status("207 Multi-Status", "not xml"),
        xml(multi(&[denied], Some(NEW_TOKEN))),
        xml(multi(&[nonstandard], Some(NEW_TOKEN))),
        xml(multi(&[outside], Some(NEW_TOKEN))),
        xml(multi(&[gone("x")], None)),
    ] {
        let (address, worker) = server(vec![discovery(), reply]);
        let cfg = config(address);
        let existing = [cached(&cfg, "kept", "\"1\"")];
        assert!(matches!(
            changes(&cfg, &existing),
            CalendarChanges::Unusable(_)
        ));
        worker.join().unwrap();
    }
}

#[test]
fn authentication_and_connection_failures_stay_errors() {
    let (address, worker) = server(vec![discovery(), status("401 Unauthorized", "")]);
    let cfg = config(address);
    assert!(dav::sync_calendar_changes(&cfg, "test-password", &[], &tokens(&cfg)).is_err());
    worker.join().unwrap();
    // A broken HTTP response is a transport error, not a server answer.
    let (address, worker) = server(vec![discovery(), status("garbage", "")]);
    let cfg = config(address);
    assert!(dav::sync_calendar_changes(&cfg, "test-password", &[], &tokens(&cfg)).is_err());
    worker.join().unwrap();
}

#[test]
fn delta_keeps_unresolvable_cache_rows_and_canonical_collection_id() {
    let (address, worker) = server(vec![discovery(), xml(multi(&[], Some(NEW_TOKEN)))]);
    let cfg = config(address);
    let mut odd = cached(&cfg, "odd", "\"1\"");
    odd.remote_id = "http://elsewhere.test/cal/odd.ics".into();
    let mut encoded = cached(&cfg, "encoded", "\"1\"");
    encoded.collection = cfg.caldav_url.replace("/cal/", "/c%61l/");
    let CalendarChanges::Synced(sync) = changes(&cfg, &[odd, encoded]) else {
        panic!("expected a sync-collection delta");
    };
    assert_eq!(
        titles(&sync),
        ["encoded", "odd"],
        "no 404/410, nothing dropped"
    );
    let collection = &sync.snapshot.collections[0].id;
    assert!(sync
        .snapshot
        .items
        .iter()
        .all(|item| &item.collection == collection));
    worker.join().unwrap();
}

#[test]
fn collection_without_a_stored_token_keeps_the_etag_strategy() {
    let (address, worker) = server(vec![discovery()]);
    let cfg = config(address);
    let other = BTreeMap::from([("http://elsewhere/cal/".to_owned(), OLD_TOKEN.to_owned())]);
    assert!(matches!(
        dav::sync_calendar_changes(&cfg, "test-password", &[], &other).unwrap(),
        CalendarChanges::Unavailable
    ));
    assert_eq!(worker.join().unwrap().len(), 1, "only discovery was sent");
}

#[test]
fn full_sync_records_a_token_only_when_sync_collection_is_advertised() {
    let cases = [
        (
            probe(&format!(
                "<D:sync-token>{NEW_TOKEN}</D:sync-token>{SUPPORTED}"
            )),
            Some(NEW_TOKEN),
        ),
        // A token without sync-collection in supported-report-set is unusable.
        (
            probe(&format!("<D:sync-token>{NEW_TOKEN}</D:sync-token>")),
            None,
        ),
        (probe(SUPPORTED), None),
        // Probe failures must not fail the sync.
        (status("400 Bad Request", ""), None),
        (status("207 Multi-Status", "not xml"), None),
    ];
    for (reply, expected) in cases {
        let (address, worker) = server(vec![
            discovery(),
            reply,
            xml(multi(&[changed("event", "\"v1\"")], None)),
        ]);
        let cfg = config(address);
        let previous = cached(&cfg, "event", "\"v1\"");
        let sync = dav::sync_calendar_full(&cfg, "test-password", std::slice::from_ref(&previous))
            .unwrap();
        assert_eq!(sync.snapshot.items.len(), 1);
        assert_eq!(sync.snapshot.items[0].id, previous.id);
        assert_eq!(
            sync.sync_tokens.get(&cfg.caldav_url).map(String::as_str),
            expected
        );
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("PROPFIND /cal/ "));
        assert!(requests[1].contains("<D:sync-token/>"));
        assert!(
            requests[2].contains("calendar-query"),
            "listing still uses ETag metadata"
        );
    }
}

#[test]
fn delta_matches_cached_hrefs_across_percent_encoding() {
    let (address, worker) = server(vec![
        discovery(),
        xml(multi(
            &[gone("a%40b"), changed("c@d", "\"v1\"")],
            Some(NEW_TOKEN),
        )),
    ]);
    let cfg = config(address);
    let existing = [
        cached(&cfg, "a@b", "\"1\""),
        cached(&cfg, "c%40d", "\"v1\""),
    ];
    let CalendarChanges::Synced(sync) = changes(&cfg, &existing) else {
        panic!("expected a sync-collection delta");
    };
    assert_eq!(
        titles(&sync),
        ["c%40d"],
        "deleted and unchanged, no duplicates"
    );
    assert_eq!(
        worker.join().unwrap().len(),
        2,
        "unchanged ETag needs no GET"
    );
}
