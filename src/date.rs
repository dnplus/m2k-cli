use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Duration, LocalResult, Months, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Serialize;

const DATE_EXAMPLES: &str = "next tuesday、下週五、週六、兩天前、明天、2026-10-14、下週";

#[derive(Debug, Serialize)]
pub struct DateRange {
    pub timezone: String,
    pub start: String,
    pub end: String,
}

fn shift(day: NaiveDate, days: i64) -> Result<NaiveDate> {
    let duration = Duration::try_days(days).context("天數超出範圍")?;
    day.checked_add_signed(duration).context("日期超出範圍")
}

fn normalized_expression(expression: &str) -> String {
    expression
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .replace('週', "周")
        .replace('兩', "两")
        .replace('後', "后")
}

fn english_weekday(value: &str) -> Option<i64> {
    match value {
        "monday" | "mon" => Some(0),
        "tuesday" | "tue" | "tues" => Some(1),
        "wednesday" | "wed" => Some(2),
        "thursday" | "thu" | "thur" | "thurs" => Some(3),
        "friday" | "fri" => Some(4),
        "saturday" | "sat" => Some(5),
        "sunday" | "sun" => Some(6),
        _ => None,
    }
}

fn weekday_date(expression: &str, today: NaiveDate) -> Result<Option<NaiveDate>> {
    let current = i64::from(today.weekday().num_days_from_monday());
    let (candidate, next) = match expression.strip_prefix("next ") {
        Some(day) => (day, true),
        None => (expression.strip_prefix("on ").unwrap_or(expression), false),
    };
    if let Some(target) = english_weekday(candidate) {
        let delta = if next {
            (target - current - 1).rem_euclid(7) + 1
        } else {
            (target - current).rem_euclid(7)
        };
        return Ok(Some(shift(today, delta)?));
    }
    for (prefix, next_week) in [
        ("下周", true),
        ("下星期", true),
        ("周", false),
        ("星期", false),
    ] {
        let Some(day) = expression.strip_prefix(prefix) else {
            continue;
        };
        let target = match day {
            "一" => 0,
            "二" => 1,
            "三" => 2,
            "四" => 3,
            "五" => 4,
            "六" => 5,
            "日" | "天" => 6,
            _ => continue,
        };
        let delta = if next_week {
            7 + target - current
        } else {
            (target - current).rem_euclid(7)
        };
        return Ok(Some(shift(today, delta)?));
    }
    Ok(None)
}

fn day_count(value: &str) -> Result<i64> {
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().context("天數超出範圍");
    }
    let english = match value {
        "zero" => Some(0),
        "one" => Some(1),
        "two" => Some(2),
        "three" => Some(3),
        "four" => Some(4),
        "five" => Some(5),
        "six" => Some(6),
        "seven" => Some(7),
        "eight" => Some(8),
        "nine" => Some(9),
        "ten" => Some(10),
        _ => None,
    };
    if let Some(count) = english {
        return Ok(count);
    }
    if matches!(value, "零" | "〇") {
        return Ok(0);
    }
    let mut total = 0_i64;
    let mut digit = None;
    let mut previous_unit = 10_000;
    let mut zero_separator = false;
    for character in value.chars() {
        let number = match character {
            '一' => Some(1),
            '二' | '两' => Some(2),
            '三' => Some(3),
            '四' => Some(4),
            '五' => Some(5),
            '六' => Some(6),
            '七' => Some(7),
            '八' => Some(8),
            '九' => Some(9),
            _ => None,
        };
        if let Some(number) = number {
            if digit.is_some() {
                bail!("天數文字無效");
            }
            digit = Some(number);
            zero_separator = false;
            continue;
        }
        if matches!(character, '零' | '〇') {
            if total == 0 || digit.is_some() || zero_separator || previous_unit <= 10 {
                bail!("天數文字無效");
            }
            zero_separator = true;
            continue;
        }
        let unit = match character {
            '十' => 10,
            '百' => 100,
            '千' => 1000,
            _ => bail!("天數文字無效"),
        };
        if unit >= previous_unit || zero_separator {
            bail!("天數文字無效");
        }
        let factor = match digit.take() {
            Some(number) => number,
            None if unit == 10 && total == 0 => 1,
            None => bail!("天數文字無效"),
        };
        total += factor * unit;
        previous_unit = unit;
    }
    if zero_separator || (total == 0 && digit.is_none()) {
        bail!("天數文字無效");
    }
    Ok(total + digit.unwrap_or(0))
}

fn relative_date(expression: &str, today: NaiveDate) -> Result<Option<NaiveDate>> {
    let chinese = expression
        .strip_suffix("天前")
        .map(|count| (count, -1))
        .or_else(|| expression.strip_suffix("天后").map(|count| (count, 1)));
    let words = expression.split_whitespace().collect::<Vec<_>>();
    let english = match words.as_slice() {
        [count, "day" | "days", "ago"] => Some((*count, -1)),
        ["in", count, "day" | "days"] | [count, "day" | "days", "later"] => Some((*count, 1)),
        _ => None,
    };
    if let Some((count, direction)) = chinese.or(english) {
        let days = day_count(count)?
            .checked_mul(direction)
            .context("天數超出範圍")?;
        return Ok(Some(shift(today, days)?));
    }
    Ok(None)
}

fn local_dates(expression: &str, today: NaiveDate) -> Result<(NaiveDate, NaiveDate)> {
    let expression = normalized_expression(expression);
    let weekday = i64::from(today.weekday().num_days_from_monday());
    let start = match expression.as_str() {
        "今天" | "today" => today,
        "明天" | "tomorrow" => shift(today, 1)?,
        "昨天" | "yesterday" => shift(today, -1)?,
        "本周" | "this-week" | "this week" => {
            let start = shift(today, -weekday)?;
            return Ok((start, shift(start, 7)?));
        }
        "下周" | "next-week" | "next week" => {
            let start = shift(today, 7 - weekday)?;
            return Ok((start, shift(start, 7)?));
        }
        "本月" | "this-month" | "this month" => {
            let start = today.with_day(1).context("日期無效")?;
            let end = start
                .checked_add_months(Months::new(1))
                .context("日期超出範圍")?;
            return Ok((start, end));
        }
        input => match weekday_date(input, today)? {
            Some(day) => day,
            None => match relative_date(input, today)? {
                Some(day) => day,
                None => NaiveDate::parse_from_str(input, "%Y-%m-%d").context("日期文字無效")?,
            },
        },
    };
    Ok((start, shift(start, 1)?))
}

pub fn resolve(expression: &str, timezone: &str, now: DateTime<Utc>) -> Result<DateRange> {
    let zone: Tz = timezone
        .parse()
        .map_err(|_| anyhow::anyhow!("時區無效：{timezone}"))?;
    let (start, end) = local_dates(expression, now.with_timezone(&zone).date_naive())
        .with_context(|| format!("無法解析日期「{expression}」。可用範例：{DATE_EXAMPLES}"))?;
    let at_midnight = |day: NaiveDate| -> Result<String> {
        let naive = day.and_hms_opt(0, 0, 0).context("日期無效")?;
        match zone.from_local_datetime(&naive) {
            LocalResult::Single(value) => Ok(value.to_rfc3339()),
            _ => bail!("此時區的日期邊界不存在或不明確，請改用明確 RFC3339 時間"),
        }
    };
    Ok(DateRange {
        timezone: timezone.into(),
        start: at_midnight(start)?,
        end: at_midnight(end)?,
    })
}
