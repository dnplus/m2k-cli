use chrono::Utc;
use m2k::{
    config::{self, Config},
    directory::{
        address_book_tree, contact_summaries_page, contact_to_item, department_from_memberships,
        format_contact_detail, merge_contact_items, resolve_unique_contact,
        validate_snapshot_scope, DirectoryCommand, DirectoryMembership, DirectoryNode,
        DirectoryScope, SyncArgs,
    },
    model::{Collection, Item, Kind, Snapshot},
    store::Store,
};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::Mutex,
    thread::{self, JoinHandle},
};
use tempfile::TempDir;

// A keychain file that does not exist: m2k must never read or write the
// login keychain from tests.
const NO_KEYCHAIN: &str = "/nonexistent/m2k-test.keychain-db";

static PASSWORD_LOCK: Mutex<()> = Mutex::new(());

fn collection(id: &str, name: &str) -> Collection {
    Collection {
        kind: Kind::Contacts,
        id: id.to_owned(),
        name: name.to_owned(),
    }
}

fn contact(id: &str, collection: &str, uid: &str, name: &str, email: &str) -> Item {
    Item {
        id: id.to_owned(),
        kind: Kind::Contacts,
        collection: collection.to_owned(),
        remote_id: format!("{collection}{id}.vcf"),
        etag: None,
        uid_validity: None,
        title: name.to_owned(),
        subtitle: "平台工程部".to_owned(),
        date: None,
        end_date: None,
        text: String::new(),
        raw: format!("BEGIN:VCARD\nVERSION:3.0\nUID:{uid}\nFN:{name}\nEMAIL:{email}\nEND:VCARD\n"),
        flags: Vec::new(),
        emails: vec![email.to_owned()],
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

#[test]
fn persisted_contact_pages_decode_only_selected_people_and_keep_all_memberships() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut items = (0..2000)
        .map(|index| {
            contact(
                &format!("person-{index:04}"),
                "root",
                &format!("{index:04}"),
                &format!("員工 {index:04}"),
                &format!("person{index}@example.test"),
            )
        })
        .collect::<Vec<_>>();
    let mut branch = items[0].clone();
    branch.id = "person-0000-branch".into();
    branch.collection = "branch".into();
    branch.text = "特殊中文查詢".into();
    items.push(branch);
    store
        .apply_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: vec![collection("root", "公司"), collection("branch", "部門")],
                items,
            },
            Utc::now(),
        )
        .unwrap();
    drop(store);
    let connection = rusqlite::Connection::open(dir.path().join("database.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE items SET flags_json='invalid source JSON' WHERE kind='contacts'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE contact_people SET payload='invalid unselected person' WHERE id='uid:1999'",
            [],
        )
        .unwrap();
    let reader = Store::open_reader(dir.path()).unwrap();
    let page = contact_summaries_page(&reader, None, None, 100, 0).unwrap();
    assert_eq!(page.total, 2000);
    assert_eq!(page.items.len(), 100);
    let matches =
        contact_summaries_page(&reader, Some("特殊中文"), Some("branch"), 100, 0).unwrap();
    assert_eq!(matches.total, 1);
    assert!(matches.items[0].text.contains("root"));
    assert!(matches.items[0].text.contains("branch"));
    assert!(contact_summaries_page(&reader, None, None, 100, 1900).is_err());
}

#[test]
fn contact_projection_tracks_partial_updates_removal_and_legacy_migration() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .apply_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: vec![collection("root", "公司"), collection("branch", "部門")],
                items: vec![
                    contact("root-item", "root", "same", "王小明", "ming@example.test"),
                    contact(
                        "branch-item",
                        "branch",
                        "same",
                        "王小明",
                        "ming@example.test",
                    ),
                ],
            },
            Utc::now(),
        )
        .unwrap();
    store
        .apply_collection_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: vec![collection("branch", "新部門")],
                items: Vec::new(),
            },
            Utc::now(),
        )
        .unwrap();
    let page = store.contact_page(None, None, 10, 0).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].collection, "root");
    drop(store);
    let conn = rusqlite::Connection::open(dir.path().join("database.sqlite3")).unwrap();
    conn.execute_batch(
        "DROP TABLE contact_members; DROP TABLE contact_people; PRAGMA user_version=1;",
    )
    .unwrap();
    drop(conn);
    let reader = Store::open_reader(dir.path()).unwrap();
    assert_eq!(
        reader
            .contact_page(Some("王小明"), None, 10, 0)
            .unwrap()
            .total,
        1
    );
    drop(reader);
    let mut store = Store::open(dir.path()).unwrap();
    store.remove("root-item").unwrap();
    assert_eq!(store.contact_page(None, None, 10, 0).unwrap().total, 0);
}

#[test]
fn tree_uses_mail2000_display_paths_and_preserves_parent_depth() {
    let root = "https://mail.example.test/cgi-bin/carddav/addressbooks/mPA.000/ACME/";
    let collections = vec![
        collection(root, "ACME(/ACME)"),
        collection(
            "https://mail.example.test/cgi-bin/carddav/addressbooks/mPA.000/ACME/ENG/",
            "ENG(/ACME/ENG)",
        ),
        collection(
            "https://mail.example.test/cgi-bin/carddav/addressbooks/mPA.000/ACME/ENG/ENG_TOOLS/",
            "ENG_TOOLS(/ACME/ENG/ENG_TOOLS)",
        ),
    ];
    let nodes = address_book_tree(
        &collections,
        Some("https://mail.example.test/cgi-bin/carddav/addressbooks/mPA.000/"),
    );
    assert_eq!(nodes[0].path, "ACME");
    assert_eq!(nodes[0].parent_path, None);
    assert_eq!(nodes[0].depth, 0);
    assert_eq!(nodes[1].path, "ACME/ENG");
    assert_eq!(nodes[1].parent_path.as_deref(), Some("ACME"));
    assert_eq!(nodes[1].depth, 1);
    assert_eq!(nodes[2].path, "ACME/ENG/ENG_TOOLS");
    assert_eq!(nodes[2].parent_path.as_deref(), Some("ACME/ENG"));
    assert_eq!(nodes[2].depth, 2);
}

#[test]
fn contacts_with_same_uid_merge_but_keep_all_memberships() {
    let root = "https://mail.example.test/carddav/";
    let collections = vec![
        collection("https://mail.example.test/carddav/ACME/", "ACME(/ACME)"),
        collection(
            "https://mail.example.test/carddav/ACME/ENG/",
            "ENG(/ACME/ENG)",
        ),
    ];
    let nodes = address_book_tree(&collections, Some(root));
    let items = vec![
        contact(
            "a",
            &collections[0].id,
            "same",
            "林測試",
            "person@example.test",
        ),
        contact(
            "b",
            &collections[1].id,
            "same",
            "林測試",
            "person@example.test",
        ),
    ];
    let merged = merge_contact_items(&items, &nodes);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].emails, vec!["person@example.test"]);
    assert_eq!(merged[0].memberships.len(), 2);
    assert_eq!(merged[0].memberships[0].path, "ACME");
    assert_eq!(merged[0].memberships[1].path, "ACME/ENG");
}

#[test]
fn resolve_requires_exact_unique_name_or_email() {
    let collections = vec![collection(
        "https://mail.example.test/carddav/team/",
        "Team(/team)",
    )];
    let nodes = address_book_tree(&collections, None);
    let items = vec![contact(
        "a",
        &collections[0].id,
        "one",
        "林測試",
        "one@example.test",
    )];
    let contacts = merge_contact_items(&items, &nodes);
    assert_eq!(
        resolve_unique_contact(&contacts, "one@example.test")
            .unwrap()
            .name,
        "林測試"
    );
    assert_eq!(
        resolve_unique_contact(&contacts, "林測試").unwrap().emails,
        vec!["one@example.test"]
    );
    assert!(resolve_unique_contact(&contacts, "林").is_err());
    assert!(resolve_unique_contact(&contacts, "missing@example.test").is_err());
}

#[test]
fn resolve_rejects_two_exactly_named_people() {
    let collections = vec![
        collection("https://mail.example.test/carddav/a/", "A(/a)"),
        collection("https://mail.example.test/carddav/b/", "B(/b)"),
    ];
    let nodes = address_book_tree(&collections, None);
    let items = vec![
        contact("a", &collections[0].id, "one", "同名", "one@example.test"),
        contact("b", &collections[1].id, "two", "同名", "two@example.test"),
    ];
    let contacts = merge_contact_items(&items, &nodes);
    let error = resolve_unique_contact(&contacts, "同名").expect_err("同名必須拒絕模糊解析");
    assert!(error.to_string().contains("不唯一"));
}

struct ResponseSpec {
    body: String,
}

fn spawn_server(responses: Vec<ResponseSpec>) -> (SocketAddr, JoinHandle<Vec<String>>) {
    spawn_server_with_status(
        responses
            .into_iter()
            .map(|response| (207, response))
            .collect(),
    )
}

fn spawn_server_with_status(
    responses: Vec<(u16, ResponseSpec)>,
) -> (SocketAddr, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock server");
    let address = listener.local_addr().expect("mock address");
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, spec) in responses {
            let (mut stream, _) = listener.accept().expect("accept request");
            requests.push(read_request(&mut stream));
            let body = spec.body;
            let response = format!(
                "HTTP/1.1 {status} {}\r\nContent-Type: application/xml; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status_reason(status),
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
        requests
    });
    (address, handle)
}

fn status_reason(status: u16) -> &'static str {
    match status {
        207 => "Multi-Status",
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        501 => "Not Implemented",
        _ => "Status",
    }
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

fn setup_data_dir(address: SocketAddr) -> TempDir {
    setup_data_dir_at(address, "/principals/user@example.test")
}

fn setup_data_dir_at(address: SocketAddr, carddav_path: &str) -> TempDir {
    let data_dir = TempDir::new().expect("data dir");
    let base = format!("http://{address}");
    let mut config = Config::new("user@example.test".to_owned(), &base).expect("config");
    config.caldav_url = format!("{base}/cal/");
    config.carddav_url = format!("{base}{carddav_path}");
    config.validate().expect("valid local config");
    config::save(data_dir.path(), &config).expect("save config");
    Store::open(data_dir.path())
        .expect("open store")
        .ensure_account(&config)
        .expect("ensure account");
    data_dir
}

fn multistatus(responses: impl IntoIterator<Item = String>) -> String {
    format!(
        "<?xml version=\"1.0\"?><D:multistatus xmlns:D=\"DAV:\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\">{}</D:multistatus>",
        responses.into_iter().collect::<String>()
    )
}

fn principal_response(home: &str) -> String {
    format!(
        "<D:response><D:href>/principals/user@example.test</D:href><D:propstat><D:prop><CR:addressbook-home-set><D:href>{home}</D:href></CR:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn addressbook_response(href: &str, name: &str) -> String {
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:displayname>{name}</D:displayname><D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn contact_response(href: &str) -> String {
    let vcard = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:self-1\r\nFN:王自動\r\nORG:平台工程部\r\nEMAIL:user@example.test\r\nEND:VCARD\r\n";
    format!(
        "<D:response><D:href>{href}</D:href><D:propstat><D:prop><D:getetag>&quot;self-1&quot;</D:getetag><CR:address-data><![CDATA[{vcard}]]></CR:address-data></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

#[test]
fn default_self_queries_root_once_without_scanning_sibling_branches() {
    let principal = multistatus([principal_response("/carddav/home/")]);
    let home = multistatus([
        addressbook_response("/carddav/home/ACME/", "ACME(/ACME)"),
        addressbook_response("/carddav/home/OTHER/", "OTHER(/OTHER)"),
    ]);
    let report = multistatus([contact_response("/carddav/home/ACME/self.vcf")]);
    let (address, handle) = spawn_server(vec![
        ResponseSpec { body: principal },
        ResponseSpec { body: home },
        ResponseSpec { body: report },
    ]);
    let base = format!("http://{address}");
    // The company root comes from the site profile, so OTHER is never scanned.
    let profile_dir = TempDir::new().unwrap();
    let profile = profile_dir.path().join("acme.json");
    std::fs::write(
        &profile,
        r#"{"name":"acme","company_directory_root":"ACME"}"#,
    )
    .unwrap();
    let mut config = Config::new("user@example.test".to_owned(), &base).expect("config");
    config.caldav_url = format!("{base}/cal/");
    config.carddav_url = format!("{base}/principals/user@example.test");
    config.site_profile = Some(profile.to_string_lossy().into_owned());
    config.validate().expect("valid local config");
    let snapshot = m2k::directory::sync_default(&config, "password", &[]).expect("self sync");
    let requests = handle.join().expect("join mock server");
    assert_eq!(snapshot.collections.len(), 1);
    assert_eq!(snapshot.items.len(), 1);
    assert_eq!(snapshot.collections[0].name, "ACME(/ACME)");
    assert_eq!(snapshot.items[0].title, "王自動");
    assert_eq!(snapshot.items[0].subtitle, "平台工程部");
    assert!(requests[0].starts_with("PROPFIND /principals/user@example.test HTTP/1.1"));
    assert!(requests[0].to_ascii_lowercase().contains("depth: 0"));
    assert!(requests[1].starts_with("PROPFIND /carddav/home/ HTTP/1.1"));
    assert!(requests[1].to_ascii_lowercase().contains("depth: 1"));
    assert!(requests[2].starts_with("REPORT /carddav/home/ACME/ HTTP/1.1"));
    assert!(requests[2].contains("name=\"EMAIL\""));
    assert!(requests[2].contains("match-type=\"equals\">user@example.test"));
}

#[test]
fn company_sync_reports_each_collection_once_from_complete_home_tree() {
    let _password_guard = PASSWORD_LOCK.lock().expect("password lock");
    let principal = multistatus([principal_response("/carddav/home/")]);
    let home = multistatus([
        addressbook_response("/carddav/home/ACME/", "ACME(/ACME)"),
        addressbook_response("/carddav/home/ACME/ENG/", "ENG(/ACME/ENG)"),
        addressbook_response(
            "/carddav/home/ACME/ENG/ENG_TOOLS/",
            "ENG_TOOLS(/ACME/ENG/ENG_TOOLS)",
        ),
        addressbook_response("/carddav/home/OTHER/", "OTHER(/OTHER)"),
    ]);
    let empty = multistatus(Vec::<String>::new());
    let (address, handle) = spawn_server(vec![
        ResponseSpec { body: principal },
        ResponseSpec { body: home },
        ResponseSpec {
            body: empty.clone(),
        },
        ResponseSpec {
            body: empty.clone(),
        },
        ResponseSpec {
            body: empty.clone(),
        },
        ResponseSpec {
            body: empty.clone(),
        },
        ResponseSpec {
            body: empty.clone(),
        },
        ResponseSpec {
            body: empty.clone(),
        },
        ResponseSpec {
            body: empty.clone(),
        },
        ResponseSpec { body: empty },
    ]);
    let data_dir = setup_data_dir(address);
    std::env::set_var("M2K_KEYCHAIN", NO_KEYCHAIN);
    std::env::set_var("M2K_PASSWORD", "password");
    let result = m2k::directory::sync_contacts(
        data_dir.path(),
        SyncArgs {
            scope: DirectoryScope::Company,
            addressbook: None,
        },
    );
    std::env::remove_var("M2K_PASSWORD");
    let result = result.expect("company sync");
    assert_eq!(result.scope, DirectoryScope::Company);
    assert_eq!(result.collections.len(), 4);
    assert_eq!(result.scope_paths.len(), 4);
    assert_eq!(result.count, 0);
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 10);
    assert!(requests[0].starts_with("PROPFIND /principals/user@example.test HTTP/1.1"));
    assert!(requests[1].starts_with("PROPFIND /carddav/home/ HTTP/1.1"));
    assert!(requests[1].to_ascii_lowercase().contains("depth: infinity"));
    let completeness = &requests[2..6];
    assert!(completeness.iter().all(|request| {
        request.starts_with("PROPFIND ") && request.to_ascii_lowercase().contains("depth: 1")
    }));
    assert!(requests[6..]
        .iter()
        .all(|request| request.starts_with("REPORT ")));
    for path in [
        "/carddav/home/ACME/",
        "/carddav/home/ACME/ENG/",
        "/carddav/home/ACME/ENG/ENG_TOOLS/",
        "/carddav/home/OTHER/",
    ] {
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with(&format!("REPORT {path} HTTP/1.1")))
                .count(),
            1,
            "{path} 應只查詢一次"
        );
    }
}

#[test]
fn company_sync_completeness_bfs_finds_leaves_missed_by_shallow_infinity() {
    let _password_guard = PASSWORD_LOCK.lock().expect("password lock");
    let principal = multistatus([principal_response("/carddav/home/")]);
    let shallow_home = multistatus([
        addressbook_response("/carddav/home/ACME/", "ACME(/ACME)"),
        addressbook_response("/carddav/home/OTHER/", "OTHER(/OTHER)"),
    ]);
    let root_children = multistatus([
        addressbook_response("/carddav/home/ACME/", "ACME(/ACME)"),
        addressbook_response("/carddav/home/ACME/PROD/", "PROD(/ACME/PROD)"),
    ]);
    let branch_children = multistatus([
        addressbook_response("/carddav/home/ACME/PROD/", "PROD(/ACME/PROD)"),
        addressbook_response(
            "/carddav/home/ACME/PROD/PROD_UI/",
            "PROD_UI(/ACME/PROD/PROD_UI)",
        ),
    ]);
    let other_children = multistatus([addressbook_response(
        "/carddav/home/OTHER/",
        "OTHER(/OTHER)",
    )]);
    let leaf = multistatus([addressbook_response(
        "/carddav/home/ACME/PROD/PROD_UI/",
        "PROD_UI(/ACME/PROD/PROD_UI)",
    )]);
    let empty = multistatus(Vec::<String>::new());
    let (address, handle) = spawn_server(vec![
        ResponseSpec { body: principal },
        ResponseSpec { body: shallow_home },
        ResponseSpec {
            body: root_children,
        },
        ResponseSpec {
            body: other_children,
        },
        ResponseSpec {
            body: branch_children,
        },
        ResponseSpec { body: leaf },
        ResponseSpec {
            body: empty.clone(),
        },
        ResponseSpec {
            body: empty.clone(),
        },
        ResponseSpec {
            body: empty.clone(),
        },
        ResponseSpec { body: empty },
    ]);
    let data_dir = setup_data_dir(address);
    std::env::set_var("M2K_KEYCHAIN", NO_KEYCHAIN);
    std::env::set_var("M2K_PASSWORD", "password");
    let result = m2k::directory::sync_contacts(
        data_dir.path(),
        SyncArgs {
            scope: DirectoryScope::Company,
            addressbook: None,
        },
    );
    std::env::remove_var("M2K_PASSWORD");
    let result = result.expect("shallow infinity completeness sync");
    assert_eq!(result.collections.len(), 4);
    assert!(result
        .scope_paths
        .iter()
        .any(|path| path == "ACME/PROD/PROD_UI"));
    let requests = handle.join().expect("join mock server");
    assert!(requests
        .iter()
        .any(|request| request.starts_with("PROPFIND /carddav/home/ACME/PROD/ HTTP/1.1")));
    assert!(requests.iter().any(|request| {
        request.starts_with("REPORT /carddav/home/ACME/PROD/PROD_UI/ HTTP/1.1")
    }));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("REPORT "))
            .count(),
        4
    );
}

#[test]
fn reusable_company_sync_checks_busy_lock_before_credentials_or_http() {
    let data_dir = TempDir::new().expect("temp directory");
    let _lock = m2k::sync::lock(data_dir.path()).expect("other sync lock");
    let result = m2k::directory::sync_contacts(
        data_dir.path(),
        SyncArgs {
            scope: DirectoryScope::Company,
            addressbook: None,
        },
    );
    let error = result.expect_err("busy sync must not start");
    assert!(error.to_string().contains("另一個同步"));
    assert!(!data_dir.path().join("database.sqlite3").exists());
}

#[test]
fn company_sync_falls_back_to_depth_one_and_finds_hidden_branch() {
    let _password_guard = PASSWORD_LOCK.lock().expect("password lock");
    let principal = multistatus([principal_response("/carddav/home/")]);
    let home = multistatus([
        addressbook_response("/carddav/home/ACME/", "ACME(/ACME)"),
        addressbook_response("/carddav/home/OTHER/", "OTHER(/OTHER)"),
    ]);
    let root = multistatus([
        addressbook_response("/carddav/home/ACME/", "ACME(/ACME)"),
        addressbook_response("/carddav/home/ACME/ENG/", "ENG(/ACME/ENG)"),
    ]);
    let other = multistatus([addressbook_response(
        "/carddav/home/OTHER/",
        "OTHER(/OTHER)",
    )]);
    let department = multistatus([
        addressbook_response("/carddav/home/ACME/ENG/", "ENG(/ACME/ENG)"),
        addressbook_response(
            "/carddav/home/ACME/ENG/ENG_TOOLS/",
            "ENG_TOOLS(/ACME/ENG/ENG_TOOLS)",
        ),
    ]);
    let leaf = multistatus([addressbook_response(
        "/carddav/home/ACME/ENG/ENG_TOOLS/",
        "ENG_TOOLS(/ACME/ENG/ENG_TOOLS)",
    )]);
    let empty = multistatus(Vec::<String>::new());
    let (address, handle) = spawn_server_with_status(vec![
        (
            207,
            ResponseSpec {
                body: principal.clone(),
            },
        ),
        (
            403,
            ResponseSpec {
                body: String::new(),
            },
        ),
        (207, ResponseSpec { body: principal }),
        (207, ResponseSpec { body: home }),
        (207, ResponseSpec { body: root }),
        (207, ResponseSpec { body: other }),
        (207, ResponseSpec { body: department }),
        (207, ResponseSpec { body: leaf }),
        (
            207,
            ResponseSpec {
                body: empty.clone(),
            },
        ),
        (
            207,
            ResponseSpec {
                body: empty.clone(),
            },
        ),
        (
            207,
            ResponseSpec {
                body: empty.clone(),
            },
        ),
        (207, ResponseSpec { body: empty }),
    ]);
    let data_dir = setup_data_dir(address);
    std::env::set_var("M2K_KEYCHAIN", NO_KEYCHAIN);
    std::env::set_var("M2K_PASSWORD", "password");
    let result = m2k::directory::execute(
        data_dir.path(),
        DirectoryCommand::Sync(SyncArgs {
            scope: DirectoryScope::Company,
            addressbook: None,
        }),
        true,
    );
    std::env::remove_var("M2K_PASSWORD");
    result.expect("company fallback sync");
    let requests = handle.join().expect("join mock server");
    assert_eq!(requests.len(), 12);
    assert!(requests[1].starts_with("PROPFIND /carddav/home/ HTTP/1.1"));
    assert!(requests[1].to_ascii_lowercase().contains("depth: infinity"));
    assert!(requests[3].starts_with("PROPFIND /carddav/home/ HTTP/1.1"));
    assert!(requests[3].to_ascii_lowercase().contains("depth: 1"));
    assert!(requests
        .iter()
        .any(|request| request.starts_with("REPORT /carddav/home/ACME/ENG/ENG_TOOLS/ HTTP/1.1")));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("REPORT "))
            .count(),
        4
    );
}

#[test]
fn department_sync_reports_only_selected_branch_and_preserves_sibling_memberships() {
    let _password_guard = PASSWORD_LOCK.lock().expect("password lock");
    let principal = multistatus([principal_response("/carddav/home/")]);
    let home = multistatus([
        addressbook_response("/carddav/home/ACME/", "ACME(/ACME)"),
        addressbook_response("/carddav/home/ACME/ENG/", "ENG(/ACME/ENG)"),
        addressbook_response(
            "/carddav/home/ACME/ENG/ENG_TOOLS/",
            "ENG_TOOLS(/ACME/ENG/ENG_TOOLS)",
        ),
        addressbook_response("/carddav/home/OTHER/", "OTHER(/OTHER)"),
    ]);
    let branch_contact = multistatus([contact_response("/carddav/home/ACME/ENG/department.vcf")]);
    let leaf_contact = multistatus([contact_response(
        "/carddav/home/ACME/ENG/ENG_TOOLS/department.vcf",
    )]);
    let empty = multistatus(Vec::<String>::new());
    let (address, handle) = spawn_server(vec![
        ResponseSpec { body: principal },
        ResponseSpec { body: home },
        ResponseSpec { body: empty },
        ResponseSpec {
            body: branch_contact,
        },
        ResponseSpec { body: leaf_contact },
    ]);
    let data_dir = setup_data_dir(address);
    let base = format!("http://{address}");
    let root_id = format!("{base}/carddav/home/ACME/");
    let department_id = format!("{base}/carddav/home/ACME/ENG/");
    let leaf_id = format!("{base}/carddav/home/ACME/ENG/ENG_TOOLS/");
    let sibling_id = format!("{base}/carddav/home/OTHER/");
    let mut store = Store::open(data_dir.path()).expect("open store");
    store
        .upsert_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: vec![
                    collection(&root_id, "ACME(/ACME)"),
                    collection(&department_id, "ENG(/ACME/ENG)"),
                    collection(&leaf_id, "ENG_TOOLS(/ACME/ENG/ENG_TOOLS)"),
                    collection(&sibling_id, "OTHER(/OTHER)"),
                ],
                items: vec![
                    contact(
                        "cached-self",
                        &leaf_id,
                        "self-1",
                        "王自動",
                        "user@example.test",
                    ),
                    contact(
                        "cached-sibling",
                        &sibling_id,
                        "sibling-1",
                        "同事",
                        "other@example.test",
                    ),
                ],
            },
            Utc::now(),
        )
        .expect("seed cache");
    std::env::set_var("M2K_KEYCHAIN", NO_KEYCHAIN);
    std::env::set_var("M2K_PASSWORD", "password");
    let result = m2k::directory::execute(
        data_dir.path(),
        DirectoryCommand::Sync(SyncArgs {
            scope: DirectoryScope::Department,
            addressbook: None,
        }),
        true,
    );
    let requests = handle.join().expect("join mock server");
    std::env::remove_var("M2K_PASSWORD");
    result.expect("department sync");
    assert_eq!(requests.len(), 5);
    assert!(requests[0].starts_with("PROPFIND /principals/user@example.test HTTP/1.1"));
    assert!(requests[1].starts_with("PROPFIND /carddav/home/ HTTP/1.1"));
    assert!(requests[2].starts_with("PROPFIND /carddav/home/ACME/ENG/ HTTP/1.1"));
    assert!(requests[2].to_ascii_lowercase().contains("depth: 1"));
    assert!(requests[3].starts_with("REPORT /carddav/home/ACME/ENG/ HTTP/1.1"));
    assert!(requests[4].starts_with("REPORT /carddav/home/ACME/ENG/ENG_TOOLS/ HTTP/1.1"));
    assert!(requests
        .iter()
        .all(|request| !request.starts_with("REPORT /carddav/home/ACME/ HTTP/1.1")));
    assert!(requests
        .iter()
        .all(|request| !request.starts_with("REPORT /carddav/home/OTHER/ HTTP/1.1")));

    let config = config::load(data_dir.path()).expect("load config");
    let store = Store::open(data_dir.path()).expect("reopen store");
    let nodes = address_book_tree(
        &store.collections(Kind::Contacts).expect("list collections"),
        Some(&config.carddav_url),
    );
    let contacts = merge_contact_items(
        &store
            .list(Kind::Contacts, None, None, usize::MAX)
            .expect("list contacts"),
        &nodes,
    );
    let self_contact = resolve_unique_contact(&contacts, "user@example.test")
        .expect("self contact remains resolvable");
    assert_eq!(self_contact.memberships.len(), 2);
    assert!(self_contact
        .memberships
        .iter()
        .any(|membership| membership.path == "ACME/ENG"));
    assert!(self_contact
        .memberships
        .iter()
        .any(|membership| membership.path == "ACME/ENG/ENG_TOOLS"));
    assert!(resolve_unique_contact(&contacts, "other@example.test").is_ok());
}

#[test]
fn snapshot_scope_rejects_sibling_collection() {
    let target = DirectoryNode {
        id: "http://127.0.0.1/carddav/home/ACME/".to_owned(),
        name: "ACME".to_owned(),
        display_name: "ACME(/ACME)".to_owned(),
        path: "ACME".to_owned(),
        parent_path: None,
        depth: 0,
    };
    let snapshot = Snapshot {
        kind: Kind::Contacts,
        collections: vec![collection(
            "http://127.0.0.1/carddav/home/OTHER/",
            "OTHER(/OTHER)",
        )],
        items: Vec::new(),
    };
    let error = validate_snapshot_scope(&snapshot, &[target]).expect_err("sibling must reject");
    assert!(error.to_string().contains("指定範圍外"));
}

#[test]
fn merge_skips_empty_org_and_keeps_structured_name_nickname() {
    let collections = vec![
        collection("https://mail.example.test/carddav/ACME/", "ACME(/ACME)"),
        collection(
            "https://mail.example.test/carddav/ACME/MKT/",
            "MKT(/ACME/MKT)",
        ),
    ];
    let nodes = address_book_tree(&collections, Some("https://mail.example.test/carddav/"));
    let mut first = contact(
        "a",
        &collections[0].id,
        "ZGVtb19jb250YWN0X2FAZXhhbXBsZS50ZXN0",
        "demo_contact_a",
        "demo_contact_a@example.test",
    );
    first.subtitle = ";;".into();
    first.phones = vec!["(00)55501".into()];
    first.raw = "BEGIN:VCARD\nVERSION:3.0\nUID:ZGVtb19jb250YWN0X2FAZXhhbXBsZS50ZXN0\nFN:demo_contact_a\nNICKNAME:demo_contact_a\nN:;Demo Contact A (範例聯絡人甲);;;\nORG:;;\nEMAIL:demo_contact_a@example.test\nTEL:(00)55501\nEND:VCARD\n".into();
    let mut second = first.clone();
    second.id = "b".into();
    second.collection = collections[1].id.clone();
    let merged = merge_contact_items(&[first, second], &nodes);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].name, "demo_contact_a");
    assert_eq!(merged[0].emails, vec!["demo_contact_a@example.test"]);
    assert_eq!(merged[0].phones, vec!["(00)55501"]);
    assert_eq!(merged[0].organization.as_deref(), Some("MKT"));
    assert_eq!(
        merged[0].structured_name.as_deref(),
        Some("Demo Contact A (範例聯絡人甲)")
    );
    assert_eq!(merged[0].memberships.len(), 2);
    let detail = format_contact_detail(&merged[0]);
    assert!(detail.contains("demo_contact_a"));
    assert!(detail.contains("顯示名：Demo Contact A (範例聯絡人甲)"));
    assert!(detail.contains("部門／組織：MKT"));
    assert!(detail.contains("Email：demo_contact_a@example.test"));
    assert!(detail.contains("電話：(00)55501"));
    assert!(detail.contains("資料夾／群組："));
    assert!(!detail.contains("本文沒有可顯示的文字"));
    assert!(!detail.contains(";;"));
    let item = contact_to_item(&merged[0]);
    assert_eq!(item.collection, "2 個資料夾");
    assert!(item.text.contains("電話：(00)55501"));
}

#[test]
fn contact_summaries_page_dedupes_across_folders_for_tui() {
    let temp = TempDir::new().unwrap();
    let mut store = Store::open(temp.path()).unwrap();
    let collections = vec![
        collection("https://mail.example.test/carddav/ACME/", "ACME(/ACME)"),
        collection(
            "https://mail.example.test/carddav/ACME/MKT/",
            "MKT(/ACME/MKT)",
        ),
        collection("https://mail.example.test/carddav/other/", "other(/other)"),
    ];
    let mut contact_a = contact(
        "contact-a",
        &collections[0].id,
        "uid-contact-a",
        "demo_contact_a",
        "demo_contact_a@example.test",
    );
    contact_a.phones = vec!["(00)55501".into()];
    contact_a.raw = "BEGIN:VCARD\nUID:uid-contact-a\nFN:demo_contact_a\nN:;Demo Contact A;;;\nEMAIL:demo_contact_a@example.test\nTEL:(00)55501\nEND:VCARD\n".into();
    let mut contact_b = contact_a.clone();
    contact_b.id = "contact-b".into();
    contact_b.collection = collections[1].id.clone();
    let other = contact(
        "other",
        &collections[2].id,
        "uid-other",
        "demo_test",
        "demo_test@example.test",
    );
    store
        .apply_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: collections.clone(),
                items: vec![contact_a, contact_b, other],
            },
            Utc::now(),
        )
        .unwrap();
    let page = contact_summaries_page(&store, Some("demo_contact_a"), None, 20, 0).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].title, "demo_contact_a");
    assert_eq!(page.items[0].collection, "2 個資料夾");
    assert_eq!(page.items[0].phones, vec!["(00)55501"]);
    assert!(page.items[0].text.contains("資料夾／群組："));
    assert!(!page.items[0].text.contains("本文沒有可顯示的文字"));
}

#[test]
fn department_fallback_uses_deepest_non_root_membership() {
    let memberships = vec![
        DirectoryMembership {
            collection: "https://mail.example.test/carddav/ACME/".into(),
            path: "ACME".into(),
            name: "ACME".into(),
        },
        DirectoryMembership {
            collection: "https://mail.example.test/carddav/ACME/PROD/".into(),
            path: "ACME/PROD".into(),
            name: "PROD".into(),
        },
        DirectoryMembership {
            collection: "https://mail.example.test/carddav/ACME/PROD/PROD_UI/".into(),
            path: "ACME/PROD/PROD_UI".into(),
            name: "PROD_UI".into(),
        },
    ];
    assert_eq!(
        department_from_memberships(&memberships).as_deref(),
        Some("PROD/PROD_UI")
    );
    assert_eq!(
        department_from_memberships(&memberships[..1]).as_deref(),
        None
    );
}

#[test]
fn empty_org_detail_shows_membership_department_not_empty_org() {
    let collections = vec![
        collection("https://mail.example.test/carddav/ACME/", "ACME(/ACME)"),
        collection(
            "https://mail.example.test/carddav/ACME/PROD/PROD_UI/",
            "PROD_UI(/ACME/PROD/PROD_UI)",
        ),
    ];
    let nodes = address_book_tree(&collections, Some("https://mail.example.test/carddav/"));
    let mut item = contact(
        "a",
        &collections[0].id,
        "uid-contact-b",
        "demo_contact_b",
        "demo_contact_b@example.test",
    );
    item.subtitle = ";;".into();
    item.raw = "BEGIN:VCARD\nVERSION:3.0\nUID:uid-contact-b\nFN:demo_contact_b\nN:;Demo Contact B (範例聯絡人乙);;;\nORG:;;\nEMAIL:demo_contact_b@example.test\nEND:VCARD\n".into();
    let mut leaf = item.clone();
    leaf.id = "b".into();
    leaf.collection = collections[1].id.clone();
    let merged = merge_contact_items(&[item, leaf], &nodes);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].organization.as_deref(), Some("PROD/PROD_UI"));
    let detail = format_contact_detail(&merged[0]);
    assert!(detail.contains("部門／組織：PROD/PROD_UI"));
    assert!(!detail.contains("ORG"));
    assert!(!detail.contains(";;"));
}
