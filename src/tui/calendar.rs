use super::{block, ACCENT, MUTED, SELECTED};
use crate::model::Item;
use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::{Asia::Taipei, Tz};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::Paragraph,
    Frame,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum CalendarView {
    #[default]
    ThreeDay,
    Week,
    Month,
}

impl CalendarView {
    pub fn next(self) -> Self {
        match self {
            Self::ThreeDay => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::ThreeDay,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ThreeDay => "3-day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }
}

pub(super) struct Calendar {
    pub start: NaiveDate,
    pub view: CalendarView,
}

impl Calendar {
    pub fn new() -> Self {
        Self {
            start: Utc::now().with_timezone(&Taipei).date_naive(),
            view: CalendarView::default(),
        }
    }

    pub fn days(&self) -> Vec<NaiveDate> {
        let Some((start, end)) = self.window() else {
            return vec![];
        };
        (0..(end - start).num_days())
            .filter_map(|offset| start.checked_add_signed(Duration::days(offset)))
            .collect()
    }

    pub fn shift(&mut self, delta: i32) {
        let start = match self.view {
            CalendarView::ThreeDay => self
                .start
                .checked_add_signed(Duration::days(i64::from(delta))),
            CalendarView::Week => self
                .start
                .checked_add_signed(Duration::days(i64::from(delta) * 7)),
            CalendarView::Month if delta >= 0 => {
                self.start.checked_add_months(Months::new(delta as u32))
            }
            CalendarView::Month => self
                .start
                .checked_sub_months(Months::new(delta.unsigned_abs())),
        };
        if let Some(start) = start {
            if (Self {
                start,
                view: self.view,
            })
            .window()
            .is_some()
            {
                self.start = start;
            }
        }
    }

    pub fn window(&self) -> Option<(NaiveDate, NaiveDate)> {
        let start = match self.view {
            CalendarView::ThreeDay => self.start,
            CalendarView::Week => self.start.checked_sub_signed(Duration::days(i64::from(
                self.start.weekday().num_days_from_monday(),
            )))?,
            CalendarView::Month => self.start.with_day(1)?,
        };
        let end = match self.view {
            CalendarView::ThreeDay => start.checked_add_signed(Duration::days(3))?,
            CalendarView::Week => start.checked_add_signed(Duration::days(7))?,
            CalendarView::Month => start.checked_add_months(Months::new(1))?,
        };
        Some((start, end))
    }

    pub fn center(&self) -> DateTime<Utc> {
        self.window()
            .and_then(|(start, end)| {
                start.checked_add_signed(Duration::days((end - start).num_days() / 2))
            })
            .and_then(|day| day.and_hms_opt(12, 0, 0))
            .and_then(|time| Taipei.from_local_datetime(&time).single())
            .map(|time| time.with_timezone(&Utc))
            .unwrap_or_else(Utc::now)
    }

    pub fn contains(&self, item: &Item) -> bool {
        let Some((start, end)) = Self::interval(item) else {
            return false;
        };
        let Some((first, last)) = self.window() else {
            return false;
        };
        let Some(window_start) = midnight(first) else {
            return false;
        };
        let Some(window_end) = midnight(last) else {
            return false;
        };
        start < window_end && end > window_start
    }

    pub fn interval(item: &Item) -> Option<(DateTime<Tz>, DateTime<Tz>)> {
        let date = item.date.as_deref()?;
        let start = parse_date(date)?;
        let fallback = if NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok() {
            Duration::days(1)
        } else {
            Duration::hours(1)
        };
        let end = item
            .end_date
            .as_deref()
            .and_then(parse_date)
            .filter(|end| *end > start)
            .or_else(|| start.checked_add_signed(fallback))?;
        Some((start, end))
    }

    pub fn draw_grid(
        &self,
        frame: &mut Frame,
        area: Rect,
        rows: &[Item],
        selected: Option<usize>,
    ) -> Vec<(Rect, usize)> {
        match self.view {
            CalendarView::ThreeDay | CalendarView::Week => {
                self.draw_timeslot_grid(frame, area, rows, selected)
            }
            CalendarView::Month => self.draw_month(frame, area, rows, selected),
        }
    }

    fn draw_timeslot_grid(
        &self,
        frame: &mut Frame,
        area: Rect,
        rows: &[Item],
        selected: Option<usize>,
    ) -> Vec<(Rect, usize)> {
        let days = self.days();
        let Some(first) = days.first() else {
            return vec![];
        };
        let (title, empty_state) = match self.view {
            CalendarView::ThreeDay => (
                " 3 天時段 · 台北時間 ".to_owned(),
                "這 3 天沒有符合條件的活動。按 [ / ] 換日，或 r 更新。",
            ),
            CalendarView::Week => (
                format!(
                    " 週視圖 · {}–{} · 台北時間 ",
                    first.format("%m/%d"),
                    days.last().unwrap_or(first).format("%m/%d")
                ),
                "本週沒有符合條件的活動。按 [ / ] 換週，或 r 更新。",
            ),
            CalendarView::Month => return vec![],
        };
        let panel = block(title);
        let inner = panel.inner(area);
        frame.render_widget(panel, area);
        if inner.width < 6 + days.len() as u16 || inner.height < 2 {
            return vec![];
        }
        let parts = Layout::horizontal(std::iter::once(Constraint::Length(6)).chain(
            std::iter::repeat_n(Constraint::Ratio(1, days.len() as u32), days.len()),
        ))
        .split(inner);
        for (day, part) in days.iter().zip(parts.iter().skip(1)) {
            let header = match self.view {
                CalendarView::Week if part.width >= 9 => day.format("%a %m/%d").to_string(),
                CalendarView::Week => format!(
                    "{}{:02}",
                    WEEKDAYS[day.weekday().num_days_from_monday() as usize],
                    day.day()
                ),
                _ => day.format("%Y-%m-%d").to_string(),
            };
            frame.render_widget(
                Paragraph::new(header)
                    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Rect::new(part.x, part.y, part.width, 1),
            );
        }
        let body = Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(1),
        );
        let mut spans: Vec<Vec<EventSpan>> = (0..days.len()).map(|_| Vec::new()).collect();
        let mut first_hour = 8;
        let mut last_hour = 20;
        for (index, item) in rows.iter().enumerate() {
            let Some((start, end)) = Self::interval(item) else {
                continue;
            };
            for (day_index, day) in days.iter().enumerate() {
                let Some(day_start) = midnight(*day) else {
                    continue;
                };
                let Some(day_end) = day.succ_opt().and_then(midnight) else {
                    continue;
                };
                if start >= day_end || end <= day_start {
                    continue;
                }
                let begin = minutes(start.max(day_start));
                let finish = if end >= day_end {
                    24 * 60
                } else {
                    minutes(end)
                };
                first_hour = first_hour.min(begin / 60);
                last_hour = last_hour.max(finish.div_ceil(60)).min(24);
                spans[day_index].push(EventSpan {
                    index,
                    begin,
                    finish,
                    top: 0,
                    bottom: 0,
                    lane: 0,
                });
            }
        }
        let range_start = first_hour * 60;
        let range_end = last_hour * 60;
        let map_row = |minute: u32| {
            (minute.saturating_sub(range_start) * u32::from(body.height)
                / (range_end - range_start).max(1))
            .min(u32::from(body.height)) as u16
        };
        let mut last_label_y = None;
        for hour in first_hour..last_hour {
            let row = map_row(hour * 60).min(body.height.saturating_sub(1));
            if last_label_y == Some(row) {
                continue;
            }
            last_label_y = Some(row);
            frame.render_widget(
                Paragraph::new(format!("{hour:02}:00")).style(Style::default().fg(MUTED)),
                Rect::new(parts[0].x, body.y + row, parts[0].width, 1),
            );
            for part in parts.iter().skip(1) {
                frame.render_widget(
                    Paragraph::new("·".repeat(usize::from(part.width)))
                        .style(Style::default().fg(Color::DarkGray)),
                    Rect::new(part.x, body.y + row, part.width, 1),
                );
            }
        }
        if spans.iter().all(Vec::is_empty) {
            frame.render_widget(
                Paragraph::new(empty_state).style(Style::default().fg(MUTED)),
                Rect::new(
                    parts[1].x,
                    body.y,
                    inner.width.saturating_sub(6),
                    body.height,
                ),
            );
            return vec![];
        }

        let mut blocks = Vec::new();
        for (day_index, events) in spans.iter_mut().enumerate() {
            events.sort_by_key(|event| (event.begin, event.finish, event.index));
            let mut lane_ends = Vec::new();
            for event in events.iter_mut() {
                event.top = map_row(event.begin).min(body.height.saturating_sub(1));
                event.bottom = ((event.finish.saturating_sub(range_start) * u32::from(body.height))
                    .div_ceil((range_end - range_start).max(1))
                    as u16)
                    .max(event.top.saturating_add(1))
                    .min(body.height);
                event.lane = lane_ends
                    .iter()
                    .position(|end| *end <= event.top)
                    .unwrap_or(lane_ends.len());
                if event.lane == lane_ends.len() {
                    lane_ends.push(event.bottom);
                } else {
                    lane_ends[event.lane] = event.bottom;
                }
            }
            let part = parts[day_index + 1];
            if part.width == 0 {
                continue;
            }
            let lanes = lane_ends.len().max(1);
            for event in events.iter() {
                let left = (usize::from(part.width) * event.lane / lanes)
                    .min(usize::from(part.width.saturating_sub(1)))
                    as u16;
                let right = (usize::from(part.width) * (event.lane + 1) / lanes)
                    .min(usize::from(part.width)) as u16;
                let rect = Rect::new(
                    part.x + left,
                    body.y + event.top,
                    right.saturating_sub(left).max(1),
                    event.bottom.saturating_sub(event.top),
                );
                blocks.push((rect, event.index));
            }
        }
        blocks.sort_by_key(|(_, index)| Some(*index) == selected);
        for (rect, index) in &blocks {
            let item = &rows[*index];
            let focused = selected == Some(*index);
            let time = Self::interval(item)
                .map(|(start, end)| format!("{}–{}", start.format("%H:%M"), end.format("%H:%M")))
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("{}{}", if focused { "> " } else { "" }, item.title)),
                    Line::from(time),
                ])
                .style(
                    Style::default()
                        .fg(if focused { ACCENT } else { Color::White })
                        .bg(if focused {
                            SELECTED
                        } else {
                            Color::Rgb(30, 43, 54)
                        })
                        .add_modifier(if focused {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                *rect,
            );
        }
        blocks
    }

    fn draw_month(
        &self,
        frame: &mut Frame,
        area: Rect,
        rows: &[Item],
        selected: Option<usize>,
    ) -> Vec<(Rect, usize)> {
        let Some((first, last)) = self.window() else {
            return vec![];
        };
        let inner = block("").inner(area);
        let leading = first.weekday().num_days_from_monday() as usize;
        let days = (last - first).num_days() as usize;
        let weeks = (leading + days).div_ceil(7);
        let visible_weeks = weeks.min(usize::from(inner.height.saturating_sub(1)));
        let focused_day = selected
            .and_then(|index| rows.get(index))
            .filter(|item| self.contains(item))
            .and_then(Self::interval)
            .map(|(start, _)| start.date_naive().max(first));
        let focused_week = focused_day
            .map(|day| (leading + (day - first).num_days() as usize) / 7)
            .unwrap_or(0);
        let skip_weeks = focused_week
            .saturating_sub(visible_weeks.saturating_sub(1))
            .min(weeks.saturating_sub(visible_weeks));
        let clipped = if visible_weeks > 0 && visible_weeks < weeks {
            format!(
                " · 週 {}–{}/{}",
                skip_weeks + 1,
                skip_weeks + visible_weeks,
                weeks
            )
        } else {
            String::new()
        };
        frame.render_widget(
            block(format!(
                " 月視圖 · {}{clipped} · 台北時間 ",
                first.format("%Y-%m")
            )),
            area,
        );
        if inner.width < 7 || visible_weeks == 0 {
            return vec![];
        }
        let columns = Layout::horizontal([Constraint::Ratio(1, 7); 7]).split(inner);
        for (weekday, column) in WEEKDAYS.iter().zip(columns.iter()) {
            frame.render_widget(
                Paragraph::new(*weekday).style(Style::default().fg(ACCENT)),
                Rect::new(column.x, column.y, column.width, 1),
            );
        }
        let body = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
        let week_rows = Layout::vertical(vec![
            Constraint::Ratio(1, visible_weeks as u32);
            visible_weeks
        ])
        .split(body);
        let mut hits = Vec::new();
        for (visible_week, row) in week_rows.iter().enumerate() {
            let week = skip_weeks + visible_week;
            for (weekday, column) in columns.iter().enumerate() {
                let position = week * 7 + weekday;
                if position < leading || position >= leading + days {
                    continue;
                }
                let Some(day) =
                    first.checked_add_signed(Duration::days((position - leading) as i64))
                else {
                    continue;
                };
                let rect = Rect::new(column.x, row.y, column.width, row.height);
                let content = day_column(frame, rect, weekday == 6);
                let events = events_on(day, rows);
                let focused = events.iter().any(|index| Some(*index) == selected);
                let background = if focused {
                    SELECTED
                } else if week % 2 == 0 {
                    Color::Rgb(25, 35, 45)
                } else {
                    Color::Rgb(30, 43, 54)
                };
                frame.render_widget(
                    Paragraph::new("").style(Style::default().bg(background)),
                    content,
                );
                let header = if events.is_empty() {
                    day.day().to_string()
                } else if content.width >= 5 {
                    format!("{}({})", day.day(), events.len())
                } else {
                    format!("{}•", day.day())
                };
                let header_rect = Rect::new(content.x, content.y, content.width, 1);
                frame.render_widget(
                    Paragraph::new(header).style(
                        Style::default()
                            .fg(if focused { ACCENT } else { Color::White })
                            .bg(background)
                            .add_modifier(if focused {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    header_rect,
                );
                let capacity = usize::from(content.height.saturating_sub(1));
                let skip = selection_offset(&events, selected, capacity);
                let representative = events
                    .iter()
                    .copied()
                    .find(|index| Some(*index) == selected)
                    .or_else(|| events.first().copied());
                if let Some(index) = representative.filter(|_| content.width > 0) {
                    hits.push((header_rect, index));
                }
                for (slot, index) in events.iter().skip(skip).take(capacity).enumerate() {
                    let event_rect =
                        Rect::new(content.x, content.y + 1 + slot as u16, content.width, 1);
                    frame.render_widget(
                        Paragraph::new(rows[*index].title.clone())
                            .style(event_style(selected == Some(*index))),
                        event_rect,
                    );
                    if event_rect.width > 0 {
                        hits.push((event_rect, *index));
                    }
                }
            }
        }
        hits
    }
}

const WEEKDAYS: [&str; 7] = ["一", "二", "三", "四", "五", "六", "日"];

fn day_column(frame: &mut Frame, area: Rect, last: bool) -> Rect {
    if last || area.width < 2 {
        return area;
    }
    let x = area.x + area.width - 1;
    frame.render_widget(
        Paragraph::new(vec![Line::from("|"); usize::from(area.height)])
            .style(Style::default().fg(Color::DarkGray)),
        Rect::new(x, area.y, 1, area.height),
    );
    Rect::new(area.x, area.y, area.width - 1, area.height)
}

fn events_on(day: NaiveDate, rows: &[Item]) -> Vec<usize> {
    let Some((day_start, day_end)) = midnight(day).zip(day.succ_opt().and_then(midnight)) else {
        return vec![];
    };
    let mut events: Vec<_> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let (start, end) = Calendar::interval(item)?;
            (start < day_end && end > day_start).then_some((index, start, end))
        })
        .collect();
    events.sort_by_key(|(index, start, end)| (*start, *end, *index));
    events.into_iter().map(|(index, _, _)| index).collect()
}

fn selection_offset(events: &[usize], selected: Option<usize>, capacity: usize) -> usize {
    selected
        .and_then(|selected| events.iter().position(|index| *index == selected))
        .map(|position| position.saturating_sub(capacity.saturating_sub(1)))
        .unwrap_or(0)
}

fn event_style(focused: bool) -> Style {
    Style::default()
        .fg(if focused { ACCENT } else { Color::White })
        .bg(if focused {
            SELECTED
        } else {
            Color::Rgb(30, 43, 54)
        })
        .add_modifier(if focused {
            Modifier::BOLD
        } else {
            Modifier::empty()
        })
}

struct EventSpan {
    index: usize,
    begin: u32,
    finish: u32,
    top: u16,
    bottom: u16,
    lane: usize,
}

fn midnight(day: NaiveDate) -> Option<DateTime<Tz>> {
    Taipei
        .from_local_datetime(&day.and_hms_opt(0, 0, 0)?)
        .single()
}

fn parse_date(value: &str) -> Option<DateTime<Tz>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|date| date.with_timezone(&Taipei))
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(midnight)
        })
}

fn minutes(time: DateTime<Tz>) -> u32 {
    time.hour() * 60 + time.minute()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use ratatui::{backend::TestBackend, Terminal};

    fn item(start: &str, end: Option<&str>) -> Item {
        Item {
            id: "calendar-test".into(),
            kind: Kind::Calendar,
            collection: "calendar".into(),
            remote_id: "event.ics".into(),
            etag: None,
            uid_validity: None,
            title: "Planning".into(),
            subtitle: "Meeting room".into(),
            date: Some(start.into()),
            end_date: end.map(str::to_owned),
            text: "Discuss next steps".into(),
            raw: String::new(),
            flags: vec![],
            emails: vec![],
            phones: vec![],
            attachments: vec![],
        }
    }

    fn calendar() -> Calendar {
        Calendar {
            start: NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            view: CalendarView::ThreeDay,
        }
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in buffer.area.y..buffer.area.bottom() {
            let mut x = buffer.area.x;
            while x < buffer.area.right() {
                let symbol = buffer[(x, y)].symbol();
                text.push_str(symbol);
                x += unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn views_default_and_cycle_in_order() {
        assert_eq!(Calendar::new().view, CalendarView::ThreeDay);
        let view = CalendarView::default();
        assert_eq!(view.label(), "3-day");
        assert_eq!(view.next(), CalendarView::Week);
        assert_eq!(view.next().next(), CalendarView::Month);
        assert_eq!(view.next().next().next(), view);
    }

    #[test]
    fn week_and_month_windows_use_taipei_and_exclusive_boundaries() {
        let mut calendar = calendar();
        calendar.view = CalendarView::Week;
        assert_eq!(
            calendar.window(),
            Some((
                NaiveDate::from_ymd_opt(2026, 10, 5).unwrap(),
                NaiveDate::from_ymd_opt(2026, 10, 12).unwrap()
            ))
        );
        let days = calendar.days();
        assert_eq!(days.len(), 7);
        assert_eq!(days.first().unwrap().weekday(), chrono::Weekday::Mon);
        assert_eq!(days.last().unwrap().weekday(), chrono::Weekday::Sun);
        assert!(!calendar.contains(&item("2026-10-04", Some("2026-10-05"))));
        assert!(calendar.contains(&item("2026-10-11", Some("2026-10-12"))));
        assert!(!calendar.contains(&item("2026-10-12", None)));
        assert!(calendar.contains(&item(
            "2026-10-04T23:30:00+08:00",
            Some("2026-10-05T00:30:00+08:00")
        )));
        assert_eq!(
            calendar
                .center()
                .with_timezone(&Taipei)
                .format("%F %R")
                .to_string(),
            "2026-10-08 12:00"
        );

        calendar.view = CalendarView::Month;
        assert_eq!(
            calendar.window(),
            Some((
                NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()
            ))
        );
        assert!(!calendar.contains(&item("2026-09-30", Some("2026-10-01"))));
        assert!(calendar.contains(&item("2026-10-31", Some("2026-11-01"))));
        assert!(!calendar.contains(&item("2026-11-01", None)));
        assert!(calendar.contains(&item("2026-09-30T16:30:00Z", None)));
        assert_eq!(
            calendar
                .center()
                .with_timezone(&Taipei)
                .format("%F %R")
                .to_string(),
            "2026-10-16 12:00"
        );
    }

    #[test]
    fn shifts_follow_view_and_clamp_month_end_across_years_and_leap_years() {
        let mut calendar = calendar();
        calendar.shift(1);
        assert_eq!(calendar.start.day(), 8);
        calendar.view = CalendarView::Week;
        calendar.shift(-1);
        assert_eq!(
            calendar.start,
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
        );
        calendar.shift(2);
        assert_eq!(calendar.start.day(), 15);

        calendar.view = CalendarView::Month;
        calendar.start = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        calendar.shift(1);
        assert_eq!(
            calendar.start,
            NaiveDate::from_ymd_opt(2027, 1, 31).unwrap()
        );
        calendar.shift(1);
        assert_eq!(
            calendar.start,
            NaiveDate::from_ymd_opt(2027, 2, 28).unwrap()
        );
        calendar.shift(-2);
        assert_eq!(
            calendar.start,
            NaiveDate::from_ymd_opt(2026, 12, 28).unwrap()
        );
        calendar.start = NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();
        calendar.shift(1);
        assert_eq!(
            calendar.start,
            NaiveDate::from_ymd_opt(2024, 2, 29).unwrap()
        );
        calendar.shift(-1);
        assert_eq!(
            calendar.start,
            NaiveDate::from_ymd_opt(2024, 1, 29).unwrap()
        );
        calendar.shift(i32::MAX);
        assert_eq!(
            calendar.start,
            NaiveDate::from_ymd_opt(2024, 1, 29).unwrap()
        );
        calendar.shift(i32::MIN);
        assert_eq!(
            calendar.start,
            NaiveDate::from_ymd_opt(2024, 1, 29).unwrap()
        );
    }

    #[test]
    fn per_day_events_handle_overnight_and_exclusive_midnight() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 7).unwrap();
        let rows = vec![
            item("2026-10-06", Some("2026-10-07")),
            item("2026-10-07", Some("2026-10-08")),
            item("2026-10-08", None),
            item(
                "2026-10-06T23:00:00+08:00",
                Some("2026-10-07T01:00:00+08:00"),
            ),
        ];
        assert_eq!(events_on(day, &rows), vec![3, 1]);
    }

    #[test]
    fn week_has_seven_columns_and_keeps_crowded_selection_visible() {
        let mut calendar = calendar();
        calendar.view = CalendarView::Week;
        let mut rows: Vec<_> = (5..=11)
            .map(|day| {
                item(
                    &format!("2026-10-{day:02}T09:00:00+08:00"),
                    Some(&format!("2026-10-{day:02}T11:00:00+08:00")),
                )
            })
            .collect();
        rows.extend((0..8).map(|index| {
            let mut event = item(
                "2026-10-07T10:00:00+08:00",
                Some("2026-10-07T12:00:00+08:00"),
            );
            event.title = format!("Overflow {index}");
            event
        }));
        for (width, height) in [(140, 28), (40, 12)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut hits = vec![];
            terminal
                .draw(|frame| hits = calendar.draw_grid(frame, frame.area(), &rows, Some(14)))
                .unwrap();
            let text = screen(&terminal);
            assert!(text.contains("週視圖"));
            assert!(text.contains("08:00"));
            if width == 140 {
                assert!(text.contains("Mon 10/05"));
                assert!(text.contains("Sun 10/11"));
                assert!(hits.iter().any(|(rect, _)| rect.height > 1));
            } else {
                assert!(text.contains("一05"));
                assert!(text.contains("日11"));
            }
            let columns: std::collections::BTreeSet<_> = hits
                .iter()
                .filter(|(_, index)| *index < 7)
                .map(|(rect, _)| rect.x)
                .collect();
            assert_eq!(columns.len(), 7);
            assert_eq!(hits.iter().find(|(_, index)| *index == 0).unwrap().0.x, 7);
            assert!(hits
                .iter()
                .all(|(rect, _)| rect.width > 0 && rect.height > 0));
            let focused = hits.last().unwrap();
            assert_eq!(focused.1, 14, "selected block stays visible on top");
            let point = ratatui::layout::Position::new(focused.0.x, focused.0.y);
            assert_eq!(terminal.backend().buffer()[point].bg, SELECTED);
            assert_eq!(
                hits.iter()
                    .rev()
                    .find(|(rect, _)| rect.contains(point))
                    .unwrap()
                    .1,
                14,
                "the visible selected block remains clickable"
            );
        }
    }

    #[test]
    fn month_uses_monday_alignment_and_all_six_weeks() {
        let mut calendar = calendar();
        calendar.view = CalendarView::Month;
        calendar.start = NaiveDate::from_ymd_opt(2026, 3, 7).unwrap();
        let rows = vec![
            item("2026-03-01", None),
            item("2026-03-02", None),
            item("2026-03-31", None),
        ];
        let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
        let mut hits = vec![];
        terminal
            .draw(|frame| hits = calendar.draw_grid(frame, frame.area(), &rows, Some(2)))
            .unwrap();
        assert!(screen(&terminal).contains("月視圖 · 2026-03"));
        assert!(!screen(&terminal).contains("週 "));
        let first = hits.iter().find(|(_, index)| *index == 0).unwrap().0;
        let second = hits.iter().find(|(_, index)| *index == 1).unwrap().0;
        let last = hits.iter().find(|(_, index)| *index == 2).unwrap().0;
        assert!(first.x > second.x, "March 1 is Sunday, March 2 is Monday");
        assert!(first.y < second.y);
        assert!(last.x > second.x && last.x < first.x, "March 31 is Tuesday");
        assert!(last.y > second.y, "sixth week remains visible");
        assert_eq!(terminal.backend().buffer()[(last.x, last.y)].bg, SELECTED);
    }

    #[test]
    fn month_keeps_selected_event_visible_and_clickable_when_compact_or_crowded() {
        let mut calendar = calendar();
        calendar.view = CalendarView::Month;
        calendar.start = NaiveDate::from_ymd_opt(2026, 3, 7).unwrap();
        let rows: Vec<_> = (0..8)
            .map(|index| {
                let mut event = item("2026-03-31", None);
                event.title = format!("Selected {index}");
                event
            })
            .collect();
        for height in [4, 26] {
            let mut terminal = Terminal::new(TestBackend::new(100, height)).unwrap();
            let mut hits = vec![];
            terminal
                .draw(|frame| hits = calendar.draw_grid(frame, frame.area(), &rows, Some(7)))
                .unwrap();
            let selected = hits.iter().find(|(_, index)| *index == 7).unwrap().0;
            assert_eq!(
                terminal.backend().buffer()[(selected.x, selected.y)].bg,
                SELECTED
            );
            assert!(screen(&terminal).contains("31(8)"));
            if height == 4 {
                assert!(screen(&terminal).contains("週 6–6/6"));
            } else {
                assert!(screen(&terminal).contains("Selected 7"));
            }
        }
        let crossing = vec![item("2026-02-28", Some("2026-03-03"))];
        let mut terminal = Terminal::new(TestBackend::new(40, 4)).unwrap();
        let mut hits = vec![];
        terminal
            .draw(|frame| hits = calendar.draw_grid(frame, frame.area(), &crossing, Some(0)))
            .unwrap();
        assert!(hits.iter().any(|(_, index)| *index == 0));
        assert!(screen(&terminal).contains("週 1–1/6"));
    }

    #[test]
    fn date_only_events_have_exclusive_end_and_timed_events_use_taipei() {
        let calendar = calendar();
        assert!(!calendar.contains(&item("2026-10-06", Some("2026-10-07"))));
        assert!(calendar.contains(&item("2026-10-09", Some("2026-10-10"))));
        assert!(!calendar.contains(&item("2026-10-10", None)));
        let (start, end) = Calendar::interval(&item("2026-10-07T01:30:00Z", None)).unwrap();
        assert_eq!(start.format("%H:%M").to_string(), "09:30");
        assert_eq!(end - start, Duration::hours(1));
        let (start, end) = Calendar::interval(&item("2026-10-07", Some("invalid"))).unwrap();
        assert_eq!(end - start, Duration::days(1));
    }

    #[test]
    fn overlapping_and_overnight_events_have_distinct_visible_hit_regions() {
        let mut calendar = calendar();
        let rows = vec![
            item(
                "2026-10-07T09:00:00+08:00",
                Some("2026-10-07T11:00:00+08:00"),
            ),
            item(
                "2026-10-07T10:00:00+08:00",
                Some("2026-10-07T12:00:00+08:00"),
            ),
            item(
                "2026-10-07T23:30:00+08:00",
                Some("2026-10-08T01:00:00+08:00"),
            ),
        ];
        for view in [CalendarView::ThreeDay, CalendarView::Week] {
            calendar.view = view;
            let mut terminal = Terminal::new(TestBackend::new(90, 28)).unwrap();
            let mut hits = Vec::new();
            terminal
                .draw(|frame| hits = calendar.draw_grid(frame, frame.area(), &rows, Some(0)))
                .unwrap();
            assert_eq!(hits.len(), 4, "overnight event appears on both days");
            let first = hits.iter().find(|(_, index)| *index == 0).unwrap().0;
            let second = hits.iter().find(|(_, index)| *index == 1).unwrap().0;
            assert!(first.x + first.width <= second.x || second.x + second.width <= first.x);
            assert!(hits
                .iter()
                .all(|(area, _)| area.width > 0 && area.height > 0));
            assert_eq!(hits.last().unwrap().1, 0, "focused block renders last");
            assert!(screen(&terminal).contains("00:00"));
            assert!(screen(&terminal).contains("23:00"));
        }
    }

    #[test]
    fn tiny_and_empty_grid_draw_safely() {
        let mut calendar = calendar();
        for view in [
            CalendarView::ThreeDay,
            CalendarView::Week,
            CalendarView::Month,
        ] {
            calendar.view = view;
            for (width, height) in [(0, 0), (1, 1), (8, 2), (40, 12)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal
                    .draw(|frame| {
                        assert!(calendar
                            .draw_grid(frame, frame.area(), &[], None)
                            .is_empty());
                    })
                    .unwrap();
            }
        }
    }
}
