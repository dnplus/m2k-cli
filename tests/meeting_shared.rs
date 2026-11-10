use assert_cmd::Command;
use m2k::{
    config::{self, Config},
    model::{Collection, Item, Kind, Snapshot},
    store::Store,
};
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

const ME: &str = "me@example.test";
const SHARED: &str = "teammate@example.test";
const UNSHARED: &str = "closed@example.test";
const BROKEN: &str = "broken@example.test";
const FILLERS: usize = 30;
const CALENDARS: &str = "/cgi-bin/cal/caldav/calendars";
const PRINCIPALS: &str = "/cgi-bin/cal/caldav/principals";

struct DavServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    /// `Depth PATH` of every PROPFIND, with `%40` decoded.
    requests: Arc<Mutex<Vec<String>>>,
    maximum_active: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl DavServer {
    fn start() -> Self {
        Self::start_with(route)
    }

    fn start_with(route: fn(&str) -> (&'static str, String)) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum_active = Arc::new(AtomicUsize::new(0));
        let handle = {
            let stop = stop.clone();
            let requests = requests.clone();
            let maximum_active = maximum_active.clone();
            thread::spawn(move || {
                let mut workers = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let requests = requests.clone();
                            let active = active.clone();
                            let maximum_active = maximum_active.clone();
                            workers.push(thread::spawn(move || {
                                stream.set_nonblocking(false).unwrap();
                                stream
                                    .set_read_timeout(Some(Duration::from_secs(3)))
                                    .unwrap();
                                let Some(request) = read_request(&mut stream) else {
                                    return;
                                };
                                let first = request.lines().next().unwrap_or("");
                                let path = first
                                    .split_whitespace()
                                    .nth(1)
                                    .unwrap_or("")
                                    .replace("%40", "@");
                                let depth = request
                                    .lines()
                                    .find_map(|line| line.strip_prefix("depth: "))
                                    .or_else(|| {
                                        request
                                            .lines()
                                            .find_map(|line| line.strip_prefix("Depth: "))
                                    })
                                    .unwrap_or("?")
                                    .to_owned();
                                requests.lock().unwrap().push(format!("{depth} {path}"));
                                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                                maximum_active.fetch_max(now, Ordering::SeqCst);
                                thread::sleep(Duration::from_millis(40));
                                let (status, body) = route(&path);
                                write_response(&mut stream, status, &body);
                                active.fetch_sub(1, Ordering::SeqCst);
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
            requests,
            maximum_active,
            handle: Some(handle),
        }
    }

    fn requested(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
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
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
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
    format!("<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\" xmlns:cal=\"urn:ietf:params:xml:ns:caldav\" xmlns:cs=\"http://calendarserver.org/ns/\">{body}</d:multistatus>")
}

fn principal(id: &str) -> String {
    format!("<d:href>{PRINCIPALS}/{id}/</d:href>")
}

fn filler(index: usize) -> String {
    format!("filler{index}@example.test")
}

/// The server's delegation list is deliberately in non-sorted order and holds
/// everything Mail2000 puts there: the system principal, the room domain, the
/// account itself, a colleague who never shared, one whose home errors, and a
/// colleague listed under both proxy kinds.
fn delegation_xml() -> String {
    let mut write_for = vec![
        principal(SHARED),
        principal("@system"),
        principal(UNSHARED),
        principal("@example.test"),
        principal(ME),
        principal(BROKEN),
    ];
    write_for.extend((0..FILLERS).map(|index| principal(&filler(index))));
    multistatus(&format!(
        "<d:response><d:href>{PRINCIPALS}/{ME}/</d:href><d:propstat><d:prop><cs:calendar-proxy-write-for>{}</cs:calendar-proxy-write-for><cs:calendar-proxy-read-for>{}</cs:calendar-proxy-read-for></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        write_for.join(""),
        principal(SHARED),
    ))
}

fn home_only(id: &str) -> String {
    format!("<d:response><d:href>{CALENDARS}/{id}/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>")
}

fn calendar(id: &str, name: &str, name_of_collection: &str, privileges: &str) -> String {
    format!("<d:response><d:href>{CALENDARS}/{id}/{name_of_collection}/</d:href><d:propstat><d:prop><d:displayname>{name}</d:displayname><d:resourcetype><d:collection/><cal:calendar/></d:resourcetype><d:current-user-privilege-set>{privileges}</d:current-user-privilege-set></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>")
}

const INBOX: &str = "<d:response><d:href>/cgi-bin/cal/caldav/calendars/teammate@example.test/inbox/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><cal:schedule-inbox/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>";

fn route(path: &str) -> (&'static str, String) {
    let tail = |prefix: &str| path.strip_prefix(prefix).map(|rest| rest.trim_matches('/'));
    if tail(&format!("{PRINCIPALS}/")) == Some(ME) {
        return ("207 Multi-Status", delegation_xml());
    }
    let Some(rest) = tail(&format!("{CALENDARS}/")) else {
        return ("500 Internal Server Error", String::new());
    };
    let (id, collection) = rest.split_once('/').unwrap_or((rest, ""));
    match (id, collection) {
        (SHARED, "") => (
            "207 Multi-Status",
            multistatus(&format!(
                "{}{}{}{}",
                home_only(SHARED),
                calendar(
                    SHARED,
                    "Ting的行事曆",
                    "default",
                    "<d:privilege><d:read/></d:privilege><d:privilege><d:write/></d:privilege>"
                ),
                calendar(
                    SHARED,
                    "只讀",
                    "team",
                    "<d:privilege><d:read/></d:privilege>"
                ),
                INBOX,
            )),
        ),
        (BROKEN, "") => ("500 Internal Server Error", "boom".to_owned()),
        (UNSHARED, "default") => ("404 Not Found", String::new()),
        (_, "") => ("207 Multi-Status", multistatus(&home_only(id))),
        _ => ("404 Not Found", String::new()),
    }
}

fn setup(server: &DavServer) -> TempDir {
    let dir = TempDir::new().unwrap();
    let mut cfg =
        Config::with_site_profile(ME.into(), "mail.example.test", Some("demo".into())).unwrap();
    cfg.caldav_url = format!("http://{}{CALENDARS}/{ME}/default/", server.address);
    cfg.carddav_url = format!("http://{}/card/self/", server.address);
    config::save(dir.path(), &cfg).unwrap();
    dir
}

fn contact(email: &str, vcard_n: &str) -> Item {
    let local = email.split('@').next().unwrap();
    Item {
        id: format!("contact-{local}"),
        kind: Kind::Contacts,
        collection: "book".into(),
        remote_id: format!("{local}.vcf"),
        etag: None,
        uid_validity: None,
        title: local.into(),
        subtitle: String::new(),
        date: None,
        end_date: None,
        text: String::new(),
        raw: format!(
            "BEGIN:VCARD\nVERSION:3.0\nUID:{local}\nFN:{local}\n{vcard_n}EMAIL:{email}\nEND:VCARD\n"
        ),
        flags: vec![],
        emails: vec![email.into()],
        phones: vec![],
        attachments: vec![],
    }
}

fn run(dir: &TempDir, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::new(env!("CARGO_BIN_EXE_m2k"))
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args(args)
        .env("M2K_PASSWORD", "test-only-password")
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db")
        .assert()
}

fn shared(dir: &TempDir) -> Value {
    let output = run(dir, &["meeting", "shared"])
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

#[test]
fn lists_only_colleagues_whose_calendar_is_actually_readable() {
    let server = DavServer::start();
    let dir = setup(&server);
    let mut store = Store::open(dir.path()).unwrap();
    store
        .ensure_account(&config::load(dir.path()).unwrap())
        .unwrap();
    store.upsert(&contact(SHARED, "N:李;婷;;;\n")).unwrap();
    store.upsert(&contact(UNSHARED, "")).unwrap();
    drop(store);

    let result = shared(&dir);

    let colleagues = result["colleagues"].as_array().unwrap();
    assert_eq!(
        colleagues.len(),
        1,
        "being delegated is not being shared with: {result}"
    );
    let ting = &colleagues[0];
    assert_eq!(ting["email"], SHARED);
    assert_eq!(ting["name"], "李 婷");
    let calendars = ting["calendars"].as_array().unwrap();
    assert_eq!(calendars.len(), 2, "schedule-inbox is not a calendar");
    assert_eq!(calendars[0]["display_name"], "Ting的行事曆");
    assert_eq!(calendars[0]["writable"], true);
    assert_eq!(calendars[1]["writable"], false, "read-only stays read-only");
    assert!(calendars[0]["url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/calendars/{SHARED}/default/")));

    // Self, @system and the rooms domain are never colleagues, so they must
    // not cost a request; each real colleague is probed once despite being
    // listed under both proxy kinds.
    let requested = server.requested();
    for skipped in ["@system", "@example.test", ME] {
        assert!(
            !requested
                .iter()
                .any(|request| request.contains(&format!("{CALENDARS}/{skipped}/"))),
            "{skipped} was probed: {requested:?}"
        );
    }
    let homes_of_shared = requested
        .iter()
        .filter(|request| request.contains(&format!("{CALENDARS}/{SHARED}/")))
        .count();
    assert_eq!(homes_of_shared, 1);
    assert!(requested
        .iter()
        .any(|request| request == &format!("1 {CALENDARS}/{SHARED}/")));
    assert!(requested
        .iter()
        .any(|request| request == &format!("0 {PRINCIPALS}/{ME}/")));
    assert_eq!(result["scanned"], FILLERS + 3);
}

#[test]
fn one_failing_home_becomes_a_warning_instead_of_failing_the_command() {
    let server = DavServer::start();
    let dir = setup(&server);

    let result = shared(&dir);

    assert_eq!(result["colleagues"].as_array().unwrap().len(), 1);
    let warnings = result["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{result}");
    let warning = warnings[0].as_str().unwrap();
    assert!(warning.starts_with(BROKEN), "{warning}");
    assert!(warning.contains("500"), "{warning}");
}

#[test]
fn homes_are_probed_concurrently_but_boundedly() {
    let server = DavServer::start();
    let dir = setup(&server);

    shared(&dir);

    let active = server.maximum_active.load(Ordering::SeqCst);
    assert!(active >= 2, "hundreds of homes cannot be probed serially");
    assert!(active <= 12, "at most 12 requests in flight, saw {active}");
}

#[test]
fn text_output_leads_with_the_count_and_lists_each_colleague() {
    let server = DavServer::start();
    let dir = setup(&server);

    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["meeting", "shared"])
        .env("M2K_PASSWORD", "test-only-password")
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    let mut lines = text.lines();
    assert_eq!(
        lines.next().unwrap(),
        "1 位同事分享行事曆給你（可查 free/busy）"
    );
    let line = lines.next().unwrap();
    assert!(line.starts_with(SHARED) && line.contains("Ting的行事曆（可寫入）"));
}

#[test]
fn unshared_colleague_stays_unknown_and_says_they_have_not_shared() {
    let server = DavServer::start();
    let dir = setup(&server);

    let output = run(
        &dir,
        &[
            "meeting",
            "free-busy",
            "--attendee",
            UNSHARED,
            "--start",
            "2026-10-07T10:00:00+08:00",
            "--end",
            "2026-10-07T11:00:00+08:00",
        ],
    )
    .success()
    .get_output()
    .stdout
    .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(result[0]["state"], "unknown");
    assert_eq!(
        result[0]["free"], false,
        "no access must never read as free"
    );
    let reason = result[0]["reason"].as_str().unwrap();
    assert!(reason.contains("尚未分享行事曆給你"), "{reason}");
    assert!(reason.contains("m2k meeting shared"), "{reason}");
}

#[test]
fn demo_mode_has_no_server_to_ask_and_says_so() {
    let dir = TempDir::new().unwrap();
    m2k::demo::seed(dir.path()).unwrap();

    let result = shared(&dir);

    assert!(result["colleagues"].as_array().unwrap().is_empty());
    assert_eq!(result["scanned"], 0);
    assert!(result["warnings"][0].as_str().unwrap().contains("demo"));
}

fn probed(server: &DavServer, id: &str) -> bool {
    server
        .requested()
        .iter()
        .any(|request| request == &format!("1 {CALENDARS}/{id}/"))
}

#[test]
fn later_runs_read_the_cache_and_only_refresh_asks_the_server() {
    // 完整檢查要逐一查數百位委派對象；分享很少變動，每次約會議都重查會讓人等上一分鐘。
    let server = DavServer::start();
    let dir = setup(&server);
    shared(&dir);
    let after_first = server.requested().len();

    let cached = shared(&dir);
    assert_eq!(
        server.requested().len(),
        after_first,
        "cache read hit the server"
    );
    assert_eq!(cached["from_cache"], true);
    assert_eq!(cached["colleagues"][0]["email"], SHARED);
    assert!(cached["colleagues"][0]["checked_at"].is_string());

    let refreshed: Value = serde_json::from_slice(
        &run(&dir, &["meeting", "shared", "--refresh"])
            .success()
            .get_output()
            .stdout,
    )
    .unwrap();
    assert!(server.requested().len() > after_first);
    assert_eq!(refreshed["from_cache"], false);
}

#[test]
fn branch_refresh_probes_the_branch_and_unlisted_people_and_keeps_the_rest_cached() {
    // 平常只約自己 BU 的人：只查 BU 能快好幾倍，但通訊錄快取常缺新進同事，不在通訊錄的人仍要查，
    // 否則剛分享的新同事永遠查不到；分支外的人沿用快取，不能因為只查 BU 就被刪掉。
    let server = DavServer::start();
    let dir = setup(&server);
    let inside = filler(0);
    let outside = filler(1);
    let collection = |path: &str| Collection {
        kind: Kind::Contacts,
        id: format!("book/{path}/"),
        name: format!("{}(/{path})", path.rsplit('/').next().unwrap()),
    };
    let member = |email: &str, path: &str| Item {
        collection: format!("book/{path}/"),
        ..contact(email, "")
    };
    let mut store = Store::open(dir.path()).unwrap();
    store
        .ensure_account(&config::load(dir.path()).unwrap())
        .unwrap();
    store
        .upsert_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: ["CO", "CO/BU", "CO/BU/TEAM", "CO/OTHER"]
                    .into_iter()
                    .map(collection)
                    .collect(),
                items: vec![member(&inside, "CO/BU/TEAM"), member(&outside, "CO/OTHER")],
            },
            chrono::Utc::now(),
        )
        .unwrap();
    drop(store);
    let entry = |email: &str| {
        serde_json::json!({"email": email, "name": null, "checked_at": "2026-01-01T00:00:00Z",
            "calendars": [{"url": format!("http://x/{email}/default/"), "display_name": null, "writable": true}]})
    };
    std::fs::write(
        dir.path().join("shared_calendars.json"),
        serde_json::to_vec(
            &serde_json::json!({"account": ME, "colleagues": [entry(&inside), entry(&outside)]}),
        )
        .unwrap(),
    )
    .unwrap();

    let result: Value = serde_json::from_slice(
        &run(
            &dir,
            &["meeting", "shared", "--refresh", "--branch", "/CO/BU"],
        )
        .success()
        .get_output()
        .stdout,
    )
    .unwrap();

    assert!(
        probed(&server, &inside),
        "branch member (in a sub-department) not probed"
    );
    assert!(
        probed(&server, SHARED),
        "colleague missing from the directory must still be probed"
    );
    assert!(
        !probed(&server, &outside),
        "outside the branch should not cost a request"
    );
    assert_eq!(result["branch"], "CO/BU");
    let emails = result["colleagues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|colleague| colleague["email"].as_str().unwrap())
        .collect::<Vec<_>>();
    // inside 已不再分享：重查後移除；outside 沒查，沿用快取。
    assert_eq!(emails, [outside.as_str(), SHARED]);
    assert_eq!(
        shared(&dir)["colleagues"].as_array().unwrap().len(),
        2,
        "merge was persisted"
    );
}

fn shared_json(dir: &TempDir, args: &[&str]) -> Value {
    serde_json::from_slice(&run(dir, args).success().get_output().stdout).unwrap()
}

fn emails_of(result: &Value) -> Vec<&str> {
    result["colleagues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|colleague| colleague["email"].as_str().unwrap())
        .collect()
}

fn cached_colleague(email: &str) -> Value {
    serde_json::json!({
        "email": email,
        "name": null,
        "checked_at": "2026-01-01T00:00:00Z",
        "calendars": [{
            "url": format!("http://x/{email}/default/"),
            "display_name": null,
            "writable": true
        }]
    })
}

fn write_cache(dir: &TempDir, colleagues: &[Value]) {
    std::fs::write(
        dir.path().join("shared_calendars.json"),
        serde_json::to_vec(&serde_json::json!({"account": ME, "colleagues": colleagues})).unwrap(),
    )
    .unwrap();
}

fn seed_directory(dir: &TempDir, members: &[(&str, &str)]) {
    let collection = |path: &str| Collection {
        kind: Kind::Contacts,
        id: format!("book/{path}/"),
        name: format!("{}(/{path})", path.rsplit('/').next().unwrap()),
    };
    let mut store = Store::open(dir.path()).unwrap();
    store
        .ensure_account(&config::load(dir.path()).unwrap())
        .unwrap();
    store
        .upsert_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: ["CO", "CO/BU", "CO/BU/TEAM", "CO/OTHER"]
                    .into_iter()
                    .map(collection)
                    .collect(),
                items: members
                    .iter()
                    .map(|(email, path)| Item {
                        collection: format!("book/{path}/"),
                        ..contact(email, "")
                    })
                    .collect(),
            },
            chrono::Utc::now(),
        )
        .unwrap();
}

#[test]
fn refresh_keeps_cached_colleague_when_the_probe_fails() {
    // 500 只是這次沒查到，不是對方停止分享。空的 home 仍要移除。
    let server = DavServer::start();
    let dir = setup(&server);
    let dropped = filler(0);
    write_cache(
        &dir,
        &[
            cached_colleague(BROKEN),
            cached_colleague(SHARED),
            cached_colleague(&dropped),
        ],
    );

    let refreshed = shared_json(&dir, &["meeting", "shared", "--refresh"]);

    assert_eq!(emails_of(&refreshed), [BROKEN, SHARED]);
    let colleagues = refreshed["colleagues"].as_array().unwrap();
    let broken = colleagues
        .iter()
        .find(|colleague| colleague["email"] == BROKEN)
        .unwrap();
    assert_eq!(broken["checked_at"], "2026-01-01T00:00:00Z");
    let shared_at = colleagues
        .iter()
        .find(|colleague| colleague["email"] == SHARED)
        .unwrap()["checked_at"]
        .as_str()
        .unwrap();
    assert_ne!(shared_at, "2026-01-01T00:00:00Z");
    assert!(refreshed["warnings"][0]
        .as_str()
        .unwrap()
        .starts_with(BROKEN));

    let cached = shared(&dir);
    assert_eq!(cached["from_cache"], true);
    assert_eq!(emails_of(&cached), [BROKEN, SHARED]);
}

#[test]
fn branch_refresh_keeps_cached_colleague_when_the_probe_fails() {
    let server = DavServer::start();
    let dir = setup(&server);
    let outside = filler(1);
    seed_directory(
        &dir,
        &[(BROKEN, "CO/BU/TEAM"), (outside.as_str(), "CO/OTHER")],
    );
    write_cache(
        &dir,
        &[cached_colleague(BROKEN), cached_colleague(&outside)],
    );

    let refreshed = shared_json(
        &dir,
        &["meeting", "shared", "--refresh", "--branch", "/CO/BU"],
    );

    assert_eq!(refreshed["branch"], "CO/BU");
    assert!(probed(&server, BROKEN), "in-scope colleague was not probed");
    assert!(!probed(&server, &outside), "outside colleague was probed");
    let emails = emails_of(&refreshed);
    assert!(emails.contains(&BROKEN), "{emails:?}");
    assert!(emails.contains(&outside.as_str()), "{emails:?}");
    let broken = refreshed["colleagues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|colleague| colleague["email"] == BROKEN)
        .unwrap();
    assert_eq!(broken["checked_at"], "2026-01-01T00:00:00Z");

    let cached = shared(&dir);
    assert_eq!(cached["from_cache"], true);
    assert!(emails_of(&cached).contains(&BROKEN));
    assert!(emails_of(&cached).contains(&outside.as_str()));
}

#[test]
fn branch_without_refresh_scans_when_a_cache_exists_and_stays_complete() {
    let server = DavServer::start();
    let dir = setup(&server);
    let inside = filler(0);
    seed_directory(
        &dir,
        &[(SHARED, "CO/OTHER"), (inside.as_str(), "CO/BU/TEAM")],
    );
    shared(&dir);
    let after_full = server.requested().len();

    let branched = shared_json(&dir, &["meeting", "shared", "--branch", "/CO/BU"]);

    let requests = server.requested();
    let new = &requests[after_full..];
    assert!(
        new.iter()
            .any(|request| request == &format!("1 {CALENDARS}/{inside}/")),
        "branch was ignored: {new:?}"
    );
    assert!(
        !new.iter()
            .any(|request| request.contains(&format!("{CALENDARS}/{SHARED}/"))),
        "outside colleague was probed: {new:?}"
    );
    assert_eq!(branched["from_cache"], false);
    assert_eq!(branched["branch"], "CO/BU");
    assert!(emails_of(&branched).contains(&SHARED), "{branched}");

    let after_branch = requests.len();
    let again = shared(&dir);
    assert_eq!(
        server.requested().len(),
        after_branch,
        "merged cache was not reused"
    );
    assert_eq!(again["from_cache"], true);
    assert!(again["branch"].is_null());
    assert!(emails_of(&again).contains(&SHARED));
}

#[test]
fn branch_scan_without_a_complete_cache_is_not_the_default_list() {
    let server = DavServer::start();
    let dir = setup(&server);
    let inside = filler(0);
    seed_directory(
        &dir,
        &[(SHARED, "CO/OTHER"), (inside.as_str(), "CO/BU/TEAM")],
    );

    let branched = shared_json(
        &dir,
        &["meeting", "shared", "--refresh", "--branch", "/CO/BU"],
    );

    assert_eq!(branched["branch"], "CO/BU");
    assert_eq!(branched["from_cache"], false);
    assert!(
        !emails_of(&branched).contains(&SHARED),
        "colleague outside the branch was treated as scanned: {branched}"
    );
    let cache: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("shared_calendars.json")).unwrap())
            .unwrap();
    assert_eq!(cache["complete"], false);

    let after = server.requested().len();
    let again = shared(&dir);
    assert!(
        server.requested().len() > after,
        "partial cache answered the unscoped query"
    );
    assert_eq!(again["from_cache"], false);
    assert!(again["branch"].is_null());
    assert!(
        emails_of(&again).contains(&SHARED),
        "default list lost the colleague outside the last branch: {again}"
    );
}

fn route_password_revoked_after_delegation(path: &str) -> (&'static str, String) {
    if path.trim_end_matches('/') == format!("{PRINCIPALS}/{ME}") {
        return ("207 Multi-Status", delegation_xml());
    }
    ("401 Unauthorized", String::new())
}

#[test]
fn a_rejected_login_mid_scan_stops_every_worker_instead_of_probing_everyone() {
    let server = DavServer::start_with(route_password_revoked_after_delegation);
    let dir = setup(&server);

    let output = run(&dir, &["meeting", "shared", "--refresh"])
        .failure()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["error"]["code"], "auth_failed", "{json}");

    let homes = server
        .requested()
        .iter()
        .filter(|request| request.contains(CALENDARS))
        .count();
    let candidates = FILLERS + 3;
    assert!(
        homes < candidates,
        "every one of {candidates} colleagues was probed with a rejected password ({homes} requests)"
    );
    assert!(
        homes <= 2 * 12,
        "{homes} home requests after the first rejection"
    );
}
