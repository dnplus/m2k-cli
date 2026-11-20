use m2k::{
    config::Config,
    model::{Item, Kind},
    provider::mail::{attachment, raw_bytes},
};

fn item(raw: &str) -> Item {
    Item {
        id: "mail:INBOX:7:12".into(),
        kind: Kind::Mail,
        collection: "INBOX".into(),
        remote_id: "12".into(),
        etag: Some("7:12".into()),
        uid_validity: Some(7),
        title: "測試郵件".into(),
        subtitle: "sender@example.test".into(),
        date: None,
        end_date: None,
        text: "本文".into(),
        raw: raw.into(),
        flags: Vec::new(),
        emails: vec!["sender@example.test".into()],
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

#[test]
fn attachment_decodes_nested_base64_body_without_network() {
    let raw = concat!(
        "From: sender@example.test\r\n",
        "To: receiver@example.test\r\n",
        "Subject: test\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=outer\r\n",
        "\r\n",
        "--outer\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "\r\n",
        "本文\r\n",
        "--outer\r\n",
        "Content-Type: multipart/alternative; boundary=inner\r\n",
        "\r\n",
        "--inner\r\n",
        "Content-Type: text/html\r\n",
        "\r\n",
        "<p>本文</p>\r\n",
        "--inner--\r\n",
        "--outer\r\n",
        "Content-Type: application/octet-stream\r\n",
        "Content-Disposition: attachment; filename=report.bin\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "AAEC/w==\r\n",
        "--outer--\r\n",
    );
    let (filename, body) = attachment(&item(raw), 0).expect("attachment should decode");
    assert_eq!(filename, "report.bin");
    assert_eq!(body, vec![0, 1, 2, 255]);
}

#[test]
fn attachment_preserves_attached_multipart_parent() {
    let raw = concat!(
        "Content-Type: multipart/mixed; boundary=outer\r\n",
        "\r\n",
        "--outer\r\n",
        "Content-Type: multipart/mixed; boundary=inner\r\n",
        "Content-Disposition: attachment; filename=bundle.eml\r\n",
        "\r\n",
        "--inner\r\n",
        "Content-Type: text/plain\r\n",
        "Content-Disposition: attachment; filename=child.txt\r\n",
        "\r\n",
        "child\r\n",
        "--inner--\r\n",
        "--outer--\r\n",
    );
    let (filename, bytes) = attachment(&item(raw), 0).expect("multipart attachment should remain");
    assert_eq!(filename, "bundle.eml");
    assert!(bytes
        .windows(b"child.txt".len())
        .any(|window| window == b"child.txt"));
}

#[test]
fn attachment_rejects_missing_index() {
    let raw = "Content-Type: text/plain\r\n\r\nbody\r\n";
    let error = attachment(&item(raw), 0).expect_err("missing attachment should fail");
    assert!(error.to_string().contains("找不到附件索引"));
}

#[test]
fn attachment_rejects_non_mail_item() {
    let mut calendar = item("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n");
    calendar.kind = Kind::Calendar;
    let error = attachment(&calendar, 0).expect_err("calendar cannot have mail attachment");
    assert!(error.to_string().contains("不是郵件"));
}

#[test]
fn raw_bytes_decodes_binary_rfc822_cache() {
    let mut cached = item("");
    cached.raw = "base64:SGVhZGVyOiB4DQoNCgD/".into();
    let bytes = raw_bytes(&cached).expect("encoded raw should decode");
    assert_eq!(bytes, b"Header: x\r\n\r\n\x00\xff");
}

#[test]
fn invite_rejects_non_request_before_network() {
    let config = Config::new("user@example.com".into(), "mail.example.test").unwrap();
    let ics = concat!(
        "BEGIN:VCALENDAR\r\n",
        "VERSION:2.0\r\n",
        "METHOD:PUBLISH\r\n",
        "BEGIN:VEVENT\r\n",
        "SUMMARY:測試\r\n",
        "ATTENDEE:MAILTO:guest@example.com\r\n",
        "END:VEVENT\r\n",
        "END:VCALENDAR\r\n",
    );
    let dir = tempfile::tempdir().unwrap();
    m2k::config::save(dir.path(), &config).unwrap();
    let file = dir.path().join("invite.ics");
    std::fs::write(&file, ics).unwrap();
    let error = m2k::mail_draft::invite(dir.path(), &file).expect_err("publish must be rejected");
    assert!(error.to_string().contains("REQUEST"));
}

#[test]
fn invite_builds_request_before_transport_validation() {
    let mut config = Config::new("user@example.com".into(), "mail.example.test").unwrap();
    config.smtp_port = 25;
    let ics = concat!(
        "BEGIN:VCALENDAR\r\n",
        "VERSION:2.0\r\n",
        "METHOD:REQUEST\r\n",
        "BEGIN:VEVENT\r\n",
        "SUMMARY:測試會議\r\n",
        "ATTENDEE;CN=Guest:MAILTO:guest@example.com\r\n",
        "END:VEVENT\r\n",
        "END:VCALENDAR\r\n",
    );
    let dir = tempfile::tempdir().unwrap();
    m2k::config::save(dir.path(), &config).unwrap();
    let file = dir.path().join("invite.ics");
    std::fs::write(&file, ics).unwrap();
    let id = m2k::mail_draft::invite(dir.path(), &file).expect("draft needs no transport");
    assert_eq!(
        m2k::mail_draft::show(dir.path(), &id).unwrap().state,
        "draft"
    );
}

#[test]
fn forward_rejects_non_mail_before_network() {
    let config = Config::new("user@example.com".into(), "mail.example.test").unwrap();
    let mut calendar = item("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n");
    calendar.kind = Kind::Calendar;
    let dir = tempfile::tempdir().unwrap();
    m2k::config::save(dir.path(), &config).unwrap();
    let mut store = m2k::store::Store::open(dir.path()).unwrap();
    store.ensure_account(&config).unwrap();
    store.upsert(&calendar).unwrap();
    let error = m2k::mail_action::draft_forward(
        dir.path(),
        &calendar.id,
        &["guest@example.com".into()],
        || Ok(String::new()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("資料類型不符"));
}
