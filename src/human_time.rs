//! 台北時間的人話日期時刻解析；會議室表單與行事曆事件共用。

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Asia::Taipei;

const EXAMPLES: &str = "明天 14:00、下週一 下午兩點、上午九點半、next tuesday 14:00、2026-10-14、2026-10-14T14:00:00+08:00";

pub(crate) fn parse(
    input: &str,
    now: DateTime<Utc>,
    default_time: NaiveTime,
) -> Result<DateTime<FixedOffset>> {
    parse_inner(
        input,
        now,
        default_time,
        now.with_timezone(&Taipei).date_naive(),
    )
    .with_context(|| format!("無法解析時間「{input}」。可用範例：{EXAMPLES}"))
}

pub(crate) fn parse_end(
    input: &str,
    now: DateTime<Utc>,
    default_time: NaiveTime,
    start: DateTime<FixedOffset>,
) -> Result<DateTime<FixedOffset>> {
    parse_inner(
        input,
        now,
        default_time,
        start.with_timezone(&Taipei).date_naive(),
    )
    .with_context(|| format!("無法解析時間「{input}」。可用範例：{EXAMPLES}"))
}

fn parse_inner(
    input: &str,
    now: DateTime<Utc>,
    default_time: NaiveTime,
    time_only_date: NaiveDate,
) -> Result<DateTime<FixedOffset>> {
    if let Ok(explicit) = DateTime::parse_from_rfc3339(input.trim()) {
        return Ok(explicit.with_timezone(&Taipei).fixed_offset());
    }
    let input = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(time) = clock(&input) {
        return at_time(time_only_date, time);
    }
    for (index, next) in input.char_indices().rev() {
        if index == 0 {
            continue;
        }
        let prefix = &input[..index];
        if prefix.ends_with(|character: char| character.is_ascii() && !character.is_whitespace())
            && next.is_ascii()
            && !next.is_whitespace()
        {
            continue;
        }
        let Some(time) = clock(input[index..].trim()) else {
            continue;
        };
        if let Ok(day) = date(prefix.trim(), now) {
            return at_time(day, time);
        }
    }
    at_time(date(&input, now)?, default_time)
}

fn date(input: &str, now: DateTime<Utc>) -> Result<NaiveDate> {
    let range = crate::date::resolve(input, "Asia/Taipei", now)?;
    Ok(DateTime::parse_from_rfc3339(&range.start)?.date_naive())
}

fn at_time(day: NaiveDate, time: NaiveTime) -> Result<DateTime<FixedOffset>> {
    Taipei
        .from_local_datetime(&day.and_time(time))
        .single()
        .map(|value| value.fixed_offset())
        .context("此日期的台北時間不存在或不明確")
}

fn digit(character: char) -> Option<u32> {
    match character {
        '零' | '〇' => Some(0),
        '一' => Some(1),
        '二' | '两' | '兩' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    }
}

fn number(value: &str) -> Option<u32> {
    if !value.is_empty() && value.len() <= 2 && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok();
    }
    let characters = value.chars().collect::<Vec<_>>();
    match characters.as_slice() {
        ['十'] => Some(10),
        [single] => digit(*single),
        ['十', ones] => digit(*ones)
            .filter(|value| *value > 0)
            .map(|value| 10 + value),
        [tens, '十'] => digit(*tens)
            .filter(|value| *value > 0)
            .map(|value| value * 10),
        [tens, '十', ones] => Some(
            digit(*tens).filter(|value| *value > 0)? * 10
                + digit(*ones).filter(|value| *value > 0)?,
        ),
        ['零' | '〇', single] => digit(*single),
        _ => None,
    }
}

fn clock(input: &str) -> Option<NaiveTime> {
    let compact = input.split_whitespace().collect::<String>();
    let (value, afternoon) = if let Some(value) = compact.strip_prefix("上午") {
        (value, Some(false))
    } else if let Some(value) = compact.strip_prefix("下午") {
        (value, Some(true))
    } else {
        (compact.as_str(), None)
    };
    let (mut hour, minute) = if let Some((hour, minute)) = value.split_once(':') {
        if hour.is_empty()
            || hour.len() > 2
            || minute.len() != 2
            || !hour
                .bytes()
                .chain(minute.bytes())
                .all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        (hour.parse::<u32>().ok()?, minute.parse::<u32>().ok()?)
    } else {
        let (hour, remainder) = value.split_once(['點', '点', '時', '时'])?;
        let minute = match remainder {
            "" | "整" => 0,
            "半" => 30,
            "一刻" => 15,
            "三刻" => 45,
            value => number(value.strip_suffix('分').unwrap_or(value))?,
        };
        (number(hour)?, minute)
    };
    if let Some(afternoon) = afternoon {
        if hour > 12 || (afternoon && hour == 0) {
            return None;
        }
        hour %= 12;
        if afternoon {
            hour += 12;
        }
    }
    NaiveTime::from_hms_opt(hour, minute, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn reference() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-07T04:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn default_time() -> NaiveTime {
        NaiveTime::from_hms_opt(9, 30, 0).unwrap()
    }

    #[test]
    fn human_dates_accept_spaced_and_attached_chinese_or_english_clocks() {
        for (input, expected) in [
            ("明天 14:00", "2026-10-08T14:00:00+08:00"),
            ("明天14:00", "2026-10-08T14:00:00+08:00"),
            ("下週一 下午兩點", "2026-10-12T14:00:00+08:00"),
            ("下週一下午兩點", "2026-10-12T14:00:00+08:00"),
            ("next tuesday 14:00", "2026-10-13T14:00:00+08:00"),
            (" NEXT   TUESDAY   14:00 ", "2026-10-13T14:00:00+08:00"),
            ("2026-10-14 上午九點半", "2026-10-14T09:30:00+08:00"),
            ("明天 下午 兩點", "2026-10-08T14:00:00+08:00"),
            ("兩天後 下午兩點十五分", "2026-10-09T14:15:00+08:00"),
        ] {
            assert_eq!(
                parse(input, reference(), default_time())
                    .unwrap()
                    .to_rfc3339(),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn bare_dates_use_the_supplied_working_time_without_changing_cli_semantics() {
        for (input, expected) in [
            ("明天", "2026-10-08T09:30:00+08:00"),
            ("2026-10-14", "2026-10-14T09:30:00+08:00"),
            ("下週", "2026-10-12T09:30:00+08:00"),
        ] {
            assert_eq!(
                parse(input, reference(), default_time())
                    .unwrap()
                    .to_rfc3339(),
                expected
            );
        }
        let end = NaiveTime::from_hms_opt(17, 30, 0).unwrap();
        assert_eq!(parse("明天", reference(), end).unwrap().hour(), 17);
    }

    #[test]
    fn chinese_clocks_handle_am_pm_half_and_minutes() {
        for (input, expected) in [
            ("下午兩點", "14:00:00"),
            ("上午九點半", "09:30:00"),
            ("下午十二點", "12:00:00"),
            ("上午十二點", "00:00:00"),
            ("下午二点十五分", "14:15:00"),
            ("二十三點四十五分", "23:45:00"),
            ("九點一刻", "09:15:00"),
            ("九點三刻", "09:45:00"),
            ("下午2:05", "14:05:00"),
            ("零點", "00:00:00"),
        ] {
            assert_eq!(
                parse(input, reference(), default_time())
                    .unwrap()
                    .time()
                    .to_string(),
                expected
            );
        }
    }

    #[test]
    fn time_only_uses_taipei_today_across_utc_midnight() {
        let now = DateTime::parse_from_rfc3339("2026-10-06T17:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            parse("14:00", now, default_time()).unwrap().to_rfc3339(),
            "2026-10-07T14:00:00+08:00"
        );
    }

    #[test]
    fn time_only_end_uses_start_day_and_dated_end_keeps_now_reference() {
        let start = parse("下週一下午兩點", reference(), default_time()).unwrap();
        for input in ["16:00", "下午四點"] {
            assert_eq!(
                parse_end(input, reference(), default_time(), start)
                    .unwrap()
                    .to_rfc3339(),
                "2026-10-12T16:00:00+08:00"
            );
        }
        assert_eq!(
            parse_end("明天 16:00", reference(), default_time(), start)
                .unwrap()
                .to_rfc3339(),
            "2026-10-08T16:00:00+08:00"
        );
    }

    #[test]
    fn explicit_rfc3339_keeps_the_instant_and_normalizes_to_taipei() {
        let input = "2026-10-07T23:15:30.125Z";
        let explicit = DateTime::parse_from_rfc3339(input).unwrap();
        let parsed = parse(input, reference(), default_time()).unwrap();
        assert_eq!(parsed.timestamp_millis(), explicit.timestamp_millis());
        assert_eq!(parsed.to_rfc3339(), "2026-10-08T07:15:30.125+08:00");
        assert_eq!(
            parse_end(input, reference(), default_time(), parsed).unwrap(),
            parsed
        );
    }

    #[test]
    fn unsupported_or_invalid_inputs_fail_with_examples() {
        for input in [
            "",
            "明天 25:00",
            "24:00",
            "09:60",
            "09:30:12",
            "上午十三點",
            "下午零點",
            "二二點",
            "九點七十分",
            "九點半分",
            "next tuesday please 14:00",
            "2026-10-14:00",
            "明天 14:00 junk",
            "下週八 下午兩點",
            "999999999999點",
        ] {
            let error = parse(input, reference(), default_time())
                .expect_err(input)
                .to_string();
            assert!(error.contains("明天 14:00"), "{input}: {error}");
            assert!(error.contains("上午九點半"), "{input}: {error}");
        }
    }
}
