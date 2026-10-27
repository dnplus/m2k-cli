use chrono::{TimeZone, Utc};
use m2k::{
    demo,
    meeting_rsvp::{build_rsvp_reply, invite_from_mail, RsvpResponse},
    model::{Item, Kind},
    store::Store,
};
use serde_json::Value;
use std::process::Command;

// A keychain file that does not exist: m2k must never read or write the
// login keychain from tests.
const NO_KEYCHAIN: &str = "/nonexistent/m2k-test.keychain-db";

const ME: &str = "alice@example.test";

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 19, 1, 2, 3).unwrap()
}

/// Outlook-style request: TZID + VTIMEZONE, folded lines, other attendees,
/// RSVP=TRUE and a VALARM that must not leak into the reply.
const TAIPEI_REQUEST: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Example//Outlook//EN\r\n\
METHOD:REQUEST\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:Asia/Taipei\r\n\
BEGIN:STANDARD\r\n\
DTSTART:19700101T000000\r\n\
TZOFFSETFROM:+0800\r\n\
TZOFFSETTO:+0800\r\n\
END:STANDARD\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\n\
UID:weekly-sync-42@example.test\r\n\
SEQUENCE:2\r\n\
DTSTAMP:20261018T000000Z\r\n\
DTSTART;TZID=Asia/Taipei:20261021T140000\r\n\
DTEND;TZID=Asia/Taipei:20261021T150000\r\n\
SUMMARY:PLATFORM 讀書會\\, 文件整理\r\n\
ORGANIZER;CN=Carol:mailto:carol@example.test\r\n\
ATTENDEE;CN=Bob;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailt\r\n o:bob@example.test\r\n\
ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:MAILT\r\n O:Alice@Example.test\r\n\
LOCATION:北區範例會議室\r\n\
BEGIN:VALARM\r\n\
ACTION:DISPLAY\r\n\
TRIGGER:-PT15M\r\n\
END:VALARM\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

fn lines(ics: &str) -> Vec<&str> {
    ics.split("\r\n").filter(|line| !line.is_empty()).collect()
}

fn unfolded(ics: &str) -> String {
    ics.replace("\r\n ", "")
}

#[test]
fn reply_keeps_request_identity_and_lists_only_me_with_partstat() {
    let reply =
        build_rsvp_reply(TAIPEI_REQUEST, None, ME, RsvpResponse::Accept, None, now()).unwrap();
    assert_eq!(reply.uid, "weekly-sync-42@example.test");
    assert_eq!(reply.sequence, 2);
    assert_eq!(reply.organizer, "carol@example.test");
    assert_eq!(reply.attendee, ME);
    assert_eq!(reply.partstat, "ACCEPTED");
    assert_eq!(reply.summary, "PLATFORM 讀書會, 文件整理");
    // TZID is resolved for display, and the reply keeps the original wall time.
    assert_eq!(reply.start.as_deref(), Some("2026-10-21T14:00:00+08:00"));
    assert_eq!(reply.end.as_deref(), Some("2026-10-21T15:00:00+08:00"));
    assert!(!reply.recurring);

    let ics = unfolded(&reply.ics);
    assert!(reply.ics.ends_with("\r\n"));
    assert!(lines(&reply.ics).iter().all(|line| line.len() <= 75));
    let all = lines(&ics);
    assert!(all.contains(&"METHOD:REPLY"));
    assert!(!ics.contains("METHOD:REQUEST"));
    for kept in [
        "UID:weekly-sync-42@example.test",
        "SEQUENCE:2",
        "DTSTART;TZID=Asia/Taipei:20261021T140000",
        "DTEND;TZID=Asia/Taipei:20261021T150000",
        "ORGANIZER;CN=Carol:mailto:carol@example.test",
        "DTSTAMP:20261019T010203Z",
        "TZID:Asia/Taipei",
        "END:VTIMEZONE",
    ] {
        assert!(all.contains(&kept), "missing {kept}:\n{ics}");
    }
    let attendees = all
        .iter()
        .filter(|line| line.starts_with("ATTENDEE"))
        .collect::<Vec<_>>();
    // Organizers match replies by attendee; other invitees and RSVP=TRUE must
    // not be echoed back as if we answered for them.
    assert_eq!(
        attendees,
        vec![&"ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED:MAILTO:Alice@Example.test"]
    );
    assert!(!ics.contains("VALARM") && !ics.contains("TRIGGER"));
    assert!(!ics.contains("LOCATION"));
}

#[test]
fn decline_and_tentative_set_partstat_and_escape_comment() {
    let declined = build_rsvp_reply(
        TAIPEI_REQUEST,
        None,
        ME,
        RsvpResponse::Decline,
        Some("那天出差; 改派 Frank, 謝謝\n再約"),
        now(),
    )
    .unwrap();
    let ics = unfolded(&declined.ics);
    assert!(ics.contains(";PARTSTAT=DECLINED:MAILTO:Alice@Example.test"));
    assert!(ics.contains("COMMENT:那天出差\\; 改派 Frank\\, 謝謝\\n再約"));
    let tentative = build_rsvp_reply(
        TAIPEI_REQUEST,
        None,
        ME,
        RsvpResponse::Tentative,
        None,
        now(),
    )
    .unwrap();
    assert_eq!(tentative.partstat, "TENTATIVE");
    assert!(!tentative.ics.contains("COMMENT"));
}

#[test]
fn recurring_series_with_override_replies_to_each_instance_i_attend() {
    let request = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n\
BEGIN:VEVENT\r\nUID:series@example.test\r\nDTSTART:20261021T060000Z\r\nDTEND:20261021T070000Z\r\n\
RRULE:FREQ=WEEKLY;COUNT=4\r\nSUMMARY:讀書會\r\nORGANIZER:mailto:carol@example.test\r\n\
ATTENDEE;RSVP=TRUE:mailto:alice@example.test\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:series@example.test\r\nRECURRENCE-ID:20261028T060000Z\r\nSEQUENCE:1\r\n\
DTSTART:20261028T080000Z\r\nDTEND:20261028T090000Z\r\nSUMMARY:讀書會（改時間）\r\n\
ORGANIZER:mailto:carol@example.test\r\nATTENDEE:mailto:alice@example.test\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:series@example.test\r\nRECURRENCE-ID:20261104T060000Z\r\n\
DTSTART:20261104T060000Z\r\nDTEND:20261104T070000Z\r\nSUMMARY:讀書會（Frank 代理）\r\n\
ORGANIZER:mailto:carol@example.test\r\nATTENDEE:mailto:frank@example.test\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";
    let reply = build_rsvp_reply(request, None, ME, RsvpResponse::Accept, None, now()).unwrap();
    assert!(reply.recurring);
    assert_eq!(reply.sequence, 0, "master has no SEQUENCE");
    assert_eq!(reply.recurrence_ids, vec!["20261028T060000Z"]);
    let ics = unfolded(&reply.ics);
    // The master and the override I attend; the instance I was removed from is skipped.
    assert_eq!(ics.matches("BEGIN:VEVENT").count(), 2, "{ics}");
    assert!(ics.contains("RECURRENCE-ID:20261028T060000Z\r\nSEQUENCE:1\r\n"));
    assert!(!ics.contains("20261104"));
    assert_eq!(
        ics.matches("ATTENDEE;PARTSTAT=ACCEPTED:mailto:alice@example.test")
            .count(),
        2
    );
    assert!(!ics.contains("RSVP"));
}

#[test]
fn method_comes_from_ics_or_content_type_and_must_be_request() {
    let without_method = TAIPEI_REQUEST.replace("METHOD:REQUEST\r\n", "");
    let error = build_rsvp_reply(&without_method, None, ME, RsvpResponse::Accept, None, now())
        .unwrap_err()
        .to_string();
    assert!(error.contains("缺少 METHOD"), "{error}");
    let reply = build_rsvp_reply(
        &without_method,
        Some("request"),
        ME,
        RsvpResponse::Accept,
        None,
        now(),
    )
    .unwrap();
    assert!(reply.ics.contains("METHOD:REPLY"));

    let cancel = TAIPEI_REQUEST.replace("METHOD:REQUEST", "METHOD:CANCEL");
    let error = build_rsvp_reply(
        &cancel,
        Some("REQUEST"),
        ME,
        RsvpResponse::Accept,
        None,
        now(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("METHOD 是 CANCEL"),
        "ICS METHOD wins: {error}"
    );
}

#[test]
fn not_being_an_attendee_or_being_the_organizer_is_an_explicit_error() {
    let error = build_rsvp_reply(
        TAIPEI_REQUEST,
        None,
        "dave@example.test",
        RsvpResponse::Accept,
        None,
        now(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("dave@example.test") && error.contains("不在邀請的 ATTENDEE"),
        "{error}"
    );
    let error = build_rsvp_reply(
        TAIPEI_REQUEST,
        None,
        "carol@example.test",
        RsvpResponse::Accept,
        None,
        now(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("主辦人"), "{error}");
    let no_organizer =
        TAIPEI_REQUEST.replace("ORGANIZER;CN=Carol:mailto:carol@example.test\r\n", "");
    let error = build_rsvp_reply(&no_organizer, None, ME, RsvpResponse::Accept, None, now())
        .unwrap_err()
        .to_string();
    assert!(error.contains("ORGANIZER"), "{error}");
}

#[test]
fn attendee_matches_email_parameter_and_keeps_quoted_colons() {
    let request = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u1\r\n\
DTSTART:20261021T060000Z\r\nSUMMARY:x\r\nORGANIZER:mailto:carol@example.test\r\n\
ATTENDEE;DELEGATED-FROM=\"mailto:dave@example.test\";EMAIL=alice@example.test:urn:uuid:42\r\n\
END:VEVENT\r\nEND:VCALENDAR\r\n";
    let reply = build_rsvp_reply(request, None, ME, RsvpResponse::Accept, None, now()).unwrap();
    assert!(unfolded(&reply.ics).contains(
        "ATTENDEE;DELEGATED-FROM=\"mailto:dave@example.test\";EMAIL=alice@example.test;PARTSTAT=ACCEPTED:urn:uuid:42"
    ));
}

fn invite_mail(calendar_content_type: &str, ics: &str) -> String {
    use base64::Engine as _;
    format!(
        "From: Carol <carol@example.test>\r\nTo: alice@example.test\r\nSubject: invite\r\n\
Message-ID: <invite-1@example.test>\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=outer\r\n\r\n--outer\r\n\
Content-Type: multipart/alternative; boundary=inner\r\n\r\n--inner\r\n\
Content-Type: text/plain; charset=utf-8\r\n\r\n請參加\r\n--inner\r\n\
Content-Type: {calendar_content_type}\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n\
--inner--\r\n--outer--\r\n",
        base64::engine::general_purpose::STANDARD.encode(ics)
    )
}

#[test]
fn invite_is_found_in_nested_multipart_with_content_type_method() {
    let without_method = TAIPEI_REQUEST.replace("METHOD:REQUEST\r\n", "");
    let mail = invite_mail(
        "text/calendar; charset=utf-8; method=REQUEST",
        &without_method,
    );
    let invite = invite_from_mail(mail.as_bytes()).unwrap();
    assert_eq!(invite.content_type_method.as_deref(), Some("REQUEST"));
    assert_eq!(invite.ics, without_method);
    let error = invite_from_mail(b"From: a@example.test\r\nSubject: hi\r\n\r\nplain\r\n")
        .unwrap_err()
        .to_string();
    assert!(error.contains("text/calendar"), "{error}");
}

fn m2k(dir: &std::path::Path, args: &[&str]) -> (Option<i32>, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--json")
        .arg("--data-dir")
        .arg(dir)
        .args(args)
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN)
        .output()
        .unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "non-JSON: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code(), value)
}

#[test]
fn cli_dry_run_works_offline_but_demo_refuses_to_send() {
    let temp = tempfile::tempdir().unwrap();
    demo::seed(temp.path()).unwrap();
    let mut store = Store::open(temp.path()).unwrap();
    let raw = invite_mail(
        "text/calendar; method=REQUEST; charset=utf-8",
        &TAIPEI_REQUEST.replace("Alice@Example.test", "demo@example.test"),
    );
    store
        .upsert(&Item {
            id: "demo-invite".into(),
            kind: Kind::Mail,
            collection: "INBOX".into(),
            remote_id: "9".into(),
            etag: None,
            uid_validity: Some(1),
            title: "invite".into(),
            subtitle: "carol@example.test".into(),
            date: None,
            end_date: None,
            text: "請參加".into(),
            raw,
            flags: vec![],
            emails: vec![],
            phones: vec![],
            attachments: vec![],
        })
        .unwrap();
    drop(store);

    let (status, dry) = m2k(
        temp.path(),
        &[
            "meeting",
            "rsvp",
            "demo-invite",
            "--response",
            "tentative",
            "--dry-run",
        ],
    );
    assert_eq!(status, Some(0), "{dry}");
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["sent"], false);
    assert_eq!(dry["to"], "carol@example.test");
    assert_eq!(dry["partstat"], "TENTATIVE");
    assert_eq!(dry["uid"], "weekly-sync-42@example.test");
    assert_eq!(dry["calendar_updated"], false);
    assert!(dry["ics"].as_str().unwrap().contains("METHOD:REPLY"));

    let (status, refused) = m2k(
        temp.path(),
        &["meeting", "rsvp", "demo-invite", "--response", "accept"],
    );
    assert_eq!(status, Some(1));
    assert_eq!(refused["state"], "error");
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains("展示資料不提供實際寄信"));

    let (status, plain) = m2k(
        temp.path(),
        &[
            "meeting",
            "rsvp",
            "demo-mail-2",
            "--response",
            "accept",
            "--dry-run",
        ],
    );
    assert_eq!(status, Some(1));
    assert!(plain["error"]["message"]
        .as_str()
        .unwrap()
        .contains("沒有 text/calendar"));

    let (status, invalid) = m2k(
        temp.path(),
        &["meeting", "rsvp", "demo-invite", "--response", "maybe"],
    );
    assert_eq!(status, Some(2));
    assert_eq!(invalid["error"]["code"], "invalid_arguments");
}

#[test]
fn non_mailto_attendee_with_multibyte_name_is_compared_not_panicked_on() {
    // A bare CJK address puts byte 7 inside a character; slicing [..7] would panic.
    let request = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u2\r\n\
DTSTART:20261021T060000Z\r\nSUMMARY:x\r\nORGANIZER:mailto:carol@example.test\r\n\
ATTENDEE:王小明@example.test\r\nATTENDEE:mailto:alice@example.test\r\n\
END:VEVENT\r\nEND:VCALENDAR\r\n";
    let reply = build_rsvp_reply(request, None, ME, RsvpResponse::Accept, None, now()).unwrap();
    assert!(unfolded(&reply.ics).contains("ATTENDEE;PARTSTAT=ACCEPTED:mailto:alice@example.test"));
    assert!(!reply.ics.contains("王小明"));
}

#[test]
fn calendar_part_without_charset_is_decoded_as_utf8() {
    // RFC 5545 defaults text/calendar to UTF-8; a charset-less part must keep Chinese intact.
    let mail = invite_mail("text/calendar; method=REQUEST", TAIPEI_REQUEST);
    let invite = invite_from_mail(mail.as_bytes()).unwrap();
    assert_eq!(invite.ics, TAIPEI_REQUEST);
    let reply = build_rsvp_reply(&invite.ics, None, ME, RsvpResponse::Accept, None, now()).unwrap();
    assert!(reply.ics.contains("PLATFORM 讀書會"));
}
