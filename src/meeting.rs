use crate::{
    config::{self, Config},
    model::{Item, Kind},
    provider,
    room_cache::{RoomCalendarCache, LOOKAHEAD_DAYS},
    site::SiteProfile,
    store::Store,
    sync,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{
    DateTime, Datelike, Duration, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc,
};
use clap::{ArgAction, Args, Subcommand};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const DEFAULT_LUNCH_START: u32 = 12 * 60;
const DEFAULT_LUNCH_END: u32 = 13 * 60;
const ROOM_QUERY_BATCH_SIZE: usize = 4;
pub(crate) const MAX_ROOM_SLOT_DAYS: i64 = 31;

#[derive(Debug, Subcommand)]
pub enum MeetingCommand {
    #[command(about = "列出 Mail2000 會議室資源與本機 fallback 清單")]
    Resources(ResourcesArgs),
    #[command(about = "讀取自己、同事或會議室的行事曆事件")]
    Events(EventsArgs),
    #[command(about = "查詢一組對象的 free/busy，未知狀態不會當成空檔")]
    FreeBusy(FreeBusyArgs),
    #[command(about = "找共同空檔時段")]
    Slots(SlotsArgs),
    #[command(about = "找指定條件下可用的會議室")]
    Rooms(RoomsArgs),
    #[command(about = "檢查與會者和會議室的會議方案衝突")]
    Review(ReviewArgs),
    #[command(about = "建立會議邀請 ICS 草稿，不寄信")]
    Prepare(PrepareArgs),
    #[command(about = "解析或明確寄送 ICS 會議邀請")]
    Invite(InviteArgs),
    #[command(about = "live 檢查並把會議寫入自己與指定會議室行事曆")]
    Book(BookArgs),
    #[command(about = "以 METHOD:REPLY 回覆郵件中的會議邀請（接受／拒絕／暫定）")]
    Rsvp(crate::meeting_rsvp::RsvpArgs),
}

#[derive(Debug, Args)]
pub struct ResourcesArgs {
    #[arg(long)]
    pub site: Option<String>,
    #[arg(long)]
    pub capacity: Option<u32>,
}

#[derive(Debug, Args)]
pub struct EventsArgs {
    #[arg(long, conflicts_with = "calendar")]
    pub attendee: Option<String>,
    #[arg(long, conflicts_with = "attendee")]
    pub calendar: Option<String>,
    #[arg(long)]
    pub start: String,
    #[arg(long)]
    pub end: String,
}

#[derive(Debug, Args)]
pub struct FreeBusyArgs {
    #[arg(long = "attendee", value_delimiter = ',', action = ArgAction::Append)]
    pub attendees: Vec<String>,
    #[arg(long = "calendar", value_delimiter = ',', action = ArgAction::Append)]
    pub calendars: Vec<String>,
    #[arg(long)]
    pub start: String,
    #[arg(long)]
    pub end: String,
}

#[derive(Debug, Args)]
pub struct SlotsArgs {
    #[arg(long = "attendee", value_delimiter = ',', action = ArgAction::Append)]
    pub attendees: Vec<String>,
    #[arg(long = "calendar", value_delimiter = ',', action = ArgAction::Append)]
    pub calendars: Vec<String>,
    #[arg(long)]
    pub start: String,
    #[arg(long)]
    pub end: String,
    #[arg(long, default_value_t = 30)]
    pub minutes: u32,
    #[arg(long, default_value_t = 15)]
    pub step: u32,
    #[arg(long, default_value = "09:30-17:30")]
    pub working_hours: String,
    #[arg(long, help = "把 12:00-13:00 午休納入候選時段")]
    pub include_lunch: bool,
}

#[derive(Debug, Args)]
pub struct RoomsArgs {
    #[arg(long)]
    pub site: Option<String>,
    #[arg(long)]
    pub capacity: Option<u32>,
    #[arg(long)]
    pub start: String,
    #[arg(long)]
    pub end: String,
    #[arg(long, help = "列出至少此分鐘數的最大連續空檔；範圍最多 31 天")]
    pub minutes: Option<u32>,
    #[arg(
        long,
        default_value = "09:30-17:30",
        help = "時長搜尋的每日工作時間，以開始時間的 UTC offset 解讀"
    )]
    pub working_hours: String,
    #[arg(long, help = "略過會議室目錄與行事曆快取，立即重新查詢")]
    pub refresh: bool,
}

#[derive(Debug, Args)]
pub struct ReviewArgs {
    #[arg(long = "attendee", value_delimiter = ',', action = ArgAction::Append)]
    pub attendees: Vec<String>,
    #[arg(long = "room", value_delimiter = ',', action = ArgAction::Append)]
    pub rooms: Vec<String>,
    #[arg(long)]
    pub start: String,
    #[arg(long)]
    pub end: String,
}

#[derive(Debug, Args)]
pub struct PrepareArgs {
    #[arg(long)]
    pub title: String,
    #[arg(long)]
    pub start: String,
    #[arg(long)]
    pub end: String,
    #[arg(long = "attendee", value_delimiter = ',', action = ArgAction::Append)]
    pub attendees: Vec<String>,
    #[arg(long)]
    pub location: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Debug, Args)]
pub struct InviteArgs {
    #[arg(long)]
    pub file: PathBuf,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long, conflicts_with = "dry_run")]
    pub send: bool,
}

#[derive(Debug, Args)]
pub struct BookArgs {
    #[arg(long)]
    pub file: PathBuf,
    #[arg(long, alias = "name", conflicts_with = "url")]
    pub room: Option<String>,
    #[arg(long, conflicts_with = "room")]
    pub url: Option<String>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long, value_parser = ["both", "self", "room"], default_value = "both")]
    pub target: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct TimeRange {
    pub start: DateTime<FixedOffset>,
    pub end: DateTime<FixedOffset>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeetingEvent {
    pub id: String,
    pub uid: Option<String>,
    pub title: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub location: Option<String>,
    pub description: Option<String>,
    pub attendees: Vec<String>,
    pub recurring: bool,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingRoom {
    pub name: String,
    pub access_name: String,
    pub site: String,
    pub capacity: Option<u32>,
    pub calendar_url: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Slot {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Availability {
    pub target: String,
    pub calendar_url: String,
    pub state: String,
    pub free: bool,
    pub events: Vec<MeetingEvent>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
struct Target {
    label: String,
    calendar_url: String,
}

#[derive(Debug, Clone)]
struct Probe {
    target: Target,
    events: Vec<MeetingEvent>,
    unknown: Option<String>,
    items: Vec<Item>,
    complete: bool,
}

#[derive(Debug, Clone)]
struct ParsedIcs {
    uid: Option<String>,
    title: String,
    start: Option<String>,
    end: Option<String>,
    location: Option<String>,
    description: Option<String>,
    attendees: Vec<String>,
    organizer: Option<String>,
    recurring: bool,
    has_recurrence_id: bool,
    duration: Option<Duration>,
    all_day: bool,
    parse_error: Option<String>,
    ignored: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct WorkingHours {
    pub(crate) start: u32,
    pub(crate) end: u32,
}

#[derive(Debug, Serialize)]
struct ResourcesOutput {
    resources: Vec<MeetingRoom>,
    errors: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct EventsOutput {
    target: String,
    calendar_url: String,
    events: Vec<MeetingEvent>,
    unknown: Option<String>,
}

#[derive(Debug, Serialize)]
struct SlotsOutput {
    start: String,
    end: String,
    minutes: u32,
    step: u32,
    working_hours: String,
    slots: Vec<Slot>,
    unknown: Vec<Availability>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoomsOutput {
    pub start: String,
    pub end: String,
    pub rooms: Vec<MeetingRoom>,
    pub availability: Vec<Availability>,
    pub unknown: Vec<Availability>,
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub minutes: Option<u32>,
    pub working_hours: String,
    pub available_slots: Vec<RoomAvailabilitySlots>,
    pub cache_hits: usize,
    pub calendars: Vec<RoomCalendarSource>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoomCalendarSource {
    pub room_url: String,
    pub source: String,
    pub fetched_at: Option<DateTime<Utc>>,
    pub coverage_start: Option<DateTime<Utc>>,
    pub coverage_end: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoomAvailabilitySlots {
    pub room: MeetingRoom,
    pub slots: Vec<Slot>,
}

#[derive(Debug, Serialize)]
struct ReviewOutput {
    start: String,
    end: String,
    checks: Vec<Availability>,
    feasible: bool,
}

#[derive(Debug, Serialize)]
struct PrepareOutput {
    output: PathBuf,
    uid: String,
    attendees: Vec<String>,
    start: String,
    end: String,
}

#[derive(Debug, Serialize)]
struct InviteOutput {
    file: PathBuf,
    uid: Option<String>,
    title: String,
    start: Option<String>,
    end: Option<String>,
    attendees: Vec<String>,
    dry_run: bool,
    sent: bool,
    message_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct BookOutput {
    state: String,
    uid: Option<String>,
    room: Option<String>,
    target: String,
    start: Option<String>,
    end: Option<String>,
    dry_run: bool,
    warnings: Vec<String>,
    completed: Vec<BookTargetResult>,
    failed: Option<BookFailure>,
    remediation: Option<String>,
    receipt: Option<BookReceipt>,
}

#[derive(Debug, Serialize)]
struct BookTargetResult {
    target: String,
    calendar_url: String,
    item_id: Option<String>,
    cache_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct BookFailure {
    target: String,
    calendar_url: String,
    error: String,
}

#[derive(Debug, Serialize)]
struct BookReceipt {
    collection: String,
    resource_url: String,
    uid: Option<String>,
    operation: String,
    status: String,
    remote_accepted: bool,
    reconcile_required: bool,
    reason: String,
}

pub fn execute(data_dir: &Path, command: MeetingCommand, json: bool) -> Result<()> {
    match command {
        MeetingCommand::Resources(args) => execute_resources(data_dir, args, json),
        MeetingCommand::Events(args) => execute_events(data_dir, args, json),
        MeetingCommand::FreeBusy(args) => execute_free_busy(data_dir, args, json),
        MeetingCommand::Slots(args) => execute_slots(data_dir, args, json),
        MeetingCommand::Rooms(args) => execute_rooms(data_dir, args, json),
        MeetingCommand::Review(args) => execute_review(data_dir, args, json),
        MeetingCommand::Prepare(args) => execute_prepare(data_dir, args, json),
        MeetingCommand::Invite(args) => execute_invite(data_dir, args, json),
        MeetingCommand::Book(args) => execute_book(data_dir, args, json),
        MeetingCommand::Rsvp(args) => crate::meeting_rsvp::execute(data_dir, args, json),
    }
}

pub fn parse_time_range(start: &str, end: &str) -> Result<TimeRange> {
    let start = parse_rfc3339(start, "開始時間")?;
    let end = parse_rfc3339(end, "結束時間")?;
    if end <= start {
        bail!("結束時間必須晚於開始時間");
    }
    Ok(TimeRange { start, end })
}

pub fn event_overlaps(range: &TimeRange, event: &MeetingEvent) -> bool {
    let Some((start, end)) = event_time_range(event) else {
        return false;
    };
    start < range.end && range.start < end
}

fn event_time_range(
    event: &MeetingEvent,
) -> Option<(DateTime<FixedOffset>, DateTime<FixedOffset>)> {
    let (Some(start), Some(end)) = (event.start.as_deref(), event.end.as_deref()) else {
        return None;
    };
    let start = parse_rfc3339(start, "事件開始時間").ok()?;
    let end = parse_rfc3339(end, "事件結束時間").ok()?;
    (end > start).then_some((start, end))
}

pub fn intersect_available_slots(
    range: &TimeRange,
    busy_events: &[MeetingEvent],
    minutes: u32,
    step: u32,
    working_hours: WorkingHoursInput,
) -> Result<Vec<Slot>> {
    if minutes == 0 || step == 0 {
        bail!("minutes 與 step 必須大於 0");
    }
    let hours = WorkingHours {
        start: working_hours.start,
        end: working_hours.end,
    };
    if hours.end <= hours.start || hours.end > 24 * 60 {
        bail!("工作時間範圍無效");
    }
    if busy_events
        .iter()
        .any(|event| event.recurring || event_time_range(event).is_none())
    {
        return Ok(Vec::new());
    }
    let duration = Duration::minutes(i64::from(minutes));
    let increment = Duration::minutes(i64::from(step));
    let mut slots = Vec::new();
    let offset = *range.start.offset();
    let limit = range.end.with_timezone(&offset);
    let mut day = range.start.date_naive();
    while day <= limit.date_naive() {
        let midnight = offset
            .from_local_datetime(&day.and_hms_opt(0, 0, 0).context("工作日期無效")?)
            .single()
            .context("工作日期超出可用範圍")?;
        let day_start = midnight
            .checked_add_signed(Duration::minutes(i64::from(hours.start)))
            .context("工作時間超出可用範圍")?;
        let day_end = midnight
            .checked_add_signed(Duration::minutes(i64::from(hours.end)))
            .context("工作時間超出可用範圍")?
            .min(limit);
        let mut cursor = day_start.max(range.start);
        while let Some(slot_end) = cursor.checked_add_signed(duration) {
            if slot_end > day_end {
                break;
            }
            let blocked = busy_events.iter().any(|event| {
                event_overlaps(
                    &TimeRange {
                        start: cursor,
                        end: slot_end,
                    },
                    event,
                )
            });
            if !blocked {
                slots.push(Slot {
                    start: cursor.to_rfc3339(),
                    end: slot_end.to_rfc3339(),
                });
            }
            let Some(next) = cursor.checked_add_signed(increment) else {
                break;
            };
            cursor = next;
        }
        let Some(next_day) = day.succ_opt() else {
            break;
        };
        day = next_day;
    }
    Ok(slots)
}

pub fn room_available_intervals(
    range: &TimeRange,
    busy_events: &[MeetingEvent],
    minutes: u32,
    working_hours: WorkingHoursInput,
) -> Result<Vec<Slot>> {
    if minutes == 0 {
        bail!("minutes 必須大於 0");
    }
    if range.end <= range.start {
        bail!("結束時間必須晚於開始時間");
    }
    if range.end.signed_duration_since(range.start) > Duration::days(MAX_ROOM_SLOT_DAYS) {
        bail!("會議室時長搜尋範圍最多 31 天；請縮小 --start / --end");
    }
    if working_hours.end <= working_hours.start || working_hours.end > 24 * 60 {
        bail!("工作時間範圍無效");
    }
    let offset = *range.start.offset();
    let limit = range.end.with_timezone(&offset);
    let mut busy = Vec::new();
    for event in busy_events {
        if event.recurring {
            return Ok(Vec::new());
        }
        let Some((start, end)) = event_time_range(event) else {
            return Ok(Vec::new());
        };
        if start < limit && range.start < end {
            busy.push((
                start.with_timezone(&offset).max(range.start),
                end.with_timezone(&offset).min(limit),
            ));
        }
    }
    busy.sort_unstable();
    let mut merged: Vec<(DateTime<FixedOffset>, DateTime<FixedOffset>)> = Vec::new();
    for (start, end) in busy {
        if let Some(previous) = merged.last_mut() {
            if start <= previous.1 {
                previous.1 = previous.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    let minimum = Duration::minutes(i64::from(minutes));
    let mut slots = Vec::new();
    let mut append_gap = |start: DateTime<FixedOffset>, end: DateTime<FixedOffset>| {
        if end.signed_duration_since(start) >= minimum {
            slots.push(Slot {
                start: start.to_rfc3339(),
                end: end.to_rfc3339(),
            });
        }
    };
    let mut day = range.start.date_naive();
    while day <= limit.date_naive() {
        let midnight = offset
            .from_local_datetime(&day.and_hms_opt(0, 0, 0).context("工作日期無效")?)
            .single()
            .context("工作日期超出可用範圍")?;
        let day_start = midnight
            .checked_add_signed(Duration::minutes(i64::from(working_hours.start)))
            .context("工作時間超出可用範圍")?
            .max(range.start);
        let day_end = midnight
            .checked_add_signed(Duration::minutes(i64::from(working_hours.end)))
            .context("工作時間超出可用範圍")?
            .min(limit);
        if day_start < day_end {
            let mut cursor = day_start;
            for &(busy_start, busy_end) in &merged {
                if busy_start >= day_end {
                    break;
                }
                if busy_end <= cursor {
                    continue;
                }
                append_gap(cursor, busy_start.min(day_end));
                cursor = cursor.max(busy_end.min(day_end));
                if cursor >= day_end {
                    break;
                }
            }
            append_gap(cursor, day_end);
        }
        let Some(next_day) = day.succ_opt() else {
            break;
        };
        day = next_day;
    }
    Ok(slots)
}

pub fn booking_window_in_gap(gap: &Slot, start: &str, minutes: u32) -> Result<Slot> {
    if minutes == 0 {
        bail!("會議分鐘必須大於 0");
    }
    let gap_range = parse_time_range(&gap.start, &gap.end)?;
    let start_dt = parse_rfc3339(start, "訂房開始時間")?;
    let end_dt = start_dt
        .checked_add_signed(Duration::minutes(i64::from(minutes)))
        .context("訂房結束時間超出可用範圍")?;
    if start_dt < gap_range.start || end_dt > gap_range.end {
        bail!(
            "實際起迄必須完全落在選定連續空檔 {} → {} 內",
            gap.start,
            gap.end
        );
    }
    Ok(Slot {
        start: start_dt.to_rfc3339(),
        end: end_dt.to_rfc3339(),
    })
}

pub fn booking_range_in_gap(gap: &Slot, start: &str, end: &str) -> Result<Slot> {
    let gap_range = parse_time_range(&gap.start, &gap.end)?;
    let range = parse_time_range(start, end)?;
    if range.start < gap_range.start || range.end > gap_range.end {
        bail!(
            "實際起迄必須完全落在選定連續空檔 {} → {} 內",
            gap.start,
            gap.end
        );
    }
    Ok(Slot {
        start: range.start.to_rfc3339(),
        end: range.end.to_rfc3339(),
    })
}

#[derive(Debug, Clone, Copy)]
pub struct WorkingHoursInput {
    pub start: u32,
    pub end: u32,
}

pub fn escape_ical_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace(['\r', '\n'], "\\n")
}

pub fn build_invite_ics(
    title: &str,
    range: &TimeRange,
    location: Option<&str>,
    description: Option<&str>,
    organizer_name: &str,
    organizer_email: &str,
    attendees: &[String],
) -> Result<(String, String)> {
    let uid = format!("{}@m2k", Uuid::new_v4());
    build_invite_ics_with_uid(
        &uid,
        title,
        range,
        location,
        description,
        (organizer_name, organizer_email),
        attendees,
    )
}

pub fn build_invite_ics_with_uid(
    uid: &str,
    title: &str,
    range: &TimeRange,
    location: Option<&str>,
    description: Option<&str>,
    organizer: (&str, &str),
    attendees: &[String],
) -> Result<(String, String)> {
    let title = title.trim();
    if title.is_empty() {
        bail!("會議主旨不可為空白");
    }
    let uid = validate_uid(uid)?;
    let (organizer_name, organizer_email) = organizer;
    let organizer_email = validate_email(organizer_email)?;
    let now = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".to_owned(),
        "PRODID:-//m2k//m2k//ZH-TW".to_owned(),
        "CALSCALE:GREGORIAN".to_owned(),
        "METHOD:REQUEST".to_owned(),
        "BEGIN:VEVENT".to_owned(),
        format!("UID:{}", escape_ical_text(&uid)),
        format!("DTSTAMP:{now}"),
        format!("DTSTART:{}", format_ical_datetime(range.start)),
        format!("DTEND:{}", format_ical_datetime(range.end)),
        format!("SUMMARY:{}", escape_ical_text(title)),
        format!(
            "ORGANIZER;CN={}:MAILTO:{}",
            escape_ical_text(organizer_name.trim()),
            organizer_email
        ),
    ];
    if let Some(location) = location.map(str::trim).filter(|value| !value.is_empty()) {
        lines.push(format!("LOCATION:{}", escape_ical_text(location)));
    }
    if let Some(description) = description.map(str::trim).filter(|value| !value.is_empty()) {
        lines.push(format!("DESCRIPTION:{}", escape_ical_text(description)));
    }
    let mut normalized = BTreeSet::new();
    for attendee in attendees {
        let email = validate_email(attendee)?;
        if normalized.insert(email.clone()) {
            lines.push(format!(
                "ATTENDEE;CN={};ROLE=REQ-PARTICIPANT:MAILTO:{}",
                escape_ical_text(&email),
                email
            ));
        }
    }
    lines.extend([
        "STATUS:CONFIRMED".to_owned(),
        "TRANSP:OPAQUE".to_owned(),
        "END:VEVENT".to_owned(),
        "END:VCALENDAR".to_owned(),
    ]);
    let folded = lines
        .iter()
        .flat_map(|line| fold_ical_line(line))
        .collect::<Vec<_>>()
        .join("\r\n");
    Ok((format!("{folded}\r\n"), uid))
}

pub(crate) fn fold_ical_line(line: &str) -> Vec<String> {
    if line.len() <= 75 {
        return vec![line.to_owned()];
    }
    let mut output = Vec::new();
    let mut current = String::new();
    let mut width = 0;
    for character in line.chars() {
        let size = character.len_utf8();
        if width + size > 75 && !current.is_empty() {
            output.push(current);
            current = " ".to_owned();
            width = 1;
        }
        current.push(character);
        width += size;
    }
    if !current.is_empty() {
        output.push(current);
    }
    output
}

fn execute_resources(data_dir: &Path, args: ResourcesArgs, json: bool) -> Result<()> {
    validate_site(data_dir, args.site.as_deref())?;
    let (config, password) = optional_online(data_dir)?;
    let mut session = config
        .as_ref()
        .zip(password.as_deref())
        .filter(|(_, password)| !password.starts_with("__M2K_PASSWORD_UNAVAILABLE__:"))
        .map(|(config, password)| provider::dav::DavSession::new(config, password))
        .transpose()?;
    let output = collect_rooms(
        config.as_ref(),
        password.as_deref(),
        session.as_mut(),
        args.site.as_deref(),
        args.capacity,
    )?;
    print_warnings(&output.warnings);
    if json {
        emit(&output, "", true)
    } else {
        println!("{}", resources_text(&output));
        Ok(())
    }
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("警告：{warning}");
    }
}

/// A generic profile with no rooms usually means an upgraded install whose
/// site profile is no longer compiled in.
fn profile_warnings(profile: &SiteProfile, rooms: usize) -> Vec<String> {
    if profile.is_generic() && rooms == 0 {
        vec![crate::site::migration_hint()]
    } else {
        Vec::new()
    }
}

fn resources_text(output: &ResourcesOutput) -> String {
    let summary = format!(
        "找到 {} 間會議室{}。",
        output.resources.len(),
        if output
            .resources
            .iter()
            .any(|room| room.source == "builtin_fallback")
        {
            "目前包含本機 fallback 資源"
        } else {
            ""
        }
    );
    let mut lines = vec![
        crate::plain_output::row(&[(&summary, ROOM_TEXT_WIDTH)]),
        crate::plain_output::row(&[
            ("會議室", 26),
            ("容量", 6),
            ("地點", 18),
            ("來源", 14),
            ("行事曆 URL", 51),
        ]),
    ];
    for room in &output.resources {
        let capacity = room_capacity_text(room);
        lines.push(crate::plain_output::row(&[
            (&room.name, 26),
            (&capacity, 6),
            (&room.site, 18),
            (&room.source, 14),
            (&room.calendar_url, 51),
        ]));
    }
    lines.extend(output.errors.iter().map(|error| room_problem_line(error)));
    lines.join("\n")
}

fn execute_events(data_dir: &Path, args: EventsArgs, json: bool) -> Result<()> {
    let range = parse_time_range(&args.start, &args.end)?;
    let (config, password) = required_online_or_demo(data_dir)?;
    let store = Store::open(data_dir)?;
    let targets = resolve_targets(
        &store,
        &config,
        args.attendee.as_deref().map(|value| vec![value.to_owned()]),
        args.calendar.as_deref().map(|value| vec![value.to_owned()]),
    )?;
    let target = targets
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("缺少查詢對象"))?;
    let mut session = new_dav_session(&config, password.as_deref())?;
    let probe = probe_target(
        data_dir,
        &config,
        password.as_deref(),
        session.as_mut(),
        target.clone(),
        &range,
    )?;
    if let Some(reason) = &probe.unknown {
        bail!("無法讀取 {} 的行事曆：{}", probe.target.label, reason);
    }
    let output = EventsOutput {
        target: probe.target.label.clone(),
        calendar_url: probe.target.calendar_url.clone(),
        events: probe.events,
        unknown: None,
    };
    let text = format!(
        "{} 在指定時段有 {} 筆事件。",
        output.target,
        output.events.len()
    );
    emit(&output, &text, json)
}

fn execute_free_busy(data_dir: &Path, args: FreeBusyArgs, json: bool) -> Result<()> {
    let range = parse_time_range(&args.start, &args.end)?;
    let (config, password) = required_online_or_demo(data_dir)?;
    let store = Store::open(data_dir)?;
    let targets = resolve_targets(&store, &config, Some(args.attendees), Some(args.calendars))?;
    let mut session = new_dav_session(&config, password.as_deref())?;
    let mut result = Vec::new();
    for target in targets {
        let probe = probe_target(
            data_dir,
            &config,
            password.as_deref(),
            session.as_mut(),
            target,
            &range,
        )?;
        let availability = availability_from_probe(&probe, &range);
        result.push(availability);
    }
    let busy = result.iter().filter(|item| item.state == "busy").count();
    let unknown = result.iter().filter(|item| item.state == "unknown").count();
    let text = format!(
        "{} 個對象：{} busy、{} unknown。",
        result.len(),
        busy,
        unknown
    );
    emit(&result, &text, json)
}

fn execute_slots(data_dir: &Path, args: SlotsArgs, json: bool) -> Result<()> {
    let range = parse_time_range(&args.start, &args.end)?;
    if args.minutes == 0 || args.step == 0 {
        bail!("minutes 與 step 必須大於 0");
    }
    let working_hours = parse_working_hours(&args.working_hours)?;
    let (config, password) = required_online_or_demo(data_dir)?;
    let store = Store::open(data_dir)?;
    let targets = resolve_targets(&store, &config, Some(args.attendees), Some(args.calendars))?;
    let mut session = new_dav_session(&config, password.as_deref())?;
    let mut busy_events = Vec::new();
    let mut unknown = Vec::new();
    for target in targets {
        let probe = probe_target(
            data_dir,
            &config,
            password.as_deref(),
            session.as_mut(),
            target,
            &range,
        )?;
        let availability = availability_from_probe(&probe, &range);
        if availability.state == "unknown" {
            unknown.push(availability);
        } else {
            busy_events.extend(probe.events);
        }
    }
    let slots = if unknown.is_empty() {
        intersect_available_slots(
            &range,
            &busy_events,
            args.minutes,
            args.step,
            WorkingHoursInput {
                start: working_hours.start,
                end: working_hours.end,
            },
        )?
        .into_iter()
        .filter(is_weekday)
        .filter(|slot| args.include_lunch || !slot_overlaps_lunch(slot))
        .take(5)
        .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let output = SlotsOutput {
        start: range.start.to_rfc3339(),
        end: range.end.to_rfc3339(),
        minutes: args.minutes,
        step: args.step,
        working_hours: format_working_hours(working_hours),
        slots,
        unknown,
    };
    let text = if output.unknown.is_empty() {
        format!("找到 {} 個共同空檔時段。", output.slots.len())
    } else {
        "有對象的 free/busy 未知，為避免誤排未提供候選時段。".to_owned()
    };
    emit(&output, &text, json)
}

fn slot_overlaps_lunch(slot: &Slot) -> bool {
    let Ok(start) = DateTime::parse_from_rfc3339(&slot.start) else {
        return true;
    };
    let Ok(end) = DateTime::parse_from_rfc3339(&slot.end) else {
        return true;
    };
    let start_minutes = start.hour() * 60 + start.minute();
    let end_minutes = end.hour() * 60 + end.minute();
    start_minutes < DEFAULT_LUNCH_END && end_minutes > DEFAULT_LUNCH_START
}

fn is_weekday(slot: &Slot) -> bool {
    DateTime::parse_from_rfc3339(&slot.start)
        .map(|value| value.weekday().number_from_monday() <= 5)
        .unwrap_or(false)
}

pub fn query_rooms(data_dir: &Path, args: RoomsArgs) -> Result<RoomsOutput> {
    let range = parse_time_range(&args.start, &args.end)?;
    let working_hours = parse_working_hours(&args.working_hours)?;
    if let Some(minutes) = args.minutes {
        if minutes == 0 {
            bail!("minutes 必須大於 0");
        }
        if range.end.signed_duration_since(range.start) > Duration::days(MAX_ROOM_SLOT_DAYS) {
            bail!("會議室時長搜尋範圍最多 31 天；請縮小 --start / --end");
        }
    }
    validate_site(data_dir, args.site.as_deref())?;
    let (config, password) = required_online_or_demo(data_dir)?;
    let demo = data_dir.join(".demo").exists();
    let mut errors = Vec::new();
    let cache = if demo {
        None
    } else {
        match RoomCalendarCache::open(data_dir) {
            Ok(cache) => Some(cache),
            Err(error) => {
                errors.push(format!("會議室快取無法開啟：{error:#}"));
                None
            }
        }
    };
    let mut session = new_dav_session(&config, password.as_deref())?;
    let cached_catalog = cache.as_ref().filter(|_| !args.refresh).and_then(|cache| {
        match cache.lookup_catalog(&config, Utc::now()) {
            Ok(rooms) => rooms,
            Err(error) => {
                errors.push(format!("會議室目錄快取無法讀取：{error:#}"));
                None
            }
        }
    });
    let mut catalog = if let Some(resources) = cached_catalog {
        ResourcesOutput {
            resources,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    } else {
        let catalog = collect_rooms(
            Some(&config),
            password.as_deref(),
            session.as_mut(),
            None,
            None,
        )?;
        if catalog.errors.is_empty() {
            if let Some(cache) = &cache {
                if let Err(error) = cache.store_catalog(&config, Utc::now(), &catalog.resources) {
                    errors.push(format!("會議室目錄快取無法保存：{error:#}"));
                }
            }
        }
        catalog
    };
    let warnings = profile_warnings(
        &SiteProfile::for_config(Some(&config))?,
        catalog.resources.len(),
    );
    catalog
        .resources
        .retain(|room| room_matches_filters(room, args.site.as_deref(), args.capacity));
    errors.extend(catalog.errors);
    let fetch_range = TimeRange {
        start: range.start,
        end: range
            .start
            .checked_add_signed(Duration::days(LOOKAHEAD_DAYS))
            .unwrap_or(range.end)
            .max(range.end),
    };
    let mut available = Vec::new();
    let mut available_slots = Vec::new();
    let mut unknown = Vec::new();
    let mut availability_results = Vec::new();
    let mut cache_hits = 0;
    let mut calendars = Vec::new();
    for batch in catalog.resources.chunks(ROOM_QUERY_BATCH_SIZE) {
        let prepared = batch
            .iter()
            .map(|room| {
                let target = Target {
                    label: room.name.clone(),
                    calendar_url: room.calendar_url.clone(),
                };
                let cached =
                    cache.as_ref().filter(|_| !args.refresh).and_then(|cache| {
                        match cache.lookup(
                            &config,
                            &room.calendar_url,
                            range.start.with_timezone(&Utc),
                            range.end.with_timezone(&Utc),
                            Utc::now(),
                        ) {
                            Ok(Some(cached)) => {
                                let probe = probe_from_items(target, cached.items, &range);
                                if !can_reuse_room_probe(&probe, &range, args.minutes) {
                                    return None;
                                }
                                Some((
                                    probe,
                                    RoomCalendarSource {
                                        room_url: room.calendar_url.clone(),
                                        source: "cache".into(),
                                        fetched_at: Some(cached.fetched_at),
                                        coverage_start: Some(cached.coverage_start),
                                        coverage_end: Some(cached.coverage_end),
                                    },
                                ))
                            }
                            Ok(None) => None,
                            Err(error) => {
                                errors.push(format!("{} 快取無法讀取：{error:#}", room.name));
                                None
                            }
                        }
                    });
                (room, cached)
            })
            .collect::<Vec<_>>();
        let probes = std::thread::scope(|scope| {
            let handles = prepared
                .iter()
                .map(|(room, cached)| {
                    if cached.is_some() {
                        return None;
                    }
                    let target = Target {
                        label: room.name.clone(),
                        calendar_url: room.calendar_url.clone(),
                    };
                    let config = &config;
                    let password = password.as_deref();
                    let fetch_range = &fetch_range;
                    let mut worker_session = session.clone();
                    Some(scope.spawn(move || {
                        probe_target(
                            data_dir,
                            config,
                            password,
                            worker_session.as_mut(),
                            target,
                            fetch_range,
                        )
                    }))
                })
                .collect::<Vec<_>>();
            prepared
                .into_iter()
                .zip(handles)
                .map(|((room, cached), handle)| {
                    let (cached, metadata) = match cached {
                        Some((probe, metadata)) => (Some(probe), Some(metadata)),
                        None => (None, None),
                    };
                    let result = match (cached, handle) {
                        (Some(probe), _) => Ok(probe),
                        (_, Some(handle)) => handle
                            .join()
                            .unwrap_or_else(|_| Err(anyhow!("會議室查詢工作意外結束"))),
                        _ => unreachable!("uncached room has a query worker"),
                    };
                    (room, result, metadata)
                })
                .collect::<Vec<_>>()
        });
        for (room, result, metadata) in probes {
            let cache_hit = metadata.is_some();
            let mut probe = match result {
                Ok(probe) => probe,
                Err(error) => Probe {
                    target: Target {
                        label: room.name.clone(),
                        calendar_url: room.calendar_url.clone(),
                    },
                    events: Vec::new(),
                    unknown: Some(format!("{error:#}")),
                    items: Vec::new(),
                    complete: false,
                },
            };
            let fetched_at = Utc::now();
            calendars.push(metadata.unwrap_or_else(|| {
                RoomCalendarSource {
                    room_url: room.calendar_url.clone(),
                    source: if demo {
                        "demo"
                    } else if probe.complete {
                        "remote"
                    } else {
                        "unknown"
                    }
                    .into(),
                    fetched_at: (probe.complete && !demo).then_some(fetched_at),
                    coverage_start: probe
                        .complete
                        .then_some(fetch_range.start.with_timezone(&Utc)),
                    coverage_end: probe
                        .complete
                        .then_some(fetch_range.end.with_timezone(&Utc)),
                }
            }));
            if cache_hit {
                cache_hits += 1;
            } else if probe.complete && !demo {
                if let Some(cache) = &cache {
                    if let Err(error) = cache.store_success(
                        &config,
                        &room.calendar_url,
                        fetch_range.start.with_timezone(&Utc),
                        fetch_range.end.with_timezone(&Utc),
                        fetched_at,
                        &probe.items,
                    ) {
                        errors.push(format!("{} 快取無法保存：{error:#}", room.name));
                    }
                }
                probe = probe_from_items(probe.target, probe.items, &range);
            }
            let availability = availability_from_probe(&probe, &range);
            if availability.state == "unknown" {
                if let Some(reason) = &availability.reason {
                    errors.push(format!("{}：{reason}", availability.target));
                }
                unknown.push(availability.clone());
            } else if let Some(minutes) = args.minutes {
                let slots = room_available_intervals(
                    &range,
                    &probe.events,
                    minutes,
                    WorkingHoursInput {
                        start: working_hours.start,
                        end: working_hours.end,
                    },
                )?;
                if !slots.is_empty() {
                    available.push(room.clone());
                    available_slots.push(RoomAvailabilitySlots {
                        room: room.clone(),
                        slots,
                    });
                }
            } else if availability.free {
                available.push(room.clone());
            }
            availability_results.push(availability);
        }
    }
    Ok(RoomsOutput {
        start: range.start.to_rfc3339(),
        end: range.end.to_rfc3339(),
        rooms: available,
        availability: availability_results,
        unknown,
        errors,
        warnings,
        minutes: args.minutes,
        working_hours: format_working_hours(working_hours),
        available_slots,
        cache_hits,
        calendars,
    })
}

// An unexpired snapshot cannot prove a room is still free: another user may
// have booked it since the fetch. Recheck every candidate (including slot searches).
fn can_reuse_room_probe(probe: &Probe, range: &TimeRange, minutes: Option<u32>) -> bool {
    minutes.is_none() && availability_from_probe(probe, range).state == "busy"
}

fn execute_rooms(data_dir: &Path, args: RoomsArgs, json: bool) -> Result<()> {
    let output = query_rooms(data_dir, args)?;
    print_warnings(&output.warnings);
    if json {
        emit(&output, "", true)
    } else {
        println!("{}", rooms_text(&output));
        Ok(())
    }
}

const ROOM_TEXT_WIDTH: usize = 119;

fn room_text_row(room: &MeetingRoom, start: &str, end: &str) -> String {
    let capacity = room_capacity_text(room);
    crate::plain_output::row(&[
        (&room.name, 26),
        (&capacity, 6),
        (&room.site, 18),
        (&room.source, 14),
        (start, 25),
        (end, 25),
    ])
}

fn room_capacity_text(room: &MeetingRoom) -> String {
    room.capacity
        .map(|capacity| capacity.to_string())
        .unwrap_or_else(|| "未知".to_owned())
}

fn room_problem_line(error: &str) -> String {
    crate::plain_output::row(&[("查詢問題", 8), (error, ROOM_TEXT_WIDTH - 9)])
}

fn rooms_text(output: &RoomsOutput) -> String {
    let summary = format!(
        "找到 {} 間可用會議室；{} 間查詢結果未知而已排除。",
        output.rooms.len(),
        output.unknown.len()
    );
    let mut lines = vec![
        crate::plain_output::row(&[(&summary, ROOM_TEXT_WIDTH)]),
        crate::plain_output::row(&[
            ("會議室", 26),
            ("容量", 6),
            ("地點", 18),
            ("來源", 14),
            ("開始", 25),
            ("結束", 25),
        ]),
    ];
    if output.minutes.is_some() {
        for entry in &output.available_slots {
            for slot in &entry.slots {
                lines.push(room_text_row(&entry.room, &slot.start, &slot.end));
            }
        }
    } else {
        for room in &output.rooms {
            lines.push(room_text_row(room, &output.start, &output.end));
        }
    }
    lines.extend(output.errors.iter().map(|error| room_problem_line(error)));
    lines.join("\n")
}

fn execute_review(data_dir: &Path, args: ReviewArgs, json: bool) -> Result<()> {
    let range = parse_time_range(&args.start, &args.end)?;
    let (config, password) = required_online_or_demo(data_dir)?;
    let mut session = new_dav_session(&config, password.as_deref())?;
    let store = Store::open(data_dir)?;
    let attendee_targets = resolve_targets(&store, &config, Some(args.attendees), None)?;
    let rooms = catalog_for_names(
        Some(&config),
        password.as_deref(),
        session.as_mut(),
        &args.rooms,
    )?;
    let mut targets = attendee_targets;
    targets.extend(rooms.into_iter().map(|room| Target {
        label: room.name,
        calendar_url: room.calendar_url,
    }));
    if targets.is_empty() {
        targets.push(Target {
            label: config.email.clone(),
            calendar_url: config.caldav_url.clone(),
        });
    }
    let mut checks = Vec::new();
    for target in targets {
        let probe = probe_target(
            data_dir,
            &config,
            password.as_deref(),
            session.as_mut(),
            target,
            &range,
        )?;
        checks.push(availability_from_probe(&probe, &range));
    }
    let feasible = checks.iter().all(|item| item.state == "free");
    let output = ReviewOutput {
        start: range.start.to_rfc3339(),
        end: range.end.to_rfc3339(),
        checks,
        feasible,
    };
    let text = if feasible {
        "會議方案沒有已知衝突。".to_owned()
    } else {
        "會議方案不可直接執行，請處理 busy 或 unknown 對象。".to_owned()
    };
    emit(&output, &text, json)
}

fn execute_prepare(data_dir: &Path, args: PrepareArgs, json: bool) -> Result<()> {
    let _lock = sync::lock(data_dir)?;
    let config = config::load(data_dir)?;
    let mut store = Store::open(data_dir)?;
    store.ensure_account(&config)?;
    let range = parse_time_range(&args.start, &args.end)?;
    let attendees = resolve_attendees(&store, &config, &args.attendees)?;
    let (ics, uid) = build_invite_ics(
        &args.title,
        &range,
        args.location.as_deref(),
        args.description.as_deref(),
        &config.email,
        &config.email,
        &attendees,
    )?;
    write_new(&args.output, ics.as_bytes())?;
    let output = PrepareOutput {
        output: args.output,
        uid,
        attendees,
        start: range.start.to_rfc3339(),
        end: range.end.to_rfc3339(),
    };
    emit(&output, "已建立會議邀請草稿，未寄送。", json)
}

fn execute_invite(data_dir: &Path, args: InviteArgs, json: bool) -> Result<()> {
    let raw = fs::read_to_string(&args.file)
        .with_context(|| format!("無法讀取 ICS：{}", args.file.display()))?;
    let parsed = parse_ics(&raw)?;
    let should_send = args.send && !args.dry_run;
    let mut message_id = None;
    if should_send {
        if !raw
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case("METHOD:REQUEST"))
        {
            bail!("ICS 缺少 METHOD:REQUEST；為避免誤寄普通事件，未寄送");
        }
        let _lock = sync::lock(data_dir)?;
        if data_dir.join(".demo").exists() {
            bail!("展示資料不提供實際寄信");
        }
        let config = config::load(data_dir)?;
        Store::open(data_dir)?.ensure_account(&config)?;
        let password = config::password(data_dir, &config)?;
        message_id = Some(provider::mail::send_invite(&config, &password, &raw)?);
    }
    let output = InviteOutput {
        file: args.file,
        uid: parsed.uid,
        title: parsed.title,
        start: parsed.start,
        end: parsed.end,
        attendees: parsed.attendees,
        dry_run: args.dry_run || !should_send,
        sent: should_send,
        message_id,
    };
    let text = if should_send {
        "已透過 SMTP 寄送 METHOD:REQUEST 會議邀請。"
    } else {
        "已解析會議邀請；未寄送。請明確加上 --send 才會寄信。"
    };
    emit(&output, text, json)
}

fn execute_book(data_dir: &Path, args: BookArgs, json: bool) -> Result<()> {
    let _lock = sync::lock(data_dir)?;
    let raw = fs::read_to_string(&args.file)
        .with_context(|| format!("無法讀取 ICS：{}", args.file.display()))?;
    let parsed = parse_ics(&raw)?;
    if parse_ics_events(&raw)?
        .iter()
        .any(|event| event.recurring || event.has_recurrence_id)
    {
        bail!("book 不會忽略 ICS 的 RRULE、RDATE、EXDATE 或 RECURRENCE-ID；請先提供單次事件");
    }
    let parsed_event = parsed_event_to_meeting("book".to_owned(), "ics".to_owned(), parsed.clone());
    let start = parsed_event
        .start
        .as_deref()
        .ok_or_else(|| anyhow!("ICS 缺少具時區的 DTSTART"))?;
    let end = parsed_event
        .end
        .as_deref()
        .ok_or_else(|| anyhow!("ICS 缺少具時區的 DTEND 或 DURATION"))?;
    let range = parse_time_range(start, end)?;
    let config = config::load(data_dir)?;
    if data_dir.join(".demo").exists() {
        bail!("展示資料不提供會議室預約");
    }
    let mut store = Store::open(data_dir)?;
    store.ensure_account(&config)?;
    let cached_items = store.list(Kind::Calendar, None, None, usize::MAX)?;
    let password = config::password(data_dir, &config)?;
    let mut session = new_dav_session(&config, Some(&password))?
        .ok_or_else(|| anyhow!("無法建立 Mail2000 DAV session"))?;
    let target_mode = match args.target.as_str() {
        "both" | "self" | "room" => args.target.as_str(),
        value => bail!("target 必須是 both、self 或 room，不接受：{value}"),
    };
    let room = args.room.clone().or_else(|| parsed.location.clone());
    let room_target = if target_mode == "self" {
        None
    } else if let Some(url) = args.url.as_deref() {
        Some(Target {
            label: room.clone().unwrap_or_else(|| "指定會議室".to_owned()),
            calendar_url: validate_calendar_url(&config, url)?,
        })
    } else {
        let name = room.ok_or_else(|| {
            anyhow!("請提供 --room 或 --url，或讓 ICS 的 LOCATION 對應內建會議室")
        })?;
        let catalog = catalog_for_names(
            Some(&config),
            Some(&password),
            Some(&mut session),
            std::slice::from_ref(&name),
        )?;
        let selected = catalog
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("找不到會議室資源：{name}"))?;
        Some(Target {
            label: selected.name,
            calendar_url: selected.calendar_url,
        })
    };
    let self_target = Target {
        label: config.email.clone(),
        calendar_url: config.caldav_url.clone(),
    };
    if target_mode == "both"
        && room_target.as_ref().is_some_and(|room| {
            same_calendar_collection(&self_target.calendar_url, &room.calendar_url)
        })
    {
        bail!("個人行事曆與會議室目標是同一個 collection，為避免重複寫入已停止");
    }
    let uid = match parsed.uid.clone() {
        Some(uid) => validate_uid(&uid)?,
        None => generated_uid_for_ics(&raw),
    };
    let self_probe = if target_mode != "room" {
        let probe = probe_target(
            data_dir,
            &config,
            Some(&password),
            Some(&mut session),
            self_target.clone(),
            &range,
        )?;
        if let Some(reason) = probe.unknown.as_deref() {
            bail!("無法在預約前確認自己的行事曆：{reason}");
        }
        Some(probe)
    } else {
        None
    };
    let room_probe = if target_mode != "self" {
        let room_target = room_target
            .as_ref()
            .ok_or_else(|| anyhow!("缺少會議室目標"))?;
        let probe = probe_target(
            data_dir,
            &config,
            Some(&password),
            Some(&mut session),
            room_target.clone(),
            &range,
        )?;
        if let Some(reason) = probe.unknown.as_deref() {
            bail!("無法在預約前確認會議室狀態，未寫入：{reason}");
        }
        Some(probe)
    } else {
        None
    };
    let self_conflict = self_probe.as_ref().is_some_and(|probe| {
        probe
            .events
            .iter()
            .any(|event| event_overlaps(&range, event) && event.uid.as_deref() != Some(&uid))
    });
    let room_conflict = room_probe.as_ref().is_some_and(|probe| {
        probe
            .events
            .iter()
            .any(|event| event_overlaps(&range, event) && event.uid.as_deref() != Some(&uid))
    });
    if room_conflict {
        bail!("預約前 live recheck 發現會議室衝突，未寫入任何行事曆");
    }
    let warnings = if self_conflict {
        vec!["自己的行事曆已有重疊行程；仍可在明確預約流程中寫入。".to_owned()]
    } else {
        Vec::new()
    };
    let booking_ics = booking_resource_ics(&raw, &uid)?;
    if args.dry_run {
        let output = BookOutput {
            state: "dry-run".to_owned(),
            uid: Some(uid),
            room: room_target.as_ref().map(|target| target.label.clone()),
            target: target_mode.to_owned(),
            start: Some(range.start.to_rfc3339()),
            end: Some(range.end.to_rfc3339()),
            dry_run: true,
            warnings,
            completed: Vec::new(),
            failed: None,
            remediation: None,
            receipt: None,
        };
        return emit(
            &output,
            "已完成 live recheck；dry-run 未寫入任何行事曆。",
            json,
        );
    }
    let room_label = room_target.as_ref().map(|target| target.label.clone());
    let mut targets = Vec::new();
    if target_mode != "room" {
        targets.push(self_target.clone());
    }
    if target_mode != "self" {
        targets.push(
            room_target
                .clone()
                .ok_or_else(|| anyhow!("缺少會議室目標"))?,
        );
    }
    let mut completed = Vec::new();
    for target in targets {
        let existing = if target.calendar_url == self_target.calendar_url {
            self_probe
                .as_ref()
                .and_then(|probe| find_existing_uid(&probe.items, &uid))
        } else {
            room_probe
                .as_ref()
                .and_then(|probe| find_existing_uid(&probe.items, &uid))
        }
        .or_else(|| find_existing_uid_in_collection(&cached_items, &target.calendar_url, &uid));
        match session.put(
            Kind::Calendar,
            &target.calendar_url,
            &booking_ics,
            existing.as_ref(),
        ) {
            Ok(item) => {
                let item_id = item.id.clone();
                let cache_error = store.upsert(&item).err().map(|error| format!("{error:#}"));
                completed.push(BookTargetResult {
                    target: target.label.clone(),
                    calendar_url: target.calendar_url.clone(),
                    item_id: Some(item_id),
                    cache_error: cache_error.clone(),
                });
                if let Some(error) = cache_error {
                    let retry_target = if target.calendar_url == self_target.calendar_url {
                        "self"
                    } else {
                        "room"
                    };
                    let output = BookOutput {
                        state: "partial".to_owned(),
                        uid: Some(uid.clone()),
                        room: room_label.clone(),
                        target: target_mode.to_owned(),
                        start: Some(range.start.to_rfc3339()),
                        end: Some(range.end.to_rfc3339()),
                        dry_run: false,
                        warnings,
                        completed,
                        failed: Some(BookFailure {
                            target: format!("{}（本機快取）", target.label),
                            calendar_url: target.calendar_url,
                            error: format!("遠端寫入成功，但本機快取失敗：{error}"),
                        }),
                        remediation: Some(format!(
                            "遠端已寫入並保留 receipt；請先修復本機快取，再以相同 UID 執行：{}",
                            book_retry_command(
                                &args.file,
                                room_label.as_deref().unwrap_or_default(),
                                args.url.as_deref(),
                                retry_target,
                            )
                        )),
                        receipt: None,
                    };
                    return emit(
                        &output,
                        "遠端寫入成功但本機快取未完成，預約狀態為 partial。",
                        json,
                    );
                }
            }
            Err(error) => {
                if let Some(pending) =
                    error.downcast_ref::<provider::dav::RemoteWritePendingReconcile>()
                {
                    let state = if completed.is_empty() {
                        "unknown"
                    } else {
                        "partial"
                    };
                    let resource_url = pending.resource_url.clone();
                    let output = BookOutput {
                        state: state.to_owned(),
                        uid: pending.uid.clone().or_else(|| Some(uid.clone())),
                        room: room_label.clone(),
                        target: target_mode.to_owned(),
                        start: Some(range.start.to_rfc3339()),
                        end: Some(range.end.to_rfc3339()),
                        dry_run: false,
                        warnings,
                        completed,
                        failed: Some(BookFailure {
                            target: target.label,
                            calendar_url: resource_url.clone(),
                            error: format!("{error:#}"),
                        }),
                        remediation: Some(format!(
                            "遠端 PUT 已接受但尚未完成 reconciliation；請先重新同步或檢查 resource URL {}，不要盲目重試。",
                            resource_url
                        )),
                        receipt: Some(BookReceipt {
                            collection: pending.collection.clone(),
                            resource_url,
                            uid: pending.uid.clone(),
                            operation: pending.operation.clone(),
                            status: pending.status.clone(),
                            remote_accepted: pending.remote_accepted,
                            reconcile_required: pending.reconcile_required,
                            reason: pending.reason.clone(),
                        }),
                    };
                    return emit(
                        &output,
                        "DAV PUT 已接受但尚未完成 reconciliation，預約狀態不是 complete。",
                        json,
                    );
                }
                if completed.is_empty() {
                    bail!(
                        "寫入 {} 未完成且結果不確定，請依 UID 與資源確認遠端現況後再處理：{error:#}",
                        target.label
                    );
                }
                let output = BookOutput {
                    state: "partial".to_owned(),
                    uid: Some(uid.clone()),
                    room: room_label.clone(),
                    target: target_mode.to_owned(),
                    start: Some(range.start.to_rfc3339()),
                    end: Some(range.end.to_rfc3339()),
                    dry_run: false,
                    warnings,
                    completed,
                    failed: Some(BookFailure {
                        target: target.label,
                        calendar_url: target.calendar_url,
                        error: format!("{error:#}"),
                    }),
                    remediation: Some(format!(
                        "請先人工檢查已完成與失敗目標的遠端現況，再以相同 UID 執行：{}；不要盲目重試。",
                        book_retry_command(
                            &args.file,
                            room_label.as_deref().unwrap_or_default(),
                            args.url.as_deref(),
                            "room",
                        )
                    )),
                    receipt: None,
                };
                return emit(&output, "預約只完成部分寫入，請依 remediation 處理。", json);
            }
        }
    }
    let output = BookOutput {
        state: "complete".to_owned(),
        uid: Some(uid),
        room: room_label,
        target: target_mode.to_owned(),
        start: Some(range.start.to_rfc3339()),
        end: Some(range.end.to_rfc3339()),
        dry_run: false,
        warnings,
        completed,
        failed: None,
        remediation: None,
        receipt: None,
    };
    let text = match target_mode {
        "self" => "已完成個人行事曆寫入。",
        "room" => "已完成會議室行事曆寫入。",
        _ => "已完成會議室與個人行事曆寫入。",
    };
    emit(&output, text, json)
}

fn emit<T: Serialize>(value: &T, text: &str, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{text}");
        println!("{}", serde_json::to_string_pretty(value)?);
    }
    Ok(())
}

fn parse_rfc3339(value: &str, label: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value.trim())
        .with_context(|| format!("{label} 必須是帶時區的 RFC3339 datetime"))
}

pub(crate) fn format_ical_datetime(value: DateTime<FixedOffset>) -> String {
    value
        .with_timezone(&Utc)
        .format("%Y%m%dT%H%M%SZ")
        .to_string()
}

pub(crate) fn parse_working_hours(value: &str) -> Result<WorkingHours> {
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| anyhow!("working-hours 必須是 HH:MM-HH:MM"))?;
    let parse = |value: &str| -> Result<u32> {
        let (hour, minute) = value
            .trim()
            .split_once(':')
            .ok_or_else(|| anyhow!("工作時間必須是 HH:MM"))?;
        let hour: u32 = hour.parse().context("工作時間的小時無效")?;
        let minute: u32 = minute.parse().context("工作時間的分鐘無效")?;
        if hour > 23 || minute > 59 {
            bail!("工作時間超出範圍");
        }
        Ok(hour * 60 + minute)
    };
    let hours = WorkingHours {
        start: parse(start)?,
        end: parse(end)?,
    };
    if hours.end <= hours.start {
        bail!("工作時間結束必須晚於開始");
    }
    Ok(hours)
}

fn format_working_hours(value: WorkingHours) -> String {
    format!(
        "{:02}:{:02}-{:02}:{:02}",
        value.start / 60,
        value.start % 60,
        value.end / 60,
        value.end % 60
    )
}

pub(crate) fn validate_email(value: &str) -> Result<String> {
    let value = value.trim().to_lowercase();
    if value.is_empty()
        || value.contains(['\r', '\n', ' ', '\t'])
        || value.matches('@').count() != 1
    {
        bail!("電子郵件格式無效：{value}");
    }
    let (local, domain) = value
        .split_once('@')
        .ok_or_else(|| anyhow!("電子郵件缺少 @"))?;
    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        bail!("電子郵件格式無效：{value}");
    }
    Ok(value)
}

fn validate_uid(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        bail!("ICS UID 不可為空白或含控制字元");
    }
    Ok(value.to_owned())
}

fn generated_uid_for_ics(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    let hex = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{hex}@m2k")
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("無法建立檔案 {}；不覆寫既有檔案", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn optional_online(data_dir: &Path) -> Result<(Option<Config>, Option<String>)> {
    if !data_dir.join("config.json").exists() {
        return Ok((None, None));
    }
    let config = config::load(data_dir)?;
    Store::open(data_dir)?.ensure_account(&config)?;
    if data_dir.join(".demo").exists() {
        return Ok((Some(config), None));
    }
    match config::password(data_dir, &config) {
        Ok(password) => Ok((Some(config), Some(password))),
        Err(error) => Ok((
            Some(config),
            Some(format!("__M2K_PASSWORD_UNAVAILABLE__:{error:#}")),
        )),
    }
}

fn required_online_or_demo(data_dir: &Path) -> Result<(Config, Option<String>)> {
    let config = config::load(data_dir)?;
    Store::open(data_dir)?.ensure_account(&config)?;
    if data_dir.join(".demo").exists() {
        return Ok((config, None));
    }
    Ok((config.clone(), Some(config::password(data_dir, &config)?)))
}

fn new_dav_session<'a>(
    config: &'a Config,
    password: Option<&'a str>,
) -> Result<Option<provider::dav::DavSession<'a>>> {
    password
        .filter(|password| !password.starts_with("__M2K_PASSWORD_UNAVAILABLE__:"))
        .map(|password| provider::dav::DavSession::new(config, password))
        .transpose()
}

pub(crate) fn resolve_attendees(
    store: &Store,
    config: &Config,
    values: &[String],
) -> Result<Vec<String>> {
    let mut resolved = Vec::new();
    for value in values {
        for piece in value
            .split(',')
            .map(str::trim)
            .filter(|piece| !piece.is_empty())
        {
            resolved.push(resolve_attendee(store, config, piece)?);
        }
    }
    let mut unique = BTreeSet::new();
    resolved.retain(|value| unique.insert(value.clone()));
    Ok(resolved)
}

fn resolve_attendee(store: &Store, config: &Config, input: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() {
        bail!("與會者不可為空白");
    }
    if input.contains('@') {
        return validate_email(input);
    }
    let contacts = store.list(Kind::Contacts, Some(input), None, usize::MAX)?;
    let exact = contacts
        .iter()
        .filter(|item| item.title == input)
        .flat_map(|item| item.emails.iter())
        .map(|email| email.trim().to_lowercase())
        .filter(|email| email.contains('@'))
        .collect::<BTreeSet<_>>();
    if exact.len() > 1 {
        bail!(
            "與會者名稱不唯一：{input}（{}）",
            exact.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    if let Some(email) = exact.into_iter().next() {
        return validate_email(&email);
    }
    let normalized = input.to_lowercase();
    let email_matches = contacts
        .iter()
        .flat_map(|item| item.emails.iter())
        .filter(|email| email.to_lowercase().starts_with(&format!("{normalized}@")))
        .map(|email| email.trim().to_lowercase())
        .collect::<BTreeSet<_>>();
    if email_matches.len() > 1 {
        bail!(
            "與會者前綴不唯一：{input}（{}）",
            email_matches.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    if let Some(email) = email_matches.into_iter().next() {
        return validate_email(&email);
    }
    let domain = config
        .email
        .split_once('@')
        .map(|(_, domain)| domain.trim())
        .filter(|domain| !domain.is_empty())
        .ok_or_else(|| anyhow!("帳號 email 缺少網域，無法補全與會者：{input}"))?;
    if input
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return validate_email(&format!("{normalized}@{domain}"));
    }
    bail!("找不到與會者 email：{input}")
}

fn origin(config: &Config) -> String {
    let Some(url) = Url::parse(&config.caldav_url)
        .ok()
        .filter(|url| url.host_str().is_some())
    else {
        return format!("https://{}", config.imap_host);
    };
    let mut value = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
    if let Some(port) = url.port() {
        value.push(':');
        value.push_str(&port.to_string());
    }
    value
}

fn percent_encode_segment(value: &str) -> String {
    let mut result = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            result.push(*byte as char);
        } else {
            result.push('%');
            result.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
            result.push(char::from(b"0123456789ABCDEF"[(byte & 0x0f) as usize]));
        }
    }
    result
}

fn user_calendar_url(config: &Config, profile: &SiteProfile, email: &str) -> String {
    format!(
        "{}/{}/default",
        profile.caldav_calendars_url(&origin(config)),
        percent_encode_segment(email)
    )
}

fn room_calendar_url(config: &Config, profile: &SiteProfile, access_name: &str) -> String {
    format!(
        "{}/{}/{}",
        profile.caldav_calendars_url(&origin(config)),
        percent_encode_segment(&profile.room_provider(&config.email)),
        percent_encode_segment(access_name)
    )
}

/// Validate `--site` against the site profile of the configured account.
pub(crate) fn validate_site(data_dir: &Path, value: Option<&str>) -> Result<()> {
    if value.is_none() {
        return Ok(());
    }
    let config = if data_dir.join("config.json").exists() {
        Some(config::load(data_dir)?)
    } else {
        None
    };
    SiteProfile::for_config(config.as_ref())?.validate_site(value)
}

pub(crate) fn builtin_rooms(config: &Config, profile: &SiteProfile) -> Vec<MeetingRoom> {
    profile
        .rooms
        .iter()
        .map(|room| MeetingRoom {
            name: room.name.clone(),
            access_name: room.access.clone(),
            site: profile.site_for_room(&room.name),
            capacity: profile.room_capacity(&room.name),
            calendar_url: room_calendar_url(config, profile, &room.access),
            source: "builtin_fallback".to_owned(),
        })
        .collect()
}

fn collect_rooms(
    config: Option<&Config>,
    password: Option<&str>,
    session: Option<&mut provider::dav::DavSession<'_>>,
    site: Option<&str>,
    capacity: Option<u32>,
) -> Result<ResourcesOutput> {
    let profile = SiteProfile::for_config(config)?;
    let mut rooms = config
        .map(|config| builtin_rooms(config, &profile))
        .unwrap_or_default();
    let mut errors = Vec::new();
    if let (Some(config), Some(password)) = (config, password) {
        if password.starts_with("__M2K_PASSWORD_UNAVAILABLE__:") {
            errors.push(password.replacen("__M2K_PASSWORD_UNAVAILABLE__:", "", 1));
        } else {
            let provider = profile.room_provider(&config.email);
            let resource_url = format!(
                "{}/{}/",
                profile.caldav_calendars_url(&origin(config)),
                percent_encode_segment(&provider)
            );
            let discovered = match session {
                Some(session) => session.discover(Kind::Calendar, &resource_url),
                None => provider::dav::discover(config, password, Kind::Calendar, &resource_url),
            };
            match discovered {
                Ok(collections) => {
                    let mut live = Vec::new();
                    for collection in collections {
                        let Some((access_name, name)) =
                            room_collection_parts(&collection.id, &collection.name, &provider)
                        else {
                            continue;
                        };
                        live.push(MeetingRoom {
                            name,
                            access_name,
                            site: String::new(),
                            capacity: None,
                            calendar_url: collection.id,
                            source: "dav".to_owned(),
                        });
                    }
                    merge_live_rooms(&mut rooms, live, &profile);
                }
                Err(error) => errors.push(format!("DAV resource discovery 失敗：{error:#}")),
            }
        }
    }
    for room in &mut rooms {
        if room.site.is_empty() {
            room.site = profile.site_for_room(&room.name);
        }
        if room.capacity.is_none() {
            room.capacity = profile.room_capacity(&room.name);
        }
    }
    let warnings = if config.is_some() {
        profile_warnings(&profile, rooms.len())
    } else {
        Vec::new()
    };
    rooms.retain(|room| room_matches_filters(room, site, capacity));
    Ok(ResourcesOutput {
        resources: rooms,
        errors,
        warnings,
    })
}

fn room_matches_filters(room: &MeetingRoom, site: Option<&str>, capacity: Option<u32>) -> bool {
    site.map(|value| room.site == value.trim()).unwrap_or(true)
        && capacity
            .map(|value| {
                room.capacity
                    .is_some_and(|room_capacity| room_capacity >= value)
            })
            .unwrap_or(true)
}

fn room_collection_parts(
    url: &str,
    display_name: &str,
    provider: &str,
) -> Option<(String, String)> {
    let parsed = Url::parse(url).ok()?;
    let mut segments = parsed
        .path_segments()?
        .filter(|segment| !segment.is_empty());
    let segment = segments.next_back()?;
    if segment.eq_ignore_ascii_case(provider)
        || segment.eq_ignore_ascii_case(&percent_encode_segment(provider))
    {
        return None;
    }
    let access_name = percent_decode_segment(segment);
    if access_name.is_empty() {
        return None;
    }
    let name = if display_name.trim().is_empty() {
        access_name.clone()
    } else {
        display_name.trim().to_owned()
    };
    Some((access_name, name))
}

fn percent_decode_segment(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let high = chars.next();
            let low = chars.next();
            if let (Some(high), Some(low)) = (high, low) {
                let hex = |value: u8| -> Option<u8> {
                    match value {
                        b'0'..=b'9' => Some(value - b'0'),
                        b'a'..=b'f' => Some(value - b'a' + 10),
                        b'A'..=b'F' => Some(value - b'A' + 10),
                        _ => None,
                    }
                };
                if let (Some(high), Some(low)) = (hex(high), hex(low)) {
                    bytes.push(high * 16 + low);
                    continue;
                }
            }
        }
        bytes.push(byte);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn merge_live_rooms(rooms: &mut Vec<MeetingRoom>, live: Vec<MeetingRoom>, profile: &SiteProfile) {
    for mut item in live {
        if let Some(existing) = rooms.iter_mut().find(|room| {
            room.access_name.eq_ignore_ascii_case(&item.access_name)
                || room.name == item.name
                || room.calendar_url.trim_end_matches('/')
                    == item.calendar_url.trim_end_matches('/')
        }) {
            existing.calendar_url = item.calendar_url;
            existing.source = "builtin+dav".to_owned();
            if existing.name.is_empty() {
                existing.name = item.name;
            }
            if existing.site.is_empty() {
                existing.site = profile.site_for_room(&existing.name);
            }
            if existing.capacity.is_none() {
                existing.capacity = profile.room_capacity(&existing.name);
            }
        } else {
            item.source = "dav".to_owned();
            rooms.push(item);
        }
    }
}

fn catalog_for_names(
    config: Option<&Config>,
    password: Option<&str>,
    session: Option<&mut provider::dav::DavSession<'_>>,
    names: &[String],
) -> Result<Vec<MeetingRoom>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let catalog = collect_rooms(config, password, session, None, None)?;
    let mut result = Vec::new();
    for name in names {
        let target = name.trim();
        if target.is_empty() {
            continue;
        }
        let found = catalog.resources.iter().find(|room| {
            room.name == target
                || room.name.eq_ignore_ascii_case(target)
                || room.access_name.eq_ignore_ascii_case(target)
                || room.calendar_url.trim_end_matches('/') == target.trim_end_matches('/')
        });
        let Some(room) = found else {
            bail!("找不到會議室資源：{target}");
        };
        result.push(room.clone());
    }
    Ok(result)
}

fn validate_calendar_url(config: &Config, value: &str) -> Result<String> {
    let target = Url::parse(value.trim()).map_err(|_| anyhow!("calendar URL 格式錯誤"))?;
    if !matches!(target.scheme(), "http" | "https") || target.host_str().is_none() {
        bail!("calendar URL 必須是 http(s) URL");
    }
    let configured = Url::parse(&config.caldav_url).context("CalDAV URL 格式錯誤")?;
    let same_host = configured
        .host_str()
        .zip(target.host_str())
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right));
    if configured.scheme() != target.scheme()
        || !same_host
        || configured.port_or_known_default() != target.port_or_known_default()
    {
        bail!("calendar URL 必須與已設定 CalDAV origin 相同");
    }
    if target.username() != ""
        || target.password().is_some()
        || target.query().is_some()
        || target.fragment().is_some()
    {
        bail!("calendar URL 不可包含帳密、query 或 fragment");
    }
    Ok(target.to_string())
}

fn same_calendar_collection(left: &str, right: &str) -> bool {
    crate::dav_url::identity(left) == crate::dav_url::identity(right)
}

fn resolve_targets(
    store: &Store,
    config: &Config,
    attendees: Option<Vec<String>>,
    calendars: Option<Vec<String>>,
) -> Result<Vec<Target>> {
    let attendees = attendees.unwrap_or_default();
    let calendars = calendars.unwrap_or_default();
    let profile = SiteProfile::for_config(Some(config))?;
    let rooms = builtin_rooms(config, &profile);
    let mut targets = Vec::new();
    for value in attendees {
        for piece in value
            .split(',')
            .map(str::trim)
            .filter(|piece| !piece.is_empty())
        {
            let room = rooms
                .iter()
                .find(|room| {
                    room.name == piece
                        || room.name.eq_ignore_ascii_case(piece)
                        || room.access_name.eq_ignore_ascii_case(piece)
                })
                .cloned();
            if let Some(room) = room {
                targets.push(Target {
                    label: room.name,
                    calendar_url: room.calendar_url,
                });
            } else {
                let email = resolve_attendee(store, config, piece)?;
                targets.push(Target {
                    label: email.clone(),
                    calendar_url: user_calendar_url(config, &profile, &email),
                });
            }
        }
    }
    for value in calendars {
        for piece in value
            .split(',')
            .map(str::trim)
            .filter(|piece| !piece.is_empty())
        {
            let url = validate_calendar_url(config, piece)?;
            targets.push(Target {
                label: url.clone(),
                calendar_url: url,
            });
        }
    }
    if targets.is_empty() {
        targets.push(Target {
            label: config.email.clone(),
            calendar_url: config.caldav_url.clone(),
        });
    }
    let mut seen = BTreeSet::new();
    targets.retain(|target| seen.insert(target.calendar_url.clone()));
    Ok(targets)
}

fn probe_target(
    data_dir: &Path,
    config: &Config,
    password: Option<&str>,
    session: Option<&mut provider::dav::DavSession<'_>>,
    target: Target,
    range: &TimeRange,
) -> Result<Probe> {
    if data_dir.join(".demo").exists() {
        let store = Store::open(data_dir)?;
        let items = store.list(Kind::Calendar, None, None, usize::MAX)?;
        let is_self = target.label == config.email || target.calendar_url == config.caldav_url;
        let relevant = items
            .iter()
            .filter(|item| is_self || item.collection == target.calendar_url)
            .collect::<Vec<_>>();
        if !is_self && relevant.is_empty() {
            return Ok(Probe {
                target,
                events: Vec::new(),
                unknown: Some("本機沒有此對象的行事曆快取".to_owned()),
                items: Vec::new(),
                complete: false,
            });
        }
        return Ok(probe_from_items(
            target,
            relevant.into_iter().cloned().collect(),
            range,
        ));
    }
    let password = password.ok_or_else(|| anyhow!("缺少 Mail2000 密碼"))?;
    let expand = true;
    let remote_items = match session {
        Some(session) => session.fetch_range(
            &target.calendar_url,
            range.start.with_timezone(&Utc),
            range.end.with_timezone(&Utc),
            expand,
        ),
        None => provider::dav::fetch_range(
            config,
            password,
            &target.calendar_url,
            range.start.with_timezone(&Utc),
            range.end.with_timezone(&Utc),
            expand,
        ),
    };
    let remote_items = match remote_items {
        Ok(items) => items,
        Err(error) => {
            return Ok(Probe {
                target,
                events: Vec::new(),
                unknown: Some(format!("{error:#}")),
                items: Vec::new(),
                complete: false,
            });
        }
    };
    Ok(probe_from_items(target, remote_items, range))
}

fn probe_from_items(target: Target, items: Vec<Item>, range: &TimeRange) -> Probe {
    let (all_events, parse_unknown) = collect_calendar_events(items.iter());
    let events = all_events
        .iter()
        .filter(|event| event.recurring || event_overlaps(range, event))
        .cloned()
        .collect::<Vec<_>>();
    let unknown = parse_unknown.or_else(|| {
        all_events
            .iter()
            .find(|event| event.recurring || event_time_range(event).is_none())
            .map(|event| format!("事件 {} 含未展開的重複規則或不完整時間", event.id))
    });
    Probe {
        target,
        events,
        unknown,
        items,
        complete: true,
    }
}

fn availability_from_probe(probe: &Probe, range: &TimeRange) -> Availability {
    classify_availability(
        &probe.target.label,
        &probe.target.calendar_url,
        range,
        &probe.events,
        probe.unknown.as_deref(),
    )
}

pub fn classify_availability(
    target: &str,
    calendar_url: &str,
    range: &TimeRange,
    events: &[MeetingEvent],
    unknown: Option<&str>,
) -> Availability {
    if let Some(reason) = unknown {
        return Availability {
            target: target.to_owned(),
            calendar_url: calendar_url.to_owned(),
            state: "unknown".to_owned(),
            free: false,
            events: events.to_vec(),
            reason: Some(reason.to_owned()),
        };
    }
    if let Some(event) = events
        .iter()
        .find(|event| event.recurring || event_time_range(event).is_none())
    {
        return Availability {
            target: target.to_owned(),
            calendar_url: calendar_url.to_owned(),
            state: "unknown".to_owned(),
            free: false,
            events: events.to_vec(),
            reason: Some(format!("事件 {} 含未展開的重複規則或無效時間", event.id)),
        };
    }
    let conflicts = events
        .iter()
        .filter(|event| event_overlaps(range, event))
        .cloned()
        .collect::<Vec<_>>();
    if conflicts.is_empty() {
        Availability {
            target: target.to_owned(),
            calendar_url: calendar_url.to_owned(),
            state: "free".to_owned(),
            free: true,
            events: Vec::new(),
            reason: None,
        }
    } else {
        Availability {
            target: target.to_owned(),
            calendar_url: calendar_url.to_owned(),
            state: "busy".to_owned(),
            free: false,
            events: conflicts,
            reason: None,
        }
    }
}

pub fn parse_meeting_ics(raw: &str) -> Result<Vec<MeetingEvent>> {
    Ok(parse_ics_events(raw)?
        .into_iter()
        .filter(|event| !event.ignored)
        .enumerate()
        .map(|(index, event)| {
            parsed_event_to_meeting(format!("ics:{index}"), "ics".to_owned(), event)
        })
        .collect())
}

pub(crate) fn single_calendar_interval(
    raw: &str,
) -> Option<(DateTime<FixedOffset>, DateTime<FixedOffset>)> {
    let mut events = parse_ics_events(raw).ok()?;
    if events.len() != 1 {
        return None;
    }
    let event = events.pop()?;
    if event.recurring || event.has_recurrence_id || event.parse_error.is_some() {
        return None;
    }
    event_time_range(&parsed_event_to_meeting("".into(), "".into(), event))
}

fn parsed_event_to_meeting(id: String, source: String, event: ParsedIcs) -> MeetingEvent {
    let end = event.end.clone().or_else(|| {
        let start = event
            .start
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())?;
        if let Some(duration) = event.duration {
            start
                .checked_add_signed(duration)
                .map(|end| end.to_rfc3339())
        } else if event.all_day {
            start
                .checked_add_signed(Duration::days(1))
                .map(|end| end.to_rfc3339())
        } else {
            None
        }
    });
    MeetingEvent {
        id,
        uid: event.uid,
        title: event.title,
        start: event.start,
        end,
        location: event.location,
        description: event.description,
        attendees: event.attendees,
        recurring: event.recurring,
        source,
    }
}

fn parsed_event_unknown_reason(
    index: usize,
    event: &ParsedIcs,
    mapped: &MeetingEvent,
) -> Option<String> {
    if let Some(error) = event.parse_error.as_deref() {
        return Some(format!("第 {} 個 VEVENT 無法完整解析：{error}", index + 1));
    }
    if event.recurring {
        return Some(format!("第 {} 個 VEVENT 含未展開的重複規則", index + 1));
    }
    if event_time_range(mapped).is_none() {
        return Some(format!(
            "第 {} 個 VEVENT 缺少有效 DTSTART/DTEND 或 DURATION",
            index + 1
        ));
    }
    None
}

fn fallback_item_event(item: &Item) -> MeetingEvent {
    MeetingEvent {
        id: item.id.clone(),
        uid: None,
        title: item.title.clone(),
        start: item.date.clone(),
        end: item.end_date.clone(),
        location: (!item.subtitle.is_empty()).then(|| item.subtitle.clone()),
        description: (!item.text.is_empty()).then(|| item.text.clone()),
        attendees: item.emails.clone(),
        recurring: item.raw.to_ascii_uppercase().contains("RRULE:"),
        source: item.collection.clone(),
    }
}

fn item_events_with_status(item: &Item) -> (Vec<MeetingEvent>, Option<String>) {
    if item.raw.trim().is_empty() {
        let event = fallback_item_event(item);
        let reason = if event.recurring {
            Some(format!("事件 {} 含未展開的重複規則", event.id))
        } else if event_time_range(&event).is_none() {
            Some(format!("事件 {} 缺少有效 DTSTART/DTEND", event.id))
        } else {
            None
        };
        return (vec![event], reason);
    }
    match parse_ics_events(&item.raw) {
        Ok(parsed) => {
            let mut events = Vec::with_capacity(parsed.len());
            let mut unknown = None;
            for (index, parsed_event) in parsed.into_iter().enumerate() {
                if parsed_event.ignored {
                    continue;
                }
                let event = parsed_event_to_meeting(
                    if index == 0 {
                        item.id.clone()
                    } else {
                        format!("{}#{index}", item.id)
                    },
                    item.collection.clone(),
                    parsed_event.clone(),
                );
                if unknown.is_none() {
                    unknown = parsed_event_unknown_reason(index, &parsed_event, &event);
                }
                events.push(event);
            }
            (events, unknown)
        }
        Err(error) => (
            vec![fallback_item_event(item)],
            Some(format!("事件 {} 的 ICS 無法完整解析：{error:#}", item.id)),
        ),
    }
}

fn collect_calendar_events<'a, I>(items: I) -> (Vec<MeetingEvent>, Option<String>)
where
    I: IntoIterator<Item = &'a Item>,
{
    let mut events = Vec::new();
    let mut unknown = None;
    for item in items {
        let (mut item_events, item_unknown) = item_events_with_status(item);
        if unknown.is_none() {
            unknown = item_unknown;
        }
        events.append(&mut item_events);
    }
    (events, unknown)
}

fn find_existing_uid(items: &[Item], uid: &str) -> Option<Item> {
    items.iter().find_map(|item| {
        parse_ics_events(&item.raw).ok().and_then(|events| {
            events
                .iter()
                .any(|event| event.uid.as_deref() == Some(uid))
                .then(|| item.clone())
        })
    })
}

fn find_existing_uid_in_collection(items: &[Item], collection: &str, uid: &str) -> Option<Item> {
    let collection = collection.trim_end_matches('/');
    items
        .iter()
        .filter(|item| item.collection.trim_end_matches('/') == collection)
        .find_map(|item| find_existing_uid(std::slice::from_ref(item), uid))
}

fn book_retry_command(file: &Path, room: &str, url: Option<&str>, target: &str) -> String {
    if target == "self" {
        return format!("m2k meeting book --file {} --target self", file.display());
    }
    let (flag, value) = url.map(|url| ("--url", url)).unwrap_or(("--room", room));
    format!(
        "m2k meeting book --file {} {flag} {value} --target {target}",
        file.display()
    )
}

fn parse_ics_events(raw: &str) -> Result<Vec<ParsedIcs>> {
    let lines = unfold_ics_lines(raw);
    let mut events = Vec::new();
    let mut current: Option<ParsedIcs> = None;
    for line in lines {
        let upper = line.to_ascii_uppercase();
        if upper == "BEGIN:VEVENT" {
            if current.is_some() {
                bail!("ICS VEVENT 缺少 END:VEVENT");
            }
            current = Some(ParsedIcs {
                uid: None,
                title: String::new(),
                start: None,
                end: None,
                location: None,
                description: None,
                attendees: Vec::new(),
                organizer: None,
                recurring: false,
                has_recurrence_id: false,
                duration: None,
                all_day: false,
                parse_error: None,
                ignored: false,
            });
            continue;
        }
        if upper == "END:VEVENT" {
            if let Some(event) = current.take() {
                events.push(event);
            }
            continue;
        }
        let Some(event) = current.as_mut() else {
            continue;
        };
        let Some((raw_key, value)) = line.split_once(':') else {
            continue;
        };
        let key = raw_key
            .split(';')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        match key.as_str() {
            "UID" => event.uid = Some(unescape_ical_text(value)),
            "SUMMARY" => event.title = unescape_ical_text(value),
            "LOCATION" => event.location = Some(unescape_ical_text(value)),
            "DESCRIPTION" => event.description = Some(unescape_ical_text(value)),
            "STATUS" => event.ignored |= value.trim().eq_ignore_ascii_case("CANCELLED"),
            "TRANSP" => event.ignored |= value.trim().eq_ignore_ascii_case("TRANSPARENT"),
            "DTSTART" => {
                event.all_day = has_ical_parameter(raw_key, "VALUE", "DATE");
                match parse_ics_datetime(raw_key, value) {
                    Ok(start) => event.start = Some(start),
                    Err(error) => {
                        event
                            .parse_error
                            .get_or_insert_with(|| format!("{error:#}"));
                    }
                }
            }
            "DTEND" => match parse_ics_datetime(raw_key, value) {
                Ok(end) => event.end = Some(end),
                Err(error) => {
                    event
                        .parse_error
                        .get_or_insert_with(|| format!("{error:#}"));
                }
            },
            "DURATION" => match parse_ical_duration(value) {
                Ok(duration) => event.duration = Some(duration),
                Err(error) => {
                    event
                        .parse_error
                        .get_or_insert_with(|| format!("{error:#}"));
                }
            },
            "ATTENDEE" => {
                let attendee = parse_ics_attendee(raw_key, value);
                if !attendee.is_empty() {
                    event.attendees.push(attendee);
                }
            }
            "ORGANIZER" => {
                let attendee = parse_ics_attendee(raw_key, value);
                if !attendee.is_empty() {
                    event.organizer = Some(attendee);
                }
            }
            "RRULE" | "RDATE" | "EXDATE" => event.recurring = true,
            "RECURRENCE-ID" => event.has_recurrence_id = true,
            _ => {}
        }
    }
    if current.is_some() {
        bail!("ICS VEVENT 缺少 END:VEVENT");
    }
    if events.is_empty() {
        bail!("ICS 內容缺少 VEVENT");
    }
    Ok(events)
}

pub(crate) fn unfold_ics_lines(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(previous) = lines.last_mut() {
                previous.push_str(line.trim_start_matches([' ', '\t']));
            }
        } else if !line.is_empty() {
            lines.push(line.to_owned());
        }
    }
    lines
}

pub(crate) fn unescape_ical_text(value: &str) -> String {
    let mut result = String::new();
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            match character {
                'n' | 'N' => result.push('\n'),
                ',' => result.push(','),
                ';' => result.push(';'),
                '\\' => result.push('\\'),
                _ => {
                    result.push('\\');
                    result.push(character);
                }
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            result.push(character);
        }
    }
    if escaped {
        result.push('\\');
    }
    result
}

pub(crate) fn parse_ics_datetime(raw_key: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if has_ical_parameter(raw_key, "VALUE", "DATE") {
        let date = NaiveDate::parse_from_str(value, "%Y%m%d")
            .or_else(|_| NaiveDate::parse_from_str(value, "%Y-%m-%d"))
            .with_context(|| format!("ICS 全天日期格式錯誤：{value}"))?;
        let offset = FixedOffset::east_opt(8 * 60 * 60).expect("valid Asia/Taipei offset");
        let local = offset
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).expect("valid midnight"))
            .single()
            .ok_or_else(|| anyhow!("ICS 全天日期無法轉換：{value}"))?;
        return Ok(local.to_rfc3339());
    }
    if value.len() == 8 && value.chars().all(|character| character.is_ascii_digit()) {
        bail!("ICS 日期事件沒有明確時區：{value}");
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.to_rfc3339());
    }
    if let Some(naive) = value.strip_suffix('Z') {
        let parsed = NaiveDateTime::parse_from_str(naive, "%Y%m%dT%H%M%S")
            .with_context(|| format!("ICS datetime 格式錯誤：{value}"))?;
        return Ok(DateTime::<Utc>::from_naive_utc_and_offset(parsed, Utc).to_rfc3339());
    }
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
        .with_context(|| format!("ICS datetime 缺少可解析的時區：{value}"))?;
    let tzid = raw_key
        .split(';')
        .skip(1)
        .find_map(|part| {
            let (key, value) = part.split_once('=')?;
            key.eq_ignore_ascii_case("TZID").then_some(value)
        })
        .map(|value| value.trim_matches('"'))
        .ok_or_else(|| anyhow!("ICS datetime 缺少 TZID 或 UTC 時區：{value}"))?;
    let zone: chrono_tz::Tz = tzid
        .parse()
        .map_err(|_| anyhow!("ICS 不支援 TZID：{tzid}"))?;
    let local = zone
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(|| anyhow!("ICS TZID 時間不唯一：{tzid}"))?;
    Ok(local.to_rfc3339())
}

fn has_ical_parameter(raw_key: &str, key: &str, expected: &str) -> bool {
    raw_key.split(';').skip(1).any(|part| {
        let Some((name, value)) = part.split_once('=') else {
            return false;
        };
        name.eq_ignore_ascii_case(key) && value.trim_matches('"').eq_ignore_ascii_case(expected)
    })
}

fn parse_ical_duration(value: &str) -> Result<Duration> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('-') || !value.starts_with('P') {
        bail!("ICS DURATION 格式錯誤：{value}");
    }
    let bytes = value.as_bytes();
    let mut index = 1;
    let mut in_time = false;
    let mut number_start = index;
    let mut total_seconds = 0i64;
    let mut components = 0u32;
    let mut time_components = 0u32;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'T' {
            if in_time || (components == 0 && index != 1) {
                bail!("ICS DURATION 格式錯誤：{value}");
            }
            in_time = true;
            index += 1;
            number_start = index;
            continue;
        }
        if !byte.is_ascii_digit() {
            if index == number_start {
                bail!("ICS DURATION 缺少數值：{value}");
            }
            let number: i64 = value[number_start..index]
                .parse()
                .with_context(|| format!("ICS DURATION 數值無效：{value}"))?;
            let seconds = match byte {
                b'W' if !in_time => number.checked_mul(7 * 24 * 60 * 60),
                b'D' if !in_time => number.checked_mul(24 * 60 * 60),
                b'H' if in_time => number.checked_mul(60 * 60),
                b'M' if in_time => number.checked_mul(60),
                b'S' if in_time => Some(number),
                _ => None,
            }
            .ok_or_else(|| anyhow!("ICS DURATION 單位順序無效：{value}"))?;
            total_seconds = total_seconds
                .checked_add(seconds)
                .ok_or_else(|| anyhow!("ICS DURATION 超出範圍：{value}"))?;
            components += 1;
            if in_time {
                time_components += 1;
            }
            index += 1;
            number_start = index;
            continue;
        }
        index += 1;
    }
    if number_start != index || components == 0 || (in_time && time_components == 0) {
        bail!("ICS DURATION 格式錯誤：{value}");
    }
    Ok(Duration::seconds(total_seconds))
}

fn parse_ics_attendee(raw_key: &str, value: &str) -> String {
    let value = unescape_ical_text(
        value
            .trim()
            .trim_start_matches("mailto:")
            .trim_start_matches("MAILTO:"),
    );
    if value.contains('@') {
        return value.to_lowercase();
    }
    raw_key
        .split(';')
        .skip(1)
        .find_map(|part| {
            part.strip_prefix("CN=")
                .or_else(|| part.strip_prefix("cn="))
        })
        .map(|value| value.trim_matches('"').to_owned())
        .unwrap_or(value)
}

fn parse_ics(raw: &str) -> Result<ParsedIcs> {
    let events = parse_ics_events(raw)?;
    if let Some((index, event)) = events
        .iter()
        .enumerate()
        .find(|(_, event)| event.parse_error.is_some())
    {
        bail!(
            "ICS 第 {} 個 VEVENT 無法完整解析：{}",
            index + 1,
            event.parse_error.as_deref().unwrap_or("未知錯誤")
        );
    }
    let event = events
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("ICS 缺少 VEVENT"))?;
    Ok(event)
}

pub fn booking_resource_ics(raw: &str, uid: &str) -> Result<String> {
    let uid = validate_uid(uid)?;
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut physical = normalized.split('\n').collect::<Vec<_>>();
    if physical.last() == Some(&"") {
        physical.pop();
    }
    let mut in_event = false;
    let mut existing_uid = false;
    for line in &physical {
        let upper = line.trim().to_ascii_uppercase();
        if upper == "BEGIN:VEVENT" {
            in_event = true;
            continue;
        }
        if upper == "END:VEVENT" {
            break;
        }
        if in_event && upper.starts_with("UID:") {
            existing_uid = true;
            break;
        }
    }
    let mut lines = Vec::<String>::new();
    let mut saw_event = false;
    for line in physical {
        let upper = line.trim().to_ascii_uppercase();
        if upper.starts_with("METHOD:") {
            continue;
        }
        if upper == "BEGIN:VEVENT" {
            saw_event = true;
        }
        lines.push(line.to_owned());
        if saw_event && !existing_uid && upper == "BEGIN:VEVENT" {
            lines.push(format!("UID:{}", escape_ical_text(&uid)));
            existing_uid = true;
        }
    }
    if !saw_event {
        bail!("ICS 內容缺少 VEVENT");
    }
    if !existing_uid {
        bail!("ICS 內容缺少 UID");
    }
    Ok(format!("{}\r\n", lines.join("\r\n")))
}

#[cfg(test)]
mod room_output_tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn fixture() -> RoomsOutput {
        RoomsOutput {
            start: "2026-10-08T09:30:00+08:00".into(),
            end: "2026-10-08T10:30:00+08:00".into(),
            rooms: vec![MeetingRoom {
                name: "北區 甲室\r\n次行".into(),
                access_name: "room-a".into(),
                site: "北區".into(),
                capacity: Some(12),
                calendar_url: "https://example.test/room-a/".into(),
                source: "\u{1b}[32mMail2000\u{1b}[0m".into(),
            }],
            availability: Vec::new(),
            unknown: Vec::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            minutes: None,
            working_hours: "09:30-17:30".into(),
            available_slots: Vec::new(),
            cache_hits: 0,
            calendars: Vec::new(),
        }
    }

    fn assert_plain_lines(text: &str, expected: usize) {
        assert_eq!(text.lines().count(), expected);
        for line in text.lines() {
            assert_eq!(line.width(), ROOM_TEXT_WIDTH);
            assert!(line.chars().all(|character| !character.is_control()));
            assert!(!line.contains('|'));
            assert!(!line.contains('│'));
        }
    }

    #[test]
    fn precise_mode_lists_actual_rooms_and_keeps_serializable_values_unchanged() {
        let output = fixture();
        let before = serde_json::to_value(&output).unwrap();
        let text = rooms_text(&output);
        assert_plain_lines(&text, 3);
        assert!(text.contains("找到 1 間可用會議室"));
        assert!(text.contains("北區 甲室 次行"));
        assert!(text.contains("12"));
        assert!(text.contains("Mail2000"));
        assert!(text.contains(&output.start));
        assert!(text.contains(&output.end));
        assert_eq!(serde_json::to_value(&output).unwrap(), before);
    }

    #[test]
    fn minutes_mode_prints_each_available_slot_on_one_row() {
        let mut output = fixture();
        output.minutes = Some(30);
        output.available_slots = vec![RoomAvailabilitySlots {
            room: output.rooms[0].clone(),
            slots: vec![
                Slot {
                    start: output.start.clone(),
                    end: "2026-10-08T10:00:00+08:00".into(),
                },
                Slot {
                    start: "2026-10-08T10:00:00+08:00".into(),
                    end: output.end.clone(),
                },
            ],
        }];
        let text = rooms_text(&output);
        assert_plain_lines(&text, 4);
        assert_eq!(text.matches("北區 甲室 次行").count(), 2);
        assert_eq!(text.matches("2026-10-08T10:00:00+08:00").count(), 2);
    }

    #[test]
    fn long_room_fields_and_errors_are_bounded_without_extra_physical_lines() {
        let mut output = fixture();
        output.rooms[0].name = "很長的會議室名稱".repeat(30);
        output.rooms[0].site = "遠端地點".repeat(20);
        output.errors = vec!["403\r\nserver\t\u{1b}[31merror\u{1b}[0m ".repeat(30)];
        let text = rooms_text(&output);
        assert_plain_lines(&text, 4);
        assert!(text.lines().nth(2).unwrap().contains('…'));
        let problem = text.lines().last().unwrap();
        assert!(problem.starts_with("查詢問題 403 server error"));
        assert!(problem.contains('…'));
    }

    #[test]
    fn resources_catalog_lists_metadata_and_bounds_each_problem_line() {
        let rooms = fixture().rooms;
        let output = ResourcesOutput {
            resources: rooms,
            errors: vec!["catalog\nproblem ".repeat(30)],
            warnings: Vec::new(),
        };
        let before = serde_json::to_value(&output).unwrap();
        let text = resources_text(&output);
        assert_plain_lines(&text, 4);
        assert!(text.contains("找到 1 間會議室"));
        assert!(text.contains("北區 甲室 次行"));
        assert!(text.contains("12"));
        assert!(text.contains("Mail2000"));
        assert!(text.contains("https://example.test/room-a/"));
        assert!(text.lines().last().unwrap().contains('…'));
        assert_eq!(serde_json::to_value(&output).unwrap(), before);
    }
}

#[cfg(test)]
mod room_freshness_tests {
    use super::*;

    #[test]
    fn oct20_room_c_cached_free_requires_recheck_and_fixture_is_busy() {
        let range =
            parse_time_range("2026-10-20T16:00:00+08:00", "2026-10-20T17:30:00+08:00").unwrap();
        let target = Target {
            label: "北區丙室_14".into(),
            calendar_url: "https://example.test/rooms/c/".into(),
        };
        let stale = probe_from_items(target.clone(), vec![], &range);
        assert!(availability_from_probe(&stale, &range).free);
        assert!(!can_reuse_room_probe(&stale, &range, None));
        assert!(!can_reuse_room_probe(&stale, &range, Some(30)));
        let events = parse_meeting_ics(include_str!("../tests/fixtures/room-c-oct20.ics")).unwrap();
        let live = Probe {
            target,
            events,
            unknown: None,
            items: vec![],
            complete: true,
        };
        let result = availability_from_probe(&live, &range);
        assert_eq!(result.state, "busy");
        assert!(!result.free);
        assert!(can_reuse_room_probe(&live, &range, None));
        assert!(!can_reuse_room_probe(&live, &range, Some(30)));
        let failed = Probe {
            unknown: Some("fixture: recheck denied".into()),
            events: vec![],
            complete: false,
            ..live
        };
        assert_eq!(availability_from_probe(&failed, &range).state, "unknown");
        assert!(!availability_from_probe(&failed, &range).free);
    }
}
