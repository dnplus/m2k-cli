use crate::{
    config, meeting,
    model::{Attachment, Collection, Item, Kind, Snapshot},
    store::Store,
};
use anyhow::{bail, Result};
use chrono::{Duration, FixedOffset, TimeZone, Utc};
use std::path::Path;

const MAIL2_QUOTE: &str = "On Mon, 5 Jan 2026 at 10:00, 工程團隊 <team@example.test> wrote:\n> 上週的同步結果請一併整理。\n> 這是被引用的歷史內容。";

pub fn seed(data_dir: &Path) -> Result<()> {
    seed_with(data_dir, true)
}

/// Seed without cached room calendars, so every room stays "unknown" until a test adds its own.
pub fn seed_without_rooms(data_dir: &Path) -> Result<()> {
    seed_with(data_dir, false)
}

fn seed_with(data_dir: &Path, rooms: bool) -> Result<()> {
    if data_dir.join("config.json").exists() && !data_dir.join(".demo").exists() {
        bail!("此資料目錄已有正式設定；請使用 --data-dir 指定獨立展示目錄");
    }
    let cfg = config::Config::with_site_profile(
        "demo@example.test".into(),
        "mail.example.test",
        Some("demo".into()),
    )?;
    config::save(data_dir, &cfg)?;
    std::fs::write(data_dir.join(".demo"), "m2k isolated demo\n")?;
    let mut store = Store::open(data_dir)?;
    store.ensure_account(&cfg)?;
    let now = Utc::now();
    let make =
        |kind: Kind, id: &str, collection: &str, title: &str, subtitle: &str, text: &str| Item {
            id: id.into(),
            kind,
            collection: collection.into(),
            remote_id: id.into(),
            etag: Some("\"demo-v1\"".into()),
            uid_validity: None,
            title: title.into(),
            subtitle: subtitle.into(),
            date: Some(now.to_rfc3339()),
            end_date: None,
            text: text.into(),
            raw: String::new(),
            flags: vec![],
            emails: vec![],
            phones: vec![],
            attachments: vec![],
        };
    let mut welcome = make(Kind::Mail, "demo-mail-1", "INBOX", "歡迎使用 m2k 原生工作台", "工程團隊 <team@example.test>", "這是獨立的展示資料。\n\n1 / 2 / 3 切換郵件、行事曆、聯絡人。\n按 / 搜尋、Enter 閱讀詳情、Esc 返回。\n\n正式使用：m2k init --username 你的帳號\n接著 m2k auth login 與 m2k sync。\n\n本機 SQLite 讓你在離線時仍能搜尋與閱讀。所有資料都是虛構的。");
    welcome.remote_id = "1".into();
    welcome.uid_validity = Some(1);
    welcome.raw = "From: team@example.test\r\nTo: demo@example.test\r\nSubject: Welcome to m2k\r\nMessage-ID: <demo-1@example.test>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=m2kdemo\r\n\r\n--m2kdemo\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nWelcome to m2k\r\n--m2kdemo\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=welcome.txt\r\n\r\nOffline attachment demo\r\n--m2kdemo--\r\n".into();
    welcome.attachments.push(Attachment {
        index: 0,
        filename: "welcome.txt".into(),
        content_type: "text/plain".into(),
        size: 25,
    });
    let mut mail2 = make(
        Kind::Mail,
        "demo-mail-2",
        "INBOX",
        "本週產品同步會議",
        "王小明 <ming@example.test>",
        "週三下午兩點討論本機同步進度。\n請帶上你的 CLI 與 TUI 驗證結果。\n\n這封郵件為展示用途。",
    );
    mail2.text = format!("{}\n\n{}", mail2.text, MAIL2_QUOTE);
    mail2.raw = format!(
        "From: =?utf-8?B?546L5bCP5piO?= <ming@example.test>\r\nTo: demo@example.test\r\nSubject: =?utf-8?B?5pys6YCx55Si5ZOB5ZCM5q2l5pyD6K2w?=\r\nMessage-ID: <demo-2@example.test>\r\nIn-Reply-To: <demo-0@example.test>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n{}\r\n",
        mail2.text.replace('\n', "\r\n")
    );
    mail2.remote_id = "2".into();
    mail2.uid_validity = Some(1);
    mail2.flags.push("\\Seen".into());
    let mut event = make(
        Kind::Calendar,
        "demo-event-1",
        "demo-calendar",
        "m2k 骨架驗收",
        "北區辦公室 · 討論室",
        "驗證 CLI、SQLite 持久化、TUI 離線瀏覽與保存週期。\n與會者：工程團隊",
    );
    event.date = Some((now + Duration::days(1)).to_rfc3339());
    event.end_date = Some((now + Duration::days(1) + Duration::hours(1)).to_rfc3339());
    event.raw = format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//m2k//Demo//EN\r\nBEGIN:VEVENT\r\nUID:demo-event-1\r\nDTSTAMP:{}\r\nDTSTART:{}\r\nDTEND:{}\r\nSUMMARY:m2k 骨架驗收\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n", now.format("%Y%m%dT%H%M%SZ"), (now+Duration::days(1)).format("%Y%m%dT%H%M%SZ"), (now+Duration::days(1)+Duration::hours(1)).format("%Y%m%dT%H%M%SZ"));
    let mut contact = make(
        Kind::Contacts,
        "demo-contact-1",
        "demo-addressbook",
        "王小明",
        "產品工程部",
        "聯絡人支援姓名、電子郵件、電話與原始 vCard。\n此為虛構聯絡人。",
    );
    contact.emails.push("ming@example.test".into());
    contact.phones.push("+886-2-0000-0000".into());
    contact.date = None;
    contact.raw = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:demo-contact-1\r\nFN:王小明\r\nORG:產品工程部\r\nEMAIL:ming@example.test\r\nTEL:+886-2-0000-0000\r\nEND:VCARD\r\n".into();
    let mut self_contact = make(
        Kind::Contacts,
        "demo-self",
        "demo-addressbook",
        "展示使用者",
        "產品工程部",
        "目前登入帳號的虛構聯絡人資料。",
    );
    self_contact.date = None;
    self_contact.emails.push(cfg.email.clone());
    self_contact.raw="BEGIN:VCARD\r\nVERSION:3.0\r\nUID:demo-self\r\nFN:展示使用者\r\nORG:產品工程部\r\nEMAIL:demo@example.test\r\nEND:VCARD\r\n".into();
    let mut calendar_collections = vec![Collection {
        kind: Kind::Calendar,
        id: event.collection.clone(),
        name: "個人行事曆".into(),
    }];
    let mut calendar_items = vec![event];
    let room_events = if rooms {
        room_events(&cfg, now, make)
    } else {
        Vec::new()
    };
    for (collection, items) in room_events {
        calendar_collections.push(Collection {
            kind: Kind::Calendar,
            id: collection.clone(),
            name: collection,
        });
        calendar_items.extend(items);
    }
    for (kind, name, items) in [
        (Kind::Mail, "收件匣", vec![welcome, mail2]),
        (Kind::Contacts, "聯絡人", vec![contact, self_contact]),
    ] {
        let collection = items[0].collection.clone();
        store.apply_snapshot(
            &Snapshot {
                kind,
                collections: vec![Collection {
                    kind,
                    id: collection,
                    name: name.into(),
                }],
                items,
            },
            now,
        )?;
    }
    store.apply_snapshot(
        &Snapshot {
            kind: Kind::Calendar,
            collections: calendar_collections,
            items: calendar_items,
        },
        now,
    )?;
    Ok(())
}

/// Offline room calendars for `meeting rooms --site 北區` on tomorrow 10:00-11:00 (+08:00):
/// 北區甲室 is free, 北區乙室 is busy 10:30-11:30, other rooms stay unknown (no cache).
fn room_events(
    cfg: &config::Config,
    now: chrono::DateTime<Utc>,
    make: impl Fn(Kind, &str, &str, &str, &str, &str) -> Item,
) -> Vec<(String, Vec<Item>)> {
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let day = (now.with_timezone(&tz) + Duration::days(1)).date_naive();
    let at = |hour, minute| {
        tz.from_local_datetime(&day.and_hms_opt(hour, minute, 0).unwrap())
            .unwrap()
            .with_timezone(&Utc)
    };
    let profile = crate::site::SiteProfile::builtin("demo").expect("demo site profile is builtin");
    let rooms = meeting::builtin_rooms(cfg, &profile);
    [
        ("北區甲室", "demo-room-a", (14, 0), (15, 0), "下午簡報"),
        ("北區乙室", "demo-room-b", (10, 30), (11, 30), "客戶訪談"),
    ]
    .into_iter()
    .filter_map(|(prefix, id, start, end, title)| {
        let room = rooms.iter().find(|room| room.name.starts_with(prefix))?;
        let (start, end) = (at(start.0, start.1), at(end.0, end.1));
        let mut item = make(Kind::Calendar, id, &room.calendar_url, title, &room.name, title);
        item.date = Some(start.to_rfc3339());
        item.end_date = Some(end.to_rfc3339());
        item.raw = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{id}\r\nDTSTART:{}\r\nDTEND:{}\r\nSUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            start.format("%Y%m%dT%H%M%SZ"),
            end.format("%Y%m%dT%H%M%SZ")
        );
        Some((room.calendar_url.clone(), vec![item]))
    })
    .collect()
}
