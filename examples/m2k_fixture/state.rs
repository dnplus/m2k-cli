use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
};

pub const CALENDAR_ROOT: &str = "/cgi-bin/cal/caldav/calendars";
/// Mail2000 CardDAV principal for the account's email domain.
pub fn carddav_principal(user: &str) -> String {
    let domain = user.rsplit_once('@').map_or("", |(_, domain)| domain);
    format!("/cgi-bin/carddav/principals/mPA.000@{domain}")
}
pub const ADDRESSBOOK_HOME: &str = "/cgi-bin/carddav/addressbooks/company/";
pub const ROOM_EMAIL: &str = "room-a@example.test";

pub struct Fixture {
    pub user: String,
    pub password: String,
    pub data_dir: PathBuf,
    pub mail: Mutex<MailStore>,
    pub dav: Mutex<DavStore>,
    events: Mutex<u64>,
}

impl Fixture {
    pub fn seeded(
        user: String,
        password: String,
        data_dir: PathBuf,
        now: DateTime<Utc>,
        sync_collection: bool,
    ) -> Self {
        let validity = u32::try_from(now.timestamp() % i64::from(u32::MAX)).unwrap_or(1);
        Self {
            mail: Mutex::new(MailStore::seeded(&user, validity, now)),
            dav: Mutex::new(DavStore::seeded(&user, now, sync_collection)),
            user,
            password,
            data_dir,
            events: Mutex::new(0),
        }
    }

    /// Appends one side effect to `events.jsonl`; `artifact` is saved under
    /// `<data-dir>/<dir>/<seq>-<name>` and referenced from the event.
    pub fn record(&self, kind: &str, mut detail: Value, artifact: Option<(&str, &str, &[u8])>) {
        let mut seq = self.events.lock().expect("event lock");
        *seq += 1;
        let result = (|| -> Result<()> {
            if let Some((dir, name, bytes)) = artifact {
                let name = format!("{:04}-{}", *seq, safe_name(name));
                let relative = format!("{dir}/{name}");
                fs::create_dir_all(self.data_dir.join(dir))?;
                fs::write(self.data_dir.join(&relative), bytes)?;
                detail["file"] = json!(relative);
            }
            detail["seq"] = json!(*seq);
            detail["kind"] = json!(kind);
            detail["at"] = json!(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.data_dir.join("events.jsonl"))
                .context("cannot open events.jsonl")?;
            writeln!(file, "{detail}")?;
            Ok(())
        })();
        match result {
            Ok(()) => eprintln!("[fixture] {kind} {detail}"),
            Err(error) => eprintln!("[fixture] failed to record {kind}: {error:#}"),
        }
    }
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub struct Message {
    pub uid: u32,
    pub flags: BTreeSet<String>,
    pub internal_date: DateTime<Utc>,
    pub raw: Vec<u8>,
}

pub struct Mailbox {
    pub name: String,
    pub special: Option<&'static str>,
    pub uid_validity: u32,
    pub uid_next: u32,
    pub messages: Vec<Message>,
}

impl Mailbox {
    pub fn append(&mut self, raw: Vec<u8>, flags: BTreeSet<String>, date: DateTime<Utc>) -> u32 {
        let uid = self.uid_next;
        self.uid_next += 1;
        self.messages.push(Message {
            uid,
            flags,
            internal_date: date,
            raw,
        });
        uid
    }
}

pub struct MailStore {
    pub boxes: Vec<Mailbox>,
}

impl MailStore {
    pub fn get(&self, name: &str) -> Option<&Mailbox> {
        self.boxes.iter().find(|b| same_mailbox(&b.name, name))
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Mailbox> {
        self.boxes.iter_mut().find(|b| same_mailbox(&b.name, name))
    }

    fn seeded(user: &str, validity: u32, now: DateTime<Utc>) -> Self {
        let mailbox = |name: &str, special, offset: u32| Mailbox {
            name: name.to_owned(),
            special,
            uid_validity: validity + offset,
            uid_next: 1,
            messages: Vec::new(),
        };
        let mut store = Self {
            boxes: vec![
                mailbox("INBOX", None, 0),
                mailbox("Archive", Some("\\Archive"), 1),
                mailbox("Sent", Some("\\Sent"), 2),
                mailbox("Trash", Some("\\Trash"), 3),
            ],
        };
        let seen = || BTreeSet::from(["\\Seen".to_owned()]);
        let inbox = store.get_mut("INBOX").expect("inbox");
        let date = now - Duration::days(2);
        inbox.append(
            text_mail(
                "bob@example.test",
                user,
                "午餐訂購",
                date,
                "大家好，\r\n\r\n本週午餐訂購：請於週三前回覆便當選項。\r\n\r\nBob\r\n",
            ),
            seen(),
            date,
        );
        let date = now - Duration::days(1);
        inbox.append(
            attachment_mail("customer@client.example", user, "Q4 報價確認", date),
            BTreeSet::new(),
            date,
        );
        let date = now - Duration::hours(3);
        inbox.append(
            text_mail(
                "carol@example.test",
                user,
                "設備借用登記",
                date,
                "附上投影機借用登記表，週五前回覆即可。\r\n",
            ),
            BTreeSet::new(),
            date,
        );
        let date = now - Duration::hours(1);
        inbox.append(
            invite_mail("carol@example.test", user, "季度回顧會議", date, now),
            BTreeSet::new(),
            date,
        );
        let date = now - Duration::days(20);
        store.get_mut("Archive").expect("archive").append(
            text_mail(
                "erin@example.test",
                user,
                "文件範本更新",
                date,
                "範本已更新。\r\n",
            ),
            seen(),
            date,
        );
        let date = now - Duration::days(5);
        store.get_mut("Sent").expect("sent").append(
            text_mail(
                user,
                "dave@example.test",
                "讀書會時間",
                date,
                "讀書會改到下個月。\r\n",
            ),
            seen(),
            date,
        );
        store
    }
}

pub fn same_mailbox(left: &str, right: &str) -> bool {
    if left.eq_ignore_ascii_case("INBOX") {
        right.eq_ignore_ascii_case("INBOX")
    } else {
        left == right
    }
}

fn encoded_word(value: &str) -> String {
    if value.is_ascii() {
        value.to_owned()
    } else {
        format!("=?UTF-8?B?{}?=", STANDARD.encode(value))
    }
}

fn headers(from: &str, to: &str, subject: &str, date: DateTime<Utc>) -> String {
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {}\r\nDate: {}\r\nMessage-ID: <{}@fixture.m2k>\r\nMIME-Version: 1.0\r\n",
        encoded_word(subject),
        date.to_rfc2822(),
        uuid::Uuid::new_v4()
    )
}

fn text_mail(from: &str, to: &str, subject: &str, date: DateTime<Utc>, body: &str) -> Vec<u8> {
    format!(
        "{}Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{body}",
        headers(from, to, subject, date)
    )
    .into_bytes()
}

fn attachment_mail(from: &str, to: &str, subject: &str, date: DateTime<Utc>) -> Vec<u8> {
    format!(
        "{}Content-Type: multipart/mixed; boundary=fixture\r\n\r\n--fixture\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n報價單如附件，請確認。\r\n--fixture\r\nContent-Type: text/csv; name=quote.csv\r\nContent-Disposition: attachment; filename=quote.csv\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n--fixture--\r\n",
        headers(from, to, subject, date),
        STANDARD.encode("item,amount\nWidget A,120000\n")
    )
    .into_bytes()
}

/// An iTIP METHOD:REQUEST from `from` inviting `to`, two days after `now` at
/// 14:00 Asia/Taipei, so `m2k meeting rsvp` has something to answer.
fn invite_mail(
    from: &str,
    to: &str,
    subject: &str,
    date: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<u8> {
    let day = (now + Duration::days(2)).format("%Y%m%d");
    let ics = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//m2k fixture//EN\r\nMETHOD:REQUEST\r\n\
BEGIN:VTIMEZONE\r\nTZID:Asia/Taipei\r\nBEGIN:STANDARD\r\nDTSTART:19700101T000000\r\n\
TZOFFSETFROM:+0800\r\nTZOFFSETTO:+0800\r\nTZNAME:CST\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nUID:fixture-invite-1@example.test\r\nSEQUENCE:1\r\n\
DTSTAMP:{}\r\nDTSTART;TZID=Asia/Taipei:{day}T140000\r\nDTEND;TZID=Asia/Taipei:{day}T150000\r\n\
SUMMARY:{subject}\r\nORGANIZER;CN=Carol:mailto:{from}\r\n\
ATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:bob@example.test\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:{to}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        date.format("%Y%m%dT%H%M%SZ"),
    );
    format!(
        "{}Content-Type: multipart/alternative; boundary=fixture\r\n\r\n--fixture\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n會議邀請：{subject}\r\n--fixture\r\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n--fixture--\r\n",
        headers(from, to, subject, date),
        STANDARD.encode(ics)
    )
    .into_bytes()
}

pub enum NodeKind {
    Principal {
        calendar_home: Option<String>,
        addressbook_home: Option<String>,
    },
    Home,
    Calendar,
    Addressbook,
    Resource {
        data: String,
        etag: String,
        content_type: &'static str,
    },
}

pub struct DavNode {
    pub href: String,
    pub name: Option<String>,
    pub kind: NodeKind,
}

pub struct DavStore {
    pub nodes: BTreeMap<String, DavNode>,
    etag_counter: u64,
    /// Advertise and answer RFC 6578 `sync-collection` on calendars.
    pub sync_collection: bool,
    /// Per-start token prefix, so tokens from a previous run are invalid.
    pub sync_epoch: String,
    /// Change log for sync-collection: (sequence, resource key, href).
    pub changes: Vec<(u64, String, String)>,
}

/// Lookup key: percent-decoded path without a trailing slash.
pub fn dav_key(path: &str) -> String {
    let decoded = percent_decode(path.split('?').next().unwrap_or(""));
    let trimmed = decoded.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed.to_owned()
    }
}

pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |byte: u8| char::from(byte).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Hrefs encode `@` as `%40`, the same form as m2k's default configured URL.
pub fn calendar_href(email: &str) -> String {
    format!("{CALENDAR_ROOT}/{}/default/", email.replace('@', "%40"))
}

impl DavStore {
    pub fn next_etag(&mut self) -> String {
        self.etag_counter += 1;
        format!("\"fixture-{}\"", self.etag_counter)
    }

    pub fn insert(&mut self, href: &str, name: Option<&str>, kind: NodeKind) {
        let key = dav_key(href);
        if matches!(kind, NodeKind::Resource { .. }) {
            self.log_change(&key, href);
        }
        self.nodes.insert(
            key,
            DavNode {
                href: href.to_owned(),
                name: name.map(ToOwned::to_owned),
                kind,
            },
        );
    }

    pub fn remove(&mut self, key: &str) -> Option<DavNode> {
        let node = self.nodes.remove(key)?;
        self.log_change(key, &node.href.clone());
        Some(node)
    }

    fn log_change(&mut self, key: &str, href: &str) {
        let seq = self.sync_seq() + 1;
        self.changes.push((seq, key.to_owned(), href.to_owned()));
    }

    pub fn sync_seq(&self) -> u64 {
        self.changes.last().map_or(0, |(seq, _, _)| *seq)
    }

    pub fn sync_token(&self) -> String {
        format!(
            "http://m2k.fixture/sync/{}/{}",
            self.sync_epoch,
            self.sync_seq()
        )
    }

    /// `Some(seq)` for a token issued by this run; `""` is the initial sync.
    pub fn parse_sync_token(&self, token: &str) -> Option<u64> {
        if token.is_empty() {
            return Some(0);
        }
        let (epoch, seq) = token
            .strip_prefix("http://m2k.fixture/sync/")?
            .split_once('/')?;
        let seq = seq.parse().ok()?;
        (epoch == self.sync_epoch && seq <= self.sync_seq()).then_some(seq)
    }

    fn put_seed(&mut self, href: &str, data: String, content_type: &'static str) {
        let etag = self.next_etag();
        self.insert(
            href,
            None,
            NodeKind::Resource {
                data,
                etag,
                content_type,
            },
        );
    }

    fn seeded(user: &str, now: DateTime<Utc>, sync_collection: bool) -> Self {
        let mut store = Self {
            nodes: BTreeMap::new(),
            etag_counter: 0,
            sync_collection,
            sync_epoch: uuid::Uuid::new_v4().simple().to_string()[..8].to_owned(),
            changes: Vec::new(),
        };
        for (email, name) in [(user, "個人行事曆"), (ROOM_EMAIL, "第一會議室 (A)")] {
            let calendar = calendar_href(email);
            let home = calendar.trim_end_matches("default/").to_owned();
            store.insert(&home, Some(email), NodeKind::Home);
            store.insert(&calendar, Some(name), NodeKind::Calendar);
        }
        let tomorrow = (now + Duration::days(1)).date_naive();
        let at = |day: chrono::NaiveDate, hour: u32| {
            day.and_hms_opt(hour, 0, 0)
                .expect("valid hour")
                .and_utc()
                .format("%Y%m%dT%H%M%SZ")
                .to_string()
        };
        let own = calendar_href(user);
        let room = calendar_href(ROOM_EMAIL);
        let events = [
            (&own, "fixture-sync", "專案同步", tomorrow, 2, ""),
            (
                &own,
                "fixture-customer",
                "客戶拜訪",
                tomorrow + Duration::days(1),
                5,
                "",
            ),
            (
                &room,
                "fixture-room-busy",
                "既有預約",
                tomorrow,
                6,
                "第一會議室 (A)",
            ),
        ];
        for (collection, uid, summary, day, hour, location) in events {
            let location = if location.is_empty() {
                String::new()
            } else {
                format!("LOCATION:{location}\r\n")
            };
            let data = format!(
                "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//m2k//fixture//ZH-TW\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:{}\r\nDTSTART:{}\r\nDTEND:{}\r\nSUMMARY:{summary}\r\n{location}ORGANIZER:mailto:{user}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
                at(now.date_naive(), 0),
                at(day, hour),
                at(day, hour + 1)
            );
            store.put_seed(
                &format!("{collection}{uid}.ics"),
                data,
                "text/calendar; charset=utf-8",
            );
        }
        store.insert(
            &carddav_principal(user),
            Some("公司通訊錄"),
            NodeKind::Principal {
                calendar_home: None,
                addressbook_home: Some(ADDRESSBOOK_HOME.to_owned()),
            },
        );
        store.insert(ADDRESSBOOK_HOME, Some("company"), NodeKind::Home);
        let book = format!("{ADDRESSBOOK_HOME}directory/");
        store.insert(&book, Some("公司通訊錄"), NodeKind::Addressbook);
        let contacts = [
            ("bob", "Bob", "bob@example.test", "PLATFORM"),
            ("carol", "Carol", "carol@example.test", "PLATFORM"),
            ("room-a", "第一會議室 (A)", ROOM_EMAIL, "會議室"),
        ];
        for (id, name, email, org) in contacts {
            let data = format!(
                "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:fixture-{id}\r\nFN:{name}\r\nN:{name};;;;\r\nEMAIL;TYPE=INTERNET:{email}\r\nORG:{org}\r\nEND:VCARD\r\n"
            );
            store.put_seed(
                &format!("{book}{id}.vcf"),
                data,
                "text/vcard; charset=utf-8",
            );
        }
        store
    }
}
