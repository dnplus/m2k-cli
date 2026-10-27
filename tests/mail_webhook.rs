use chrono::Utc;
use m2k::{
    config::{self, Config},
    mail_webhook::{Delivery, Subscription},
    model::{Item, Kind},
    provider::mail::watch::{Cursor, Delta},
    store::Store,
};
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::atomic::AtomicBool,
    thread,
};

fn fixture() -> (tempfile::TempDir, Config, Store) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::new("person@example.test".into(), "mail.example.test").unwrap();
    config::save(dir.path(), &cfg).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    (dir, cfg, store)
}

fn mail(uid: u32) -> Item {
    Item {
        id: format!("mail:INBOX:7:{uid}"),
        kind: Kind::Mail,
        collection: "INBOX".into(),
        remote_id: uid.to_string(),
        uid_validity: Some(7),
        etag: None,
        title: "增量郵件".into(),
        subtitle: "sender@example.test".into(),
        date: Some("2026-10-09T02:00:00Z".into()),
        end_date: None,
        text: "新正文，含歷史引用".into(),
        raw: "RFC822 preserved locally".into(),
        flags: vec![],
        emails: vec!["sender@example.test".into()],
        phones: vec![],
        attachments: vec![],
    }
}

fn baseline(store: &mut Store, sub: &Subscription) -> Cursor {
    let cursor = Cursor {
        uid_validity: 7,
        last_uid: 10,
    };
    store
        .record_mail_delta(
            sub,
            None,
            &Delta {
                cursor,
                items: vec![],
                baseline: true,
                has_more: false,
                parse_errors: Default::default(),
            },
            Utc::now(),
        )
        .unwrap();
    cursor
}

type HttpRequests = Vec<(String, Vec<u8>)>;

fn http_receiver(statuses: Vec<&'static str>) -> (String, thread::JoinHandle<HttpRequests>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let receiver = thread::spawn(move || {
        let mut requests = Vec::new();
        for status in statuses {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut header = Vec::new();
            let mut byte = [0];
            while !header.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap();
            let length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            requests.push((header, body));
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        }
        requests
    });
    (url, receiver)
}

#[test]
fn failed_event_does_not_delay_other_due_events() {
    let (url, receiver) = http_receiver(vec!["400 Bad Request", "204 No Content"]);
    let (_dir, cfg, mut store) = fixture();
    let sub = Subscription::new(&cfg, "INBOX", &url).unwrap();
    let old = baseline(&mut store, &sub);
    store
        .record_mail_delta(
            &sub,
            Some(old),
            &Delta {
                cursor: Cursor {
                    uid_validity: 7,
                    last_uid: 12,
                },
                items: vec![mail(11), mail(12)],
                baseline: false,
                has_more: false,
                parse_errors: Default::default(),
            },
            Utc::now(),
        )
        .unwrap();
    let receipt = Delivery::new(None)
        .unwrap()
        .drain(&mut store, &sub, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(receipt["delivered"], 1);
    assert_eq!(receipt["error"]["message"], "webhook HTTP 400");
    let status = store.mail_webhook_status().unwrap();
    assert_eq!(status[0]["pending"], 1);
    assert_eq!(status[0]["delivered"], 1);
    let requests = receiver.join().unwrap();
    assert_eq!(requests.len(), 2);
    let first: Value = serde_json::from_slice(&requests[0].1).unwrap();
    let second: Value = serde_json::from_slice(&requests[1].1).unwrap();
    assert_ne!(first["event_id"], second["event_id"]);
}

#[test]
fn webhook_failure_survives_restart_and_retries_identical_event_until_success() {
    let (url, receiver) = http_receiver(vec!["500 Internal Server Error", "204 No Content"]);
    let (dir, cfg, mut store) = fixture();
    let sub = Subscription::new(&cfg, "INBOX", &url).unwrap();
    let old = baseline(&mut store, &sub);
    store.upsert(&mail(11)).unwrap();
    store
        .record_mail_delta(
            &sub,
            Some(old),
            &Delta {
                cursor: Cursor {
                    uid_validity: 7,
                    last_uid: 11,
                },
                items: vec![mail(11)],
                baseline: false,
                has_more: false,
                parse_errors: Default::default(),
            },
            Utc::now(),
        )
        .unwrap();
    let pending = store
        .mail_webhook_pending(&sub, Utc::now())
        .unwrap()
        .unwrap();
    let payload: Value = serde_json::from_str(&pending.1).unwrap();
    assert_eq!(payload["event"], "mail.received");
    assert_eq!(payload["mail"]["raw"], "");
    assert_eq!(payload["uid"], 11);
    assert_eq!(
        store.get("mail:INBOX:7:11").unwrap().raw,
        "RFC822 preserved locally"
    );
    let mut retention = cfg.clone();
    retention.mail_retention_days = 1;
    assert_eq!(
        store
            .prune_kind(
                &retention,
                Utc::now() + chrono::Duration::days(2),
                Kind::Mail
            )
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .mail_webhook_pending(&sub, Utc::now())
            .unwrap()
            .unwrap()
            .1,
        pending.1
    );
    let delivery = Delivery::new(Some("fixture-only-token".into())).unwrap();
    let receipt = delivery
        .drain(&mut store, &sub, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(receipt["error"]["code"], "webhook_delivery_failed");
    assert_eq!(receipt["error"]["retry_after_seconds"], 5);
    assert_eq!(
        store.mail_webhook_status().unwrap()[0]["last_error"],
        "webhook HTTP 500"
    );
    assert_eq!(
        delivery
            .drain(&mut store, &sub, &AtomicBool::new(false))
            .unwrap()["delivered"],
        0
    );
    drop(store);
    let mut store = Store::open(dir.path()).unwrap();
    assert_eq!(
        store.mail_webhook_cursor(&sub).unwrap().unwrap().last_uid,
        11
    );
    let preserved = store
        .mail_webhook_pending(&sub, Utc::now() + chrono::Duration::minutes(10))
        .unwrap()
        .unwrap();
    assert_eq!(preserved.0, pending.0);
    assert_eq!(preserved.1, pending.1);
    assert_eq!(preserved.2, 1);
    store
        .mail_webhook_failed(
            &sub,
            &pending.0,
            "fixture time advance",
            Utc::now() - chrono::Duration::seconds(1),
        )
        .unwrap();
    assert_eq!(
        delivery
            .drain(&mut store, &sub, &AtomicBool::new(false))
            .unwrap()["delivered"],
        1
    );
    assert!(store
        .mail_webhook_pending(&sub, Utc::now())
        .unwrap()
        .is_none());
    store.mail_webhook_delivered(&sub, &pending.0).unwrap();
    let status = store.mail_webhook_status().unwrap();
    assert_eq!(status[0]["delivered"], 1);
    assert_eq!(status[0]["pending"], 0);
    let requests = receiver.join().unwrap();
    assert_eq!(requests[0].1, requests[1].1);
    for (header, _) in requests {
        let header = header.to_ascii_lowercase();
        assert!(header.contains("authorization: bearer fixture-only-token"));
        assert!(header.contains(&format!("idempotency-key: {}", pending.0)));
    }
    let other = Config::new("other@example.test".into(), "mail.example.test").unwrap();
    assert!(store.ensure_account(&other).is_err());
}

#[test]
fn invalid_delta_rolls_back_mail_event_and_cursor_together() {
    let (_dir, cfg, mut store) = fixture();
    let sub = Subscription::new(&cfg, "INBOX", "http://127.0.0.1:1/hook").unwrap();
    let old = baseline(&mut store, &sub);
    let mut invalid = mail(12);
    invalid.collection = "other-folder".into();
    let delta = Delta {
        cursor: Cursor {
            uid_validity: 7,
            last_uid: 12,
        },
        items: vec![mail(11), invalid],
        baseline: false,
        has_more: false,
        parse_errors: Default::default(),
    };
    assert!(store
        .record_mail_delta(&sub, Some(old), &delta, Utc::now())
        .is_err());
    assert_eq!(store.mail_webhook_cursor(&sub).unwrap(), Some(old));
    assert!(store.get("mail:INBOX:7:11").is_err());
    assert!(store
        .mail_webhook_pending(&sub, Utc::now())
        .unwrap()
        .is_none());
    let missing = Delta {
        cursor: Cursor {
            uid_validity: 7,
            last_uid: 12,
        },
        items: vec![],
        baseline: false,
        has_more: false,
        parse_errors: Default::default(),
    };
    assert!(store
        .record_mail_delta(&sub, Some(old), &missing, Utc::now())
        .is_err());
    let reset = Delta {
        cursor: Cursor {
            uid_validity: 7,
            last_uid: 12,
        },
        items: vec![],
        baseline: true,
        has_more: false,
        parse_errors: Default::default(),
    };
    assert!(store
        .record_mail_delta(&sub, Some(old), &reset, Utc::now())
        .is_err());
    let fresh = Subscription::new(&cfg, "INBOX", "http://127.0.0.1:1/other").unwrap();
    for items in [vec![], vec![mail(11)]] {
        assert!(store
            .record_mail_delta(
                &fresh,
                None,
                &Delta {
                    cursor: Cursor {
                        uid_validity: 7,
                        last_uid: 11
                    },
                    items,
                    baseline: false,
                    has_more: false,
                    parse_errors: Default::default()
                },
                Utc::now()
            )
            .is_err());
    }
    assert_eq!(store.mail_webhook_cursor(&fresh).unwrap(), None);
}

#[test]
fn subscriptions_isolate_accounts_folders_destinations_and_reject_invalid_input() {
    let (_dir, cfg, _store) = fixture();
    let first = Subscription::new(&cfg, "INBOX", "https://receiver.example.test/hook").unwrap();
    let other = Config::new("other@example.test".into(), "mail.example.test").unwrap();
    assert_ne!(
        first.id,
        Subscription::new(&other, "INBOX", &first.url).unwrap().id
    );
    assert_ne!(
        first.id,
        Subscription::new(&cfg, "Archive", &first.url).unwrap().id
    );
    assert_ne!(
        first.id,
        Subscription::new(&cfg, "INBOX", "https://another.example.test/hook")
            .unwrap()
            .id
    );
    for url in [
        "file:///tmp/webhook",
        "https://name:password@example.test/hook",
        "https://example.test/hook#fragment",
    ] {
        assert!(Subscription::new(&cfg, "INBOX", url).is_err());
    }
    assert!(Subscription::new(&cfg, "", &first.url).is_err());
    assert!(Delivery::new(Some("invalid\r\ntoken".into())).is_err());
}

#[test]
fn destination_queues_remain_independent_and_stale_cursor_cannot_skip_mail() {
    let (_dir, cfg, mut store) = fixture();
    let first = Subscription::new(&cfg, "INBOX", "https://one.example.test/hook").unwrap();
    let second = Subscription::new(&cfg, "INBOX", "https://two.example.test/hook").unwrap();
    let old = baseline(&mut store, &first);
    baseline(&mut store, &second);
    let delta = Delta {
        cursor: Cursor {
            uid_validity: 7,
            last_uid: 11,
        },
        items: vec![mail(11)],
        baseline: false,
        has_more: false,
        parse_errors: Default::default(),
    };
    for sub in [&first, &second] {
        store
            .record_mail_delta(sub, Some(old), &delta, Utc::now())
            .unwrap();
    }
    let first_event = store
        .mail_webhook_pending(&first, Utc::now())
        .unwrap()
        .unwrap();
    let second_event = store
        .mail_webhook_pending(&second, Utc::now())
        .unwrap()
        .unwrap();
    assert_eq!(first_event.0, second_event.0);
    assert!(store
        .record_mail_delta(&first, Some(old), &delta, Utc::now())
        .is_err());
    store
        .mail_webhook_delivered(&first, &first_event.0)
        .unwrap();
    assert!(store
        .mail_webhook_pending(&first, Utc::now())
        .unwrap()
        .is_none());
    assert!(store
        .mail_webhook_pending(&second, Utc::now())
        .unwrap()
        .is_some());
    assert_eq!(
        store.mail_webhook_cursor(&first).unwrap(),
        Some(delta.cursor)
    );
}

#[test]
fn schema_two_upgrade_preserves_contact_projection_and_adds_durable_watch_state() {
    let (dir, cfg, mut store) = fixture();
    let mut contact = mail(1);
    contact.id = "contact-1".into();
    contact.kind = Kind::Contacts;
    contact.collection = "directory".into();
    contact.uid_validity = None;
    contact.raw =
        "BEGIN:VCARD\nVERSION:3.0\nUID:person-1\nFN:王小明\nEMAIL:ming@example.test\nEND:VCARD\n"
            .into();
    store.upsert(&contact).unwrap();
    let before =
        serde_json::to_value(store.contact_page(None, None, 10, 0).unwrap().items).unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(dir.path().join("database.sqlite3")).unwrap();
    conn.execute_batch("DROP TABLE mail_webhook_events; DROP TABLE mail_webhook_subscriptions; PRAGMA user_version=2; UPDATE items SET flags_json='broken raw projection' WHERE kind='contacts';").unwrap();
    drop(conn);
    let mut store = Store::open(dir.path()).unwrap();
    store.ensure_account(&cfg).unwrap();
    assert_eq!(
        serde_json::to_value(store.contact_page(None, None, 10, 0).unwrap().items).unwrap(),
        before
    );
    let sub = Subscription::new(&cfg, "INBOX", "https://receiver.example.test/hook").unwrap();
    let old = baseline(&mut store, &sub);
    drop(store);
    let store = Store::open_reader(dir.path()).unwrap();
    assert_eq!(store.mail_webhook_cursor(&sub).unwrap(), Some(old));
    assert_eq!(store.mail_webhook_status().unwrap()[0]["pending"], 0);
}

#[test]
fn mail_watch_cli_help_and_demo_guard_are_available() {
    let dir = tempfile::tempdir().unwrap();
    m2k::demo::seed(dir.path()).unwrap();
    let executable = env!("CARGO_BIN_EXE_m2k");
    let help = std::process::Command::new(executable)
        .args(["mail", "watch", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--webhook-url"));
    let output = std::process::Command::new(executable)
        .arg("--data-dir")
        .arg(dir.path())
        .args([
            "--json",
            "mail",
            "watch",
            "--webhook-url",
            "http://127.0.0.1:1/hook",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(error["error"]["message"].as_str().unwrap().contains("展示"));
}

fn queue_one(store: &mut Store, sub: &Subscription) {
    let old = baseline(store, sub);
    store.upsert(&mail(11)).unwrap();
    store
        .record_mail_delta(
            sub,
            Some(old),
            &Delta {
                cursor: Cursor {
                    uid_validity: 7,
                    last_uid: 11,
                },
                items: vec![mail(11)],
                baseline: false,
                has_more: false,
                parse_errors: Default::default(),
            },
            Utc::now(),
        )
        .unwrap();
}

fn header_value(header: &str, name: &str) -> Option<String> {
    header.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

/// 接收端獨立的驗證實作：與 README 範例相同的演算法。
fn receiver_verifies(secret: &[u8], header: &str, body: &[u8]) -> i64 {
    use hmac::{Hmac, Mac};
    let timestamp: i64 = header_value(header, "x-m2k-timestamp")
        .expect("timestamp header")
        .parse()
        .unwrap();
    let signature = header_value(header, "x-m2k-signature").expect("signature header");
    let hex = signature.strip_prefix("sha256=").expect("sha256= prefix");
    let provided: Vec<u8> = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret).unwrap();
    mac.update(format!("{timestamp}.").as_bytes());
    mac.update(body);
    mac.verify_slice(&provided).expect("signature must verify");
    timestamp
}

#[test]
fn signed_delivery_verifies_and_resigns_on_retry_with_same_body_and_event_id() {
    const SECRET: &str = "fixture-only-signing-secret";
    let (url, receiver) = http_receiver(vec!["500 Internal Server Error", "204 No Content"]);
    let (dir, cfg, mut store) = fixture();
    let sub = Subscription::new(&cfg, "INBOX", &url).unwrap();
    queue_one(&mut store, &sub);
    let delivery = Delivery::new(None)
        .unwrap()
        .with_secret(Some(SECRET.as_bytes().to_vec()));
    let receipt = delivery
        .drain(&mut store, &sub, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(receipt["error"]["code"], "webhook_delivery_failed");
    let event_id = receipt["error"]["event_id"].as_str().unwrap().to_string();
    // 時間戳以秒計：等到下一秒，才能證明重送使用重送當下的時間。
    thread::sleep(std::time::Duration::from_millis(1100));
    store
        .mail_webhook_failed(
            &sub,
            &event_id,
            "fixture time advance",
            Utc::now() - chrono::Duration::seconds(1),
        )
        .unwrap();
    let receipt = delivery
        .drain(&mut store, &sub, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(receipt["delivered"], 1);
    let requests = receiver.join().unwrap();
    assert_eq!(requests[0].1, requests[1].1, "body must not change");
    let first = receiver_verifies(SECRET.as_bytes(), &requests[0].0, &requests[0].1);
    let second = receiver_verifies(SECRET.as_bytes(), &requests[1].0, &requests[1].1);
    assert!(
        second > first,
        "retry must be signed with a fresh timestamp"
    );
    assert_ne!(
        header_value(&requests[0].0, "x-m2k-signature"),
        header_value(&requests[1].0, "x-m2k-signature")
    );
    for (header, body) in &requests {
        assert_eq!(
            header_value(header, "idempotency-key").as_deref(),
            Some(event_id.as_str())
        );
        assert!(!header.contains(SECRET));
        assert!(!String::from_utf8_lossy(body).contains(SECRET));
    }
    // secret 不進 SQLite、狀態輸出或錯誤訊息。
    let status = store.mail_webhook_status().unwrap().to_string();
    assert!(!status.contains(SECRET));
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()),
                "{path:?} leaks the secret"
            );
        }
    }
}

#[test]
fn unsigned_delivery_has_no_signature_headers() {
    let (url, receiver) = http_receiver(vec!["204 No Content"]);
    let (_dir, cfg, mut store) = fixture();
    let sub = Subscription::new(&cfg, "INBOX", &url).unwrap();
    queue_one(&mut store, &sub);
    let delivery = Delivery::new(None).unwrap().with_secret(None);
    assert_eq!(
        delivery
            .drain(&mut store, &sub, &AtomicBool::new(false))
            .unwrap()["delivered"],
        1
    );
    let requests = receiver.join().unwrap();
    let header = requests[0].0.to_ascii_lowercase();
    assert!(!header.contains("x-m2k-signature"));
    assert!(!header.contains("x-m2k-timestamp"));
    assert!(header.contains("idempotency-key"));
}

#[test]
fn status_reports_signing_enabled_without_exposing_the_secret() {
    const SECRET: &str = "fixture-only-status-secret";
    let (dir, _cfg, _store) = fixture();
    let run = |secret: Option<&str>| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_m2k"));
        command
            .arg("--data-dir")
            .arg(dir.path())
            .args(["--json", "status"])
            .env_remove("M2K_WEBHOOK_SECRET");
        if let Some(secret) = secret {
            command.env("M2K_WEBHOOK_SECRET", secret);
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains(SECRET));
        serde_json::from_str::<Value>(&text).unwrap()
    };
    assert_eq!(run(None)["webhook_signing"], false);
    assert_eq!(run(Some(SECRET))["webhook_signing"], true);
}
