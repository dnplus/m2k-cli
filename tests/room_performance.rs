use assert_cmd::Command;
use chrono::{Duration as ChronoDuration, Utc};
use m2k::{
    config::{self, Config},
    room_cache::TTL_SECONDS,
};
use rusqlite::Connection;
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use tempfile::TempDir;

const ROOMS: usize = 8;

struct DavServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    deny_first: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    reports: Arc<AtomicUsize>,
    maximum_active_reports: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl DavServer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let deny_first = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let reports = Arc::new(AtomicUsize::new(0));
        let active_reports = Arc::new(AtomicUsize::new(0));
        let maximum_active_reports = Arc::new(AtomicUsize::new(0));
        let handle = {
            let stop = stop.clone();
            let deny_first = deny_first.clone();
            let requests = requests.clone();
            let reports = reports.clone();
            let maximum_active_reports = maximum_active_reports.clone();
            thread::spawn(move || {
                let mut workers = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let deny_first = deny_first.clone();
                            let requests = requests.clone();
                            let reports = reports.clone();
                            let active_reports = active_reports.clone();
                            let maximum_active_reports = maximum_active_reports.clone();
                            workers.push(thread::spawn(move || {
                                stream.set_nonblocking(false).unwrap();
                                stream
                                    .set_read_timeout(Some(Duration::from_secs(3)))
                                    .unwrap();
                                let Some(request) = read_request(&mut stream) else {
                                    return;
                                };
                                let first_line = request.lines().next().unwrap_or("").to_owned();
                                requests.lock().unwrap().push(request);
                                if first_line.starts_with("PROPFIND ") {
                                    write_response(&mut stream, "207 Multi-Status", &catalog_xml());
                                } else if first_line.starts_with("REPORT ") {
                                    reports.fetch_add(1, Ordering::SeqCst);
                                    let active = active_reports.fetch_add(1, Ordering::SeqCst) + 1;
                                    maximum_active_reports.fetch_max(active, Ordering::SeqCst);
                                    thread::sleep(Duration::from_millis(60));
                                    if first_line.contains("/r0/")
                                        && deny_first.load(Ordering::SeqCst)
                                    {
                                        write_response(&mut stream, "403 Forbidden", "denied");
                                    } else {
                                        let body = if first_line.contains("/r0/") {
                                            future_event_xml()
                                        } else {
                                            multistatus("")
                                        };
                                        write_response(&mut stream, "207 Multi-Status", &body);
                                    }
                                    active_reports.fetch_sub(1, Ordering::SeqCst);
                                } else {
                                    write_response(
                                        &mut stream,
                                        "405 Method Not Allowed",
                                        "unexpected method",
                                    );
                                }
                            }));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("mock server failed: {error}"),
                    }
                }
                for worker in workers {
                    worker.join().unwrap();
                }
            })
        };
        Self {
            address,
            stop,
            deny_first,
            requests,
            reports,
            maximum_active_reports,
            handle: Some(handle),
        }
    }
}

impl Drop for DavServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let result = handle.join();
            if !thread::panicking() {
                result.expect("mock server thread failed");
            }
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut header = Vec::new();
    let mut byte = [0_u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) => header.push(byte[0]),
        }
    }
    let mut request = String::from_utf8(header).unwrap();
    let content_length = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; content_length];
    stream.read_exact(&mut body).ok()?;
    request.push_str(&String::from_utf8(body).unwrap());
    Some(request)
}

fn write_response(stream: &mut TcpStream, status: &str, body: &str) {
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

fn multistatus(body: &str) -> String {
    format!("<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\">{body}</D:multistatus>")
}

fn catalog_xml() -> String {
    let rooms = (0..ROOMS).map(|index| format!(
        "<D:response><D:href>/rooms/r{index}/</D:href><D:propstat><D:prop><D:displayname>中區測試{index}_1000</D:displayname><D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )).collect::<String>();
    multistatus(&rooms)
}

fn future_event_xml() -> String {
    multistatus("<D:response><D:href>/rooms/r0/future.ics</D:href><D:propstat><D:prop><D:getetag>&quot;future-v1&quot;</D:getetag><C:calendar-data><![CDATA[BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:future-room-event\r\nDTSTART:20261010T020000Z\r\nDTEND:20261010T030000Z\r\nSUMMARY:未來會議\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n]]></C:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>")
}

fn setup(server: &DavServer) -> TempDir {
    let dir = TempDir::new().unwrap();
    let mut cfg = Config::with_site_profile(
        "sample@example.test".into(),
        "mail.example.test",
        Some("demo".into()),
    )
    .unwrap();
    cfg.caldav_url = format!("http://{}/cal/self/", server.address);
    cfg.carddav_url = format!("http://{}/card/self/", server.address);
    config::save(dir.path(), &cfg).unwrap();
    dir
}

fn query(dir: &TempDir, start: &str, end: &str) -> Value {
    query_with_refresh(dir, start, end, false)
}

fn query_with_refresh(dir: &TempDir, start: &str, end: &str, refresh: bool) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_m2k"));
    command
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args([
            "meeting",
            "rooms",
            "--site",
            "中區",
            "--capacity",
            "1000",
            "--start",
            start,
            "--end",
            end,
            "--minutes",
            "60",
        ])
        .env("M2K_PASSWORD", "test-only-password")
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db");
    if refresh {
        command.arg("--refresh");
    }
    let output = command.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&output).unwrap()
}

#[test]
fn room_probes_are_bounded_and_free_slots_are_rechecked_even_with_fresh_cache() {
    let server = DavServer::start();
    let dir = setup(&server);
    let first = query(
        &dir,
        "2026-10-07T09:30:00+08:00",
        "2026-10-07T17:30:00+08:00",
    );
    assert_eq!(first["rooms"].as_array().unwrap().len(), ROOMS);
    assert_eq!(first["available_slots"].as_array().unwrap().len(), ROOMS);
    for room in first["available_slots"].as_array().unwrap() {
        assert_eq!(room["slots"].as_array().unwrap().len(), 1);
        assert_eq!(room["slots"][0]["start"], "2026-10-07T09:30:00+08:00");
        assert_eq!(room["slots"][0]["end"], "2026-10-07T17:30:00+08:00");
    }
    assert!(first["errors"].as_array().unwrap().is_empty());
    assert_eq!(server.reports.load(Ordering::SeqCst), ROOMS);
    let concurrent = server.maximum_active_reports.load(Ordering::SeqCst);
    assert!(concurrent >= 2, "room REPORTs should overlap");
    assert!(concurrent <= 4, "at most four room REPORTs may run at once");
    let before = server.requests.lock().unwrap().len();
    assert_eq!(
        before,
        ROOMS + 1,
        "one shared catalog discovery, then one REPORT per room"
    );
    for request in server
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.starts_with("REPORT "))
    {
        assert!(
            request.contains("20261106T013000Z"),
            "successful cache fetch expands to 30 days"
        );
    }

    let second = query(
        &dir,
        "2026-10-07T09:30:00+08:00",
        "2026-10-07T17:30:00+08:00",
    );
    assert_eq!(second["cache_hits"], 0);
    assert_eq!(second["available_slots"], first["available_slots"]);
    assert!(server.requests.lock().unwrap().len() > before);

    let future = query(
        &dir,
        "2026-10-10T10:00:00+08:00",
        "2026-10-10T11:00:00+08:00",
    );
    assert_eq!(future["cache_hits"], 0);
    assert_eq!(future["rooms"].as_array().unwrap().len(), ROOMS - 1);
    assert!(server.requests.lock().unwrap().len() > before);
    let busy = future["availability"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["target"] == "中區測試0_1000")
        .unwrap();
    assert_eq!(busy["state"], "busy");
    assert_eq!(busy["free"], false);
    assert_eq!(first["calendars"][0]["source"], "remote");
    assert_eq!(second["calendars"][0]["source"], "remote");
    assert_eq!(server.reports.load(Ordering::SeqCst), 3 * ROOMS);
    assert_eq!(
        first["calendars"][0]["coverage_end"],
        "2026-11-06T01:30:00Z"
    );
    let refreshed = query_with_refresh(
        &dir,
        "2026-10-07T09:30:00+08:00",
        "2026-10-07T17:30:00+08:00",
        true,
    );
    assert_eq!(refreshed["cache_hits"], 0);
    assert_eq!(refreshed["calendars"][0]["source"], "remote");
    assert_eq!(server.reports.load(Ordering::SeqCst), 4 * ROOMS);
}

#[test]
fn expired_cache_refresh_403_stays_unknown_and_is_retried() {
    let server = DavServer::start();
    let dir = setup(&server);
    let first = query(
        &dir,
        "2026-10-07T09:30:00+08:00",
        "2026-10-07T17:30:00+08:00",
    );
    assert_eq!(first["rooms"].as_array().unwrap().len(), ROOMS);
    let expired = (Utc::now() - ChronoDuration::seconds(TTL_SECONDS + 1)).to_rfc3339();
    let conn = Connection::open(dir.path().join("room_cache.sqlite3")).unwrap();
    conn.execute("UPDATE room_calendars SET fetched_at = ?1", [&expired])
        .unwrap();
    conn.execute("UPDATE room_catalogs SET fetched_at = ?1", [&expired])
        .unwrap();
    drop(conn);
    server.deny_first.store(true, Ordering::SeqCst);

    let refreshed = query(
        &dir,
        "2026-10-07T09:30:00+08:00",
        "2026-10-07T17:30:00+08:00",
    );
    assert_eq!(refreshed["cache_hits"], 0);
    assert_eq!(refreshed["rooms"].as_array().unwrap().len(), ROOMS - 1);
    assert_eq!(refreshed["unknown"].as_array().unwrap().len(), 1);
    assert_eq!(refreshed["unknown"][0]["state"], "unknown");
    assert_eq!(refreshed["unknown"][0]["free"], false);
    assert!(refreshed["unknown"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("403"));
    assert!(refreshed["errors"]
        .as_array()
        .unwrap()
        .iter()
        .any(|error| error.as_str().unwrap().contains("403")));
    assert_eq!(server.reports.load(Ordering::SeqCst), 2 * ROOMS);

    let retried = query(
        &dir,
        "2026-10-07T09:30:00+08:00",
        "2026-10-07T17:30:00+08:00",
    );
    assert_eq!(retried["cache_hits"], 0);
    assert_eq!(retried["unknown"].as_array().unwrap().len(), 1);
    assert_eq!(retried["rooms"].as_array().unwrap().len(), ROOMS - 1);
    assert_eq!(
        server.reports.load(Ordering::SeqCst),
        3 * ROOMS,
        "free and denied rooms are rechecked"
    );
}
