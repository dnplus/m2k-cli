//! `meeting cancel`：取消 `meeting book` 建立的會議。
//!
//! book 在自己與會議室行事曆各寫一份同 UID 的副本（Mail2000 未提供 RFC 6638
//! 伺服器排程時的 fallback），所以取消時要 live 依 UID 找出每個 collection 裡的
//! 副本，逐筆以 ETag 條件刪除並逐筆回報。本機 SQLite 只是快取，不當查找依據。
//! 預設不寄任何信；`--notify-attendees` 才以 SMTP 寄 METHOD:CANCEL。

use crate::{
    config,
    meeting::{
        catalog_for_names, collect_rooms, fold_ical_line, unfold_ics_lines, validate_calendar_url,
    },
    model::{Item, Kind},
    provider::{self, dav::DeleteOutcome},
    store::Store,
    sync,
};
use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use clap::Args;
use serde::Serialize;
use std::{collections::BTreeSet, fmt, path::Path};

const LOOKUP_BATCH_SIZE: usize = 4;

#[derive(Debug, Args)]
pub struct CancelArgs {
    #[arg(help = "要取消的會議 UID（meeting book 輸出的 uid）")]
    pub uid: String,
    #[arg(
        long = "room",
        alias = "name",
        help = "只檢查這些會議室（可重複）；省略時檢查所有會議室"
    )]
    pub rooms: Vec<String>,
    #[arg(long = "url", help = "另外檢查的行事曆 collection URL（可重複）")]
    pub urls: Vec<String>,
    #[arg(long, help = "只列出會刪除的副本，不刪除、不寄信")]
    pub dry_run: bool,
    #[arg(
        long,
        help = "刪除完成後以 SMTP 寄 METHOD:CANCEL 給與會者（不含會議室）；預設不寄"
    )]
    pub notify_attendees: bool,
}

#[derive(Debug, Serialize)]
pub struct CancelOutput {
    /// dry-run、complete、partial、incomplete（有行事曆查不到，未刪除任何副本）或 not_found。
    pub state: String,
    pub uid: String,
    pub dry_run: bool,
    pub checked: Vec<CheckedCalendar>,
    pub copies: Vec<CancelCopy>,
    pub lookup_errors: Vec<LookupError>,
    pub notify_attendees: bool,
    pub notify_recipients: Vec<String>,
    pub notified: bool,
    pub message_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CheckedCalendar {
    pub target: String,
    pub calendar_url: String,
    pub found: usize,
}

#[derive(Debug, Serialize)]
pub struct CancelCopy {
    pub target: String,
    pub calendar_url: String,
    pub href: String,
    pub etag: Option<String>,
    /// would_delete、deleted、not_found、precondition_failed、error 或 skipped。
    pub result: String,
    pub error: Option<String>,
    pub cache_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LookupError {
    pub target: String,
    pub calendar_url: String,
    pub error: String,
}

/// 取消未完成（找不到副本、部分刪除失敗或有 collection 查不到）。
/// 讓 CLI 以非零結束，JSON 錯誤輸出仍帶完整逐筆結果。
#[derive(Debug)]
pub struct CancelIncomplete(pub CancelOutput);

impl fmt::Display for CancelIncomplete {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.state.as_str() {
            "not_found" => write!(f, "所有已檢查的行事曆都找不到 UID {}", self.0.uid),
            "incomplete" => write!(
                f,
                "{} 個行事曆查詢失敗，為避免只刪一半，未刪除任何副本；請排除錯誤或用 --room／--url 指定會議室後再執行",
                self.0.lookup_errors.len()
            ),
            _ => write!(
                f,
                "取消未完成：{} 筆副本未刪除、{} 個行事曆查詢失敗；請依逐筆結果確認後再處理，不要盲目重試",
                self.0
                    .copies
                    .iter()
                    .filter(|copy| !matches!(copy.result.as_str(), "deleted" | "not_found"))
                    .count(),
                self.0.lookup_errors.len()
            ),
        }
    }
}

impl std::error::Error for CancelIncomplete {}

#[derive(Clone)]
struct Target {
    label: String,
    calendar_url: String,
}

pub fn execute(data_dir: &Path, args: CancelArgs, json: bool) -> Result<()> {
    let output = cancel(data_dir, args)?;
    let text = match output.state.as_str() {
        "dry-run" => "dry-run：以下為 live 找到、將會刪除的副本；未刪除、未寄信。",
        "complete" if output.notified => "已刪除所有副本並寄出取消通知。",
        "complete" => "已刪除自己與會議室行事曆裡的所有副本；未寄信。",
        _ => "",
    };
    if !json {
        if !text.is_empty() {
            println!("{text}");
        }
        println!("{}", serde_json::to_string_pretty(&output)?);
    }
    if matches!(
        output.state.as_str(),
        "not_found" | "partial" | "incomplete"
    ) {
        return Err(CancelIncomplete(output).into());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    }
    Ok(())
}

pub fn cancel(data_dir: &Path, args: CancelArgs) -> Result<CancelOutput> {
    let uid = args.uid.trim().to_owned();
    if uid.is_empty() || uid.contains(['\r', '\n']) {
        bail!("UID 不可為空白或包含換行");
    }
    let _lock = sync::lock(data_dir)?;
    if data_dir.join(".demo").exists() {
        bail!("展示資料不提供會議取消");
    }
    let config = config::load(data_dir)?;
    let mut store = Store::open(data_dir)?;
    store.ensure_account(&config)?;
    let password = config::password(&config)?;
    let mut session = provider::dav::DavSession::new(&config, &password)?;

    let self_target = Target {
        label: config.email.clone(),
        calendar_url: config.caldav_url.clone(),
    };
    let mut lookup_errors = Vec::new();
    let mut targets = vec![self_target.clone()];
    if args.rooms.is_empty() && args.urls.is_empty() {
        let catalog = collect_rooms(
            Some(&config),
            Some(&password),
            Some(&mut session),
            None,
            None,
        )?;
        for error in catalog.errors {
            lookup_errors.push(LookupError {
                target: "會議室清單".to_owned(),
                calendar_url: String::new(),
                error,
            });
        }
        targets.extend(catalog.resources.into_iter().map(|room| Target {
            label: room.name,
            calendar_url: room.calendar_url,
        }));
    } else {
        let rooms = catalog_for_names(
            Some(&config),
            Some(&password),
            Some(&mut session),
            &args.rooms,
        )?;
        targets.extend(rooms.into_iter().map(|room| Target {
            label: room.name,
            calendar_url: room.calendar_url,
        }));
        for url in &args.urls {
            let url = validate_calendar_url(&config, url)?;
            targets.push(Target {
                label: url.clone(),
                calendar_url: url,
            });
        }
    }
    let mut seen = BTreeSet::new();
    targets.retain(|target| seen.insert(crate::dav_url::identity(&target.calendar_url)));

    // live 依 UID 查每個 collection；401 立即停止，避免拿錯誤密碼連打所有會議室。
    let mut checked = Vec::new();
    let mut found: Vec<(Target, Item)> = Vec::new();
    for batch in targets.chunks(LOOKUP_BATCH_SIZE) {
        let results = std::thread::scope(|scope| {
            let handles = batch
                .iter()
                .map(|target| {
                    let mut worker = session.clone();
                    let uid = uid.as_str();
                    scope.spawn(move || worker.find_uid(&target.calendar_url, uid))
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err(anyhow!("UID 查詢執行緒異常結束")))
                })
                .collect::<Vec<_>>()
        });
        for (target, result) in batch.iter().zip(results) {
            match result {
                Ok(items) => {
                    checked.push(CheckedCalendar {
                        target: target.label.clone(),
                        calendar_url: target.calendar_url.clone(),
                        found: items.len(),
                    });
                    found.extend(items.into_iter().map(|item| (target.clone(), item)));
                }
                Err(error) if crate::error::is_auth_failed(&error) => return Err(error),
                Err(error) => lookup_errors.push(LookupError {
                    target: target.label.clone(),
                    calendar_url: target.calendar_url.clone(),
                    error: format!("{error:#}"),
                }),
            }
        }
    }

    // 別人主辦的會議只能回覆，不能替對方釋放會議室。
    for (target, item) in &found {
        if let Some(organizer) = organizer(&item.raw) {
            if !organizer.eq_ignore_ascii_case(config.email.trim()) {
                bail!(
                    "{} 的副本主辦人是 {organizer}，不是目前帳號；未刪除任何副本（受邀會議請用 meeting rsvp --response decline）",
                    target.label
                );
            }
        }
    }

    let notify_recipients = if args.notify_attendees {
        let source = found
            .iter()
            .find(|(target, _)| target.calendar_url == self_target.calendar_url)
            .or_else(|| found.first())
            .map(|(_, item)| item.raw.as_str());
        match source {
            Some(raw) => notify_recipients(raw, &config.email, &found),
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // 有行事曆查不到時一份都不刪：留下自己那份（含 LOCATION）才找得回會議室副本。
    let lookup_complete = lookup_errors.is_empty();
    let mut copies = Vec::new();
    for (target, item) in &found {
        let mut copy = CancelCopy {
            target: target.label.clone(),
            calendar_url: target.calendar_url.clone(),
            href: item.remote_id.clone(),
            etag: item.etag.clone(),
            result: "would_delete".to_owned(),
            error: None,
            cache_error: None,
        };
        if !args.dry_run && !lookup_complete {
            copy.result = "skipped".to_owned();
        } else if !args.dry_run {
            match session.delete_resource(Kind::Calendar, &item.remote_id, item.etag.as_deref()) {
                Ok(outcome) => {
                    copy.result = match outcome {
                        DeleteOutcome::Deleted => "deleted",
                        DeleteOutcome::NotFound => "not_found",
                        DeleteOutcome::PreconditionFailed => "precondition_failed",
                    }
                    .to_owned();
                    if outcome != DeleteOutcome::PreconditionFailed {
                        copy.cache_error = store
                            .remove(&item.id)
                            .err()
                            .map(|error| format!("{error:#}"));
                    } else {
                        copy.error = Some(
                            "412：副本在查詢後已被修改，未刪除；請重新執行 dry-run 確認".to_owned(),
                        );
                    }
                }
                Err(error) => {
                    copy.result = "error".to_owned();
                    copy.error = Some(format!("{error:#}"));
                }
            }
        }
        copies.push(copy);
    }

    let all_removed = copies
        .iter()
        .all(|copy| matches!(copy.result.as_str(), "deleted" | "not_found"));
    let state = if args.dry_run {
        "dry-run"
    } else if !lookup_complete {
        "incomplete"
    } else if found.is_empty() {
        "not_found"
    } else if all_removed {
        "complete"
    } else {
        "partial"
    };

    let mut message_id = None;
    if state == "complete" && !notify_recipients.is_empty() {
        let raw = found
            .iter()
            .find(|(target, _)| target.calendar_url == self_target.calendar_url)
            .or_else(|| found.first())
            .map(|(_, item)| item.raw.as_str())
            .unwrap_or_default();
        let ics = build_cancel_ics(raw, &config.email, &notify_recipients)?;
        message_id = Some(provider::mail::send_cancel(&config, &password, &ics)?);
    }

    Ok(CancelOutput {
        state: state.to_owned(),
        uid,
        dry_run: args.dry_run,
        checked,
        copies,
        lookup_errors,
        notify_attendees: args.notify_attendees,
        notify_recipients,
        notified: message_id.is_some(),
        message_id,
    })
}

struct Property<'a> {
    name: String,
    params: &'a str,
    value: &'a str,
}

fn split_property(line: &str) -> Option<Property<'_>> {
    let (key, value) = line.split_once(':')?;
    let (name, params) = key.split_once(';').unwrap_or((key, ""));
    Some(Property {
        name: name.trim().to_ascii_uppercase(),
        params,
        value,
    })
}

fn event_lines(raw: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut depth = 0usize;
    let mut in_event = false;
    for line in unfold_ics_lines(raw) {
        let upper = line.trim().to_ascii_uppercase();
        if upper == "BEGIN:VEVENT" && !in_event {
            in_event = true;
            depth = 0;
            continue;
        }
        if !in_event {
            continue;
        }
        if upper.starts_with("BEGIN:") {
            depth += 1;
        } else if upper.starts_with("END:") {
            if depth == 0 {
                break;
            }
            depth -= 1;
            continue;
        }
        if depth == 0 {
            lines.push(line);
        }
    }
    lines
}

fn mailto(value: &str) -> Option<String> {
    let value = value.trim();
    value
        .get(..7)
        .filter(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
        .map(|_| value[7..].trim().to_owned())
        .filter(|email| !email.is_empty())
}

fn organizer(raw: &str) -> Option<String> {
    event_lines(raw).iter().find_map(|line| {
        let property = split_property(line)?;
        (property.name == "ORGANIZER")
            .then(|| mailto(property.value))
            .flatten()
    })
}

fn is_room_attendee(params: &str) -> bool {
    params.split(';').any(|param| {
        param.split_once('=').is_some_and(|(key, value)| {
            key.trim().eq_ignore_ascii_case("CUTYPE")
                && matches!(
                    value.trim().trim_matches('"').to_ascii_uppercase().as_str(),
                    "ROOM" | "RESOURCE"
                )
        })
    })
}

/// 取消通知收件者：ATTENDEE 裡的人，排除自己與會議室（CUTYPE=ROOM／RESOURCE）。
fn notify_recipients(raw: &str, self_email: &str, found: &[(Target, Item)]) -> Vec<String> {
    let room_labels = found
        .iter()
        .map(|(target, _)| target.label.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let mut recipients = Vec::<String>::new();
    for line in event_lines(raw) {
        let Some(property) = split_property(&line) else {
            continue;
        };
        if property.name != "ATTENDEE" || is_room_attendee(property.params) {
            continue;
        }
        let Some(email) = mailto(property.value) else {
            continue;
        };
        if email.eq_ignore_ascii_case(self_email.trim())
            || room_labels.contains(&email.to_ascii_lowercase())
            || recipients
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&email))
        {
            continue;
        }
        recipients.push(email);
    }
    recipients
}

/// RFC 5546 METHOD:CANCEL：沿用原事件的 UID、時間與主旨，SEQUENCE 加一，
/// ATTENDEE 只留要通知的人。
fn build_cancel_ics(raw: &str, self_email: &str, recipients: &[String]) -> Result<String> {
    let lines = event_lines(raw);
    let keep = [
        "UID",
        "DTSTART",
        "DTEND",
        "DURATION",
        "SUMMARY",
        "LOCATION",
        "RECURRENCE-ID",
    ];
    let mut sequence = 0u32;
    let mut event = Vec::new();
    for line in &lines {
        let Some(property) = split_property(line) else {
            continue;
        };
        if property.name == "SEQUENCE" {
            sequence = property.value.trim().parse().unwrap_or(0);
        } else if keep.contains(&property.name.as_str())
            || (property.name == "ATTENDEE"
                && mailto(property.value)
                    .is_some_and(|email| recipients.iter().any(|r| r.eq_ignore_ascii_case(&email))))
        {
            event.push(line.clone());
        }
    }
    if !event
        .iter()
        .any(|line| line.to_ascii_uppercase().starts_with("UID:"))
    {
        bail!("原事件缺少 UID，無法產生取消通知");
    }
    let mut out = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".to_owned(),
        "PRODID:-//m2k//meeting cancel//ZH-TW".to_owned(),
        "METHOD:CANCEL".to_owned(),
        "BEGIN:VEVENT".to_owned(),
        format!("DTSTAMP:{}", Utc::now().format("%Y%m%dT%H%M%SZ")),
        format!("ORGANIZER:mailto:{}", self_email.trim()),
        format!("SEQUENCE:{}", sequence + 1),
        "STATUS:CANCELLED".to_owned(),
    ];
    out.extend(event);
    out.push("END:VEVENT".to_owned());
    out.push("END:VCALENDAR".to_owned());
    let folded = out
        .iter()
        .flat_map(|line| fold_ical_line(line))
        .collect::<Vec<_>>()
        .join("\r\n");
    Ok(format!("{folded}\r\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:abc@m2k\r\nSEQUENCE:2\r\nSUMMARY:週會\r\nDTSTART:20261120T100000Z\r\nDTEND:20261120T110000Z\r\nORGANIZER:mailto:me@example.test\r\nATTENDEE;CN=Me:mailto:me@example.test\r\nATTENDEE;CUTYPE=ROOM:mailto:room@example.test\r\nATTENDEE:mailto:Bob@example.test\r\nBEGIN:VALARM\r\nATTENDEE:mailto:alarm@example.test\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn notify_skips_self_rooms_and_alarm_attendees() {
        // 只通知真人與會者；寄給會議室或自己沒有意義，VALARM 裡的 ATTENDEE 是提醒對象不是與會者。
        assert_eq!(
            notify_recipients(RAW, "ME@example.test", &[]),
            vec!["Bob@example.test".to_owned()]
        );
    }

    #[test]
    fn cancel_ics_bumps_sequence_and_keeps_identity() {
        // 對方的行事曆靠 UID 與較大的 SEQUENCE 才會套用取消。
        let ics =
            build_cancel_ics(RAW, "me@example.test", &["Bob@example.test".to_owned()]).unwrap();
        for expected in [
            "METHOD:CANCEL",
            "UID:abc@m2k",
            "SEQUENCE:3",
            "STATUS:CANCELLED",
            "DTSTART:20261120T100000Z",
            "ATTENDEE:mailto:Bob@example.test",
        ] {
            assert!(ics.contains(expected), "{expected} missing in\n{ics}");
        }
        assert!(!ics.contains("room@example.test") && !ics.contains("alarm@"));
    }

    #[test]
    fn organizer_is_read_from_the_event_only() {
        assert_eq!(organizer(RAW).as_deref(), Some("me@example.test"));
        assert_eq!(organizer("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n"), None);
    }
}
