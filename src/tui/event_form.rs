//! TUI 行事曆事件的新增／編輯／刪除狀態機：不碰終端或網路，寫入由 `App` 依 `Action` 執行。

use crate::{
    calendar_event::{self, EventFields, EventInput, When},
    model::Item,
};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Asia::Taipei;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub const LABELS: [&str; 6] = ["標題", "開始", "結束", "地點", "說明", "全天"];
const ALL_DAY: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Create,
    Edit,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Editing,
    /// 顯示將寫入的內容，等使用者按 y。
    Confirm,
    /// 背景寫入中，忽略按鍵。
    Writing,
    /// 遠端已接受但本機未確認：只能關閉並重新同步，不可再送一次。
    Accepted,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    Write,
}

pub struct EventForm {
    pub mode: Mode,
    /// 編輯／刪除的對象；寫入時用它的 ETag 做 If-Match。
    pub base: Option<Item>,
    pub fields: [String; 5],
    pub all_day: bool,
    initial: ([String; 5], bool),
    pub focus: usize,
    pub phase: Phase,
    pub demo: bool,
    pub notice: Option<String>,
    /// 確認後要 PUT 的 ICS；刪除時為 `None`。
    pub raw: Option<String>,
    /// 曾寫入失敗（非遠端已接受）；關閉時保留輸入，更新後再開可帶回。
    failed: bool,
    /// 起迄無法可靠解析時的原因；開始、結束、全天三欄唯讀，其他欄位照常可改。
    pub time_locked: Option<String>,
}

/// 寫入失敗後關閉表單時留下的輸入；`id` 為編輯對象，新增時為 `None`。
/// 只記使用者改過的欄位，帶回時不會蓋掉別人在伺服器上改的其他欄位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub id: Option<String>,
    fields: [Option<String>; 5],
    all_day: Option<bool>,
}

impl EventForm {
    fn new(mode: Mode, base: Option<Item>, fields: [String; 5], all_day: bool, demo: bool) -> Self {
        Self {
            mode,
            base,
            initial: (fields.clone(), all_day),
            fields,
            all_day,
            focus: 0,
            phase: if mode == Mode::Delete {
                Phase::Confirm
            } else {
                Phase::Editing
            },
            demo,
            notice: None,
            raw: None,
            failed: false,
            time_locked: None,
        }
    }

    /// 目前欄位是否因時間鎖定而唯讀。
    pub fn locked(&self, index: usize) -> bool {
        self.time_locked.is_some() && matches!(index, 1 | 2 | ALL_DAY)
    }

    /// 欄位是否和開啟時不同。
    pub fn dirty(&self) -> bool {
        self.mode != Mode::Delete
            && (self.fields != self.initial.0 || self.all_day != self.initial.1)
    }

    /// 寫入失敗過的新增／編輯輸入，關閉表單時交給 App 暫存。
    pub fn kept(&self) -> Option<Kept> {
        (self.failed && self.mode != Mode::Delete).then(|| Kept {
            id: self.base.as_ref().map(|item| item.id.clone()),
            fields: std::array::from_fn(|index| {
                (self.fields[index] != self.initial.0[index]).then(|| self.fields[index].clone())
            }),
            all_day: (self.all_day != self.initial.1).then_some(self.all_day),
        })
    }

    /// 以更新後的事件重新開啟時帶回先前輸入；比對基準仍是新版本。
    pub fn restore(&mut self, kept: Kept) {
        for (field, value) in self.fields.iter_mut().zip(kept.fields) {
            if let Some(value) = value {
                *field = value;
            }
        }
        if let Some(all_day) = kept.all_day {
            self.all_day = all_day;
        }
        self.notice = Some("已帶回上次未寫入的輸入；欄位比對以伺服器最新版本為準".into());
    }

    /// Ctrl-C 離開前保存的草稿文字。
    pub fn draft_text(&self) -> String {
        let mut text = String::new();
        if let Some(item) = &self.base {
            text.push_str(&format!("事件 ID: {}\n", item.id));
        }
        for (label, value) in LABELS.iter().zip(&self.fields) {
            text.push_str(&format!("{label}: {}\n", value.replace('\n', " ")));
        }
        text.push_str(&format!(
            "{}: {}\n",
            LABELS[ALL_DAY],
            if self.all_day { "是" } else { "否" }
        ));
        text
    }

    pub fn create(start: DateTime<Utc>, end: DateTime<Utc>, demo: bool) -> Self {
        let (start, end) = When::Timed(start.fixed_offset(), end.fixed_offset()).form_text();
        Self::new(
            Mode::Create,
            None,
            [String::new(), start, end, String::new(), String::new()],
            false,
            demo,
        )
    }

    pub fn edit(item: Item, fields: EventFields, demo: bool) -> Self {
        let (start, end) = fields.when.map(|when| when.form_text()).unwrap_or_default();
        let all_day = fields.when.is_some_and(|when| when.is_all_day());
        let time_locked = fields.time_locked;
        let mut form = Self::new(
            Mode::Edit,
            Some(item),
            [
                fields.title,
                start,
                end,
                fields.location,
                fields.description,
            ],
            all_day,
            demo,
        );
        form.time_locked = time_locked;
        form
    }

    pub fn delete(item: Item, demo: bool) -> Self {
        Self::new(Mode::Delete, Some(item), Default::default(), false, demo)
    }

    /// 建立時送出全部欄位；編輯時只送有改動的欄位。
    pub fn input(&self) -> EventInput {
        let edit = self.mode == Mode::Edit;
        let field = |index: usize| {
            let value = self.fields[index].clone();
            (!edit || value != self.initial.0[index]).then_some(value)
        };
        let times_changed = !edit
            || self.all_day != self.initial.1
            || self.fields[1] != self.initial.0[1]
            || self.fields[2] != self.initial.0[2];
        EventInput {
            title: field(0),
            // 改了起迄任一或全天時，兩者一起送出，讓畫面上的值就是寫入的值。
            start: times_changed.then(|| self.fields[1].clone()),
            end: times_changed.then(|| self.fields[2].clone()),
            location: field(3),
            description: field(4),
            all_day: (!edit || self.all_day != self.initial.1).then_some(self.all_day),
        }
    }

    /// 產生要寫入的 ICS；不能寫入時回傳原因。
    pub fn prepare(&mut self, now: DateTime<Utc>) -> Result<(), String> {
        let raw = match self.mode {
            Mode::Delete => return Ok(()),
            Mode::Create => calendar_event::build_new(&self.input(), now),
            Mode::Edit => {
                let base = self.base.as_ref().expect("edit has a base item");
                calendar_event::apply_update(&base.raw, &self.input(), now)
            }
        }
        .map_err(|error| format!("{error:#}"))?;
        self.raw = Some(raw);
        Ok(())
    }

    pub fn key(&mut self, key: KeyEvent, now: DateTime<Utc>) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match self.phase {
            Phase::Writing => Action::None,
            Phase::Accepted => match key.code {
                KeyCode::Esc | KeyCode::Enter => Action::Close,
                _ => Action::None,
            },
            Phase::Editing => {
                match key.code {
                    KeyCode::Esc => return Action::Close,
                    KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % LABELS.len(),
                    KeyCode::BackTab | KeyCode::Up => {
                        self.focus = (self.focus + LABELS.len() - 1) % LABELS.len()
                    }
                    KeyCode::Enter => match self.prepare(now) {
                        Ok(()) => {
                            self.phase = Phase::Confirm;
                            self.notice = None;
                        }
                        Err(reason) => self.notice = Some(reason),
                    },
                    _ if self.locked(self.focus)
                        && matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace) =>
                    {
                        self.notice = self
                            .time_locked
                            .as_ref()
                            .map(|reason| format!("時間欄位唯讀：{reason}"));
                    }
                    KeyCode::Char(' ') if self.focus == ALL_DAY => self.toggle_all_day(),
                    KeyCode::Backspace if self.focus < ALL_DAY => {
                        self.fields[self.focus].pop();
                    }
                    KeyCode::Char(c) if !ctrl && self.focus < ALL_DAY => {
                        self.fields[self.focus].push(c)
                    }
                    _ => {}
                }
                Action::None
            }
            Phase::Confirm => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if self.demo {
                        self.notice = Some("展示資料不會寫入行事曆".into());
                        return Action::None;
                    }
                    self.phase = Phase::Writing;
                    self.notice = None;
                    Action::Write
                }
                KeyCode::Char('e') | KeyCode::Char('E') if self.mode != Mode::Delete => {
                    self.phase = Phase::Editing;
                    Action::None
                }
                KeyCode::Esc if self.mode == Mode::Delete => Action::Close,
                KeyCode::Esc => {
                    self.phase = Phase::Editing;
                    Action::None
                }
                _ => Action::None,
            },
        }
    }

    /// 切換全天時改寫起迄文字：全天只留日期，定時補上 09:00／10:00。
    fn toggle_all_day(&mut self) {
        self.all_day = !self.all_day;
        let date = |value: &str| {
            value
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned()
        };
        let start = date(&self.fields[1]);
        if self.all_day {
            let end = date(&self.fields[2]);
            self.fields[2] = if end.is_empty() { start.clone() } else { end };
            self.fields[1] = start;
        } else {
            self.fields[2] = format!("{start} 10:00");
            self.fields[1] = format!("{start} 09:00");
        }
    }

    /// 寫入失敗：輸入保留，不自動重試。遠端已接受時鎖住流程，避免重複建立。
    pub fn write_failed(&mut self, reason: &str, remote_accepted: bool) {
        if remote_accepted {
            self.phase = Phase::Accepted;
            self.notice = Some(format!(
                "{reason}\n遠端已接受寫入；請 Esc 後按 r 更新確認，不要重新建立或再送一次"
            ));
            return;
        }
        self.phase = Phase::Confirm;
        self.failed = true;
        let hint = if reason.contains("412") {
            "；伺服器上的事件已被修改（ETag 不符）。按 Esc 兩次關閉、按 r 更新，再按 e（新增則按 n）會帶回這次的輸入"
        } else {
            ""
        };
        self.notice = Some(format!(
            "寫入失敗：{reason}{hint}（不會自動重試；y 再試、Esc 返回）"
        ));
    }
}

/// 預填新增事件的時段：在 `day` 09:00–18:00 找第一個與現有活動不重疊的整點一小時；
/// 今天則從下一個整點開始。都滿了就回到 09:00。
pub fn free_slot(
    day: NaiveDate,
    busy: &[(DateTime<Utc>, DateTime<Utc>)],
    now: DateTime<Utc>,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let at = |hour: u32| {
        Taipei
            .from_local_datetime(&day.and_hms_opt(hour, 0, 0).expect("valid hour"))
            .single()
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or(now)
    };
    let local_now = now.with_timezone(&Taipei);
    let first = if local_now.date_naive() == day {
        (local_now.hour() + 1).max(9)
    } else {
        9
    };
    (first..18)
        .map(|hour| (at(hour), at(hour) + Duration::hours(1)))
        .find(|(start, end)| !busy.iter().any(|(b0, b1)| b0 < end && b1 > start))
        .unwrap_or_else(|| (at(9), at(10)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-21T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn press(form: &mut EventForm, code: KeyCode) -> Action {
        form.key(KeyEvent::new(code, KeyModifiers::NONE), now())
    }

    fn typed(form: &mut EventForm, text: &str) {
        for c in text.chars() {
            press(form, KeyCode::Char(c));
        }
    }

    fn clear(form: &mut EventForm) {
        for _ in 0..40 {
            press(form, KeyCode::Backspace);
        }
    }

    fn item() -> Item {
        Item {
            id: "e1".into(),
            kind: Kind::Calendar,
            collection: "demo-calendar".into(),
            remote_id: "e1.ics".into(),
            etag: Some("\"v1\"".into()),
            uid_validity: None,
            title: "週會".into(),
            subtitle: String::new(),
            date: None,
            end_date: None,
            text: String::new(),
            raw: "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20261022T060000Z\r\nDTEND:20261022T070000Z\r\nSUMMARY:週會\r\nLOCATION:A 室\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n".into(),
            flags: vec![],
            emails: vec![],
            phones: vec![],
            attachments: vec![],
        }
    }

    fn edit_form(demo: bool) -> EventForm {
        let item = item();
        let fields = calendar_event::fields(&item.raw).unwrap();
        EventForm::edit(item, fields, demo)
    }

    #[test]
    fn create_prefills_free_slot_and_requires_title_before_confirm() {
        let busy = [(
            DateTime::parse_from_rfc3339("2026-10-22T09:00:00+08:00")
                .unwrap()
                .into(),
            DateTime::parse_from_rfc3339("2026-10-22T10:30:00+08:00")
                .unwrap()
                .into(),
        )];
        let day = NaiveDate::from_ymd_opt(2026, 10, 22).unwrap();
        let (start, end) = free_slot(day, &busy, now());
        let mut form = EventForm::create(start, end, false);
        assert_eq!(
            form.fields[1], "2026-10-22 11:00",
            "first hour not overlapping 09:00–10:30"
        );
        assert_eq!(form.fields[2], "2026-10-22 12:00");
        assert_eq!(press(&mut form, KeyCode::Enter), Action::None);
        assert_eq!(form.phase, Phase::Editing);
        assert!(form.notice.as_deref().unwrap().contains("標題"));
        typed(&mut form, "讀書會");
        press(&mut form, KeyCode::Enter);
        assert_eq!(form.phase, Phase::Confirm);
        let raw = form.raw.clone().unwrap();
        assert!(raw.contains("SUMMARY:讀書會") && raw.contains("DTSTART:20261022T030000Z"));
    }

    #[test]
    fn today_slot_starts_after_now_and_full_day_falls_back_to_nine() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 21).unwrap();
        let (start, _) = free_slot(today, &[], now());
        assert_eq!(start.with_timezone(&Taipei).hour(), 11, "10:00 now → 11:00");
        let day = NaiveDate::from_ymd_opt(2026, 10, 22).unwrap();
        let all = [(
            DateTime::parse_from_rfc3339("2026-10-22T08:00:00+08:00")
                .unwrap()
                .into(),
            DateTime::parse_from_rfc3339("2026-10-22T19:00:00+08:00")
                .unwrap()
                .into(),
        )];
        assert_eq!(
            free_slot(day, &all, now()).0.with_timezone(&Taipei).hour(),
            9
        );
    }

    #[test]
    fn only_y_in_confirm_writes_and_writing_ignores_keys() {
        let mut form = edit_form(false);
        form.focus = 3;
        clear(&mut form);
        typed(&mut form, "B 室");
        assert_eq!(press(&mut form, KeyCode::Enter), Action::None);
        for code in [KeyCode::Enter, KeyCode::Char('n'), KeyCode::Char(' ')] {
            assert_eq!(press(&mut form, code), Action::None);
            assert_eq!(form.phase, Phase::Confirm);
        }
        assert_eq!(press(&mut form, KeyCode::Char('y')), Action::Write);
        assert_eq!(form.phase, Phase::Writing);
        assert_eq!(press(&mut form, KeyCode::Esc), Action::None);
        assert_eq!(press(&mut form, KeyCode::Char('y')), Action::None);
    }

    #[test]
    fn edit_sends_only_changed_fields_and_keeps_uid() {
        let mut form = edit_form(false);
        assert_eq!(form.fields[1], "2026-10-22 14:00");
        form.focus = 3;
        clear(&mut form);
        typed(&mut form, "B 室");
        let input = form.input();
        assert_eq!(input.location.as_deref(), Some("B 室"));
        assert!(input.title.is_none() && input.start.is_none() && input.all_day.is_none());
        form.prepare(now()).unwrap();
        let raw = form.raw.unwrap();
        assert!(raw.contains("UID:u1\r\n") && raw.contains("LOCATION:B 室\r\n"));
        assert!(raw.contains("DTSTART:20261022T060000Z\r\n"));
        let mut unchanged = edit_form(false);
        press(&mut unchanged, KeyCode::Enter);
        assert_eq!(unchanged.phase, Phase::Editing);
        assert!(unchanged.notice.unwrap().contains("沒有指定"));
    }

    #[test]
    fn all_day_toggle_rewrites_times_and_round_trips() {
        let mut form = edit_form(false);
        form.focus = ALL_DAY;
        press(&mut form, KeyCode::Char(' '));
        assert!(form.all_day);
        assert_eq!(
            (form.fields[1].as_str(), form.fields[2].as_str()),
            ("2026-10-22", "2026-10-22")
        );
        form.prepare(now()).unwrap();
        assert!(form
            .raw
            .as_ref()
            .unwrap()
            .contains("DTSTART;VALUE=DATE:20261022\r\nDTEND;VALUE=DATE:20261023"));
        press(&mut form, KeyCode::Char(' '));
        assert_eq!(form.fields[1], "2026-10-22 09:00");
        press(&mut form, KeyCode::Char('x'));
        assert_eq!(
            form.fields[1], "2026-10-22 09:00",
            "typing on the toggle row is ignored"
        );
    }

    #[test]
    fn demo_refuses_write_but_keeps_form() {
        let mut form = EventForm::delete(item(), true);
        assert_eq!(form.phase, Phase::Confirm);
        assert_eq!(press(&mut form, KeyCode::Char('y')), Action::None);
        assert_eq!(form.phase, Phase::Confirm);
        assert!(form.notice.as_deref().unwrap().contains("展示資料"));
        assert_eq!(
            press(&mut form, KeyCode::Char('e')),
            Action::None,
            "delete has nothing to edit"
        );
        assert_eq!(press(&mut form, KeyCode::Esc), Action::Close);
    }

    #[test]
    fn failure_keeps_input_and_needs_new_y_while_accepted_locks_resend() {
        let mut form = edit_form(false);
        form.fields[0] = "改名".into();
        press(&mut form, KeyCode::Enter);
        assert_eq!(press(&mut form, KeyCode::Char('y')), Action::Write);
        form.write_failed("DAV PUT 回應狀態錯誤：412 Precondition Failed", false);
        assert_eq!(form.phase, Phase::Confirm);
        assert_eq!(form.fields[0], "改名");
        let notice = form.notice.clone().unwrap();
        assert!(
            notice.contains("412") && notice.contains("ETag") && notice.contains("不會自動重試")
        );
        assert_eq!(press(&mut form, KeyCode::Esc), Action::None);
        assert_eq!(
            form.phase,
            Phase::Editing,
            "Esc goes back to the kept input"
        );
        press(&mut form, KeyCode::Enter);
        assert_eq!(press(&mut form, KeyCode::Char('y')), Action::Write);
        form.write_failed("DAV 遠端寫入已接受，但本機 reconciliation 未完成", true);
        assert_eq!(form.phase, Phase::Accepted);
        assert_eq!(press(&mut form, KeyCode::Char('y')), Action::None);
        assert_eq!(press(&mut form, KeyCode::Esc), Action::Close);
    }

    #[test]
    fn failed_edit_is_kept_on_close_and_restored_over_a_newer_base() {
        let mut form = edit_form(false);
        assert!(!form.dirty());
        form.fields[0] = "改名".into();
        assert!(form.dirty());
        assert!(form.kept().is_none(), "nothing to keep before a failure");
        press(&mut form, KeyCode::Enter);
        press(&mut form, KeyCode::Char('y'));
        form.write_failed("DAV PUT 回應狀態錯誤：412 Precondition Failed", false);
        assert!(form.notice.as_deref().unwrap().contains("再按 e"));
        let kept = form.kept().expect("kept after failure");
        assert_eq!(kept.id.as_deref(), Some("e1"));
        let mut newer = item();
        newer.raw = newer.raw.replace("LOCATION:A 室", "LOCATION:別人改的");
        let fields = calendar_event::fields(&newer.raw).unwrap();
        let mut reopened = EventForm::edit(newer, fields, false);
        reopened.restore(kept);
        assert_eq!(reopened.fields[0], "改名");
        let input = reopened.input();
        assert_eq!(input.title.as_deref(), Some("改名"));
        assert_eq!(
            reopened.fields[3], "別人改的",
            "untouched fields keep the newer server value"
        );
        assert!(input.location.is_none() && input.start.is_none());
        assert!(reopened.draft_text().contains("標題: 改名"));
        let mut deleting = EventForm::delete(item(), false);
        deleting.write_failed("412", false);
        assert!(deleting.kept().is_none() && !deleting.dirty());
    }

    #[test]
    fn unparseable_duration_locks_only_the_time_fields() {
        let mut item = item();
        item.raw = item
            .raw
            .replace("DTEND:20261022T070000Z\r\n", "DURATION:P99999999W\r\n");
        let fields = calendar_event::fields(&item.raw).unwrap();
        let mut form = EventForm::edit(item, fields, false);
        assert!(form.time_locked.is_some());
        for focus in [1, 2, ALL_DAY] {
            form.focus = focus;
            press(&mut form, KeyCode::Char(' '));
            press(&mut form, KeyCode::Char('1'));
            press(&mut form, KeyCode::Backspace);
        }
        assert_eq!(
            (
                form.fields[1].as_str(),
                form.fields[2].as_str(),
                form.all_day
            ),
            ("", "", false)
        );
        assert!(form.notice.as_deref().unwrap().contains("唯讀"));
        form.focus = 0;
        typed(&mut form, "改");
        assert_eq!(press(&mut form, KeyCode::Enter), Action::None);
        assert_eq!(form.phase, Phase::Confirm, "{:?}", form.notice);
        let raw = form.raw.unwrap();
        assert!(raw.contains("SUMMARY:週會改\r\n") && raw.contains("DURATION:P99999999W\r\n"));
    }
}
