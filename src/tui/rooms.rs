use super::{block, receive, ACCENT, MUTED, SELECTED};
use crate::meeting::{self, RoomsArgs, RoomsOutput, Slot};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Duration, NaiveTime, TimeZone, Timelike, Utc};
use chrono_tz::Asia::Taipei;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Cell, Paragraph, Row, Table, TableState, Wrap},
    Frame,
};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
};

mod prefs;

const LABELS: [&str; 6] = [
    "開始（日期＋時間，Asia/Taipei）",
    "結束（選填；留空掃到當天下班）",
    "站點（選填）",
    "最少人數（選填）",
    "會議分鐘（選填，留空查精確起迄）",
    "工作時段（HH:MM-HH:MM）",
];

struct BookingPick {
    room_name: String,
    room: meeting::MeetingRoom,
    gap: Slot,
    minutes: u32,
    fields: [String; 4],
    focus: usize,
    end_edited: bool,
    field_areas: [Rect; 4],
    confirm_area: Rect,
}

pub(super) struct RoomFinder {
    dir: PathBuf,
    fields: [String; 6],
    focus: usize,
    pub status: String,
    pending: Option<Receiver<Result<RoomsOutput>>>,
    result: Option<RoomsOutput>,
    table: TableState,
    field_areas: [Rect; 6],
    submit_area: Rect,
    result_area: Rect,
    errors_area: Rect,
    scroll: u16,
    prefs_warning: Option<String>,
    entered: bool,
    start_edited: bool,
    booking: Option<BookingPick>,
    draft: Option<crate::meeting_draft::Draft>,
    sites: String,
}

impl RoomFinder {
    pub fn new(dir: &Path) -> Self {
        Self::new_at(dir, Utc::now())
    }

    fn new_at(dir: &Path, now: DateTime<Utc>) -> Self {
        let (preferences, prefs_warning) = match prefs::Preferences::load(dir) {
            Ok(preferences) => (preferences, None),
            Err(error) => (
                prefs::Preferences::default(),
                Some(format!("偏好讀取失敗：{error:#}")),
            ),
        };
        let status = with_warning(
            "日期／時間使用 Asia/Taipei · 結束留空會搜尋開始當日剩餘工作時段".into(),
            prefs_warning.as_deref(),
        );
        let hours = meeting::parse_working_hours(&preferences.working_hours).unwrap_or_else(|_| {
            meeting::parse_working_hours(prefs::DEFAULT_WORKING_HOURS)
                .expect("default working hours are valid")
        });
        let sites = crate::config::load(dir)
            .ok()
            .and_then(|config| crate::site::SiteProfile::for_config(Some(&config)).ok())
            .map(|profile| {
                profile
                    .sites
                    .iter()
                    .map(|site| site.name.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            })
            .unwrap_or_default();
        Self {
            dir: dir.to_owned(),
            fields: [
                default_start(now, hours, preferences.minutes),
                String::new(),
                preferences.site,
                preferences
                    .capacity
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                preferences
                    .minutes
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                preferences.working_hours,
            ],
            focus: 0,
            status,
            pending: None,
            result: None,
            table: TableState::default(),
            field_areas: [Rect::default(); 6],
            submit_area: Rect::default(),
            result_area: Rect::default(),
            errors_area: Rect::default(),
            scroll: 0,
            prefs_warning,
            entered: false,
            start_edited: false,
            booking: None,
            draft: None,
            sites,
        }
    }

    pub fn enter(&mut self) {
        self.enter_at(Utc::now());
    }

    fn enter_at(&mut self, now: DateTime<Utc>) {
        if self.entered {
            return;
        }
        self.entered = true;
        if !self.start_edited {
            if let Ok(hours) = meeting::parse_working_hours(&self.fields[5]) {
                let minutes = positive_number(&self.fields[4], "會議分鐘").ok().flatten();
                self.fields[0] = default_start(now, hours, minutes);
            }
        }
    }

    fn args(&self) -> Result<RoomsArgs> {
        self.args_at(Utc::now())
    }

    fn args_at(&self, now: DateTime<Utc>) -> Result<RoomsArgs> {
        let working_hours = self.fields[5].trim().to_owned();
        let hours = meeting::parse_working_hours(&working_hours)?;
        let capacity = positive_number(&self.fields[3], "最少人數")?;
        let minutes = positive_number(&self.fields[4], "會議分鐘")?;
        let start = crate::human_time::parse(&self.fields[0], now, time_at(hours.start))
            .context("開始時間無法解析")?;
        let end = if self.fields[1].trim().is_empty() {
            let duration = minutes.context("結束留空時請填會議分鐘；精確起迄查詢需填結束")?;
            let work_start = start
                .offset()
                .from_local_datetime(&start.date_naive().and_time(time_at(hours.start)))
                .single()
                .context("工作時間超出可用範圍")?;
            let work_end = start
                .offset()
                .from_local_datetime(&start.date_naive().and_time(time_at(hours.end)))
                .single()
                .context("工作時間超出可用範圍")?;
            let first_end = start
                .max(work_start)
                .checked_add_signed(Duration::minutes(i64::from(duration)))
                .context("會議時間超出可用範圍")?;
            if first_end > work_end {
                bail!(
                    "當天剩餘工作時段不足 {duration} 分鐘；請提前開始、縮短分鐘，或填明確結束日期"
                );
            }
            work_end
        } else {
            crate::human_time::parse_end(&self.fields[1], now, time_at(hours.end), start)
                .context("結束時間無法解析")?
        };
        let start = start.to_rfc3339();
        let end = end.to_rfc3339();
        let range = meeting::parse_time_range(&start, &end)?;
        if minutes.is_some()
            && range.end.signed_duration_since(range.start)
                > Duration::days(meeting::MAX_ROOM_SLOT_DAYS)
        {
            bail!("會議室時長搜尋範圍最多 31 天；請縮小開始／結束範圍");
        }
        let site = (!self.fields[2].trim().is_empty()).then(|| self.fields[2].trim().to_owned());
        meeting::validate_site(&self.dir, site.as_deref())?;
        Ok(RoomsArgs {
            refresh: false,
            site,
            capacity,
            start,
            end,
            minutes,
            working_hours,
        })
    }

    fn submit(&mut self, refresh: bool) {
        if self.pending.is_some() {
            self.status = with_warning(
                "會議室查詢仍在進行...".into(),
                self.prefs_warning.as_deref(),
            );
            return;
        }
        let mut args = match self.args() {
            Ok(args) => args,
            Err(error) => {
                self.status = format!("輸入錯誤：{error:#}");
                return;
            }
        };
        args.refresh = refresh;
        let preferences = prefs::Preferences {
            site: args.site.clone().unwrap_or_default(),
            capacity: args.capacity,
            minutes: args.minutes,
            working_hours: args.working_hours.clone(),
        };
        self.prefs_warning = preferences
            .save(&self.dir)
            .err()
            .map(|error| format!("偏好保存失敗：{error:#}"));
        let dir = self.dir.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(meeting::query_rooms(&dir, args));
        });
        self.pending = Some(rx);
        self.result = None;
        self.booking = None;
        self.table = TableState::default();
        self.scroll = 0;
        self.status = with_warning(
            "正在查詢會議室與可用時段...".into(),
            self.prefs_warning.as_deref(),
        );
    }

    pub fn poll(&mut self) -> bool {
        let Some(response) = self.pending.as_ref().and_then(receive) else {
            return false;
        };
        self.pending = None;
        match response {
            Ok(result) => {
                self.status = format!(
                    "找到 {} 間可用會議室 · {} 間未知 · {} 間命中快取{}",
                    result.rooms.len(),
                    result.unknown.len(),
                    result.cache_hits,
                    if result.errors.is_empty() {
                        String::new()
                    } else {
                        format!(" · {} 項查詢錯誤", result.errors.len())
                    }
                );
                self.table.select((!result.rooms.is_empty()).then_some(0));
                self.result = Some(result);
                self.focus = 6;
            }
            Err(error) => self.status = format!("會議室查詢失敗：{error:#}"),
        }
        self.status = with_warning(
            std::mem::take(&mut self.status),
            self.prefs_warning.as_deref(),
        );
        true
    }

    pub fn consume_escape(&mut self) -> bool {
        if self.draft.take().is_some() {
            self.scroll = 0;
            return true;
        }
        if self.booking.take().is_some() {
            self.status = with_warning(
                "已取消段內訂房起迄；仍可↑↓選空檔後 Enter 再選實際時間".into(),
                self.prefs_warning.as_deref(),
            );
            true
        } else {
            false
        }
    }

    pub fn key(&mut self, key: KeyEvent) {
        if self.draft.is_some() {
            match key.code {
                KeyCode::PageUp | KeyCode::Up => self.scroll = self.scroll.saturating_sub(5),
                KeyCode::PageDown | KeyCode::Down => self.scroll = self.scroll.saturating_add(5),
                _ => {}
            }
            return;
        }
        if self.booking.is_some() {
            self.booking_key(key);
            return;
        }
        match key.code {
            KeyCode::F(5) => self.submit(true),
            KeyCode::Tab => self.focus = (self.focus + 1) % 7,
            KeyCode::BackTab => self.focus = (self.focus + 6) % 7,
            KeyCode::Enter => {
                if self.focus == 6 && self.open_booking_from_selection() {
                    return;
                }
                self.submit(false);
            }
            KeyCode::Backspace if self.focus < 6 => {
                self.start_edited |= self.focus == 0;
                self.fields[self.focus].pop();
            }
            KeyCode::Char('u')
                if self.focus < 6 && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.start_edited |= self.focus == 0;
                self.fields[self.focus].clear()
            }
            KeyCode::Char(c)
                if self.focus < 6 && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.start_edited |= self.focus == 0;
                self.fields[self.focus].push(c)
            }
            KeyCode::Up if self.focus == 6 => self.move_result(-1),
            KeyCode::Down if self.focus == 6 => self.move_result(1),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(5),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(5),
            _ => {}
        }
    }

    fn booking_key(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Enter) {
            self.confirm_booking();
            return;
        }
        let Some(booking) = self.booking.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Tab => booking.focus = (booking.focus + 1) % 5,
            KeyCode::BackTab => booking.focus = (booking.focus + 4) % 5,
            KeyCode::Backspace if booking.focus < 4 => {
                booking.end_edited |= booking.focus == 1;
                booking.fields[booking.focus].pop();
            }
            KeyCode::Char('u')
                if booking.focus < 4 && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                booking.end_edited |= booking.focus == 1;
                booking.fields[booking.focus].clear();
            }
            KeyCode::Char(c)
                if booking.focus < 4 && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                booking.end_edited |= booking.focus == 1;
                booking.fields[booking.focus].push(c);
            }
            _ => {}
        }
    }

    fn selected_gap(&self) -> Option<(meeting::MeetingRoom, Slot, u32)> {
        let result = self.result.as_ref()?;
        let minutes = result.minutes?;
        let index = self.table.selected()?;
        let mut cursor = 0usize;
        for entry in &result.available_slots {
            for slot in &entry.slots {
                if cursor == index {
                    return Some((entry.room.clone(), slot.clone(), minutes));
                }
                cursor += 1;
            }
        }
        None
    }

    fn open_booking_from_selection(&mut self) -> bool {
        let Some((room, gap, minutes)) = self.selected_gap() else {
            return false;
        };
        let default_end = match meeting::booking_window_in_gap(&gap, &gap.start, minutes) {
            Ok(window) => window.end,
            Err(_) => String::new(),
        };
        self.booking = Some(BookingPick {
            room_name: room.name.clone(),
            room,
            gap: gap.clone(),
            minutes,
            fields: [gap.start, default_end, "會議".into(), String::new()],
            focus: 0,
            end_edited: false,
            field_areas: [Rect::default(); 4],
            confirm_area: Rect::default(),
        });
        self.status = with_warning(
            format!(
                "在選定空檔內挑實際 {} 分鐘起迄；結束可改寫自訂長度 · Enter 確認 · Esc 取消",
                minutes
            ),
            self.prefs_warning.as_deref(),
        );
        true
    }

    fn confirm_booking(&mut self) {
        let Some(booking) = self.booking.as_ref() else {
            return;
        };
        let start_input = booking.fields[0].trim();
        if start_input.is_empty() {
            self.status = "輸入錯誤：請填實際開始時間".into();
            return;
        }
        let now = Utc::now();
        let gap_start = match chrono::DateTime::parse_from_rfc3339(&booking.gap.start) {
            Ok(value) => value,
            Err(error) => {
                self.status = format!("輸入錯誤：空檔開始時間無效：{error}");
                return;
            }
        };
        let start = match crate::human_time::parse_end(
            start_input,
            now,
            NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
            gap_start,
        ) {
            Ok(value) => value,
            Err(error) => {
                self.status = format!("輸入錯誤：{error:#}");
                return;
            }
        };
        let window = if booking.fields[1].trim().is_empty() || !booking.end_edited {
            meeting::booking_window_in_gap(&booking.gap, &start.to_rfc3339(), booking.minutes)
        } else {
            match crate::human_time::parse_end(
                booking.fields[1].trim(),
                now,
                NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
                start,
            ) {
                Ok(end) => meeting::booking_range_in_gap(
                    &booking.gap,
                    &start.to_rfc3339(),
                    &end.to_rfc3339(),
                ),
                Err(error) => {
                    self.status = format!("輸入錯誤：{error:#}");
                    return;
                }
            }
        };
        let window = match window {
            Ok(window) => window,
            Err(error) => {
                self.status = format!("輸入錯誤：{error:#}");
                return;
            }
        };
        match crate::meeting_draft::save_draft(
            &self.dir,
            &booking.room,
            &meeting::parse_time_range(&window.start, &window.end)
                .expect("validated booking range"),
            &booking.fields[2],
            &[booking.fields[3].clone()],
        ) {
            Ok(draft) => {
                self.status = format!(
                    "草稿已保存 {} → {} · 尚未寫入行事曆",
                    draft.start, draft.end
                );
                self.draft = Some(draft);
                self.booking = None;
                self.scroll = 0;
            }
            Err(error) => self.status = format!("草稿保存失敗：{error:#}"),
        }
    }

    fn row_count(&self) -> usize {
        self.result
            .as_ref()
            .map(|result| {
                if result.minutes.is_some() {
                    result
                        .available_slots
                        .iter()
                        .map(|room| room.slots.len())
                        .sum()
                } else {
                    result.rooms.len()
                }
            })
            .unwrap_or(0)
    }

    fn move_result(&mut self, delta: i32) {
        let count = self.row_count();
        if count > 0 {
            self.table.select(Some(
                (self.table.selected().unwrap_or(0) as i64 + i64::from(delta))
                    .clamp(0, count as i64 - 1) as usize,
            ));
        }
    }

    pub fn mouse(&mut self, mouse: MouseEvent) {
        if self.draft.is_some() {
            return;
        }
        let point = Position::new(mouse.column, mouse.row);
        if self.booking.is_some() {
            if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
                let confirm = self
                    .booking
                    .as_ref()
                    .is_some_and(|booking| booking.confirm_area.contains(point));
                if confirm {
                    if let Some(booking) = self.booking.as_mut() {
                        booking.focus = 4;
                    }
                    self.confirm_booking();
                    return;
                }
                if let Some(booking) = self.booking.as_mut() {
                    if let Some(index) = booking.field_areas.iter().position(|r| r.contains(point))
                    {
                        booking.focus = index;
                    }
                }
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = self.field_areas.iter().position(|r| r.contains(point)) {
                    self.focus = index;
                } else if self.submit_area.contains(point) {
                    self.focus = 6;
                    self.submit(false);
                } else if self.result_area.contains(point) {
                    self.focus = 6;
                    let index = self.table.offset() + usize::from(mouse.row - self.result_area.y);
                    if index < self.row_count() {
                        self.table.select(Some(index));
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = mouse.kind == MouseEventKind::ScrollDown;
                if self.result_area.contains(point) {
                    self.move_result(if down { 3 } else { -3 });
                } else if self.errors_area.contains(point) {
                    self.scroll = if down {
                        self.scroll.saturating_add(3)
                    } else {
                        self.scroll.saturating_sub(3)
                    };
                }
            }
            _ => {}
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        if let Some(draft) = &self.draft {
            let mut text = format!(
                "{}\n{} → {}\n與會者：{}\nUID：{}\nICS：{}\nJSON：{}\n\n",
                draft.title,
                draft.start,
                draft.end,
                draft.attendees.join(", "),
                draft.uid,
                draft.invitation.display(),
                draft.manifest.display()
            );
            for (name, command) in &draft.actions {
                text.push_str(&format!("{name}\n{command}\n\n"));
            }
            frame.render_widget(
                Paragraph::new(text)
                    .block(block(" 草稿已保存 · PageUp/Down 捲動 · Esc 返回 "))
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll, 0)),
                area,
            );
            return;
        }
        if self.booking.is_some() {
            self.draw_booking(frame, area);
            return;
        }
        self.field_areas = [Rect::default(); 6];
        self.submit_area = Rect::default();
        self.result_area = Rect::default();
        self.errors_area = Rect::default();
        let sections = Layout::vertical([Constraint::Length(10), Constraint::Min(1)]).split(area);
        let form = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(sections[0]);
        for row in 0..3 {
            let cols = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(form[row]);
            for col in 0..2 {
                let index = row * 2 + col;
                self.field_areas[index] = cols[col];
                frame.render_widget(
                    Paragraph::new(format!(
                        "{}{}",
                        self.fields[index],
                        if self.focus == index { "|" } else { "" }
                    ))
                    .block(block(LABELS[index]).border_style(
                        Style::default().fg(if self.focus == index { ACCENT } else { MUTED }),
                    )),
                    cols[col],
                );
            }
        }
        self.submit_area = form[3];
        frame.render_widget(
            Paragraph::new(if self.pending.is_some() {
                "[ 查詢中... ]"
            } else {
                "[ 查詢會議室 · Enter 或點擊 ]"
            })
            .style(Style::default().fg(ACCENT).bg(if self.focus == 6 {
                SELECTED
            } else {
                ratatui::style::Color::Reset
            })),
            form[3],
        );
        let Some(result) = &self.result else {
            frame.render_widget(Paragraph::new(format!("開始／結束可填人話日期與時間，使用 Asia/Taipei；也接受完整 RFC3339。\n\n結束留空＋會議分鐘：搜尋開始當天剩餘工作時段。\n會議分鐘留空：請填結束，查精確起迄。\n站點：{}（選填）。\n查詢時記住站點、人數、分鐘與工作時段；日期不保存，首次進入重新帶入。\n\n分鐘模式結果：↑↓ 選一筆最長連續空檔後 Enter，在該段內挑實際起迄。\n\nTab 切換欄位 · Ctrl-U 清空欄位 · Esc 返回郵件", if self.sites.is_empty() { "site profile 未定義" } else { self.sites.as_str() })).block(block(" 可用會議室 ")).wrap(Wrap{trim:false}),sections[1]);
            return;
        };
        let parts = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(if result.errors.is_empty() { 3 } else { 4 }),
        ])
        .split(sections[1]);
        let panel = block(format!(
            " 可用會議室 {} 間 · {} → {} ",
            result.rooms.len(),
            result.start,
            result.end
        ));
        let inner = panel.inner(parts[0]);
        self.result_area = Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(1),
        );
        let mut rows = Vec::new();
        let row = |room: &meeting::MeetingRoom, last: String| {
            Row::new([
                Cell::from(room.name.clone()),
                Cell::from(
                    room.capacity
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "未知".into()),
                ),
                Cell::from(room.site.clone()),
                Cell::from(last),
            ])
        };
        if result.minutes.is_some() {
            for room in &result.available_slots {
                for slot in &room.slots {
                    rows.push(row(
                        &room.room,
                        format!(
                            "{} -> {}",
                            super::display_time(&slot.start),
                            super::display_time(&slot.end)
                        ),
                    ));
                }
            }
        } else {
            rows.extend(
                result
                    .rooms
                    .iter()
                    .map(|room| row(room, room.source.clone())),
            );
        }
        frame.render_stateful_widget(
            Table::new(
                rows,
                [
                    Constraint::Min(12),
                    Constraint::Length(6),
                    Constraint::Length(8),
                    Constraint::Length(if result.minutes.is_some() { 30 } else { 18 }),
                ],
            )
            .header(
                Row::new([
                    "名稱",
                    "容納",
                    "站點",
                    if result.minutes.is_some() {
                        "可用空檔"
                    } else {
                        "來源"
                    },
                ])
                .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            )
            .block(panel)
            .row_highlight_style(Style::default().bg(SELECTED)),
            parts[0],
            &mut self.table,
        );
        let messages = if result.errors.is_empty() {
            vec![Line::from(if result.rooms.is_empty() {
                "指定條件沒有可用會議室。"
            } else if result.minutes.is_some() {
                "以上為最長連續空檔（一空檔一筆）。↑↓ 選取後 Enter，在該段內挑實際會議起迄；點上方查詢可重搜。"
            } else {
                "以上房間已確認指定時段可用。"
            })]
        } else {
            result
                .errors
                .iter()
                .map(|error| Line::from(error.clone()))
                .collect::<Vec<_>>()
        };
        self.errors_area = parts[1];
        frame.render_widget(
            Paragraph::new(messages)
                .block(block(" 查詢狀態 / 未知原因（PageUp/Down 捲動） "))
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .style(Style::default().fg(MUTED)),
            parts[1],
        );
    }

    fn draw_booking(&mut self, frame: &mut Frame, area: Rect) {
        let Some(booking) = self.booking.as_mut() else {
            return;
        };
        booking.field_areas = [Rect::default(); 4];
        booking.confirm_area = Rect::default();
        if area.height < 18 {
            frame.render_widget(
                Paragraph::new("訂房草稿需要較高視窗，請將終端調整至至少 24 列；Esc 返回。")
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        let sections = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(area);
        let title = format!(
            " 段內訂房 · {} · 空檔 {} → {} · 預設 {} 分 ",
            booking.room_name,
            super::display_time(&booking.gap.start),
            super::display_time(&booking.gap.end),
            booking.minutes
        );
        frame.render_widget(
            Paragraph::new(format!(
                "實際開始／結束必須完全落在選定空檔內。結束留空或未改寫時，長度為查詢的 {} 分鐘。確認後保存 ICS 與完整操作指令，保留帳號與 UID。",
                booking.minutes
            ))
            .block(block(title))
            .wrap(Wrap { trim: false }),
            sections[0],
        );
        let labels = [
            "實際開始",
            "實際結束（選填；留空＝開始＋會議分鐘）",
            "會議主旨",
            "與會者（姓名／email，以逗號分隔）",
        ];
        for index in 0..4 {
            booking.field_areas[index] = sections[index + 1];
            frame.render_widget(
                Paragraph::new(format!(
                    "{}{}",
                    booking.fields[index],
                    if booking.focus == index { "|" } else { "" }
                ))
                .block(block(labels[index]).border_style(Style::default().fg(
                    if booking.focus == index {
                        ACCENT
                    } else {
                        MUTED
                    },
                ))),
                sections[index + 1],
            );
        }
        booking.confirm_area = sections[5];
        frame.render_widget(
            Paragraph::new("[ 確認實際起迄 · Enter 或點擊 ]").style(
                Style::default().fg(ACCENT).bg(if booking.focus == 4 {
                    SELECTED
                } else {
                    ratatui::style::Color::Reset
                }),
            ),
            sections[5],
        );
        frame.render_widget(
            Paragraph::new("Tab 切換 · Ctrl-U 清空欄位 · Esc 回空檔列表")
                .style(Style::default().fg(MUTED)),
            sections[6],
        );
    }
}

fn positive_number(value: &str, label: &str) -> Result<Option<u32>> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let number = value
        .trim()
        .parse()
        .with_context(|| format!("{label}必須是正整數"))?;
    if number == 0 {
        bail!("{label}必須大於 0");
    }
    Ok(Some(number))
}

fn time_at(minutes: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(minutes / 60, minutes % 60, 0)
        .expect("validated working hours fit within a day")
}

fn default_start(now: DateTime<Utc>, hours: meeting::WorkingHours, minutes: Option<u32>) -> String {
    let local = now.with_timezone(&Taipei);
    let mut day = local.date_naive();
    let elapsed = local.hour() * 60
        + local.minute()
        + u32::from(local.second() > 0 || local.nanosecond() > 0);
    let rounded = elapsed.div_ceil(15) * 15;
    let candidate = rounded.max(hours.start);
    let duration = minutes.unwrap_or(0);
    let today_fits = local.weekday().number_from_monday() <= 5
        && candidate < hours.end
        && duration <= hours.end - candidate;
    let start = if today_fits {
        candidate
    } else {
        for _ in 0..7 {
            let Some(next) = day.succ_opt() else { break };
            day = next;
            if day.weekday().number_from_monday() <= 5 {
                break;
            }
        }
        hours.start
    };
    format!("{day} {:02}:{:02}", start / 60, start % 60)
}

fn with_warning(mut status: String, warning: Option<&str>) -> String {
    if let Some(warning) = warning {
        status.push_str(" · ");
        status.push_str(warning);
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn reference(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn fresh_form_has_defaults_and_uses_taipei_workday() {
        let dir = tempdir().unwrap();
        let now = reference("2026-10-07T08:00:00+08:00");
        let finder = RoomFinder::new_at(dir.path(), now);
        assert_eq!(
            finder.fields,
            ["2026-10-07 09:30", "", "", "6", "120", "09:30-17:30"]
        );
        let args = finder.args_at(now).unwrap();
        assert_eq!(args.start, "2026-10-07T09:30:00+08:00");
        assert_eq!(args.end, "2026-10-07T17:30:00+08:00");
        assert_eq!(args.capacity, Some(6));
        assert_eq!(args.minutes, Some(120));
        assert!(!dir.path().join(prefs::FILE_NAME).exists());
    }

    #[test]
    fn first_entry_refreshes_automatic_start_and_preserves_later_edits() {
        let dir = tempdir().unwrap();
        let mut finder = RoomFinder::new_at(dir.path(), reference("2026-10-07T08:00:00+08:00"));
        finder.enter_at(reference("2026-10-07T16:00:00+08:00"));
        assert_eq!(finder.fields[0], "2026-10-08 09:30");
        finder.fields[0] = "下週一 下午兩點".into();
        finder.enter_at(reference("2026-10-09T18:00:00+08:00"));
        assert_eq!(finder.fields[0], "下週一 下午兩點");

        let mut edited = RoomFinder::new_at(dir.path(), reference("2026-10-07T08:00:00+08:00"));
        edited.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "明天 14:00".chars() {
            edited.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        edited.enter_at(reference("2026-10-07T16:00:00+08:00"));
        assert_eq!(edited.fields[0], "明天 14:00");
    }

    #[test]
    fn repeated_submit_during_loading_keeps_preference_warning() {
        let dir = tempdir().unwrap();
        let mut finder = RoomFinder::new(dir.path());
        let (_tx, rx) = mpsc::channel();
        finder.pending = Some(rx);
        finder.prefs_warning = Some("偏好保存失敗".into());
        finder.submit(false);
        assert!(finder.status.contains("查詢仍在進行"));
        assert!(finder.status.contains("偏好保存失敗"));
    }

    #[test]
    fn default_start_rounds_forward_and_skips_closed_days() {
        let dir = tempdir().unwrap();
        for (now, expected) in [
            ("2026-10-07T09:30:01+08:00", "2026-10-07 09:45"),
            ("2026-10-07T10:01:00+08:00", "2026-10-07 10:15"),
            ("2026-10-07T15:30:00+08:00", "2026-10-07 15:30"),
            ("2026-10-07T15:30:01+08:00", "2026-10-08 09:30"),
            ("2026-10-09T16:00:00+08:00", "2026-10-12 09:30"),
            ("2026-10-11T08:00:00+08:00", "2026-10-12 09:30"),
        ] {
            let finder = RoomFinder::new_at(dir.path(), reference(now));
            assert_eq!(finder.fields[0], expected, "{now}");
            assert!(finder.args_at(reference(now)).is_ok());
        }
    }

    #[test]
    fn human_times_and_blank_end_become_shared_query_rfc3339() {
        let dir = tempdir().unwrap();
        let now = reference("2026-10-07T12:00:00+08:00");
        let mut finder = RoomFinder::new_at(dir.path(), now);
        finder.fields[0] = "下週一 下午兩點".into();
        let args = finder.args_at(now).unwrap();
        assert_eq!(args.start, "2026-10-12T14:00:00+08:00");
        assert_eq!(args.end, "2026-10-12T17:30:00+08:00");
        finder.fields[1] = "16:00".into();
        finder.fields[4].clear();
        let args = finder.args_at(now).unwrap();
        assert_eq!(args.end, "2026-10-12T16:00:00+08:00");
        assert_eq!(args.minutes, None);
        finder.fields[0] = "2026-10-12T06:00:00Z".into();
        finder.fields[1] = "2026-10-12T08:00:00Z".into();
        let args = finder.args_at(now).unwrap();
        assert_eq!(args.start, "2026-10-12T14:00:00+08:00");
        assert_eq!(args.end, "2026-10-12T16:00:00+08:00");
    }

    #[test]
    fn blank_end_requires_minutes_that_fit_the_remaining_workday() {
        let dir = tempdir().unwrap();
        let now = reference("2026-10-07T12:00:00+08:00");
        let mut finder = RoomFinder::new_at(dir.path(), now);
        finder.fields[0] = "明天 16:00".into();
        assert!(finder
            .args_at(now)
            .unwrap_err()
            .to_string()
            .contains("當天剩餘工作時段不足"));
        finder.fields[4] = "60".into();
        assert_eq!(
            finder.args_at(now).unwrap().end,
            "2026-10-08T17:30:00+08:00"
        );
        finder.fields[4].clear();
        assert!(finder
            .args_at(now)
            .unwrap_err()
            .to_string()
            .contains("結束留空時請填會議分鐘"));
        finder.fields[0] = "明天 08:00".into();
        finder.fields[4] = "600".into();
        assert!(finder
            .args_at(now)
            .unwrap_err()
            .to_string()
            .contains("當天剩餘工作時段不足"));
        finder.fields[4] = "120".into();
        finder.fields[5] = "10:00-16:00".into();
        assert_eq!(
            finder.args_at(now).unwrap().end,
            "2026-10-08T16:00:00+08:00"
        );
    }

    #[test]
    fn saved_preferences_override_defaults_and_dates_stay_fresh() {
        let dir = tempdir().unwrap();
        let preferences = prefs::Preferences {
            site: "北區".into(),
            capacity: Some(12),
            minutes: Some(90),
            working_hours: "10:00-18:00".into(),
        };
        preferences.save(dir.path()).unwrap();
        assert_eq!(prefs::Preferences::load(dir.path()).unwrap(), preferences);
        let finder = RoomFinder::new_at(dir.path(), reference("2026-10-07T08:00:00+08:00"));
        assert_eq!(
            finder.fields,
            ["2026-10-07 10:00", "", "北區", "12", "90", "10:00-18:00"]
        );
        let later = RoomFinder::new_at(dir.path(), reference("2026-10-09T19:00:00+08:00"));
        assert_eq!(later.fields[0], "2026-10-12 10:00");
        assert!(dir.path().read_dir().unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join(prefs::FILE_NAME))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn missing_prefs_fields_default_but_explicit_empty_values_survive() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(prefs::FILE_NAME);
        std::fs::write(&path, r#"{"site":"中區"}"#).unwrap();
        let now = reference("2026-10-07T08:00:00+08:00");
        let finder = RoomFinder::new_at(dir.path(), now);
        assert_eq!(finder.fields[2], "中區");
        assert_eq!(finder.fields[3], "6");
        assert_eq!(finder.fields[4], "120");
        assert_eq!(finder.fields[5], "09:30-17:30");
        std::fs::write(
            &path,
            r#"{"site":"","capacity":null,"minutes":null,"working_hours":"10:00-18:00"}"#,
        )
        .unwrap();
        let finder = RoomFinder::new_at(dir.path(), now);
        assert_eq!(finder.fields[2], "");
        assert_eq!(finder.fields[3], "");
        assert_eq!(finder.fields[4], "");
        assert_eq!(finder.fields[5], "10:00-18:00");
    }

    #[test]
    fn corrupted_preferences_show_warning_without_overwriting_history() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(prefs::FILE_NAME);
        std::fs::write(&path, "broken JSON").unwrap();
        let finder = RoomFinder::new_at(dir.path(), reference("2026-10-07T08:00:00+08:00"));
        assert!(finder.status.contains("偏好讀取失敗"));
        assert_eq!(finder.fields[3], "6");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken JSON");
    }

    #[test]
    fn oversized_saved_duration_does_not_loop_selecting_a_default_day() {
        let dir = tempdir().unwrap();
        prefs::Preferences {
            minutes: Some(600),
            ..Default::default()
        }
        .save(dir.path())
        .unwrap();
        let now = reference("2026-10-09T12:00:00+08:00");
        let finder = RoomFinder::new_at(dir.path(), now);
        assert_eq!(finder.fields[0], "2026-10-12 09:30");
        assert_eq!(finder.fields[4], "600");
        assert!(finder.args_at(now).is_err());
    }

    #[test]
    fn validated_submit_saves_preferences_before_starting_query() {
        let dir = tempdir().unwrap();
        crate::demo::seed_without_rooms(dir.path()).unwrap();
        let mut finder = RoomFinder::new(dir.path());
        finder.fields = [
            "2026-10-12 14:00".into(),
            String::new(),
            "北區".into(),
            "8".into(),
            "60".into(),
            "10:00-18:00".into(),
        ];
        finder.submit(false);
        assert!(finder.pending.is_some());
        assert_eq!(
            prefs::Preferences::load(dir.path()).unwrap(),
            prefs::Preferences {
                site: "北區".into(),
                capacity: Some(8),
                minutes: Some(60),
                working_hours: "10:00-18:00".into()
            }
        );
        assert!(!std::fs::read_to_string(dir.path().join(prefs::FILE_NAME))
            .unwrap()
            .contains("2026"));
        finder
            .pending
            .take()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap()
            .unwrap();
        let reopened = RoomFinder::new_at(dir.path(), reference("2026-10-13T08:00:00+08:00"));
        assert_eq!(reopened.fields[0], "2026-10-13 10:00");
        assert_eq!(reopened.fields[2], "北區");
        assert_eq!(reopened.fields[3], "8");
        assert_eq!(reopened.fields[4], "60");
    }

    #[test]
    fn invalid_range_and_capacity_remain_in_form_without_starting_worker() {
        let dir = tempdir().unwrap();
        crate::demo::seed(dir.path()).unwrap();
        let mut finder = RoomFinder::new(dir.path());
        finder.fields[0] = "無法解析的日期".into();
        finder.submit(false);
        assert!(finder.pending.is_none());
        assert!(finder.status.contains("輸入錯誤"));
        assert!(!dir.path().join(prefs::FILE_NAME).exists());
        finder.fields = [
            "2026-10-07T14:00:00+08:00".into(),
            "2026-10-07T15:00:00+08:00".into(),
            "北區".into(),
            "abc".into(),
            String::new(),
            "09:30-17:30".into(),
        ];
        finder.submit(false);
        assert!(finder.pending.is_none());
        assert!(finder.status.contains("人數"));
        finder.fields[3] = "8".into();
        let args = finder.args().unwrap();
        assert_eq!(args.capacity, Some(8));
        assert_eq!(args.site.as_deref(), Some("北區"));
    }

    #[test]
    fn oversized_minutes_window_is_rejected_before_preferences_or_worker() {
        let dir = tempdir().unwrap();
        let mut finder = RoomFinder::new(dir.path());
        finder.fields[0] = "2026-10-12 09:30".into();
        finder.fields[1] = "2026-11-13 09:30".into();
        finder.submit(false);
        assert!(finder.pending.is_none());
        assert!(finder.status.contains("最多 31 天"));
        assert!(!dir.path().join(prefs::FILE_NAME).exists());
        finder.fields[1] = "2026-11-12 09:30".into();
        assert!(finder.args().is_ok());
    }

    #[test]
    fn unicode_fields_can_be_edited_and_mouse_selects_visible_fields() {
        let dir = tempdir().unwrap();
        let mut finder = RoomFinder::new(dir.path());
        finder.focus = 2;
        finder.key(KeyEvent::new(KeyCode::Char('北'), KeyModifiers::NONE));
        finder.key(KeyEvent::new(KeyCode::Char('區'), KeyModifiers::NONE));
        finder.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(finder.fields[2], "北");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| finder.draw(frame, frame.area()))
            .unwrap();
        let area = finder.field_areas[1];
        finder.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 1,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(finder.focus, 1);
    }

    #[test]
    fn minutes_form_and_all_slot_rows_share_room_query_result() {
        let dir = tempdir().unwrap();
        let mut finder = RoomFinder::new(dir.path());
        finder.fields[0] = "2026-10-12T00:00:00+08:00".into();
        finder.fields[1] = "2026-10-13T00:00:00+08:00".into();
        finder.fields[4] = "120".into();
        let args = finder.args().unwrap();
        assert_eq!(args.minutes, Some(120));
        assert_eq!(args.working_hours, "09:30-17:30");
        let room = meeting::MeetingRoom {
            name: "展示會議室".into(),
            access_name: "demo-room".into(),
            site: "北區".into(),
            capacity: Some(8),
            calendar_url: "https://mail.example.test/room/".into(),
            source: "demo".into(),
        };
        let slots = vec![
            meeting::Slot {
                start: "2026-10-12T13:00:00+08:00".into(),
                end: "2026-10-12T15:00:00+08:00".into(),
            },
            meeting::Slot {
                start: "2026-10-12T15:00:00+08:00".into(),
                end: "2026-10-12T17:00:00+08:00".into(),
            },
        ];
        let (tx, rx) = mpsc::channel();
        finder.pending = Some(rx);
        tx.send(Ok(RoomsOutput {
            start: args.start,
            end: args.end,
            rooms: vec![room.clone()],
            availability: vec![],
            unknown: vec![],
            errors: vec![],
            warnings: Vec::new(),
            minutes: Some(120),
            working_hours: args.working_hours,
            available_slots: vec![meeting::RoomAvailabilitySlots { room, slots }],
            cache_hits: 1,
            calendars: Vec::new(),
        }))
        .unwrap();
        assert!(finder.poll());
        assert!(finder.status.contains("1 間命中快取"));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal
            .draw(|frame| finder.draw(frame, frame.area()))
            .unwrap();
        assert_eq!(finder.row_count(), 2);
        let output = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(output.contains("10/12 13:00"));
        assert!(output.contains("10/12 17:00"));
        finder.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(finder.table.selected(), Some(1));
        finder.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(finder.table.selected(), Some(1));
        let area = finder.result_area;
        finder.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(finder.table.selected(), Some(0));
        finder.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: finder.field_areas[0].x,
            row: finder.field_areas[0].y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(finder.table.selected(), Some(0));
    }

    #[test]
    fn selecting_a_maximal_gap_lets_user_pick_actual_start_inside_it() {
        let dir = tempdir().unwrap();
        crate::config::save(
            dir.path(),
            &crate::config::Config::new("user@example.test".into(), "mail.example.test").unwrap(),
        )
        .unwrap();
        let mut finder = RoomFinder::new(dir.path());
        finder.fields[0] = "2026-10-07T09:30:00+08:00".into();
        finder.fields[1] = "2026-10-07T17:30:00+08:00".into();
        finder.fields[4] = "120".into();
        let room = meeting::MeetingRoom {
            name: "北區大會議室".into(),
            access_name: "demo-room".into(),
            site: "北區".into(),
            capacity: Some(8),
            calendar_url: "https://mail.example.test/room/".into(),
            source: "demo".into(),
        };
        let slots = vec![
            meeting::Slot {
                start: "2026-10-07T09:30:00+08:00".into(),
                end: "2026-10-07T12:00:00+08:00".into(),
            },
            meeting::Slot {
                start: "2026-10-07T13:00:00+08:00".into(),
                end: "2026-10-07T17:30:00+08:00".into(),
            },
        ];
        finder.result = Some(RoomsOutput {
            start: finder.fields[0].clone(),
            end: finder.fields[1].clone(),
            rooms: vec![room.clone()],
            availability: vec![],
            unknown: vec![],
            errors: vec![],
            warnings: Vec::new(),
            minutes: Some(120),
            working_hours: "09:30-17:30".into(),
            available_slots: vec![meeting::RoomAvailabilitySlots { room, slots }],
            cache_hits: 0,
            calendars: Vec::new(),
        });
        finder.focus = 6;
        finder.table.select(Some(1));
        finder.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        {
            let booking = finder.booking.as_ref().expect("opens booking pick");
            assert_eq!(booking.room_name, "北區大會議室");
            assert_eq!(booking.gap.start, "2026-10-07T13:00:00+08:00");
            assert_eq!(booking.minutes, 120);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| finder.draw(frame, frame.area()))
            .unwrap();
        assert!(finder
            .booking
            .as_ref()
            .unwrap()
            .field_areas
            .iter()
            .all(|area| area.height == 3));
        {
            let booking = finder.booking.as_mut().unwrap();
            booking.focus = 0;
            booking.fields[0] = "2026-10-07T14:00:00+08:00".into();
            booking.end_edited = false;
        }
        finder.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(finder.booking.is_none());
        assert!(finder.status.contains("2026-10-07T14:00:00+08:00"));
        assert!(finder.status.contains("2026-10-07T16:00:00+08:00"));
        let draft = finder.draft.as_ref().unwrap();
        assert!(draft.actions["check_booking"].contains("--data-dir"));
        assert!(draft.invitation.exists());
        assert!(finder.status.contains("尚未寫入行事曆"));
        assert!(finder.consume_escape());
        finder.focus = 6;
        finder.table.select(Some(0));
        finder.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(finder.booking.is_some());
        assert!(finder.consume_escape());
        assert!(finder.booking.is_none());
        assert!(finder.status.contains("已取消"));
    }
}
