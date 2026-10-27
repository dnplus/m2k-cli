use assert_cmd::Command;
use m2k::{config, config::Config, store::Store};
use serde_json::Value;
use std::{
    ffi::OsStr,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tempfile::{tempdir, TempDir};

const PASSWORD: &str = "test-only";
const BOOK_UID: &str = "workflow-book-uid";

struct ResponseSpec {
    status: &'static str,
    body: String,
    headers: Vec<(&'static str, String)>,
}

fn response(status: &'static str, body: impl Into<String>) -> ResponseSpec {
    ResponseSpec {
        status,
        body: body.into(),
        headers: vec![("Content-Type", "application/xml; charset=utf-8".to_owned())],
    }
}

fn put_response(status: &'static str, etag: &str) -> ResponseSpec {
    ResponseSpec {
        status,
        body: String::new(),
        headers: vec![("ETag", etag.to_owned())],
    }
}

fn spawn_server(responses: Vec<ResponseSpec>) -> (SocketAddr, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback mock");
    let address = listener.local_addr().expect("mock address");
    listener
        .set_nonblocking(true)
        .expect("set nonblocking listener");
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut requests = Vec::new();
        let mut index = 0;
        while index < responses.len() && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).expect("set blocking stream");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .expect("set read timeout");
                    let request = read_request(&mut stream);
                    if !request.contains("\r\n\r\n") {
                        continue;
                    }
                    write_response(&mut stream, &responses[index]);
                    requests.push(request);
                    index += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        requests
    });
    (address, handle)
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut one = [0_u8; 1];
    loop {
        match stream.read(&mut one) {
            Ok(0) => break,
            Ok(_) => {
                bytes.push(one[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let headers = String::from_utf8_lossy(&bytes).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; content_length];
    let _ = stream.read_exact(&mut body);
    let mut request = headers;
    request.push_str(&String::from_utf8_lossy(&body));
    request
}

fn write_response(stream: &mut TcpStream, spec: &ResponseSpec) {
    let mut output = format!(
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        spec.status,
        spec.body.len()
    );
    for (name, value) in &spec.headers {
        output.push_str(name);
        output.push_str(": ");
        output.push_str(value);
        output.push_str("\r\n");
    }
    output.push_str("\r\n");
    output.push_str(&spec.body);
    stream
        .write_all(output.as_bytes())
        .expect("write mock response");
}

fn command(data_dir: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_m2k"));
    command
        .args(["--json", "--data-dir"])
        .arg(data_dir.path())
        .env("M2K_PASSWORD", PASSWORD)
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db");
    command
}

fn run_json<I, S>(data_dir: &TempDir, args: I) -> Value
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = command(data_dir)
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("valid CLI JSON")
}

fn setup_data_dir(address: SocketAddr, caldav_path: &str, carddav_path: &str) -> TempDir {
    let data_dir = tempdir().expect("data tempdir");
    let origin = format!("http://{address}");
    let mut config =
        Config::new("sample@example.test".to_owned(), "mail.example.test").expect("sample config");
    config.caldav_url = format!("{origin}{caldav_path}");
    config.carddav_url = format!("{origin}{carddav_path}");
    config.validate().expect("loopback DAV config");
    config::save(data_dir.path(), &config).expect("save config");
    let mut store = Store::open(data_dir.path()).expect("open store");
    store.ensure_account(&config).expect("ensure identity");
    data_dir
}

fn origin(address: SocketAddr) -> String {
    format!("http://{address}")
}

fn multistatus(responses: impl IntoIterator<Item = String>) -> String {
    format!(
        "<?xml version=\"1.0\"?><D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\">{}</D:multistatus>",
        responses.into_iter().collect::<String>()
    )
}

fn calendar_response(href: &str, etag: &str, data: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:getetag>&quot;{etag}&quot;</D:getetag><C:calendar-data><![CDATA[{data}]]></C:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn calendar_collection(href: &str, name: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:displayname>{name}</D:displayname><D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn contact_collection(href: &str, name: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:displayname>{name}</D:displayname><D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn contact_response(href: &str, data: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:getetag>&quot;contact-1&quot;</D:getetag><CR:address-data><![CDATA[{data}]]></CR:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn report_request(request: &str) -> bool {
    request.starts_with("REPORT ")
}

fn has_header(request: &str, name: &str, value: &str) -> bool {
    request.lines().any(|line| {
        let Some((current_name, current_value)) = line.split_once(':') else {
            return false;
        };
        current_name.eq_ignore_ascii_case(name) && current_value.trim() == value
    })
}

fn has_header_prefix(request: &str, name: &str, prefix: &str) -> bool {
    request.lines().any(|line| {
        let Some((current_name, current_value)) = line.split_once(':') else {
            return false;
        };
        current_name.eq_ignore_ascii_case(name) && current_value.trim().starts_with(prefix)
    })
}

#[test]
fn meeting_free_busy_keeps_expanded_recurrence_busy_and_handles_all_day_duration() {
    let recurrence = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:recurrence-1\r\nDTSTART:20261006T020000Z\r\nDTEND:20261006T030000Z\r\nRECURRENCE-ID:20261006T020000Z\r\nSUMMARY:第一個展開事件\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:recurrence-1\r\nDTSTART:20261006T040000Z\r\nDTEND:20261006T050000Z\r\nRECURRENCE-ID:20261006T040000Z\r\nSUMMARY:第二個展開事件\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let all_day = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:all-day-1\r\nDTSTART;VALUE=DATE:20261006\r\nDTEND;VALUE=DATE:20261007\r\nSUMMARY:全天事件\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let duration = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:duration-1\r\nDTSTART:20261006T020000Z\r\nDURATION:PT2H\r\nSUMMARY:持續時間事件\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let discovery = multistatus([calendar_collection("/cal/target/", "目標行事曆")]);
    let responses = vec![
        response("207 Multi-Status", discovery.clone()),
        response(
            "207 Multi-Status",
            multistatus([calendar_response(
                "/cal/target/recurrence.ics",
                "r1",
                recurrence,
            )]),
        ),
        response("207 Multi-Status", discovery.clone()),
        response(
            "207 Multi-Status",
            multistatus([calendar_response("/cal/target/all-day.ics", "a1", all_day)]),
        ),
        response("207 Multi-Status", discovery),
        response(
            "207 Multi-Status",
            multistatus([calendar_response(
                "/cal/target/duration.ics",
                "d1",
                duration,
            )]),
        ),
    ];
    let (address, handle) = spawn_server(responses);
    let data_dir = setup_data_dir(address, "/cal/self/", "/card/principal");
    let target = format!("{}/cal/target/", origin(address));
    let common = [
        "meeting".to_owned(),
        "free-busy".to_owned(),
        "--calendar".to_owned(),
        target,
        "--start".to_owned(),
        "2026-10-06T00:00:00+00:00".to_owned(),
        "--end".to_owned(),
        "2026-10-06T06:00:00+00:00".to_owned(),
    ];

    let recurrence_output = run_json(&data_dir, common.clone());
    let recurrence_availability = &recurrence_output[0];
    assert_eq!(recurrence_availability["state"], "busy");
    assert_eq!(recurrence_availability["free"], false);
    assert_ne!(recurrence_availability["state"], "unknown");
    assert!(recurrence_availability["events"]
        .as_array()
        .is_some_and(|events| events.len() >= 2));
    assert!(recurrence_availability["events"]
        .as_array()
        .expect("expanded events")
        .iter()
        .all(|event| event["recurring"] == false));

    let all_day_output = run_json(&data_dir, common.clone());
    let all_day_availability = &all_day_output[0];
    assert!(matches!(
        all_day_availability["state"].as_str(),
        Some("busy") | Some("unknown")
    ));
    assert_eq!(all_day_availability["free"], false);

    let duration_output = run_json(&data_dir, common);
    let duration_availability = &duration_output[0];
    assert!(matches!(
        duration_availability["state"].as_str(),
        Some("busy") | Some("unknown")
    ));
    assert_eq!(duration_availability["free"], false);

    let requests = handle.join().expect("join free-busy mock");
    assert_eq!(requests.len(), 6);
    for pair in requests.chunks(2) {
        assert!(pair[0].starts_with("PROPFIND /cal/target/ HTTP/1.1"));
        assert!(pair[1].starts_with("REPORT /cal/target/ HTTP/1.1"));
        assert!(report_request(&pair[1]));
        assert!(pair[1].contains("calendar-query"));
        assert!(pair[1].contains("<C:expand start="));
    }
    assert!(requests
        .iter()
        .all(|request| has_header_prefix(request, "Authorization", "Basic ")));
}

fn booking_ics() -> &'static str {
    "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//m2k//m2k test//ZH-TW\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:workflow-book-uid\r\nDTSTAMP:20261005T000000Z\r\nDTSTART:20261006T020000Z\r\nDTEND:20261006T030000Z\r\nSUMMARY:m2k 核心情境\r\nORGANIZER;CN=Sample:MAILTO:sample@example.test\r\nLOCATION:測試會議室\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
}

#[test]
fn failed_calendar_sync_returns_one_partial_json_and_finished_progress() {
    let (address, handle) = spawn_server(vec![response("403 Forbidden", "forbidden")]);
    let data_dir = setup_data_dir(address, "/cal/self/", "/card/principal");
    let assertion = command(&data_dir)
        .args(["sync", "--kind", "calendar"])
        .assert()
        .failure();
    let output: Value = serde_json::from_slice(&assertion.get_output().stdout).unwrap();
    assert!(assertion.get_output().stderr.is_empty());
    assert_eq!(output["state"], "partial");
    assert_eq!(output["error"]["code"], "partial_sync");
    assert_eq!(output["error"]["retryable"], false);
    assert_eq!(output["results"][0]["kind"], "calendar");
    assert!(output["results"][0]["error"]
        .as_str()
        .unwrap()
        .contains("403"));
    let status = run_json(&data_dir, ["status"]);
    assert_eq!(status["progress"]["state"], "failed");
    assert_eq!(status["progress"]["phase"], "finished");
    assert_eq!(handle.join().unwrap().len(), 1);
}

#[test]
#[cfg(unix)]
fn watch_partial_failure_uses_the_sync_error_contract_per_line() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command as ProcessCommand, Stdio};
    let (address, handle) = spawn_server(vec![response("403 Forbidden", "forbidden")]);
    let data_dir = setup_data_dir(address, "/cal/self/", "/card/principal");
    let mut child = ProcessCommand::new(env!("CARGO_BIN_EXE_m2k"))
        .args(["--json", "--data-dir"])
        .arg(data_dir.path())
        .args(["watch", "--kind", "calendar"])
        .env("M2K_PASSWORD", PASSWORD)
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let _ = tx.send(line);
    });
    let line = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(line) => line,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("watch output timeout: {error}");
        }
    };
    ProcessCommand::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let exit = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("watch did not stop on SIGINT");
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(exit.success());
    reader.join().unwrap();
    let output: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(output["state"], "partial");
    assert_eq!(output["error"]["code"], "partial_sync");
    assert_eq!(output["error"]["retryable"], false);
    assert!(output["time"].is_string());
    assert!(output["results"][0]["error"]
        .as_str()
        .unwrap()
        .contains("403"));
    assert_eq!(handle.join().unwrap().len(), 1);
}

fn write_booking_file(data_dir: &TempDir) -> std::path::PathBuf {
    write_calendar_file(data_dir, "workflow-book.ics", booking_ics())
}

fn write_calendar_file(data_dir: &TempDir, name: &str, raw: &str) -> std::path::PathBuf {
    let path = data_dir.path().join(name);
    std::fs::write(&path, raw).expect("write calendar ICS");
    path
}

#[test]
fn meeting_book_rejects_expanded_instance_before_connecting() {
    let data_dir = tempdir().expect("data tempdir");
    let raw = booking_ics().replace(
        "DTSTART:20261006T020000Z",
        "DTSTART:20261006T020000Z\r\nRECURRENCE-ID:20261006T020000Z",
    );
    let file = write_calendar_file(&data_dir, "expanded.ics", &raw);
    command(&data_dir)
        .args(["meeting", "book", "--target", "self", "--file"])
        .arg(file)
        .assert()
        .failure()
        .stdout(predicates::str::contains("RECURRENCE-ID"));
}

#[test]
fn meeting_book_rejects_same_collection_before_connecting() {
    let (address, handle) = spawn_server(Vec::new());
    let data_dir = setup_data_dir(address, "/cal/self/", "/card/principal");
    let file = write_booking_file(&data_dir);
    let target = format!("{}/cal/self", origin(address));
    command(&data_dir)
        .args(["meeting", "book", "--file"])
        .arg(file)
        .args(["--url", &target])
        .assert()
        .failure()
        .stdout(predicates::str::contains("同一個 collection"));
    assert!(handle.join().expect("join unused mock").is_empty());
}

#[test]
fn meeting_book_dry_run_checks_both_calendars_without_put() {
    let self_discovery = multistatus([calendar_collection("/cal/self/", "個人行事曆")]);
    let room_discovery = multistatus([calendar_collection("/cal/room/", "測試會議室")]);
    let responses = vec![
        response("207 Multi-Status", self_discovery),
        response("207 Multi-Status", multistatus(Vec::<String>::new())),
        response("207 Multi-Status", room_discovery),
        response("207 Multi-Status", multistatus(Vec::<String>::new())),
    ];
    let (address, handle) = spawn_server(responses);
    let data_dir = setup_data_dir(address, "/cal/self/", "/card/principal");
    let file = write_booking_file(&data_dir);
    let room = format!("{}/cal/room/", origin(address));
    let output = run_json(
        &data_dir,
        vec![
            "meeting".to_owned(),
            "book".to_owned(),
            "--file".to_owned(),
            file.to_string_lossy().into_owned(),
            "--url".to_owned(),
            room,
            "--dry-run".to_owned(),
        ],
    );
    assert_eq!(output["state"], "dry-run");
    assert_eq!(output["dry_run"], true);
    assert_eq!(output["uid"], BOOK_UID);
    assert_eq!(output["target"], "both");
    let requests = handle.join().expect("join dry-run mock");
    assert_eq!(requests.len(), 4);
    assert!(requests[0].starts_with("PROPFIND /cal/self/ HTTP/1.1"));
    assert!(requests[1].starts_with("REPORT /cal/self/ HTTP/1.1"));
    assert!(requests[2].starts_with("PROPFIND /cal/room/ HTTP/1.1"));
    assert!(requests[3].starts_with("REPORT /cal/room/ HTTP/1.1"));
    assert!(requests[1].contains("calendar-query"));
    assert!(requests[3].contains("calendar-query"));
    assert!(requests.iter().all(|request| !request.starts_with("PUT ")));
}

#[test]
fn meeting_book_partial_keeps_source_uid_and_returns_room_only_remediation() {
    let self_discovery = multistatus([calendar_collection("/cal/self/", "個人行事曆")]);
    let room_discovery = multistatus([calendar_collection("/cal/room/", "測試會議室")]);
    let responses = vec![
        response("207 Multi-Status", self_discovery),
        response("207 Multi-Status", multistatus(Vec::<String>::new())),
        response("207 Multi-Status", room_discovery),
        response("207 Multi-Status", multistatus(Vec::<String>::new())),
        put_response("201 Created", "\"self-v1\""),
        response("500 Internal Server Error", "room write rejected"),
    ];
    let (address, handle) = spawn_server(responses);
    let data_dir = setup_data_dir(address, "/cal/self/", "/card/principal");
    let file = write_booking_file(&data_dir);
    let room = format!("{}/cal/room/", origin(address));
    let output = run_json(
        &data_dir,
        vec![
            "meeting".to_owned(),
            "book".to_owned(),
            "--file".to_owned(),
            file.to_string_lossy().into_owned(),
            "--url".to_owned(),
            room.clone(),
        ],
    );
    assert_eq!(output["state"], "partial");
    assert_eq!(output["uid"], BOOK_UID);
    assert_eq!(output["target"], "both");
    assert_eq!(output["completed"].as_array().map(Vec::len), Some(1));
    assert!(output["completed"][0]["target"]
        .as_str()
        .is_some_and(|target| target == "sample@example.test" || target.contains("self")));
    assert!(output["failed"]["calendar_url"]
        .as_str()
        .is_some_and(|url| url == room));
    let remediation = output["remediation"].as_str().expect("remediation");
    assert!(remediation.contains("--target"));
    assert!(remediation.contains("room"));

    let requests = handle.join().expect("join partial mock");
    let put_requests = requests
        .iter()
        .filter(|request| request.starts_with("PUT "))
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 6);
    assert_eq!(put_requests.len(), 2);
    assert_eq!(
        put_requests
            .iter()
            .filter(|request| request.starts_with("PUT /cal/self/"))
            .count(),
        1
    );
    assert_eq!(
        put_requests
            .iter()
            .filter(|request| request.starts_with("PUT /cal/room/"))
            .count(),
        1
    );
    assert!(put_requests.iter().all(|request| {
        !request.to_ascii_uppercase().contains("METHOD:")
            && request.contains(&format!("UID:{BOOK_UID}"))
    }));
}

#[test]
fn meeting_book_self_target_needs_no_room_and_writes_once() {
    let self_only_ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:workflow-self-uid\r\nDTSTART:20261006T020000Z\r\nDTEND:20261006T030000Z\r\nSUMMARY:個人行事曆核心情境\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let discovery = multistatus([calendar_collection("/cal/self/", "個人行事曆")]);
    let responses = vec![
        response("207 Multi-Status", discovery),
        response("207 Multi-Status", multistatus(Vec::<String>::new())),
        put_response("201 Created", "\"self-only-v1\""),
    ];
    let (address, handle) = spawn_server(responses);
    let data_dir = setup_data_dir(address, "/cal/self/", "/card/principal");
    let file = write_calendar_file(&data_dir, "workflow-self.ics", self_only_ics);
    let output = run_json(
        &data_dir,
        vec![
            "meeting".to_owned(),
            "book".to_owned(),
            "--target".to_owned(),
            "self".to_owned(),
            "--file".to_owned(),
            file.to_string_lossy().into_owned(),
        ],
    );
    assert_eq!(output["state"], "complete");
    assert_eq!(output["target"], "self");
    assert_eq!(output["uid"], "workflow-self-uid");
    assert!(output["room"].is_null());
    assert_eq!(output["completed"].as_array().map(Vec::len), Some(1));

    let requests = handle.join().expect("join self-only mock");
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("PROPFIND /cal/self/ HTTP/1.1"));
    assert!(requests[1].starts_with("REPORT /cal/self/ HTTP/1.1"));
    assert!(requests[2].starts_with("PUT /cal/self/workflow-self-uid.ics HTTP/1.1"));
    assert!(requests[2].contains("UID:workflow-self-uid"));
    assert!(!requests[2].to_ascii_uppercase().contains("METHOD:"));
    assert!(requests
        .iter()
        .all(|request| !request.contains("/cal/room/")));
}

#[test]
fn calendar_create_returns_remote_write_receipt_when_etag_refresh_fails() {
    let calendar = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:receipt-event-uid\r\nDTSTART:20261006T020000Z\r\nDTEND:20261006T030000Z\r\nSUMMARY:等待 reconciliation\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let discovery = multistatus([calendar_collection("/cal/self/", "個人行事曆")]);
    let responses = vec![
        response("207 Multi-Status", discovery),
        response("201 Created", String::new()),
        response("200 OK", calendar),
    ];
    let (address, handle) = spawn_server(responses);
    let data_dir = setup_data_dir(address, "/cal/self/", "/card/principal");
    let file = write_calendar_file(&data_dir, "receipt-event.ics", calendar);
    let assertion = command(&data_dir)
        .args([
            "calendar",
            "create",
            "--file",
            file.to_str().expect("calendar path"),
        ])
        .assert()
        .failure();
    let stdout = assertion.get_output().stdout.clone();
    let output: Value = serde_json::from_slice(&stdout).unwrap_or_else(|error| {
        panic!(
            "receipt JSON: {error}; stderr: {}",
            String::from_utf8_lossy(&assertion.get_output().stderr)
        )
    });
    assert_eq!(output["state"], "partial");
    assert_eq!(output["remote_write"]["remote_accepted"], true);
    assert_eq!(output["remote_write"]["reconcile_required"], true);
    assert_eq!(output["remote_write"]["uid"], "receipt-event-uid");
    assert!(output["remote_write"]["resource_url"]
        .as_str()
        .is_some_and(|url| url.ends_with("/cal/self/receipt-event-uid.ics")));

    let requests = handle.join().expect("join receipt mock");
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("PROPFIND /cal/self/ HTTP/1.1"));
    assert!(requests[1].starts_with("PUT /cal/self/receipt-event-uid.ics HTTP/1.1"));
    assert!(requests[2].starts_with("GET /cal/self/receipt-event-uid.ics HTTP/1.1"));
    assert!(!requests[1].to_ascii_uppercase().contains("METHOD:"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("PUT "))
            .count(),
        1
    );
}

#[test]
fn directory_self_follows_principal_home_and_queries_only_root() {
    let principal = multistatus(["<D:response><D:href>/card/principal</D:href><D:propstat><D:prop><CR:addressbook-home-set><D:href>/card/</D:href></CR:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>".to_owned()]);
    let home = multistatus([
        contact_collection("/card/root/", "ACME(/ACME)"),
        contact_collection("/card/root/child/", "部門(/ACME/部門)"),
    ]);
    let contact = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:sample-self\r\nFN:Sample Example\r\nEMAIL:sample@example.test\r\nORG:Example Corp\r\nEND:VCARD\r\n";
    let responses = vec![
        response("207 Multi-Status", principal),
        response("207 Multi-Status", home),
        response(
            "207 Multi-Status",
            multistatus([contact_response("/card/root/sample.vcf", contact)]),
        ),
    ];
    let (address, handle) = spawn_server(responses);
    let data_dir = setup_data_dir(address, "/cal/self/", "/card/principal");
    let output = run_json(&data_dir, ["directory", "self", "--online"]);
    assert_eq!(output["emails"][0], "sample@example.test");
    assert_eq!(output["name"], "Sample Example");
    assert_eq!(output["memberships"][0]["path"], "ACME");

    let requests = handle.join().expect("join directory mock");
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("PROPFIND /card/principal HTTP/1.1"));
    assert!(has_header(&requests[0], "Depth", "0"));
    assert!(requests[1].starts_with("PROPFIND /card/ HTTP/1.1"));
    assert!(has_header(&requests[1], "Depth", "1"));
    assert!(requests[2].starts_with("REPORT /card/root/ HTTP/1.1"));
    assert!(requests[2].contains("addressbook-query"));
    assert!(requests[2].contains("sample@example.test"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.contains("addressbook-query"))
            .count(),
        1
    );
    assert!(requests
        .iter()
        .all(|request| !request.starts_with("REPORT /card/root/child/")));
}

#[test]
fn field_based_calendar_create_keeps_the_same_remote_write_receipt() {
    let discovery = multistatus([calendar_collection("/cal/self/", "個人行事曆")]);
    let responses = vec![
        response("207 Multi-Status", discovery),
        response("201 Created", String::new()),
        response("200 OK", "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"),
    ];
    let (address, handle) = spawn_server(responses);
    let data_dir = setup_data_dir(address, "/cal/self/", "/card/principal");
    let assertion = command(&data_dir)
        .args([
            "calendar",
            "create",
            "--title",
            "欄位 receipt",
            "--start",
            "2026-11-03 14:00",
            "--end",
            "15:00",
        ])
        .assert()
        .failure();
    let output: Value = serde_json::from_slice(&assertion.get_output().stdout).expect("receipt");
    assert_eq!(output["state"], "partial");
    assert_eq!(output["remote_write"]["remote_accepted"], true);
    assert_eq!(output["remote_write"]["reconcile_required"], true);
    let uid = output["remote_write"]["uid"]
        .as_str()
        .expect("generated uid");
    assert!(output["remote_write"]["resource_url"]
        .as_str()
        .is_some_and(|url| url.ends_with(&format!("/cal/self/{}.ics", uid.replace('@', "%40")))));

    let requests = handle.join().expect("join receipt mock");
    let puts = requests
        .iter()
        .filter(|request| request.starts_with("PUT "))
        .collect::<Vec<_>>();
    assert_eq!(puts.len(), 1, "no automatic retry");
    assert!(has_header(puts[0], "If-None-Match", "*"));
    assert!(puts[0].contains(&format!("UID:{uid}")) && puts[0].contains("SUMMARY:欄位 receipt"));
    assert!(requests[2].starts_with("GET "));
}
