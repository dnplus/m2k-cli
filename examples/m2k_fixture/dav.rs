//! Plain-HTTP CalDAV/CardDAV on loopback (m2k only allows `http://` for
//! localhost). Supports PROPFIND, REPORT (query/multiget, plus RFC 6578
//! sync-collection when enabled), GET, PUT, DELETE, plus an unauthenticated
//! `POST /webhook` sink for `m2k mail watch`. Every request is appended to
//! `<data-dir>/http.jsonl` so tests can count what a sync really cost.

use super::net::LineConn;
use super::state::{dav_key, DavNode, DavStore, Fixture, NodeKind};
use anyhow::Result;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap},
    fs::OpenOptions,
    io::Write,
    net::TcpStream,
    sync::Arc,
};

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Response {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: &'static str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn xml(body: String) -> Self {
        Self {
            status: "207 Multi-Status",
            headers: vec![("Content-Type", "application/xml; charset=utf-8".to_owned())],
            body: body.into_bytes(),
        }
    }
}

pub fn serve(fixture: Arc<Fixture>, stream: TcpStream) -> Result<()> {
    let mut conn = LineConn::new(stream);
    let Some(request) = read_request(&mut conn)? else {
        return Ok(());
    };
    let response = if request.method == "POST" && dav_key(&request.path) == "/webhook" {
        webhook(&fixture, &request)
    } else if authorized(&fixture, &request) {
        route(&fixture, &request)
    } else {
        let mut response = Response::new("401 Unauthorized");
        response
            .headers
            .push(("WWW-Authenticate", "Basic realm=\"m2k fixture\"".to_owned()));
        response
    };
    let depth = request.headers.get("depth").map_or("-", String::as_str);
    let responses = String::from_utf8_lossy(&response.body)
        .matches("<D:response>")
        .count();
    eprintln!(
        "[fixture] http {} {} depth={depth} -> {} responses={responses}",
        request.method, request.path, response.status
    );
    let line = json!({
        "method": request.method,
        "path": request.path,
        "depth": depth,
        "status": response.status,
        "responses": responses,
        "report": report_name(&request.body),
    });
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(fixture.data_dir.join("http.jsonl"))
    {
        let _ = writeln!(file, "{line}");
    }
    let mut head = format!(
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend(response.body);
    conn.write(bytes)?;
    Ok(())
}

fn read_request(conn: &mut LineConn<TcpStream>) -> Result<Option<Request>> {
    let Some(line) = conn.read_line()? else {
        return Ok(None);
    };
    let line = String::from_utf8_lossy(&line).into_owned();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("/").to_owned();
    let mut headers = HashMap::new();
    while let Some(header) = conn.read_line()? {
        if header.is_empty() {
            break;
        }
        let header = String::from_utf8_lossy(&header).into_owned();
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let body = conn.read_bytes(length)?;
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

fn authorized(fixture: &Fixture, request: &Request) -> bool {
    let Some(value) = request
        .headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Basic "))
    else {
        return false;
    };
    let expected = format!("{}:{}", fixture.user, fixture.password);
    STANDARD
        .decode(value.trim())
        .is_ok_and(|decoded| decoded == expected.as_bytes())
}

fn route(fixture: &Fixture, request: &Request) -> Response {
    let key = dav_key(&request.path);
    match request.method.as_str() {
        "OPTIONS" => {
            let mut response = Response::new("200 OK");
            response
                .headers
                .push(("DAV", "1, 3, calendar-access, addressbook".to_owned()));
            response.headers.push((
                "Allow",
                "OPTIONS, GET, PUT, DELETE, PROPFIND, REPORT".to_owned(),
            ));
            response
        }
        "PROPFIND" => propfind(fixture, request, &key),
        "REPORT" => report(fixture, request, &key),
        "GET" => {
            let dav = fixture.dav.lock().expect("dav lock");
            match dav.nodes.get(&key).map(|node| &node.kind) {
                Some(NodeKind::Resource {
                    data,
                    etag,
                    content_type,
                }) => Response {
                    status: "200 OK",
                    headers: vec![
                        ("Content-Type", (*content_type).to_owned()),
                        ("ETag", etag.clone()),
                    ],
                    body: data.clone().into_bytes(),
                },
                Some(_) => Response::new("405 Method Not Allowed"),
                None => Response::new("404 Not Found"),
            }
        }
        "PUT" => put(fixture, request, &key),
        "DELETE" => delete(fixture, request, &key),
        _ => Response::new("405 Method Not Allowed"),
    }
}

fn report_name(body: &[u8]) -> Option<String> {
    let body = String::from_utf8_lossy(body);
    roxmltree::Document::parse(&body)
        .ok()
        .map(|document| document.root_element().tag_name().name().to_owned())
}

fn children<'a>(dav: &'a DavStore, key: &str, depth: &str) -> Vec<&'a DavNode> {
    let mut nodes = Vec::new();
    if let Some(node) = dav.nodes.get(key) {
        nodes.push(node);
    }
    if depth == "0" {
        return nodes;
    }
    let prefix = if key == "/" {
        "/".to_owned()
    } else {
        format!("{key}/")
    };
    for (child, node) in dav.nodes.range(prefix.clone()..) {
        let Some(rest) = child.strip_prefix(&prefix) else {
            break;
        };
        if depth == "1" && rest.contains('/') {
            continue;
        }
        nodes.push(node);
    }
    nodes
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn multistatus(responses: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\">{responses}</D:multistatus>"
    )
}

fn response_xml(href: &str, props: &str) -> String {
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop>{props}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        escape(href)
    )
}

fn href_prop(name: &str, href: &str) -> String {
    format!("<{name}><D:href>{}</D:href></{name}>", escape(href))
}

fn node_props(dav: &DavStore, node: &DavNode) -> String {
    let mut props = String::new();
    if let Some(name) = &node.name {
        props.push_str(&format!("<D:displayname>{}</D:displayname>", escape(name)));
    }
    match &node.kind {
        NodeKind::Principal {
            calendar_home,
            addressbook_home,
        } => {
            props.push_str("<D:resourcetype><D:principal/></D:resourcetype>");
            props.push_str(&href_prop("D:current-user-principal", &node.href));
            if let Some(home) = calendar_home {
                props.push_str(&href_prop("C:calendar-home-set", home));
            }
            if let Some(home) = addressbook_home {
                props.push_str(&href_prop("CR:addressbook-home-set", home));
            }
        }
        NodeKind::Home => props.push_str("<D:resourcetype><D:collection/></D:resourcetype>"),
        NodeKind::Calendar => {
            props.push_str(
                "<D:resourcetype><D:collection/><C:calendar/></D:resourcetype><C:supported-calendar-component-set><C:comp name=\"VEVENT\"/></C:supported-calendar-component-set>",
            );
            if dav.sync_collection {
                props.push_str(&format!(
                    "<D:sync-token>{}</D:sync-token><D:supported-report-set><D:supported-report><D:report><D:sync-collection/></D:report></D:supported-report></D:supported-report-set>",
                    escape(&dav.sync_token())
                ));
            }
        }
        NodeKind::Addressbook => props.push_str(
            "<D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype><CR:supported-address-data><CR:address-data-type content-type=\"text/vcard\" version=\"3.0\"/></CR:supported-address-data>",
        ),
        NodeKind::Resource {
            etag, content_type, ..
        } => props.push_str(&format!(
            "<D:resourcetype/><D:getetag>{}</D:getetag><D:getcontenttype>{}</D:getcontenttype>",
            escape(etag),
            escape(content_type)
        )),
    }
    props
}

fn propfind(fixture: &Fixture, request: &Request, key: &str) -> Response {
    let depth = request
        .headers
        .get("depth")
        .map(|d| d.to_ascii_lowercase())
        .unwrap_or_else(|| "infinity".to_owned());
    let dav = fixture.dav.lock().expect("dav lock");
    if !dav.nodes.contains_key(key) {
        return Response::new("404 Not Found");
    }
    let body = children(&dav, key, &depth)
        .into_iter()
        .map(|node| response_xml(&node.href, &node_props(&dav, node)))
        .collect::<String>();
    Response::xml(multistatus(&body))
}

struct ReportQuery {
    hrefs: Option<Vec<String>>,
    time_range: Option<(DateTime<Utc>, DateTime<Utc>)>,
    email: Option<String>,
    with_data: bool,
}

fn parse_report(body: &str) -> Option<ReportQuery> {
    let document = roxmltree::Document::parse(body).ok()?;
    let root = document.root_element();
    let named = |name: &'static str| {
        document
            .descendants()
            .filter(move |node| node.is_element() && node.tag_name().name() == name)
    };
    let multiget = root.tag_name().name().ends_with("-multiget");
    let hrefs = multiget.then(|| {
        root.children()
            .filter(|node| node.is_element() && node.tag_name().name() == "href")
            .filter_map(|node| node.text().map(|t| t.trim().to_owned()))
            .collect()
    });
    let time_range = named("time-range").next().and_then(|node| {
        Some((
            parse_ical_time(node.attribute("start")?)?,
            parse_ical_time(node.attribute("end")?)?,
        ))
    });
    let email = named("prop-filter")
        .find(|node| node.attribute("name") == Some("EMAIL"))
        .and_then(|node| {
            node.children()
                .find(|child| child.is_element() && child.tag_name().name() == "text-match")
        })
        .and_then(|node| node.text().map(|t| t.trim().to_lowercase()));
    let with_data =
        named("calendar-data").next().is_some() || named("address-data").next().is_some();
    Some(ReportQuery {
        hrefs,
        time_range,
        email,
        with_data,
    })
}

fn parse_ical_time(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(time) = NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S") {
        return Some(time.and_utc());
    }
    NaiveDate::parse_from_str(value, "%Y%m%d")
        .ok()
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|time| time.and_utc())
}

fn event_range(data: &str) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let value = |name: &str| {
        data.lines()
            .map(|line| line.trim_end_matches('\r'))
            .find(|line| {
                line.starts_with(&format!("{name}:")) || line.starts_with(&format!("{name};"))
            })
            .and_then(|line| line.rsplit_once(':'))
            .and_then(|(_, value)| parse_ical_time(value))
    };
    let start = value("DTSTART")?;
    let end = value("DTEND").unwrap_or(start + chrono::Duration::hours(1));
    Some((start, end))
}

fn report(fixture: &Fixture, request: &Request, key: &str) -> Response {
    let body = String::from_utf8_lossy(&request.body);
    if report_name(&request.body).as_deref() == Some("sync-collection") {
        return sync_collection(fixture, &body, key);
    }
    let Some(query) = parse_report(&body) else {
        return Response::new("400 Bad Request");
    };
    let dav = fixture.dav.lock().expect("dav lock");
    let Some(collection) = dav.nodes.get(key) else {
        return Response::new("404 Not Found");
    };
    if !matches!(collection.kind, NodeKind::Calendar | NodeKind::Addressbook) {
        return Response::new("403 Forbidden");
    }
    let resources: Vec<&DavNode> = match &query.hrefs {
        Some(hrefs) => hrefs
            .iter()
            .filter_map(|href| dav.nodes.get(&dav_key(href)))
            .collect(),
        None => children(&dav, key, "1").into_iter().skip(1).collect(),
    };
    let mut out = String::new();
    for node in resources {
        let NodeKind::Resource { data, etag, .. } = &node.kind else {
            continue;
        };
        if let (Some((start, end)), Some((event_start, event_end))) =
            (query.time_range, event_range(data))
        {
            if !(event_start < end && start < event_end) {
                continue;
            }
        }
        if let Some(email) = &query.email {
            let matches = data.lines().any(|line| {
                line.to_ascii_uppercase().starts_with("EMAIL")
                    && line
                        .rsplit_once(':')
                        .is_some_and(|(_, value)| value.trim().eq_ignore_ascii_case(email))
            });
            if !matches {
                continue;
            }
        }
        let mut props = format!("<D:getetag>{}</D:getetag>", escape(etag));
        if query.with_data {
            let element = if matches!(collection.kind, NodeKind::Calendar) {
                "C:calendar-data"
            } else {
                "CR:address-data"
            };
            props.push_str(&format!("<{element}><![CDATA[{data}]]></{element}>"));
        }
        out.push_str(&response_xml(&node.href, &props));
    }
    Response::xml(multistatus(&out))
}

fn dav_error(status: &'static str, condition: &str) -> Response {
    Response {
        status,
        headers: vec![("Content-Type", "application/xml; charset=utf-8".to_owned())],
        body: format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:error xmlns:D=\"DAV:\"><D:{condition}/></D:error>"
        )
        .into_bytes(),
    }
}

/// RFC 6578 sync-collection: members changed since the token, deletions as
/// 404 responses, and a fresh token. Unknown or foreign tokens get 403
/// `valid-sync-token`, as after a server-side reset.
fn sync_collection(fixture: &Fixture, body: &str, key: &str) -> Response {
    let dav = fixture.dav.lock().expect("dav lock");
    if !dav.sync_collection {
        return dav_error("403 Forbidden", "supported-report");
    }
    if !matches!(
        dav.nodes.get(key).map(|node| &node.kind),
        Some(NodeKind::Calendar)
    ) {
        return dav_error("403 Forbidden", "supported-report");
    }
    let token = roxmltree::Document::parse(body).ok().and_then(|document| {
        document
            .descendants()
            .find(|node| node.is_element() && node.tag_name().name() == "sync-token")
            .map(|node| node.text().unwrap_or("").trim().to_owned())
    });
    // Test hooks for servers that answer sync-collection badly.
    match token.as_deref() {
        Some("http://m2k.fixture/broken/501") => return Response::new("501 Not Implemented"),
        Some("http://m2k.fixture/broken/no-token") => return Response::xml(multistatus("")),
        _ => {}
    }
    let Some(since) = token.and_then(|token| dav.parse_sync_token(&token)) else {
        return dav_error("403 Forbidden", "valid-sync-token");
    };
    let prefix = format!("{key}/");
    let changed = dav
        .changes
        .iter()
        .filter(|(seq, changed, _)| {
            *seq > since
                && changed
                    .strip_prefix(&prefix)
                    .is_some_and(|rest| !rest.contains('/'))
        })
        .map(|(_, changed, href)| (changed.as_str(), href.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut out = String::new();
    for (changed, href) in changed {
        match dav.nodes.get(changed).map(|node| &node.kind) {
            Some(NodeKind::Resource { etag, .. }) => out.push_str(&response_xml(
                href,
                &format!("<D:getetag>{}</D:getetag>", escape(etag)),
            )),
            _ => out.push_str(&format!(
                "<D:response><D:href>{}</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>",
                escape(href)
            )),
        }
    }
    let body = multistatus(&format!(
        "{out}<D:sync-token>{}</D:sync-token>",
        escape(&dav.sync_token())
    ));
    Response::xml(body)
}

fn precondition_failed(request: &Request, current: Option<&str>) -> bool {
    if request.headers.get("if-none-match").map(String::as_str) == Some("*") && current.is_some() {
        return true;
    }
    match request.headers.get("if-match") {
        Some(expected) => current != Some(expected.as_str()) && expected != "*",
        None => false,
    }
}

fn parent_key(key: &str) -> String {
    match key.rsplit_once('/') {
        Some(("", _)) | None => "/".to_owned(),
        Some((parent, _)) => parent.to_owned(),
    }
}

fn put(fixture: &Fixture, request: &Request, key: &str) -> Response {
    let data = String::from_utf8_lossy(&request.body).into_owned();
    let mut dav = fixture.dav.lock().expect("dav lock");
    let parent = parent_key(key);
    let (content_type, collection_kind) = match dav.nodes.get(&parent).map(|n| &n.kind) {
        Some(NodeKind::Calendar) => ("text/calendar; charset=utf-8", "calendar"),
        Some(NodeKind::Addressbook) => ("text/vcard; charset=utf-8", "addressbook"),
        _ => return Response::new("409 Conflict"),
    };
    let current = match dav.nodes.get(key).map(|n| &n.kind) {
        Some(NodeKind::Resource { etag, .. }) => Some(etag.clone()),
        Some(_) => return Response::new("405 Method Not Allowed"),
        None => None,
    };
    if precondition_failed(request, current.as_deref()) {
        return Response::new("412 Precondition Failed");
    }
    let etag = dav.next_etag();
    let href = request.path.split('?').next().unwrap_or("").to_owned();
    dav.insert(
        &href,
        None,
        NodeKind::Resource {
            data: data.clone(),
            etag: etag.clone(),
            content_type,
        },
    );
    let collection = dav.nodes.get(&parent).map(|n| n.href.clone());
    drop(dav);
    let uid = data
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("UID:"))
        .map(ToOwned::to_owned);
    let name = href.rsplit('/').next().unwrap_or("resource");
    fixture.record(
        "dav.put",
        json!({
            "path": href,
            "collection": collection,
            "collection_kind": collection_kind,
            "uid": uid,
            "created": current.is_none(),
            "etag": etag,
        }),
        Some(("dav", name, data.as_bytes())),
    );
    let mut response = Response::new(if current.is_none() {
        "201 Created"
    } else {
        "204 No Content"
    });
    response.headers.push(("ETag", etag));
    response
}

fn delete(fixture: &Fixture, request: &Request, key: &str) -> Response {
    let mut dav = fixture.dav.lock().expect("dav lock");
    let current = match dav.nodes.get(key).map(|n| &n.kind) {
        Some(NodeKind::Resource { etag, .. }) => etag.clone(),
        Some(_) => return Response::new("403 Forbidden"),
        None => return Response::new("404 Not Found"),
    };
    if precondition_failed(request, Some(&current)) {
        return Response::new("412 Precondition Failed");
    }
    let node = dav.remove(key).expect("checked");
    drop(dav);
    fixture.record(
        "dav.delete",
        json!({"path": node.href, "etag": current}),
        None,
    );
    Response::new("204 No Content")
}

fn webhook(fixture: &Fixture, request: &Request) -> Response {
    let payload = serde_json::from_slice::<serde_json::Value>(&request.body).ok();
    fixture.record(
        "webhook",
        json!({
            "authorization": request.headers.contains_key("authorization"),
            "event": payload.as_ref().and_then(|p| p.get("event")).cloned(),
            "event_id": payload.as_ref().and_then(|p| p.get("event_id")).cloned(),
            "uid": payload.as_ref().and_then(|p| p.get("uid")).cloned(),
        }),
        Some(("webhook", "payload.json", &request.body)),
    );
    Response::new("204 No Content")
}
