use chrono::{TimeZone, Utc};
use m2k::{
    config::Config,
    model::{Collection, Kind},
    provider::dav,
};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    thread::{self, JoinHandle},
};

struct ResponseSpec {
    status: &'static str,
    body: String,
    headers: Vec<(&'static str, &'static str)>,
}

fn response(status: &'static str, body: impl Into<String>) -> ResponseSpec {
    ResponseSpec {
        status,
        body: body.into(),
        headers: vec![("Content-Type", "application/xml; charset=utf-8")],
    }
}

fn spawn_server(responses: Vec<ResponseSpec>) -> (SocketAddr, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
    let address = listener.local_addr().expect("mock address");
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for spec in responses {
            let (mut stream, _) = listener.accept().expect("accept request");
            requests.push(read_request(&mut stream));
            let mut response = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                spec.status,
                spec.body.len()
            );
            for (name, value) in spec.headers {
                response.push_str(name);
                response.push_str(": ");
                response.push_str(value);
                response.push_str("\r\n");
            }
            response.push_str("\r\n");
            response.push_str(&spec.body);
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
        requests
    });
    (address, handle)
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut one = [0_u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut one).expect("read request headers");
        bytes.push(one[0]);
    }
    let headers = String::from_utf8_lossy(&bytes).to_string();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; content_length];
    stream.read_exact(&mut body).expect("read request body");
    let mut request = headers;
    request.push_str(&String::from_utf8_lossy(&body));
    request
}

fn request_contains(request: &str, fragment: &str) -> bool {
    request
        .to_ascii_lowercase()
        .contains(&fragment.to_ascii_lowercase())
}

fn config(address: SocketAddr, path: &str) -> Config {
    let base = format!("http://{address}");
    let mut config = Config::new("user@example.test".to_owned(), &base).expect("config");
    config.caldav_url = format!("{base}{path}");
    config.carddav_url = format!("{base}/card/");
    config.validate().expect("valid local config");
    config
}

fn calendar_collection(href: &str, name: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:displayname>{name}</D:displayname><D:resourcetype><D:collection/><C:calendar/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn addressbook_collection(href: &str, name: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:displayname>{name}</D:displayname><D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn multistatus(responses: impl IntoIterator<Item = String>) -> String {
    format!(
        "<?xml version=\"1.0\"?><D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\">{}</D:multistatus>",
        responses.into_iter().collect::<String>()
    )
}

fn event_response(href: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:getetag>&quot;event-1&quot;</D:getetag><C:calendar-data><![CDATA[BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:event-1\r\nDTSTART:20261006T100000Z\r\nDTEND:20261006T110000Z\r\nSUMMARY:同步測試\r\nLOCATION:北區\r\nDESCRIPTION:原始內容\r\nRRULE:FREQ=DAILY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n]]></C:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

#[test]
fn discover_multiple_calendars_and_sync_preserves_raw_event() {
    let propfind = multistatus([
        calendar_collection("/cal/team/", "同事行事曆"),
        calendar_collection("/cal/room/", "會議室行事曆"),
    ]);
    let team_report = multistatus([event_response("/cal/team/event-1.ics")]);
    let room_report = multistatus(Vec::<String>::new());
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", propfind),
        response("207 Multi-Status", team_report),
        response("207 Multi-Status", room_report),
    ]);
    let config = config(address, "/cal/");
    let snapshot =
        dav::sync_collection(&config, "password", Kind::Calendar, &config.caldav_url, &[])
            .expect("sync calendars");
    assert_eq!(snapshot.collections.len(), 2);
    assert_eq!(snapshot.items.len(), 1);
    assert_eq!(snapshot.items[0].title, "同步測試");
    assert!(snapshot.items[0].raw.contains("RRULE:FREQ=DAILY"));
    let requests = handle.join().expect("join mock server");
    assert!(requests[0].starts_with("PROPFIND /cal/ HTTP/1.1"));
    assert!(request_contains(&requests[0], "Depth: 1"));
    assert!(requests[1].starts_with("REPORT /cal/team/ HTTP/1.1"));
    assert!(request_contains(&requests[1], "calendar-query"));
    assert!(request_contains(&requests[0], "Authorization: Basic "));
}

#[test]
fn conditional_create_update_and_delete_require_etag() {
    let discovery = multistatus([calendar_collection("/cal/", "個人")]);
    let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:task-1\r\nDTSTART:20261006T100000Z\r\nSUMMARY:建立\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let mut created = response("201 Created", "");
    created.headers = vec![("ETag", "\"v1\"")];
    let mut updated = response("204 No Content", "");
    updated.headers = vec![("ETag", "\"v2\"")];
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", discovery.clone()),
        created,
        response("207 Multi-Status", discovery.clone()),
        updated,
        response("207 Multi-Status", discovery),
        response("204 No Content", ""),
    ]);
    let config = config(address, "/cal/");
    let created_item =
        dav::put(&config, "password", Kind::Calendar, "/cal/", raw, None).expect("create calendar");
    assert_eq!(created_item.etag.as_deref(), Some("\"v1\""));
    let updated_item = dav::put(
        &config,
        "password",
        Kind::Calendar,
        "/cal/",
        &raw.replace("建立", "更新"),
        Some(&created_item),
    )
    .expect("update calendar");
    assert_eq!(updated_item.etag.as_deref(), Some("\"v2\""));
    dav::delete(&config, "password", &updated_item).expect("delete calendar");
    let requests = handle.join().expect("join mock server");
    assert!(request_contains(&requests[1], "If-None-Match: *"));
    assert!(request_contains(&requests[3], "If-Match: \"v1\""));
    assert!(requests[5].starts_with("DELETE /cal/task-1.ics HTTP/1.1"));
    assert!(request_contains(&requests[5], "If-Match: \"v2\""));
}

#[test]
fn put_refreshes_missing_etag_with_get() {
    let discovery = multistatus([calendar_collection("/cal/", "個人")]);
    let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:refresh-1\r\nDTSTART:20261006T100000Z\r\nSUMMARY:重新取得\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let put = response("204 No Content", "");
    let mut get = response("200 OK", raw);
    get.headers = vec![("ETag", "\"refreshed\"")];
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", discovery), put, get]);
    let config = config(address, "/cal/");
    let item = dav::put(&config, "password", Kind::Calendar, "/cal/", raw, None)
        .expect("PUT should refresh the missing ETag");
    assert_eq!(item.etag.as_deref(), Some("\"refreshed\""));
    let requests = handle.join().expect("join mock server");
    assert!(request_contains(&requests[1], "If-None-Match: *"));
    assert!(requests[2].starts_with("GET /cal/refresh-1.ics HTTP/1.1"));
}

#[test]
fn put_reports_remote_acceptance_when_etag_reconciliation_fails() {
    let discovery = multistatus([calendar_collection("/cal/", "個人")]);
    let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:pending-1\r\nDTSTART:20261006T100000Z\r\nSUMMARY:待重新同步\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let put = response("204 No Content", "");
    let get = response("200 OK", raw);
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", discovery), put, get]);
    let config = config(address, "/cal/");
    let error = dav::put(&config, "password", Kind::Calendar, "/cal/", raw, None)
        .expect_err("missing ETag must report reconciliation state");
    let receipt = error
        .downcast_ref::<dav::RemoteWritePendingReconcile>()
        .expect("typed remote mutation receipt");
    assert_eq!(receipt.kind, Kind::Calendar);
    assert_eq!(receipt.uid.as_deref(), Some("pending-1"));
    assert_eq!(receipt.collection, config.caldav_url);
    assert!(receipt.resource_url.ends_with("/cal/pending-1.ics"));
    assert_eq!(receipt.operation, "PUT");
    assert_eq!(receipt.status, "204");
    assert!(receipt.remote_accepted);
    assert!(receipt.reconcile_required);
    assert!(receipt.reason.contains("未提供 ETag"));
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 3);
    assert!(requests[2].starts_with("GET /cal/pending-1.ics HTTP/1.1"));
}

#[test]
fn put_reports_remote_acceptance_when_location_is_invalid() {
    let discovery = multistatus([calendar_collection("/cal/", "個人")]);
    let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:location-1\r\nDTSTART:20261006T100000Z\r\nSUMMARY:位置錯誤\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let mut put = response("201 Created", "");
    put.headers = vec![
        ("Location", "http://localhost:9/evil.ics"),
        ("ETag", "\"v1\""),
    ];
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", discovery), put]);
    let config = config(address, "/cal/");
    let error = dav::put(&config, "password", Kind::Calendar, "/cal/", raw, None)
        .expect_err("invalid Location must report reconciliation state");
    let receipt = error
        .downcast_ref::<dav::RemoteWritePendingReconcile>()
        .expect("typed remote mutation receipt");
    assert!(receipt.remote_accepted);
    assert!(receipt.reconcile_required);
    assert!(receipt.resource_url.ends_with("/cal/location-1.ics"));
    assert_eq!(receipt.operation, "PUT");
    assert_eq!(receipt.status, "201");
    assert!(receipt.reason.contains("Location"));
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 2);
}

#[test]
fn put_reports_remote_acceptance_when_written_resource_cannot_be_parsed() {
    let discovery = multistatus([calendar_collection("/cal/", "個人")]);
    let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:invalid-date-1\r\nDTSTART:invalid\r\nSUMMARY:日期錯誤\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let mut put = response("204 No Content", "");
    put.headers = vec![("ETag", "\"v1\"")];
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", discovery), put]);
    let config = config(address, "/cal/");
    let error = dav::put(&config, "password", Kind::Calendar, "/cal/", raw, None)
        .expect_err("unparseable resource must report reconciliation state");
    let receipt = error
        .downcast_ref::<dav::RemoteWritePendingReconcile>()
        .expect("typed remote mutation receipt");
    assert!(receipt.remote_accepted);
    assert!(receipt.reconcile_required);
    assert!(receipt.resource_url.ends_with("/cal/invalid-date-1.ics"));
    assert_eq!(receipt.operation, "PUT");
    assert_eq!(receipt.status, "204");
    assert!(receipt.reason.contains("解析"));
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 2);
}

#[test]
fn cross_origin_href_is_rejected_before_follow_up_request() {
    let malicious = multistatus([calendar_collection("http://localhost:9/evil/", "外部")]);
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", malicious)]);
    let config = config(address, "/cal/");
    let error = dav::sync_collection(&config, "password", Kind::Calendar, &config.caldav_url, &[])
        .expect_err("cross-origin href must fail");
    assert!(error.to_string().contains("跨 origin"));
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 1);
}

#[test]
fn missing_collection_is_rejected_without_empty_snapshot() {
    let malformed = multistatus([
        "<D:response><D:href>/cal/</D:href><D:propstat><D:prop/><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response>".to_owned(),
    ]);
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", malformed)]);
    let config = config(address, "/cal/");
    let error = dav::sync_collection(&config, "password", Kind::Calendar, &config.caldav_url, &[])
        .expect_err("sync without usable collection must fail");
    assert!(error
        .to_string()
        .contains("找不到可用的 calendar DAV collection"));
    let _ = handle.join().expect("join mock server");
}

#[test]
fn optional_404_propstat_is_ignored_when_200_properties_exist() {
    let body = multistatus(["<D:response><D:href>/cal/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><C:calendar/></D:resourcetype><C:calendar-home-set><D:href>/cal/sibling/</D:href></C:calendar-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat><D:propstat><D:prop><D:displayname/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response>".to_owned()]);
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", body)]);
    let config = config(address, "/cal/");
    let collections = dav::discover(&config, "password", Kind::Calendar, &config.caldav_url)
        .expect("200 properties should be sufficient");
    assert_eq!(collections.len(), 1);
    assert_eq!(collections[0].id, config.caldav_url);
    let _ = handle.join().expect("join mock server");
}

#[test]
fn calendar_put_rejects_method_and_mixed_uids_before_http() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind address");
    let address = listener.local_addr().expect("address");
    drop(listener);
    let config = config(address, "/cal/");
    let method = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:meeting-1\r\nDTSTART:20261006T100000Z\r\nSUMMARY:邀請\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let method_error = dav::put(&config, "password", Kind::Calendar, "/cal/", method, None)
        .expect_err("METHOD calendar must use meeting flow");
    assert!(method_error.to_string().contains("含 METHOD"));
    let mixed = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:one\r\nDTSTART:20261006T100000Z\r\nSUMMARY:一\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:two\r\nDTSTART:20261006T110000Z\r\nSUMMARY:二\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let mixed_error = dav::put(&config, "password", Kind::Calendar, "/cal/", mixed, None)
        .expect_err("different UID events must be rejected");
    assert!(mixed_error.to_string().contains("不同 UID"));
}

#[test]
fn floating_calendar_datetime_uses_taipei_default() {
    let discovery = multistatus([calendar_collection("/cal/", "個人")]);
    let report = multistatus(["<D:response><D:href>/cal/floating.ics</D:href><D:propstat><D:prop><D:getetag>&quot;floating-1&quot;</D:getetag><C:calendar-data><![CDATA[BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:floating-1\r\nDTSTART:20261006T100000\r\nSUMMARY:浮動時間\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n]]></C:calendar-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>".to_owned()]);
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", discovery),
        response("207 Multi-Status", report),
    ]);
    let config = config(address, "/cal/");
    let snapshot =
        dav::sync_collection(&config, "password", Kind::Calendar, &config.caldav_url, &[])
            .expect("floating datetime should be accepted");
    assert_eq!(
        snapshot.items[0].date.as_deref(),
        Some("2026-10-06T10:00:00+08:00")
    );
    let _ = handle.join().expect("join mock server");
}

#[test]
fn fetch_range_discovers_collection_and_requests_expansion() {
    let discovery = multistatus([calendar_collection("/cal/", "個人")]);
    let report = multistatus([event_response("/cal/range.ics")]);
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", discovery),
        response("207 Multi-Status", report),
    ]);
    let config = config(address, "/cal/");
    let start = Utc.with_ymd_and_hms(2026, 10, 6, 0, 0, 0).unwrap();
    let end = Utc.with_ymd_and_hms(2026, 10, 7, 0, 0, 0).unwrap();
    let items = dav::fetch_range(&config, "password", "/cal/", start, end, true)
        .expect("range fetch should succeed");
    assert_eq!(items.len(), 1);
    let requests = handle.join().expect("join mock server");
    assert!(requests[0].starts_with("PROPFIND /cal/ HTTP/1.1"));
    assert!(requests[1].starts_with("REPORT /cal/ HTTP/1.1"));
    assert!(request_contains(
        &requests[1],
        "time-range start=\"20261006T000000Z\""
    ));
    assert!(request_contains(
        &requests[1],
        "expand start=\"20261006T000000Z\""
    ));
}

#[test]
fn carddav_principal_home_set_discovers_addressbook_and_preserves_vcard() {
    let principal = multistatus(["<D:response><D:href>/principals/user</D:href><D:propstat><D:prop><CR:addressbook-home-set><D:href>/card/</D:href></CR:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>".to_owned()]);
    let home = multistatus([addressbook_collection("/card/personal/", "個人")]);
    let contact = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:person-1\r\nFN:測試聯絡人\r\nEMAIL:person@example.test\r\nTEL:+886-2-0000-0000\r\nX-CUSTOM:保留\r\nEND:VCARD\r\n";
    let report = multistatus([format!("<D:response><D:href>/card/personal/person-1</D:href><D:propstat><D:prop><D:getetag>&quot;contact-1&quot;</D:getetag><CR:address-data><![CDATA[{contact}]]></CR:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>")]);
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", principal),
        response("207 Multi-Status", home),
        response("207 Multi-Status", report),
    ]);
    let mut config = config(address, "/cal/");
    config.carddav_url = format!("http://{address}/principals/user");
    let snapshot = dav::sync(&config, "password", Kind::Contacts, &[]).expect("sync contacts");
    assert_eq!(snapshot.collections.len(), 1);
    assert_eq!(snapshot.items.len(), 1);
    assert_eq!(snapshot.items[0].emails, vec!["person@example.test"]);
    assert!(snapshot.items[0].raw.contains("X-CUSTOM:保留"));
    let requests = handle.join().expect("join mock server");
    assert!(requests[0].starts_with("PROPFIND /principals/user HTTP/1.1"));
    assert!(request_contains(&requests[0], "Depth: 0"));
    assert!(requests[1].starts_with("PROPFIND /card/ HTTP/1.1"));
    assert!(request_contains(&requests[1], "Depth: 1"));
    assert!(request_contains(&requests[2], "addressbook-query"));
}

#[test]
fn session_sync_known_collection_does_not_rediscover_children() {
    let discovery = multistatus([
        calendar_collection("/cal/team/", "同事行事曆"),
        calendar_collection("/cal/room/", "會議室行事曆"),
    ]);
    let report = multistatus([event_response("/cal/team/session-event.ics")]);
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", discovery),
        response("207 Multi-Status", report),
    ]);
    let config = config(address, "/cal/");
    let mut session = dav::DavSession::new(&config, "password").expect("session");
    let collections = session
        .discover(Kind::Calendar, &config.caldav_url)
        .expect("discover calendars");
    let team = collections
        .iter()
        .find(|collection| collection.id.ends_with("/cal/team/"))
        .cloned()
        .expect("team collection");
    let snapshot = session
        .sync_collection(&team, &[])
        .expect("sync known collection");
    assert_eq!(snapshot.collections.len(), 1);
    assert_eq!(snapshot.items.len(), 1);
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("PROPFIND /cal/ HTTP/1.1"));
    assert!(requests[1].starts_with("REPORT /cal/team/ HTTP/1.1"));
}

#[test]
fn session_put_known_room_uses_allowlist_without_extra_propfind() {
    let discovery = multistatus([
        calendar_collection("/cal/personal/", "個人行事曆"),
        calendar_collection("/cal/room/", "會議室行事曆"),
    ]);
    let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:room-session-1\r\nDTSTART:20261006T100000Z\r\nSUMMARY:會議室\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let mut put = response("201 Created", "");
    put.headers = vec![("ETag", "\"room-v1\"")];
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", discovery), put]);
    let config = config(address, "/cal/");
    let mut session = dav::DavSession::new(&config, "password").expect("session");
    let collections = session
        .discover(Kind::Calendar, &config.caldav_url)
        .expect("discover calendars");
    let room = collections
        .iter()
        .find(|collection| collection.id.ends_with("/cal/room/"))
        .cloned()
        .expect("room collection");
    let item = session
        .put(Kind::Calendar, &room.id, raw, None)
        .expect("put room event");
    assert_eq!(item.collection, room.id);
    assert_eq!(item.etag.as_deref(), Some("\"room-v1\""));
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("PROPFIND /cal/ HTTP/1.1"));
    assert!(requests[1].starts_with("PUT /cal/room/room-session-1.ics HTTP/1.1"));
}

#[test]
fn fresh_put_discovers_requested_room_instead_of_configured_personal_collection() {
    let discovery = multistatus([calendar_collection("/cal/room/", "會議室行事曆")]);
    let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:fresh-room-1\r\nDTSTART:20261006T100000Z\r\nSUMMARY:新增會議室事件\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let mut put = response("201 Created", "");
    put.headers = vec![("ETag", "\"fresh-room-v1\"")];
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", discovery), put]);
    let config = config(address, "/cal/personal/");
    let item = dav::put(&config, "password", Kind::Calendar, "/cal/room/", raw, None)
        .expect("fresh room put");
    assert_eq!(item.collection, format!("http://{address}/cal/room/"));
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("PROPFIND /cal/room/ HTTP/1.1"));
    assert!(requests[1].starts_with("PUT /cal/room/fresh-room-1.ics HTTP/1.1"));
}

#[test]
fn full_tree_principal_uses_infinity_home_listing() {
    let principal = multistatus(["<D:response><D:href>/principals/user</D:href><D:propstat><D:prop><D:current-user-principal><D:href>/principals/user</D:href></D:current-user-principal><CR:addressbook-home-set><D:href>/card/</D:href></CR:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>".to_owned()]);
    let home = multistatus([
        addressbook_collection("/card/ACME/", "ACME"),
        addressbook_collection("/card/ACME/engineering/team/", "engineering/team"),
    ]);
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", principal),
        response("207 Multi-Status", home),
    ]);
    let mut config = config(address, "/cal/");
    config.carddav_url = format!("http://{address}/principals/user");
    let mut session = dav::DavSession::new(&config, "password").expect("session");
    let result = session
        .discover_full_tree(Kind::Contacts, &config.carddav_url)
        .expect("full tree discovery");
    assert!(result.complete);
    assert_eq!(result.collections.len(), 2);
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("PROPFIND /principals/user HTTP/1.1"));
    assert!(request_contains(&requests[0], "Depth: 0"));
    assert!(requests[1].starts_with("PROPFIND /card/ HTTP/1.1"));
    assert!(request_contains(&requests[1], "Depth: infinity"));
}

#[test]
fn full_tree_follows_principal_chain_before_home_listing() {
    let current = multistatus(["<D:response><D:href>/principals/current</D:href><D:propstat><D:prop><D:current-user-principal><D:href>/principals/actual</D:href></D:current-user-principal></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>".to_owned()]);
    let actual = multistatus(["<D:response><D:href>/principals/actual</D:href><D:propstat><D:prop><CR:addressbook-home-set><D:href>/card/</D:href></CR:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>".to_owned()]);
    let home = multistatus([addressbook_collection("/card/team/", "team")]);
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", current),
        response("207 Multi-Status", actual),
        response("207 Multi-Status", home),
    ]);
    let mut config = config(address, "/cal/");
    config.carddav_url = format!("http://{address}/principals/current");
    let mut session = dav::DavSession::new(&config, "password").expect("session");
    let result = session
        .discover_full_tree(Kind::Contacts, &config.carddav_url)
        .expect("principal chain discovery");
    assert!(result.complete);
    assert_eq!(result.collections.len(), 1);
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 3);
    assert!(request_contains(&requests[0], "Depth: 0"));
    assert!(requests[1].starts_with("PROPFIND /principals/actual HTTP/1.1"));
    assert!(request_contains(&requests[1], "Depth: 0"));
    assert!(requests[2].starts_with("PROPFIND /card/ HTTP/1.1"));
    assert!(request_contains(&requests[2], "Depth: infinity"));
}

#[test]
fn full_tree_direct_branch_uses_infinity_listing() {
    let home = multistatus([
        addressbook_collection("/card/department/", "department"),
        addressbook_collection("/card/department/people/", "people"),
    ]);
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", home)]);
    let config = config(address, "/cal/");
    let mut session = dav::DavSession::new(&config, "password").expect("session");
    let result = session
        .discover_full_tree(Kind::Contacts, &config.carddav_url)
        .expect("full branch discovery");
    assert!(result.complete);
    assert_eq!(result.collections.len(), 2);
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("PROPFIND /card/ HTTP/1.1"));
    assert!(request_contains(&requests[0], "Depth: infinity"));
}

#[test]
fn full_tree_unsupported_infinity_falls_back_without_claiming_complete() {
    let principal = multistatus(["<D:response><D:href>/principals/user</D:href><D:propstat><D:prop><CR:addressbook-home-set><D:href>/card/</D:href></CR:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>".to_owned()]);
    let fallback_home = multistatus([addressbook_collection("/card/team/", "team")]);
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", principal.clone()),
        response("403 Forbidden", ""),
        response("207 Multi-Status", principal),
        response("207 Multi-Status", fallback_home),
    ]);
    let mut config = config(address, "/cal/");
    config.carddav_url = format!("http://{address}/principals/user");
    let mut session = dav::DavSession::new(&config, "password").expect("session");
    let result = session
        .discover_full_tree(Kind::Contacts, &config.carddav_url)
        .expect("unsupported infinity should use normal discovery");
    assert!(!result.complete);
    assert_eq!(result.collections.len(), 1);
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 4);
    assert!(request_contains(&requests[1], "Depth: infinity"));
    assert!(request_contains(&requests[3], "Depth: 1"));
}

#[test]
fn full_tree_server_error_does_not_fallback_or_remember_partial_results() {
    let principal = multistatus(["<D:response><D:href>/principals/user</D:href><D:propstat><D:prop><CR:addressbook-home-set><D:href>/card/</D:href></CR:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>".to_owned()]);
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", principal),
        response("500 Internal Server Error", ""),
    ]);
    let mut config = config(address, "/cal/");
    config.carddav_url = format!("http://{address}/principals/user");
    let mut session = dav::DavSession::new(&config, "password").expect("session");
    let error = session
        .discover_full_tree(Kind::Contacts, &config.carddav_url)
        .expect_err("unexpected server error must fail");
    assert!(error.to_string().contains("DAV PROPFIND 回應狀態錯誤"));
    let error = session
        .sync_collection(
            &Collection {
                kind: Kind::Contacts,
                id: format!("http://{address}/card/team/"),
                name: "team".to_owned(),
            },
            &[],
        )
        .expect_err("failed full discovery must not remember partial collection");
    assert!(error.to_string().contains("尚未由本 session 驗證"));
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 2);
}

#[test]
fn full_tree_malformed_xml_fails_without_fallback() {
    let (address, handle) = spawn_server(vec![response(
        "207 Multi-Status",
        "<D:multistatus xmlns:D=\"DAV:\"><D:response>",
    )]);
    let config = config(address, "/cal/");
    let mut session = dav::DavSession::new(&config, "password").expect("session");
    let error = session
        .discover_full_tree(Kind::Contacts, &config.carddav_url)
        .expect_err("malformed full tree response must fail");
    assert!(error.to_string().contains("DAV XML 格式錯誤"));
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 1);
}

#[test]
fn free_query_contacts_delegates_to_session_collection_query() {
    let discovery = multistatus([addressbook_collection("/card/personal/", "個人")]);
    let contact = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:query-person-1\r\nFN:查詢聯絡人\r\nEMAIL:query@example.test\r\nEND:VCARD\r\n";
    let report = multistatus([format!(
        "<D:response><D:href>/card/personal/query-person-1</D:href><D:propstat><D:prop><D:getetag>&quot;query-1&quot;</D:getetag><CR:address-data><![CDATA[{contact}]]></CR:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )]);
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", discovery),
        response("207 Multi-Status", report),
    ]);
    let config = config(address, "/cal/");
    let snapshot =
        dav::query_contacts(&config, "password", "/card/personal/", "query@example.test")
            .expect("query contact");
    assert_eq!(snapshot.items.len(), 1);
    assert_eq!(snapshot.items[0].emails, vec!["query@example.test"]);
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("PROPFIND /card/personal/ HTTP/1.1"));
    assert!(requests[1].starts_with("REPORT /card/personal/ HTTP/1.1"));
    assert!(request_contains(&requests[1], "query@example.test"));
}

#[test]
fn discovery_keeps_readable_collections_beside_denied_responses() {
    let denied = "<D:response><D:href>/cal/private/</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 403 Forbidden</D:status></D:propstat></D:response>";
    let body = multistatus([
        denied.to_owned(),
        calendar_collection("/cal/public/", "Public"),
    ]);
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", body)]);
    let cfg = config(address, "/cal/");
    let found = dav::discover(&cfg, "password", Kind::Calendar, &cfg.caldav_url).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "Public");
    handle.join().unwrap();
}

#[test]
fn discovery_of_only_denied_collection_reports_href_and_permission() {
    let body = multistatus(["<D:response><D:href>/cal/private/</D:href><D:status>HTTP/1.1 403 Forbidden</D:status></D:response>".to_owned()]);
    let (address, handle) = spawn_server(vec![response("207 Multi-Status", body)]);
    let cfg = config(address, "/cal/");
    let error = dav::discover(&cfg, "password", Kind::Calendar, &cfg.caldav_url)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("403") && error.contains("權限") && error.contains("/cal/private/"),
        "{error}"
    );
    handle.join().unwrap();
}

#[test]
fn percent_encoded_at_hrefs_stay_inside_discovered_collection() {
    // m2k's default caldav_url encodes the mailbox as `%40`, and Mail2000 echoes that form in
    // REPORT hrefs; the canonical collection id decodes it to `@`, so containment must match both.
    let discovery = multistatus([calendar_collection(
        "/cal/user%40example.test/default/",
        "個人行事曆",
    )]);
    let report = multistatus([event_response(
        "/cal/user%40example.test/default/event-1.ics",
    )]);
    let (address, handle) = spawn_server(vec![
        response("207 Multi-Status", discovery),
        response("207 Multi-Status", report),
    ]);
    let config = config(address, "/cal/user%40example.test/");
    let mut session = dav::DavSession::new(&config, "password").expect("session");
    let collections = session
        .discover(Kind::Calendar, &config.caldav_url)
        .expect("discover calendars");
    assert_eq!(collections.len(), 1);
    let snapshot = session
        .sync_collection(&collections[0], &[])
        .expect("sync %40 collection");
    assert_eq!(snapshot.items.len(), 1);
    assert_eq!(snapshot.items[0].title, "同步測試");
    let requests = handle.join().expect("join mock server");
    assert!(requests[1].starts_with("REPORT /cal/user"));
    assert!(requests[1].contains("example.test/default/ HTTP/1.1"));
}
