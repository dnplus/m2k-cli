use base64::{engine::general_purpose::STANDARD, Engine};
use m2k::{
    model::{Attachment, Item, Kind},
    provider::mail::{display_collection, display_header, normalize_cached},
};

fn item(raw: &str) -> Item {
    Item {
        id: "mail:INBOX:7:12".into(),
        kind: Kind::Mail,
        collection: "INBOX".into(),
        remote_id: "12".into(),
        etag: Some("7:12".into()),
        uid_validity: Some(7),
        title: "existing title".into(),
        subtitle: "sender@example.test".into(),
        date: Some("2026-10-07T01:00:00Z".into()),
        end_date: None,
        text: String::new(),
        raw: format!("base64:{}", STANDARD.encode(raw)),
        flags: vec!["\\Seen".into()],
        emails: vec!["sender@example.test".into()],
        phones: Vec::new(),
        attachments: vec![Attachment {
            index: 0,
            filename: "existing.bin".into(),
            content_type: "application/octet-stream".into(),
            size: 4,
        }],
    }
}

#[test]
fn cached_nested_plain_repairs_text_and_headers_without_changing_identity() {
    let raw = concat!(
        "Subject: =?UTF-8?Q?=E6=B8=AC=E8=A9=A6?=\r\n",
        "From: =?UTF-8?B?5ris6Kmm?= <sender@example.test>\r\n",
        "Content-Type: multipart/mixed; boundary=outer\r\n\r\n",
        "--outer\r\nContent-Type: multipart/alternative; boundary=inner\r\n\r\n",
        "--inner\r\nContent-Type: text/plain; charset=utf-8\r\n",
        "Content-Transfer-Encoding: quoted-printable\r\n\r\n",
        "Hi =E5=90=84=E4=BD=8D\r\nSynthetic text.\r\n",
        "--inner--\r\n--outer--\r\n",
    );
    let mut cached = item(raw);
    let mut expected = cached.clone();
    assert!(normalize_cached(&mut cached).unwrap());
    assert_eq!(cached.title, "測試");
    assert_eq!(cached.subtitle, "測試 <sender@example.test>");
    assert!(cached.text.starts_with("Hi 各位\r\nSynthetic text."));
    expected.title = cached.title.clone();
    expected.subtitle = cached.subtitle.clone();
    expected.text = cached.text.clone();
    assert_eq!(
        serde_json::to_value(&cached).unwrap(),
        serde_json::to_value(&expected).unwrap(),
        "normalization must preserve raw, flags, dates, attachments and all identities"
    );
    assert!(
        !normalize_cached(&mut cached).unwrap(),
        "repair is idempotent"
    );
}

#[test]
fn nonempty_base64_plain_beats_html_empty_plain_and_text_attachment() {
    let plain = STANDARD.encode("Useful 中文\nSecond line");
    let raw = format!(
        "Content-Type: multipart/mixed; boundary=outer\r\n\r\n\
         --outer\r\nContent-Type: text/plain\r\n\
         Content-Disposition: attachment; filename=ignore.txt\r\n\r\nAttachment text\r\n\
         --outer\r\nContent-Type: text/html\r\n\r\n<p>HTML fallback</p>\r\n\
         --outer\r\nContent-Type: multipart/alternative; boundary=inner\r\n\r\n\
         --inner\r\nContent-Type: text/plain\r\n\r\n  \r\n\
         --inner\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n{plain}\r\n\
         --inner--\r\n--outer--\r\n"
    );
    let mut cached = item(&raw);
    assert!(normalize_cached(&mut cached).unwrap());
    assert_eq!(cached.text, "Useful 中文\nSecond line");
}

#[test]
fn html_fallback_is_readable_text_without_markup_script_or_style() {
    let raw = concat!(
        "Content-Type: multipart/alternative; boundary=outer\r\n\r\n",
        "--outer\r\nContent-Type: text/plain\r\n\r\n \r\n",
        "--outer\r\nContent-Type: multipart/related; boundary=inner\r\n\r\n",
        "--inner\r\nContent-Type: text/html; charset=utf-8\r\n\r\n",
        "<!doctype html><html><head><style>hidden style</style></head><body>",
        "<p title=\"a > b\">Hello&nbsp;&amp; &#20013;&#x6587;</p>",
        "<script>hidden script</script><!-- hidden comment -->",
        "<p>Second&lt;tag&gt;<br/>Next&#39;s</p></body></html>\r\n",
        "--inner--\r\n--outer--\r\n",
    );
    let mut cached = item(raw);
    assert!(normalize_cached(&mut cached).unwrap());
    assert_eq!(cached.text, "Hello & 中文\nSecond<tag>\nNext's");
}

#[test]
fn empty_html_does_not_hide_a_later_useful_html_part() {
    let raw = concat!(
        "Content-Type: multipart/mixed; boundary=outer\r\n\r\n",
        "--outer\r\nContent-Type: text/html\r\n\r\n<style>hidden</style>\r\n",
        "--outer\r\nContent-Type: text/html\r\n\r\n<p>Visible</p>\r\n",
        "--outer--\r\n",
    );
    let mut cached = item(raw);
    assert!(normalize_cached(&mut cached).unwrap());
    assert_eq!(cached.text, "Visible");
}

#[test]
fn useful_cached_body_skips_raw_parsing_but_decodes_legacy_headers() {
    let mut cached = item("");
    cached.raw = "base64:invalid".into();
    cached.text = "Preserve useful text".into();
    cached.title = "=?UTF-8?Q?=E6=B8=AC=E8=A9=A6?=".into();
    cached.subtitle = "=?UTF-8?B?5ris6Kmm?= <sender@example.test>".into();
    assert!(normalize_cached(&mut cached).unwrap());
    assert_eq!(cached.title, "測試");
    assert_eq!(cached.subtitle, "測試 <sender@example.test>");
    assert_eq!(cached.text, "Preserve useful text");
    assert_eq!(cached.raw, "base64:invalid");
}

#[test]
fn headerless_legacy_raw_does_not_replace_existing_metadata_or_body() {
    let mut cached = item("cached body");
    let original = serde_json::to_value(&cached).unwrap();
    assert!(!normalize_cached(&mut cached).unwrap());
    assert_eq!(serde_json::to_value(cached).unwrap(), original);
}

#[test]
fn malformed_raw_does_not_partially_mutate_cached_item() {
    let mut cached = item("");
    cached.raw = "base64:invalid".into();
    cached.title = "=?UTF-8?B?5ris6Kmm?=".into();
    let original = serde_json::to_value(&cached).unwrap();
    assert!(normalize_cached(&mut cached).is_err());
    assert_eq!(serde_json::to_value(cached).unwrap(), original);
}

#[test]
fn normalization_leaves_non_mail_items_unchanged() {
    let mut cached = item("Subject: mail\r\n\r\nbody");
    cached.kind = Kind::Calendar;
    let original = serde_json::to_value(&cached).unwrap();
    assert!(!normalize_cached(&mut cached).unwrap());
    assert_eq!(serde_json::to_value(cached).unwrap(), original);
}

#[test]
fn display_header_decodes_q_b_folded_words_and_preserves_plain_text() {
    assert_eq!(display_header("=?UTF-8?Q?J=C3=B6rg?="), "Jörg");
    assert_eq!(display_header("=?UTF-8?B?5ris6Kmm?="), "測試");
    assert_eq!(
        display_header("=?UTF-8?Q?=E6=B8=AC?=\r\n =?UTF-8?B?6Kmm?="),
        "測試"
    );
    assert_eq!(
        display_header("plain 中文 <sender@example.test>"),
        "plain 中文 <sender@example.test>"
    );
}

#[test]
fn display_header_decodes_flush_plain_text_and_concatenated_words() {
    assert_eq!(
        display_header(
            "Re: =?utf-8?Q?=E8=AC=B9=E6=8F=90=E4=BE=9B_=E7=AF=84=E4=BE=8B=E5=B0=88=E6=A1=88_?=Alpha demo"
        ),
        "Re: 謹提供 範例專案 Alpha demo"
    );
    assert_eq!(
        display_header("Team Offsite =?utf-8?Q?=E4=B8=BB=E9=A1=8C_?=rehearsal"),
        "Team Offsite 主題 rehearsal"
    );
    assert_eq!(
        display_header("Fwd: 2026 Q1 =?utf-8?Q?=E5=AD=A3=E5=BA=A6=E7=B6=AD=E8=AD=B7_?=- confirm"),
        "Fwd: 2026 Q1 季度維護 - confirm"
    );
    assert_eq!(
        display_header("=?UTF-8?B?57O757Wx6YCa55+l77ya5LiL6LyJ5a6M5oiQ?==?UTF-8?B??="),
        "系統通知：下載完成"
    );
    assert_eq!(display_header("=?utf-8?B?5ris6Kmm?="), "測試");
}

#[test]
fn modified_utf7_decodes_rfc_mailbox_example_ampersand_and_surrogate_pair() {
    assert_eq!(
        display_collection("~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
        "~peter/mail/台北/日本語"
    );
    assert_eq!(display_collection("Sent&-Drafts"), "Sent&Drafts");
    assert_eq!(display_collection("&2D3eAA-"), "😀");
}

#[test]
fn modified_utf7_preserves_plain_or_malformed_names() {
    for name in [
        "INBOX",
        "Archive",
        "普通資料夾",
        "R&D",
        "&abc",
        "&A-",
        "&AA-",
        "&AAA-",
        "&2AA-",
        "&QQ==-",
        "&AEE-",
        "&ZeVnLIqe/",
    ] {
        assert_eq!(display_collection(name), name);
    }
}
