use chrono::{DateTime, Utc};
use m2k::date::resolve;

fn reference() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T04:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn assert_taipei(expression: &str, start: &str, end: &str) {
    let range = resolve(expression, "Asia/Taipei", reference()).unwrap();
    assert_eq!(range.timezone, "Asia/Taipei");
    assert_eq!(
        range.start,
        format!("{start}T00:00:00+08:00"),
        "{expression}"
    );
    assert_eq!(range.end, format!("{end}T00:00:00+08:00"), "{expression}");
}

#[test]
fn taipei_today_uses_local_date_across_utc_midnight() {
    let now = DateTime::parse_from_rfc3339("2026-10-06T17:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let range = resolve("今天", "Asia/Taipei", now).unwrap();
    assert_eq!(range.start, "2026-10-07T00:00:00+08:00");
    assert_eq!(range.end, "2026-10-08T00:00:00+08:00");
}

#[test]
fn next_week_has_monday_boundaries_and_real_timezone_offsets() {
    let now = DateTime::parse_from_rfc3339("2026-10-06T04:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let range = resolve("下週", "Asia/Taipei", now).unwrap();
    assert_eq!(range.start, "2026-10-12T00:00:00+08:00");
    assert_eq!(range.end, "2026-10-19T00:00:00+08:00");
    let new_york = resolve("2026-03-08", "America/New_York", now).unwrap();
    let start = DateTime::parse_from_rfc3339(&new_york.start).unwrap();
    let end = DateTime::parse_from_rfc3339(&new_york.end).unwrap();
    assert_eq!((end - start).num_hours(), 23);
}

#[test]
fn seven_requested_examples_use_deterministic_local_ranges() {
    for (expression, start, end) in [
        ("next tuesday", "2026-10-13", "2026-10-14"),
        ("下週五", "2026-10-16", "2026-10-17"),
        ("週六", "2026-10-10", "2026-10-11"),
        ("兩天前", "2026-10-05", "2026-10-06"),
        ("明天", "2026-10-08", "2026-10-09"),
        ("2026-10-14", "2026-10-14", "2026-10-15"),
        ("下週", "2026-10-12", "2026-10-19"),
    ] {
        assert_taipei(expression, start, end);
    }
}

#[test]
fn existing_chinese_and_english_ranges_keep_their_boundaries() {
    for (aliases, start, end) in [
        (vec!["今天", "today", " TODAY "], "2026-10-07", "2026-10-08"),
        (vec!["明天", "tomorrow"], "2026-10-08", "2026-10-09"),
        (vec!["昨天", "yesterday"], "2026-10-06", "2026-10-07"),
        (
            vec!["本週", "本周", "this-week", "this week"],
            "2026-10-05",
            "2026-10-12",
        ),
        (
            vec!["下週", "下周", "next-week", "next week"],
            "2026-10-12",
            "2026-10-19",
        ),
        (
            vec!["本月", "this-month", "this month"],
            "2026-10-01",
            "2026-11-01",
        ),
    ] {
        for expression in aliases {
            assert_taipei(expression, start, end);
        }
    }
}

#[test]
fn chinese_weekdays_distinguish_upcoming_day_from_next_calendar_week() {
    for (day, expected_upcoming, next_week) in [
        ("一", "2026-10-12", "2026-10-12"),
        ("二", "2026-10-13", "2026-10-13"),
        ("三", "2026-10-07", "2026-10-14"),
        ("四", "2026-10-08", "2026-10-15"),
        ("五", "2026-10-09", "2026-10-16"),
        ("六", "2026-10-10", "2026-10-17"),
        ("日", "2026-10-11", "2026-10-18"),
    ] {
        let upcoming = resolve(&format!("週{day}"), "Asia/Taipei", reference()).unwrap();
        let next = resolve(&format!("下週{day}"), "Asia/Taipei", reference()).unwrap();
        assert_eq!(
            upcoming.start,
            format!("{expected_upcoming}T00:00:00+08:00")
        );
        assert_eq!(next.start, format!("{next_week}T00:00:00+08:00"));
        assert_eq!(
            upcoming.start,
            resolve(&format!("周{day}"), "Asia/Taipei", reference())
                .unwrap()
                .start
        );
    }
}

#[test]
fn english_weekdays_are_upcoming_and_next_is_strictly_future() {
    for expression in ["tuesday", "Tue", "tues", "on tuesday", " NEXT   TUESDAY "] {
        assert_taipei(expression, "2026-10-13", "2026-10-14");
    }
    assert_taipei("wednesday", "2026-10-07", "2026-10-08");
    assert_taipei("next wednesday", "2026-10-14", "2026-10-15");
    let monday = DateTime::parse_from_rfc3339("2026-10-12T04:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        resolve("next tuesday", "Asia/Taipei", monday)
            .unwrap()
            .start,
        "2026-10-13T00:00:00+08:00"
    );
    assert_taipei("Thurs", "2026-10-08", "2026-10-09");
    assert_taipei("下星期五", "2026-10-16", "2026-10-17");
    assert_taipei("星期天", "2026-10-11", "2026-10-12");
}

#[test]
fn relative_day_counts_accept_arabic_chinese_and_english_numbers() {
    for expression in [
        "兩天前",
        "两天前",
        "二天前",
        "2天前",
        "2 days ago",
        "two days ago",
    ] {
        assert_taipei(expression, "2026-10-05", "2026-10-06");
    }
    for expression in ["兩天後", "两天后", "2天後", "in 2 days", "two days later"] {
        assert_taipei(expression, "2026-10-09", "2026-10-10");
    }
    assert_taipei("十天後", "2026-10-17", "2026-10-18");
    assert_taipei("二十一天後", "2026-10-28", "2026-10-29");
    assert_taipei("零天前", "2026-10-07", "2026-10-08");
}

#[test]
fn month_and_relative_ranges_use_real_dst_boundaries() {
    let now = DateTime::parse_from_rfc3339("2026-03-07T17:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let range = resolve("1天後", "America/New_York", now).unwrap();
    assert_eq!(range.start, "2026-03-08T00:00:00-05:00");
    assert_eq!(range.end, "2026-03-09T00:00:00-04:00");
    let december = DateTime::parse_from_rfc3339("2026-12-15T04:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let month = resolve("本月", "Asia/Taipei", december).unwrap();
    assert_eq!(month.start, "2026-12-01T00:00:00+08:00");
    assert_eq!(month.end, "2027-01-01T00:00:00+08:00");
}

#[test]
fn invalid_or_overflowing_dates_fail_with_all_seven_examples() {
    for expression in [
        "",
        "not a date",
        "next tuesday please",
        "next blursday",
        "下週八",
        "週六晚上",
        "two days ago junk",
        "三二天前",
        "2026-02-30",
        "2026-257-01",
        "-1天前",
        "9223372036854775807天後",
        "999999999999999999999999999天前",
    ] {
        let error = resolve(expression, "Asia/Taipei", reference())
            .unwrap_err()
            .to_string();
        for example in [
            "next tuesday",
            "下週五",
            "週六",
            "兩天前",
            "明天",
            "2026-10-14",
            "下週",
        ] {
            assert!(error.contains(example), "{expression}: {error}");
        }
    }
}

#[test]
fn invalid_timezone_and_nonexistent_date_boundaries_remain_errors() {
    assert!(resolve("today", "Definitely/Not_A_Zone", reference())
        .unwrap_err()
        .to_string()
        .contains("時區無效"));
    assert!(resolve("2011-12-30", "Pacific/Apia", reference())
        .unwrap_err()
        .to_string()
        .contains("日期邊界不存在或不明確"));
}
