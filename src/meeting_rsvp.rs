//! RFC 5546 METHOD:REPLY for invitations received by mail.
//!
//! The reply copies the request's identifying lines verbatim (UID,
//! RECURRENCE-ID, SEQUENCE, DTSTART/DTEND, ORGANIZER) and replaces the attendee
//! list with the current account only, so the organizer's client can match it
//! to the exact revision that was answered.

use crate::{
    config,
    meeting::{
        escape_ical_text, fold_ical_line, parse_ics_datetime, unescape_ical_text, unfold_ics_lines,
        validate_email,
    },
    model::Kind,
    provider::{self, mail::CalendarReply},
    store::Store,
    sync,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::{Args, ValueEnum};
use mailparse::{MailHeaderMap, ParsedMail};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Args)]
pub struct RsvpArgs {
    #[arg(help = "邀請郵件的 m2k ID（mail list／mail search 的 id）")]
    pub id: String,
    #[arg(long, value_enum)]
    pub response: RsvpResponse,
    #[arg(long, help = "附在回覆中的 COMMENT")]
    pub comment: Option<String>,
    #[arg(long, help = "只輸出 METHOD:REPLY ICS 與收件者，不寄信")]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum RsvpResponse {
    Accept,
    Decline,
    Tentative,
}

impl RsvpResponse {
    pub fn partstat(self) -> &'static str {
        match self {
            Self::Accept => "ACCEPTED",
            Self::Decline => "DECLINED",
            Self::Tentative => "TENTATIVE",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Accept => "已接受",
            Self::Decline => "已拒絕",
            Self::Tentative => "暫定",
        }
    }
}

/// The calendar part of an invitation mail, with the `method` parameter of its
/// Content-Type (some clients put METHOD only there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailInvite {
    pub ics: String,
    pub content_type_method: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RsvpReply {
    pub uid: String,
    pub sequence: u32,
    pub summary: String,
    pub organizer: String,
    pub attendee: String,
    pub partstat: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub recurring: bool,
    pub recurrence_ids: Vec<String>,
    pub ics: String,
}

#[derive(Debug, Serialize)]
struct RsvpOutput {
    id: String,
    response: RsvpResponse,
    to: String,
    subject: String,
    dry_run: bool,
    sent: bool,
    message_id: Option<String>,
    calendar_updated: bool,
    #[serde(flatten)]
    reply: RsvpReply,
}

pub fn execute(data_dir: &Path, args: RsvpArgs, json: bool) -> Result<()> {
    let demo = data_dir.join(".demo").exists();
    if demo && !args.dry_run {
        bail!("展示資料不提供實際寄信；可加上 --dry-run 預覽回覆");
    }
    let _lock = (!args.dry_run).then(|| sync::lock(data_dir)).transpose()?;
    let config = config::load(data_dir)?;
    let mut store = Store::open(data_dir)?;
    store.ensure_account(&config)?;
    let mut item = store.get(&args.id)?;
    if item.kind != Kind::Mail {
        bail!("資料類型不符：{}", item.kind);
    }
    let mut password = None;
    let mut raw = provider::mail::raw_bytes(&item)?;
    if raw.is_empty() {
        if demo {
            bail!("展示郵件沒有本機正文");
        }
        let secret = config::password(data_dir, &config)?;
        item = provider::mail::fetch_item(&config, &secret, &item)?;
        raw = provider::mail::raw_bytes(&item)?;
        password = Some(secret);
    }
    let invite = invite_from_mail(&raw)?;
    let reply = build_rsvp_reply(
        &invite.ics,
        invite.content_type_method.as_deref(),
        &config.email,
        args.response,
        args.comment.as_deref(),
        Utc::now(),
    )?;
    let subject = format!("{}：{}", args.response.label(), reply.summary);
    let mut message_id = None;
    if !args.dry_run {
        let password = match password {
            Some(password) => password,
            None => config::password(data_dir, &config)?,
        };
        let parent = mailparse::parse_mail(&raw)
            .ok()
            .and_then(|mail| mail.headers.get_first_value("Message-ID"));
        let mut body = format!(
            "{} {}會議邀請：{}\n",
            reply.attendee,
            args.response.label(),
            reply.summary
        );
        if let Some(comment) = args.comment.as_deref().map(str::trim) {
            if !comment.is_empty() {
                body.push('\n');
                body.push_str(comment);
                body.push('\n');
            }
        }
        message_id = Some(provider::mail::send_calendar_reply(
            &config,
            &password,
            &CalendarReply {
                to: &reply.organizer,
                subject: &subject,
                body: &body,
                ics: &reply.ics,
                in_reply_to: parent.as_deref(),
            },
        )?);
    }
    let output = RsvpOutput {
        id: args.id,
        response: args.response,
        to: reply.organizer.clone(),
        subject,
        dry_run: args.dry_run,
        sent: message_id.is_some(),
        message_id,
        calendar_updated: false,
        reply,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if output.dry_run {
        println!("預覽 METHOD:REPLY（{}），未寄送。", output.reply.partstat);
        println!("收件者：{}", output.to);
        println!("主旨：{}", output.subject);
        println!();
        print!("{}", output.reply.ics);
    } else {
        println!(
            "已寄送 METHOD:REPLY（{}）給 {}；未修改本機或伺服器行事曆。",
            output.reply.partstat, output.to
        );
        if let Some(id) = &output.message_id {
            println!("Message-ID：{id}");
        }
    }
    Ok(())
}

/// Finds the first text/calendar (or application/ics) part of a mail.
pub fn invite_from_mail(raw: &[u8]) -> Result<MailInvite> {
    let mail = mailparse::parse_mail(raw).context("解析郵件 MIME 失敗")?;
    let mut found = None;
    find_calendar_part(&mail, &mut found)?;
    found.ok_or_else(|| anyhow!("郵件裡沒有 text/calendar 會議邀請"))
}

fn find_calendar_part(part: &ParsedMail<'_>, found: &mut Option<MailInvite>) -> Result<()> {
    if found.is_some() {
        return Ok(());
    }
    let mimetype = part.ctype.mimetype.to_ascii_lowercase();
    if part.subparts.is_empty() && (mimetype == "text/calendar" || mimetype == "application/ics") {
        *found = Some(MailInvite {
            ics: calendar_body(part)?,
            content_type_method: part
                .ctype
                .params
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("method"))
                .map(|(_, value)| value.trim().to_owned()),
        });
        return Ok(());
    }
    for child in &part.subparts {
        find_calendar_part(child, found)?;
    }
    Ok(())
}

/// RFC 5545 defaults text/calendar to UTF-8, but mailparse falls back to
/// us-ascii (windows-1252) when the part has no charset parameter.
fn calendar_body(part: &ParsedMail<'_>) -> Result<String> {
    let has_charset = part
        .ctype
        .params
        .keys()
        .any(|key| key.eq_ignore_ascii_case("charset"));
    if !has_charset {
        let raw = part.get_body_raw().context("解碼會議邀請內容失敗")?;
        if let Ok(text) = String::from_utf8(raw) {
            return Ok(text);
        }
    }
    part.get_body().context("解碼會議邀請內容失敗")
}

/// Builds the REPLY. `content_type_method` is only consulted when the ICS
/// itself has no METHOD; anything other than REQUEST is refused because only
/// a request can be answered.
pub fn build_rsvp_reply(
    invite_ics: &str,
    content_type_method: Option<&str>,
    self_email: &str,
    response: RsvpResponse,
    comment: Option<&str>,
    now: DateTime<Utc>,
) -> Result<RsvpReply> {
    let self_email = validate_email(self_email)?;
    let parsed = parse_calendar(invite_ics)?;
    let method = parsed
        .method
        .as_deref()
        .or(content_type_method)
        .map(str::trim)
        .ok_or_else(|| {
            anyhow!("會議內容缺少 METHOD（ICS 與 Content-Type 都沒有），無法確認是邀請；未回覆")
        })?;
    if !method.eq_ignore_ascii_case("REQUEST") {
        bail!("會議內容的 METHOD 是 {method}，不是 REQUEST；只能回覆邀請");
    }
    if parsed.events.is_empty() {
        bail!("會議邀請缺少 VEVENT");
    }
    let uid = parsed.events[0]
        .first("UID")
        .map(|line| unescape_ical_text(line.value.trim()))
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| anyhow!("會議邀請缺少 UID"))?;
    if parsed.events.iter().any(|event| {
        event
            .first("UID")
            .map(|line| unescape_ical_text(line.value.trim()))
            != Some(uid.clone())
    }) {
        bail!("會議邀請內含多個不同 UID 的 VEVENT，無法判斷要回覆哪一個");
    }
    let organizer_line = parsed.events[0]
        .first("ORGANIZER")
        .ok_or_else(|| anyhow!("會議邀請缺少 ORGANIZER，無法決定回覆對象"))?;
    let organizer = validate_email(&calendar_address(&organizer_line.value))
        .context("會議邀請的 ORGANIZER 不是有效的電子郵件")?;
    if organizer == self_email {
        bail!("你是這個會議的主辦人（ORGANIZER），不需要 RSVP");
    }
    let mine = parsed
        .events
        .iter()
        .filter_map(|event| {
            event
                .all("ATTENDEE")
                .find(|line| is_self_attendee(line, &self_email))
                .map(|line| (event, line))
        })
        .collect::<Vec<_>>();
    if mine.is_empty() {
        bail!("自己（{self_email}）不在邀請的 ATTENDEE 清單中；未產生回覆");
    }
    let stamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".to_owned(),
        "PRODID:-//m2k//m2k//ZH-TW".to_owned(),
        "CALSCALE:GREGORIAN".to_owned(),
        "METHOD:REPLY".to_owned(),
    ];
    lines.extend(parsed.timezones.iter().cloned());
    let mut recurrence_ids = Vec::new();
    for (event, attendee) in &mine {
        lines.push("BEGIN:VEVENT".to_owned());
        for name in ["UID", "RECURRENCE-ID", "SEQUENCE"] {
            lines.extend(event.first(name).map(|line| line.raw.clone()));
        }
        lines.push(format!("DTSTAMP:{stamp}"));
        for name in ["DTSTART", "DTEND", "DURATION", "SUMMARY", "ORGANIZER"] {
            lines.extend(event.first(name).map(|line| line.raw.clone()));
        }
        lines.push(reply_attendee(attendee, response));
        if let Some(comment) = comment.map(str::trim).filter(|value| !value.is_empty()) {
            lines.push(format!("COMMENT:{}", escape_ical_text(comment)));
        }
        lines.push("END:VEVENT".to_owned());
        if let Some(id) = event.first("RECURRENCE-ID") {
            recurrence_ids.push(id.value.trim().to_owned());
        }
    }
    lines.push("END:VCALENDAR".to_owned());
    let ics = lines
        .iter()
        .flat_map(|line| fold_ical_line(line))
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n";
    let (first, _) = mine[0];
    let sequence = match first.first("SEQUENCE") {
        Some(line) => line
            .value
            .trim()
            .parse()
            .with_context(|| format!("SEQUENCE 不是非負整數：{}", line.value))?,
        None => 0,
    };
    let time = |name: &str| {
        first
            .first(name)
            .and_then(|line| parse_ics_datetime(&line.key, &line.value).ok())
    };
    Ok(RsvpReply {
        uid,
        sequence,
        summary: first
            .first("SUMMARY")
            .map(|line| unescape_ical_text(&line.value))
            .unwrap_or_default(),
        organizer,
        attendee: self_email,
        partstat: response.partstat().to_owned(),
        start: time("DTSTART"),
        end: time("DTEND"),
        recurring: parsed
            .events
            .iter()
            .any(|event| event.first("RRULE").is_some() || event.first("RDATE").is_some()),
        recurrence_ids,
        ics,
    })
}

struct Property {
    /// The whole unfolded line, copied verbatim into the reply.
    raw: String,
    /// Name plus parameters, e.g. `DTSTART;TZID=Asia/Taipei`.
    key: String,
    name: String,
    value: String,
}

#[derive(Default)]
struct Event {
    properties: Vec<Property>,
}

impl Event {
    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> {
        self.properties
            .iter()
            .filter(move |property| property.name == name)
    }

    fn first(&self, name: &str) -> Option<&Property> {
        self.properties
            .iter()
            .find(|property| property.name == name)
    }
}

struct Calendar {
    method: Option<String>,
    timezones: Vec<String>,
    events: Vec<Event>,
}

fn parse_calendar(raw: &str) -> Result<Calendar> {
    let mut calendar = Calendar {
        method: None,
        timezones: Vec::new(),
        events: Vec::new(),
    };
    let mut stack: Vec<String> = Vec::new();
    let mut event: Option<Event> = None;
    for line in unfold_ics_lines(raw) {
        let property = split_property(&line);
        match property.name.as_str() {
            "BEGIN" => {
                let component = property.value.trim().to_ascii_uppercase();
                if component == "VEVENT" && stack.last().map(String::as_str) == Some("VCALENDAR") {
                    event = Some(Event::default());
                }
                stack.push(component);
            }
            "END" => {
                let component = property.value.trim().to_ascii_uppercase();
                if stack.last() != Some(&component) {
                    bail!("ICS 的 BEGIN／END:{component} 不成對");
                }
                stack.pop();
                if component == "VEVENT" && stack.last().map(String::as_str) == Some("VCALENDAR") {
                    calendar.events.extend(event.take());
                }
            }
            _ => {}
        }
        if stack.iter().any(|component| component == "VTIMEZONE")
            || (property.name == "END" && property.value.trim().eq_ignore_ascii_case("VTIMEZONE"))
        {
            calendar.timezones.push(line);
            continue;
        }
        match stack.as_slice() {
            [root] if root == "VCALENDAR" && property.name == "METHOD" => {
                calendar.method = Some(property.value.trim().to_owned());
            }
            // Only the event's own properties; VALARM and other children stay out.
            [root, child] if root == "VCALENDAR" && child == "VEVENT" => {
                if let Some(event) = event.as_mut() {
                    if property.name != "BEGIN" && property.name != "END" {
                        event.properties.push(property);
                    }
                }
            }
            _ => {}
        }
    }
    if !stack.is_empty() {
        bail!("ICS 缺少 END:{}", stack.last().expect("non-empty"));
    }
    Ok(calendar)
}

/// Splits at the first `:` outside a quoted parameter value, so
/// `ATTENDEE;DELEGATED-FROM="mailto:a@x":mailto:b@x` keeps both addresses.
fn split_property(line: &str) -> Property {
    let mut quoted = false;
    let split = line.char_indices().find_map(|(index, character)| {
        match character {
            '"' => quoted = !quoted,
            ':' if !quoted => return Some(index),
            _ => {}
        }
        None
    });
    let (key, value) = match split {
        Some(index) => (&line[..index], &line[index + 1..]),
        None => (line, ""),
    };
    Property {
        raw: line.to_owned(),
        key: key.to_owned(),
        name: parameters(key)[0].to_ascii_uppercase(),
        value: value.to_owned(),
    }
}

/// Splits `NAME;A=1;B="x;y"` at semicolons outside quotes.
fn parameters(key: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (index, character) in key.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(&key[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&key[start..]);
    parts
}

fn calendar_address(value: &str) -> String {
    let value = value.trim();
    let value = match value.get(..7) {
        Some(scheme) if scheme.eq_ignore_ascii_case("mailto:") => &value[7..],
        _ => value,
    };
    value.trim().to_lowercase()
}

fn is_self_attendee(line: &Property, self_email: &str) -> bool {
    calendar_address(&line.value) == self_email
        || parameters(&line.key).iter().skip(1).any(|parameter| {
            parameter.split_once('=').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("EMAIL")
                    && calendar_address(value.trim_matches('"')) == self_email
            })
        })
}

/// Keeps the attendee's own parameters (CN, ROLE, CUTYPE…) and value, drops
/// the request-only RSVP and the old PARTSTAT, then states the new PARTSTAT.
fn reply_attendee(line: &Property, response: RsvpResponse) -> String {
    let params = parameters(&line.key);
    let mut key = "ATTENDEE".to_owned();
    for parameter in params.iter().skip(1) {
        let name = parameter.split('=').next().unwrap_or_default();
        if !name.eq_ignore_ascii_case("PARTSTAT") && !name.eq_ignore_ascii_case("RSVP") {
            key.push(';');
            key.push_str(parameter);
        }
    }
    format!("{key};PARTSTAT={}:{}", response.partstat(), line.value)
}
