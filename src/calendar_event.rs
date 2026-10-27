//! 個人行事曆事件的欄位式建立與修改：產生或改寫 ICS，再交給 [`crate::dav_write`]。
//! 只處理自己主辦、單次、位於自己行事曆的事件；其他一律拒絕並說明原因。

use crate::{
    config::Config,
    human_time,
    meeting::{escape_ical_text, fold_ical_line, format_ical_datetime, unescape_ical_text},
    model::Item,
};
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Asia::Taipei;
use reqwest::Url;

/// 使用者輸入的欄位；`None` 表示不指定（更新時保留原值）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventInput {
    pub title: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub location: Option<String>,
    pub description: Option<String>,
    /// `None` 時：建立為定時事件，更新則沿用原本是否全天。
    pub all_day: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Timed(DateTime<FixedOffset>, DateTime<FixedOffset>),
    /// 起日與「不含」的迄日（ICS DTEND 語意）。
    AllDay(NaiveDate, NaiveDate),
}

impl When {
    fn lines(&self) -> [String; 2] {
        match self {
            Self::Timed(start, end) => [
                format!("DTSTART:{}", format_ical_datetime(*start)),
                format!("DTEND:{}", format_ical_datetime(*end)),
            ],
            Self::AllDay(start, end) => [
                format!("DTSTART;VALUE=DATE:{}", start.format("%Y%m%d")),
                format!("DTEND;VALUE=DATE:{}", end.format("%Y%m%d")),
            ],
        }
    }

    /// 台北時間的表單字串（起、迄）；全天事件的迄日為含當日。
    pub fn form_text(&self) -> (String, String) {
        match self {
            Self::Timed(start, end) => (
                start
                    .with_timezone(&Taipei)
                    .format("%Y-%m-%d %H:%M")
                    .to_string(),
                end.with_timezone(&Taipei)
                    .format("%Y-%m-%d %H:%M")
                    .to_string(),
            ),
            Self::AllDay(start, end) => (
                start.format("%Y-%m-%d").to_string(),
                end.pred_opt()
                    .unwrap_or(*end)
                    .format("%Y-%m-%d")
                    .to_string(),
            ),
        }
    }

    pub fn is_all_day(&self) -> bool {
        matches!(self, Self::AllDay(..))
    }
}

const DEFAULT_TIME: (u32, u32) = (9, 0);

fn moment(input: &str, now: DateTime<Utc>) -> Result<DateTime<FixedOffset>> {
    human_time::parse(input, now, default_time())
}

fn default_time() -> NaiveTime {
    NaiveTime::from_hms_opt(DEFAULT_TIME.0, DEFAULT_TIME.1, 0).expect("valid time")
}

fn day_of(value: DateTime<FixedOffset>) -> NaiveDate {
    value.with_timezone(&Taipei).date_naive()
}

/// 依輸入與原本時間（更新時）決定新的起迄；兩者皆未指定時回傳 `None`。
pub fn resolve_when(
    input: &EventInput,
    base: Option<When>,
    now: DateTime<Utc>,
) -> Result<Option<When>> {
    let start = input
        .start
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let end = input
        .end
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let all_day = input
        .all_day
        .unwrap_or_else(|| base.is_some_and(|base| base.is_all_day()));
    if start.is_none() && end.is_none() && base.is_some_and(|base| base.is_all_day() == all_day) {
        return Ok(None);
    }
    let when = if all_day {
        let first = match (start, base) {
            (Some(start), _) => day_of(moment(start, now)?),
            (None, Some(When::AllDay(first, _))) => first,
            (None, Some(When::Timed(first, _))) => day_of(first),
            (None, None) => bail!("需要開始日期（--start）"),
        };
        let after = match (end, base) {
            (Some(end), _) => {
                let last = human_time::parse_end(end, now, default_time(), at_day(first)?)?;
                next_day(day_of(last))?
            }
            (None, Some(When::AllDay(old_first, old_after))) => first
                .checked_add_signed(old_after - old_first)
                .ok_or_else(out_of_range)?,
            _ => next_day(first)?,
        };
        When::AllDay(first, after)
    } else {
        let first = match (start, base) {
            (Some(start), _) => moment(start, now)?,
            (None, Some(When::Timed(first, _))) => first,
            (None, Some(When::AllDay(..))) => {
                bail!("全天事件改成定時事件時需要開始時間（--start）")
            }
            (None, None) => bail!("需要開始時間（--start）"),
        };
        let last = match (end, base) {
            (Some(end), _) => human_time::parse_end(end, now, default_time(), first)?,
            (None, Some(When::Timed(old_first, old_last))) => first
                .checked_add_signed(old_last - old_first)
                .ok_or_else(out_of_range)?,
            (None, Some(When::AllDay(..))) => first
                .checked_add_signed(Duration::hours(1))
                .ok_or_else(out_of_range)?,
            (None, None) => bail!("需要結束時間（--end）；全天事件可加 --all-day"),
        };
        When::Timed(first, last)
    };
    let valid = match when {
        When::Timed(first, last) => last > first,
        When::AllDay(first, after) => after > first,
    };
    if !valid {
        bail!("結束時間必須晚於開始時間");
    }
    Ok(Some(when))
}

fn out_of_range() -> anyhow::Error {
    anyhow!("時間超出可處理的範圍")
}

fn next_day(day: NaiveDate) -> Result<NaiveDate> {
    day.succ_opt().ok_or_else(out_of_range)
}

fn at_day(day: NaiveDate) -> Result<DateTime<FixedOffset>> {
    Taipei
        .from_local_datetime(&day.and_time(default_time()))
        .single()
        .map(|value| value.fixed_offset())
        .ok_or_else(|| anyhow!("此日期的台北時間不存在"))
}

fn text(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn finish(lines: &[String]) -> String {
    let folded = lines
        .iter()
        .flat_map(|line| fold_ical_line(line))
        .collect::<Vec<_>>()
        .join("\r\n");
    format!("{folded}\r\n")
}

fn stamp(now: DateTime<Utc>) -> String {
    format!("DTSTAMP:{}", now.format("%Y%m%dT%H%M%SZ"))
}

/// 產生新事件的 ICS（不含 METHOD，也不含 ORGANIZER／ATTENDEE：純個人事件）。
pub fn build_new(input: &EventInput, now: DateTime<Utc>) -> Result<String> {
    let title = text(&input.title).ok_or_else(|| anyhow!("事件標題不可為空白（--title）"))?;
    let when = resolve_when(input, None, now)?.expect("create always resolves a time");
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_owned(),
        "VERSION:2.0".to_owned(),
        "PRODID:-//m2k//m2k//ZH-TW".to_owned(),
        "CALSCALE:GREGORIAN".to_owned(),
        "BEGIN:VEVENT".to_owned(),
        format!("UID:{}@m2k", uuid::Uuid::new_v4()),
        stamp(now),
    ];
    lines.extend(when.lines());
    lines.push(format!("SUMMARY:{}", escape_ical_text(title)));
    if let Some(location) = text(&input.location) {
        lines.push(format!("LOCATION:{}", escape_ical_text(location)));
    }
    if let Some(description) = text(&input.description) {
        lines.push(format!("DESCRIPTION:{}", escape_ical_text(description)));
    }
    lines.extend(["END:VEVENT".to_owned(), "END:VCALENDAR".to_owned()]);
    Ok(finish(&lines))
}

/// 已解析的單一 VEVENT 欄位，供表單預填與檢查。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventFields {
    pub title: String,
    pub when: Option<When>,
    /// 起迄無法可靠解析（例如 DURATION 無效）時的原因；這時只能改其他欄位。
    pub time_locked: Option<String>,
    pub location: String,
    pub description: String,
}

fn name_of(line: &str) -> &str {
    let end = line.find([':', ';']).unwrap_or(line.len());
    &line[..end]
}

fn value_of(line: &str) -> &str {
    // 參數值可含引號包住的冒號，因此跳過引號找第一個屬性值分隔冒號。
    let mut quoted = false;
    for (index, character) in line.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ':' if !quoted => return &line[index + 1..],
            _ => {}
        }
    }
    ""
}

/// RFC 5545 unfold：續行只去掉開頭一個空白，保留原文其餘空白。
fn unfold(raw: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in raw.replace("\r\n", "\n").split('\n') {
        if let Some(rest) = line.strip_prefix([' ', '\t']) {
            if let Some(previous) = lines.last_mut() {
                previous.push_str(rest);
                continue;
            }
        }
        if !line.is_empty() {
            lines.push(line.to_owned());
        }
    }
    lines
}

/// VEVENT 第一層（不含 VALARM 等子元件）的邏輯行範圍。
fn event_lines(lines: &[String]) -> Result<(usize, usize)> {
    let starts = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.eq_ignore_ascii_case("BEGIN:VEVENT"))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    match starts.as_slice() {
        [start] => {
            let end = lines[*start..]
                .iter()
                .position(|line| line.eq_ignore_ascii_case("END:VEVENT"))
                .map(|offset| start + offset)
                .ok_or_else(|| anyhow!("ICS 缺少 END:VEVENT"))?;
            Ok((*start, end))
        }
        [] => bail!("ICS 沒有 VEVENT"),
        _ => bail!("此資源含多個 VEVENT（重複事件的例外），不能以欄位修改"),
    }
}

/// 回傳 VEVENT 第一層屬性在 `lines` 中的索引。
fn top_level(lines: &[String], (start, end): (usize, usize)) -> Vec<usize> {
    let mut depth = 0usize;
    let mut indexes = Vec::new();
    for (index, line) in lines.iter().enumerate().take(end).skip(start + 1) {
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("BEGIN:") {
            depth += 1;
        } else if upper.starts_with("END:") {
            depth = depth.saturating_sub(1);
        } else if depth == 0 {
            indexes.push(index);
        }
    }
    indexes
}

/// RFC 5545 DURATION（例如 `PT2H`、`P1D`、`P1W`、`-PT15M`）。
fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (negative, value) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let mut rest = value.strip_prefix('P')?;
    let mut total = Duration::zero();
    let mut time = false;
    let mut any = false;
    // `T` 之後至少要有一個時、分或秒單位（拒絕 `P1DT`）。
    let mut time_any = false;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('T') {
            if time {
                return None;
            }
            time = true;
            rest = after;
            continue;
        }
        let digits = rest.find(|c: char| !c.is_ascii_digit())?;
        let number = rest[..digits].parse::<i64>().ok()?;
        let unit = rest[digits..].chars().next()?;
        // 數值過大時 chrono 的建構子與加法會 panic，一律改用 try_／checked_。
        let part = match (time, unit) {
            (false, 'W') => Duration::try_weeks(number),
            (false, 'D') => Duration::try_days(number),
            (true, 'H') => Duration::try_hours(number),
            (true, 'M') => Duration::try_minutes(number),
            (true, 'S') => Duration::try_seconds(number),
            _ => return None,
        }?;
        total = total.checked_add(&part)?;
        any = true;
        time_any |= time;
        rest = &rest[digits + 1..];
    }
    (any && time == time_any).then_some(if negative { -total } else { total })
}

/// 解析 VEVENT 的起迄；沒有 DTEND 時用 DURATION。DURATION 無法解析時失敗，
/// 避免猜錯長度後把結束時間改掉。
fn parse_when(lines: &[String], props: &[usize]) -> Result<Option<When>> {
    let find = |name: &str| {
        props
            .iter()
            .map(|index| lines[*index].as_str())
            .find(|line| name_of(line).eq_ignore_ascii_case(name))
    };
    let Some(start) = find("DTSTART") else {
        return Ok(None);
    };
    let end = find("DTEND");
    let duration = match find("DURATION").filter(|_| end.is_none()) {
        Some(line) => Some(
            parse_duration(value_of(line))
                .filter(|duration| *duration > Duration::zero())
                .ok_or_else(|| {
                    anyhow!(
                        "無法解析 DURATION「{}」，不能以欄位修改時間",
                        value_of(line)
                    )
                })?,
        ),
        None => None,
    };
    let unusable = |line: Option<&str>| {
        anyhow!(
            "無法解析 DURATION「{}」，不能以欄位修改時間",
            line.map(value_of).unwrap_or_default()
        )
    };
    let date = |line: &str| NaiveDate::parse_from_str(value_of(line).trim(), "%Y%m%d").ok();
    if let Some(first) = date(start) {
        let after = match (end.and_then(date), duration) {
            (Some(after), _) => Some(after),
            (None, Some(duration)) => Duration::try_days(duration.num_days().max(1))
                .and_then(|days| first.checked_add_signed(days)),
            (None, None) => first.succ_opt(),
        }
        .ok_or_else(|| unusable(find("DURATION")))?;
        return Ok(Some(When::AllDay(first, after)));
    }
    let instant = |line: &str| -> Option<DateTime<FixedOffset>> {
        let value = value_of(line);
        let key = &line[..line.len() - value.len()].trim_end_matches(':');
        let parsed = crate::meeting::parse_ics_datetime(key, value).ok()?;
        DateTime::parse_from_rfc3339(&parsed).ok()
    };
    let Some(first) = instant(start) else {
        return Ok(None);
    };
    let last = match (end.and_then(instant), duration) {
        (Some(last), _) => Some(last),
        (None, Some(duration)) => first.checked_add_signed(duration),
        (None, None) => first.checked_add_signed(Duration::hours(1)),
    }
    .ok_or_else(|| unusable(find("DURATION")))?;
    Ok(Some(When::Timed(first, last)))
}

/// 解析既有事件的欄位；含多個 VEVENT 時失敗。
pub fn fields(raw: &str) -> Result<EventFields> {
    let lines = unfold(raw);
    let range = event_lines(&lines)?;
    let props = top_level(&lines, range);
    let get = |name: &str| {
        props
            .iter()
            .map(|index| lines[*index].as_str())
            .find(|line| name_of(line).eq_ignore_ascii_case(name))
            .map(|line| unescape_ical_text(value_of(line)))
            .unwrap_or_default()
    };
    let (when, time_locked) = match parse_when(&lines, &props) {
        Ok(when) => (when, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    Ok(EventFields {
        title: get("SUMMARY"),
        when,
        time_locked,
        location: get("LOCATION"),
        description: get("DESCRIPTION"),
    })
}

/// 只改指定欄位，保留 UID 與其他所有屬性（含 VALARM、X- 屬性）。
pub fn apply_update(raw: &str, input: &EventInput, now: DateTime<Utc>) -> Result<String> {
    let mut lines = unfold(raw);
    let range = event_lines(&lines)?;
    let props = top_level(&lines, range);
    let changes_time = input.start.is_some() || input.end.is_some() || input.all_day.is_some();
    let base = match parse_when(&lines, &props) {
        Ok(base) => base,
        Err(error) if changes_time => return Err(error),
        Err(_) => None,
    };
    if base.is_none() && changes_time {
        bail!("原事件沒有可解析的 DTSTART，無法以欄位修改時間");
    }
    let when = if changes_time {
        resolve_when(input, base, now)?
    } else {
        None
    };
    if let Some(title) = &input.title {
        if title.trim().is_empty() {
            bail!("事件標題不可為空白");
        }
    }
    let mut replace: Vec<(&str, Option<String>)> = Vec::new();
    if let Some(title) = &input.title {
        replace.push((
            "SUMMARY",
            Some(format!("SUMMARY:{}", escape_ical_text(title.trim()))),
        ));
    }
    for (name, value) in [
        ("LOCATION", &input.location),
        ("DESCRIPTION", &input.description),
    ] {
        if let Some(value) = value {
            let value = value.trim();
            replace.push((
                name,
                (!value.is_empty()).then(|| format!("{name}:{}", escape_ical_text(value))),
            ));
        }
    }
    if let Some(when) = when {
        let [start, end] = when.lines();
        replace.push(("DTSTART", Some(start)));
        // 原本用 DURATION 的事件，DTEND 寫在 DURATION 的位置；兩者不可並存。
        if props
            .iter()
            .any(|index| name_of(&lines[*index]).eq_ignore_ascii_case("DTEND"))
        {
            replace.push(("DTEND", Some(end)));
            replace.push(("DURATION", None));
        } else {
            replace.push(("DURATION", Some(end)));
        }
        if !props
            .iter()
            .any(|index| name_of(&lines[*index]).eq_ignore_ascii_case("SEQUENCE"))
        {
            replace.push(("SEQUENCE", Some("SEQUENCE:1".to_owned())));
        }
    }
    if replace.is_empty() {
        bail!("沒有指定要修改的欄位");
    }
    replace.push(("DTSTAMP", Some(stamp(now))));
    let mut output = Vec::with_capacity(lines.len() + replace.len());
    let mut done = vec![false; replace.len()];
    for (index, line) in lines.drain(..).enumerate() {
        if index == range.1 {
            for ((_, value), done) in replace.iter().zip(&done) {
                if let (Some(value), false) = (value, done) {
                    output.push(value.clone());
                }
            }
        }
        if !props.contains(&index) {
            output.push(line);
            continue;
        }
        let name = name_of(&line).to_ascii_uppercase();
        if name == "SEQUENCE" {
            let next = value_of(&line)
                .trim()
                .parse::<u64>()
                .unwrap_or(0)
                .saturating_add(1);
            output.push(format!("SEQUENCE:{next}"));
            continue;
        }
        match replace.iter().position(|(key, _)| *key == name) {
            Some(slot) => {
                if !done[slot] {
                    done[slot] = true;
                    if let Some(value) = &replace[slot].1 {
                        output.push(value.clone());
                    }
                }
            }
            None => output.push(line),
        }
    }
    Ok(finish(&output))
}

/// 檢查事件能否在 m2k 以欄位編輯或刪除；不能時回傳給使用者看的原因。
pub fn check_editable(config: &Config, item: &Item) -> std::result::Result<(), String> {
    if !own_collection(config, &item.collection) {
        return Err(
            "這是會議室或其他人的行事曆，m2k 不在這裡編輯或刪除；訂房請用 meeting book".into(),
        );
    }
    let lines = unfold(&item.raw);
    let range = event_lines(&lines).map_err(|error| format!("{error:#}"))?;
    let props = top_level(&lines, range);
    for index in &props {
        let name = name_of(&lines[*index]).to_ascii_uppercase();
        if matches!(
            name.as_str(),
            "RRULE" | "RDATE" | "EXDATE" | "RECURRENCE-ID"
        ) {
            return Err(format!(
                "重複事件（含 {name}）不支援在此編輯或刪除，避免改到整個系列；請改用 calendar update --file"
            ));
        }
        if name == "ORGANIZER" {
            let email = address(value_of(&lines[*index]));
            if !email.eq_ignore_ascii_case(config.email.trim()) {
                return Err(format!(
                    "這是 {email} 邀請的事件，你不是主辦人，不能修改或刪除；回覆請用 meeting rsvp"
                ));
            }
        }
    }
    // 自己主辦的會議（meeting book）同一份 ICS 也在會議室與與會者那邊；
    // 只改自己這份會留下會議室預約，與會者也收不到更新或取消。
    let others = props
        .iter()
        .filter(|index| name_of(&lines[**index]).eq_ignore_ascii_case("ATTENDEE"))
        .map(|index| attendee_email(&lines[*index]))
        .filter(|email| !email.is_empty() && !email.eq_ignore_ascii_case(config.email.trim()))
        .collect::<Vec<_>>();
    if !others.is_empty() {
        return Err(format!(
            "這是有其他與會者或會議室（{}）的會議，只改自己的行事曆會留下會議室預約、與會者也收不到通知；請改用 meeting prepare／book／invite 流程",
            others.join("、")
        ));
    }
    Ok(())
}

/// `mailto:` 位址（大小寫不拘）去掉 scheme 並 trim；其他值原樣回傳。
fn address(value: &str) -> &str {
    let value = value.trim();
    value
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("mailto:"))
        .map_or(value, |_| value[7..].trim())
}

/// ATTENDEE 的 email：值是 `mailto:` 時用值，否則（例如 `urn:uuid:`）用 RFC 7986 的 `EMAIL=` 參數。
fn attendee_email(line: &str) -> String {
    let value = value_of(line);
    if value
        .trim()
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("mailto:"))
    {
        return address(value).to_owned();
    }
    let params = &line[name_of(line).len()..line.len() - value.len()];
    let mut quoted = false;
    params
        .trim_end_matches(':')
        .split(|c: char| {
            if c == '"' {
                quoted = !quoted;
            }
            c == ';' && !quoted
        })
        .filter_map(|param| param.split_once('='))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("EMAIL"))
        .map(|(_, email)| address(email.trim().trim_matches('"')).to_owned())
        .unwrap_or_else(|| value.trim().to_owned())
}

/// 集合是否在自己的行事曆 home（`caldav_url` 的上一層）之下。
/// 非 URL 的集合 ID（展示資料）視為自己的行事曆。
pub fn own_collection(config: &Config, collection: &str) -> bool {
    if Url::parse(collection.trim()).is_err() {
        return true;
    }
    // 沒有結尾斜線時 `join("..")` 會多退一層到 calendars/，先正規化成目錄。
    let configured = format!("{}/", config.caldav_url.trim().trim_end_matches('/'));
    let Some(home) = Url::parse(&configured)
        .ok()
        .and_then(|url| url.join("..").ok())
    else {
        return false;
    };
    crate::dav_url::identity(collection).starts_with(&crate::dav_url::identity(home.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-21T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn input(start: Option<&str>, end: Option<&str>, all_day: Option<bool>) -> EventInput {
        EventInput {
            start: start.map(Into::into),
            end: end.map(Into::into),
            all_day,
            ..Default::default()
        }
    }

    fn config() -> Config {
        Config::with_site_profile(
            "fixture@example.test".into(),
            "http://127.0.0.1:18080",
            None,
        )
        .unwrap()
    }

    const BASE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//x//EN\r\nBEGIN:VTIMEZONE\r\nTZID:Asia/Taipei\r\nBEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETFROM:+0800\r\nTZOFFSETTO:+0800\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:keep-me@example.test\r\nDTSTAMP:20260101T000000Z\r\nDTSTART;TZID=Asia/Taipei:20261022T140000\r\nDTEND;TZID=Asia/Taipei:20261022T150000\r\nSUMMARY:舊標題\r\nLOCATION:舊地點\r\nSEQUENCE:2\r\nX-CUSTOM:保留\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:提醒\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn human_start_end_become_utc_and_timed_create_needs_an_end() {
        let when = resolve_when(&input(Some("明天 14:00"), Some("15:30"), None), None, now())
            .unwrap()
            .unwrap();
        assert_eq!(when.lines()[0], "DTSTART:20261022T060000Z");
        assert_eq!(
            when.lines()[1],
            "DTEND:20261022T073000Z",
            "time-only end uses the start day"
        );
        let error = resolve_when(&input(Some("明天 14:00"), None, None), None, now()).unwrap_err();
        assert!(error.to_string().contains("--end"));
        let error =
            resolve_when(&input(Some("明天 14:00"), Some("13:00"), None), None, now()).unwrap_err();
        assert!(error.to_string().contains("晚於"));
    }

    #[test]
    fn all_day_end_is_inclusive_for_people_and_exclusive_in_ics() {
        let single = resolve_when(&input(Some("明天"), None, Some(true)), None, now())
            .unwrap()
            .unwrap();
        assert_eq!(single.lines()[0], "DTSTART;VALUE=DATE:20261022");
        assert_eq!(single.lines()[1], "DTEND;VALUE=DATE:20261023");
        let span = resolve_when(
            &input(Some("2026-10-22"), Some("2026-10-24"), Some(true)),
            None,
            now(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(span.lines()[1], "DTEND;VALUE=DATE:20261025");
        assert_eq!(span.form_text(), ("2026-10-22".into(), "2026-10-24".into()));
    }

    #[test]
    fn new_event_has_no_method_or_organizer_and_escapes_text() {
        let raw = build_new(
            &EventInput {
                title: Some("同步, 第一次; 會議".into()),
                location: Some("北區".into()),
                description: Some("第一行\n第二行".into()),
                ..input(Some("明天 14:00"), Some("明天 15:00"), None)
            },
            now(),
        )
        .unwrap();
        assert!(!raw.contains("METHOD") && !raw.contains("ORGANIZER"));
        assert!(raw.contains("SUMMARY:同步\\, 第一次\\; 會議\r\n"));
        assert!(raw.contains("DESCRIPTION:第一行\\n第二行\r\n"));
        assert!(raw.contains("UID:") && raw.ends_with("END:VCALENDAR\r\n"));
        let parsed = fields(&raw).unwrap();
        assert_eq!(parsed.title, "同步, 第一次; 會議");
        assert_eq!(parsed.description, "第一行\n第二行");
        let error = build_new(&input(Some("明天 14:00"), Some("15:00"), None), now()).unwrap_err();
        assert!(error.to_string().contains("標題"));
    }

    #[test]
    fn update_changes_only_named_fields_and_keeps_uid_alarm_and_extras() {
        let raw = apply_update(
            BASE,
            &EventInput {
                title: Some("新標題".into()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
        assert!(raw.contains("UID:keep-me@example.test\r\n"));
        assert!(raw.contains("SUMMARY:新標題\r\n") && !raw.contains("舊標題"));
        assert!(
            raw.contains("LOCATION:舊地點\r\n"),
            "unspecified field kept"
        );
        assert!(
            raw.contains("DTSTART;TZID=Asia/Taipei:20261022T140000\r\n"),
            "time untouched"
        );
        assert!(
            raw.contains("DTSTART:19700101T000000\r\n"),
            "VTIMEZONE untouched"
        );
        assert!(raw.contains("DESCRIPTION:提醒\r\n"), "VALARM untouched");
        assert!(raw.contains("X-CUSTOM:保留\r\nBEGIN:VALARM"));
        assert!(raw.contains("SEQUENCE:3\r\n"));
        assert!(raw.contains("DTSTAMP:20261021T020000Z\r\n"));
    }

    #[test]
    fn moving_start_keeps_duration_and_clearing_location_removes_it() {
        let raw = apply_update(
            BASE,
            &EventInput {
                start: Some("2026-10-23 10:00".into()),
                location: Some(String::new()),
                description: Some("新增說明".into()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
        assert!(raw.contains("DTSTART:20261023T020000Z\r\nDTEND:20261023T030000Z\r\n"));
        assert!(!raw.contains("LOCATION"));
        assert!(
            raw.contains("DESCRIPTION:新增說明\r\nEND:VEVENT"),
            "missing field appended to VEVENT"
        );
        let fields = fields(&raw).unwrap();
        assert_eq!(fields.description, "新增說明");
        assert!(apply_update(BASE, &EventInput::default(), now()).is_err());
    }

    #[test]
    fn all_day_toggle_converts_both_ways() {
        let raw = apply_update(BASE, &input(None, None, Some(true)), now()).unwrap();
        assert!(raw.contains("DTSTART;VALUE=DATE:20261022\r\nDTEND;VALUE=DATE:20261023\r\n"));
        let error = apply_update(&raw, &input(None, None, Some(false)), now()).unwrap_err();
        assert!(error.to_string().contains("--start"));
        let timed = apply_update(
            &raw,
            &input(Some("2026-10-22 09:30"), None, Some(false)),
            now(),
        )
        .unwrap();
        assert!(timed.contains("DTSTART:20261022T013000Z\r\nDTEND:20261022T023000Z\r\n"));
    }

    #[test]
    fn guards_reject_recurring_invited_and_room_events_with_reasons() {
        let cfg = config();
        let own = |raw: &str| Item {
            id: "e".into(),
            kind: crate::model::Kind::Calendar,
            collection: cfg.caldav_url.clone(),
            remote_id: "e.ics".into(),
            etag: Some("\"1\"".into()),
            uid_validity: None,
            title: String::new(),
            subtitle: String::new(),
            date: None,
            end_date: None,
            text: String::new(),
            raw: raw.into(),
            flags: vec![],
            emails: vec![],
            phones: vec![],
            attachments: vec![],
        };
        assert_eq!(check_editable(&cfg, &own(BASE)), Ok(()));
        let mine = BASE.replace("SEQUENCE:2", "ORGANIZER:MAILTO:Fixture@Example.test");
        assert_eq!(check_editable(&cfg, &own(&mine)), Ok(()));
        let invited = BASE.replace(
            "SEQUENCE:2",
            "ORGANIZER;CN=\"Carol: PM\":mailto:carol@example.test",
        );
        assert!(check_editable(&cfg, &own(&invited))
            .unwrap_err()
            .contains("carol@example.test 邀請"));
        for marker in [
            "RRULE:FREQ=WEEKLY",
            "RECURRENCE-ID:20261022T060000Z",
            "EXDATE:20261029T060000Z",
        ] {
            let recurring = BASE.replace("SEQUENCE:2", marker);
            assert!(check_editable(&cfg, &own(&recurring))
                .unwrap_err()
                .contains("重複事件"));
        }
        let two = BASE.replace(
            "END:VCALENDAR",
            "BEGIN:VEVENT\r\nUID:x\r\nEND:VEVENT\r\nEND:VCALENDAR",
        );
        assert!(check_editable(&cfg, &own(&two))
            .unwrap_err()
            .contains("多個 VEVENT"));
        let mut room = own(BASE);
        room.collection =
            "http://127.0.0.1:18080/cgi-bin/cal/caldav/calendars/room-a@example.test/default/"
                .into();
        assert!(check_editable(&cfg, &room).unwrap_err().contains("會議室"));
        let mut sibling = own(BASE);
        sibling.collection = cfg.caldav_url.replace("/default/", "/work/");
        assert_eq!(
            check_editable(&cfg, &sibling),
            Ok(()),
            "other calendars in own home"
        );
        let mut demo = own(BASE);
        demo.collection = "demo-calendar".into();
        assert_eq!(check_editable(&cfg, &demo), Ok(()));
    }

    #[test]
    fn duration_events_keep_their_length_and_unknown_duration_blocks_time_edits() {
        let with_duration = BASE.replace(
            "DTEND;TZID=Asia/Taipei:20261022T150000\r\n",
            "DURATION:PT2H\r\n",
        );
        let parsed = fields(&with_duration).unwrap();
        assert_eq!(
            parsed.when.unwrap().form_text(),
            ("2026-10-22 14:00".into(), "2026-10-22 16:00".into())
        );
        let raw = apply_update(
            &with_duration,
            &input(Some("2026-10-23 10:00"), None, None),
            now(),
        )
        .unwrap();
        assert!(
            raw.contains("DTSTART:20261023T020000Z\r\nDTEND:20261023T040000Z\r\n"),
            "{raw}"
        );
        assert!(!raw.contains("DURATION"));
        assert_eq!(
            parse_duration("P1W2DT3H4M5S"),
            Some(Duration::seconds(788_645))
        );
        assert_eq!(parse_duration("PT"), None);
        let broken = BASE.replace(
            "DTEND;TZID=Asia/Taipei:20261022T150000\r\n",
            "DURATION:2 hours\r\n",
        );
        let error =
            apply_update(&broken, &input(Some("2026-10-23 10:00"), None, None), now()).unwrap_err();
        assert!(error.to_string().contains("DURATION"));
        let renamed = apply_update(
            &broken,
            &EventInput {
                title: Some("只改標題".into()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
        assert!(
            renamed.contains("DURATION:2 hours\r\n"),
            "title-only edit keeps the original"
        );
    }

    #[test]
    fn time_change_adds_sequence_one_and_keeps_attendees_and_unknown_props() {
        let base = BASE
            .replace("SEQUENCE:2\r\n", "")
            .replace("X-CUSTOM:保留", "X-CUSTOM;X-PARAM=\"a:b\":保留\r\nATTENDEE;CN=Self:mailto:fixture@example.test\r\nCATEGORIES:工作,個人");
        let raw = apply_update(&base, &input(Some("2026-10-23 10:00"), None, None), now()).unwrap();
        assert!(raw.contains("SEQUENCE:1\r\n"), "{raw}");
        assert!(raw.contains("X-CUSTOM;X-PARAM=\"a:b\":保留\r\n"));
        assert!(raw.contains("ATTENDEE;CN=Self:mailto:fixture@example.test\r\n"));
        assert!(raw.contains("CATEGORIES:工作,個人\r\n"));
        let renamed = apply_update(
            &base,
            &EventInput {
                title: Some("x".into()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
        assert!(
            !renamed.contains("SEQUENCE"),
            "only time changes add SEQUENCE"
        );
    }

    #[test]
    fn self_organised_meetings_with_attendees_or_rooms_are_refused() {
        let cfg = config();
        let mut item = Item {
            id: "e".into(),
            kind: crate::model::Kind::Calendar,
            collection: cfg.caldav_url.clone(),
            remote_id: "e.ics".into(),
            etag: Some("\"1\"".into()),
            uid_validity: None,
            title: String::new(),
            subtitle: String::new(),
            date: None,
            end_date: None,
            text: String::new(),
            raw: String::new(),
            flags: vec![],
            emails: vec![],
            phones: vec![],
            attachments: vec![],
        };
        item.raw = BASE.replace(
            "SEQUENCE:2",
            "ORGANIZER:mailto:fixture@example.test\r\nATTENDEE;CN=Self:mailto:fixture@example.test",
        );
        assert_eq!(
            check_editable(&cfg, &item),
            Ok(()),
            "only self as attendee is fine"
        );
        item.raw = BASE.replace(
            "SEQUENCE:2",
            "ORGANIZER:mailto:fixture@example.test\r\nATTENDEE;CN=Room:MAILTO:room-a@example.test",
        );
        let reason = check_editable(&cfg, &item).unwrap_err();
        assert!(
            reason.contains("room-a@example.test") && reason.contains("meeting"),
            "{reason}"
        );
    }

    #[test]
    fn calendar_home_check_does_not_depend_on_trailing_slash() {
        let mut cfg = config();
        cfg.caldav_url = cfg.caldav_url.trim_end_matches('/').to_owned();
        let room = cfg.caldav_url.replace(
            "fixture%40example.test/default",
            "room-a@example.test/default/",
        );
        assert!(!own_collection(&cfg, &room), "{room}");
        assert!(own_collection(&cfg, &format!("{}/", cfg.caldav_url)));
        assert!(own_collection(
            &cfg,
            &cfg.caldav_url.replace("/default", "/work/")
        ));
    }

    #[test]
    fn huge_durations_and_sequences_never_panic() {
        for value in [
            "P99999999W",
            "P9999999999999999W",
            "PT9223372036854775807S",
            "P1DT",
            // Each part fits in a Duration; only their sum overflows.
            "P9000000000W9000000000W",
        ] {
            let raw = BASE.replace(
                "DTEND;TZID=Asia/Taipei:20261022T150000\r\n",
                &format!("DURATION:{value}\r\n"),
            );
            let parsed = fields(&raw).unwrap();
            assert!(parsed.when.is_none(), "{value}");
            assert!(
                parsed.time_locked.as_deref().unwrap().contains("DURATION"),
                "{value}"
            );
            let error = apply_update(&raw, &input(Some("2026-10-23 10:00"), None, None), now())
                .unwrap_err();
            assert!(error.to_string().contains("DURATION"), "{value}");
            let renamed = apply_update(
                &raw,
                &EventInput {
                    title: Some("只改標題".into()),
                    ..Default::default()
                },
                now(),
            )
            .unwrap();
            assert!(renamed.contains(&format!("DURATION:{value}\r\n")));
        }
        // All-day events add the duration to a NaiveDate, a separate overflow path.
        let all_day = BASE
            .replace(
                "DTSTART;TZID=Asia/Taipei:20261022T140000",
                "DTSTART;VALUE=DATE:20261022",
            )
            .replace(
                "DTEND;TZID=Asia/Taipei:20261022T150000\r\n",
                "DURATION:P99999999W\r\n",
            );
        let parsed = fields(&all_day).unwrap();
        assert!(parsed.when.is_none());
        assert!(parsed.time_locked.as_deref().unwrap().contains("DURATION"));
        assert!(apply_update(
            &all_day,
            &input(Some("2026-10-23 10:00"), None, None),
            now()
        )
        .is_err());
        assert_eq!(parse_duration("P1DT"), None);
        assert_eq!(parse_duration("P1DT2H"), Some(Duration::hours(26)));
        let last_day = BASE
            .replace(
                "DTSTART;TZID=Asia/Taipei:20261022T140000",
                "DTSTART;VALUE=DATE:99991231",
            )
            .replace("DTEND;TZID=Asia/Taipei:20261022T150000\r\n", "");
        assert!(fields(&last_day).is_ok());
        let max_sequence = BASE.replace("SEQUENCE:2", "SEQUENCE:18446744073709551615");
        let raw = apply_update(
            &max_sequence,
            &EventInput {
                title: Some("x".into()),
                ..Default::default()
            },
            now(),
        )
        .unwrap();
        assert!(raw.contains("SEQUENCE:18446744073709551615\r\n"));
    }

    #[test]
    fn urn_attendees_compare_their_email_parameter_and_blank_ones_are_ignored() {
        let cfg = config();
        let mut item = Item {
            id: "e".into(),
            kind: crate::model::Kind::Calendar,
            collection: cfg.caldav_url.clone(),
            remote_id: "e.ics".into(),
            etag: Some("\"1\"".into()),
            uid_validity: None,
            title: String::new(),
            subtitle: String::new(),
            date: None,
            end_date: None,
            text: String::new(),
            raw: String::new(),
            flags: vec![],
            emails: vec![],
            phones: vec![],
            attachments: vec![],
        };
        item.raw = BASE.replace(
            "SEQUENCE:2",
            "ATTENDEE;CN=\"Self; me\";EMAIL=\"Fixture@Example.test\":urn:uuid:1234\r\nATTENDEE:mailto: fixture@example.test \r\nATTENDEE:mailto:",
        );
        assert_eq!(check_editable(&cfg, &item), Ok(()));
        item.raw = BASE.replace(
            "SEQUENCE:2",
            "ATTENDEE;EMAIL=carol@example.test:urn:uuid:9\r\nATTENDEE:mailto:",
        );
        let reason = check_editable(&cfg, &item).unwrap_err();
        assert!(
            reason.contains("（carol@example.test）"),
            "blank attendee not listed: {reason}"
        );
    }
}
