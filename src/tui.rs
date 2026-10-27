use crate::{
    config::{self, Config},
    directory::{self, DirectoryScope, DirectorySyncResult, SyncArgs},
    model::{Item, Kind, SyncResult},
    provider::mail,
    store::Store,
    sync,
};
use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{CrosstermBackend, TestBackend},
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Wrap,
    },
    Frame, Terminal,
};
use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

mod calendar;
mod event_form;
#[cfg(test)]
#[path = "../examples/m2k_fixture/fixture.rs"]
#[allow(dead_code)]
mod fixture;
mod reply;
mod rooms;

const KINDS: [Kind; 3] = [Kind::Mail, Kind::Calendar, Kind::Contacts];
const NAMES: [&str; 4] = ["郵件", "行事曆", "聯絡人", "找會議室"];
const ACCENT: Color = Color::Rgb(94, 210, 220);
const MUTED: Color = Color::Rgb(160, 172, 185);
const SELECTED: Color = Color::Rgb(38, 65, 85);
const PAGE_SIZE: usize = 100;
const REMOTE_LIMIT: usize = 500;
const SEARCH_DELAY: Duration = Duration::from_millis(400);

#[derive(Default)]
struct HitAreas {
    tabs: Vec<(Rect, usize)>,
    search: Rect,
    folders: Rect,
    list: Rect,
    previous: Rect,
    next: Rect,
    detail: Rect,
    calendar_events: Vec<(Rect, usize)>,
}

struct SearchJob {
    generation: u64,
    receiver: Receiver<Result<mail::SearchResults>>,
}
struct BodyJob {
    generation: u64,
    id: String,
    receiver: Receiver<Result<Item>>,
}

struct App {
    dir: PathBuf,
    config: Option<Config>,
    tab: usize,
    rows: Vec<Item>,
    collections: Vec<(String, String)>,
    collection: usize,
    folders: ListState,
    table: TableState,
    page: usize,
    total: usize,
    query: String,
    searching: bool,
    detail: bool,
    scroll: u16,
    help: bool,
    status: String,
    counts: serde_json::Value,
    sync_state: serde_json::Value,
    progress: Option<crate::sync_progress::Progress>,
    progress_due: Instant,
    pending: Option<Receiver<Result<Vec<SyncResult>>>>,
    pending_directory: Option<Receiver<Result<DirectorySyncResult>>>,
    remote: Vec<Item>,
    remote_truncated: bool,
    search_generation: u64,
    search_due: Option<Instant>,
    local_due: Option<Instant>,
    pending_search: Option<SearchJob>,
    body_generation: u64,
    pending_body: Option<BodyJob>,
    hits: HitAreas,
    room_finder: rooms::RoomFinder,
    calendar: calendar::Calendar,
    reply: Option<reply::ReplyFlow>,
    reply_editor: Option<String>,
    editor_request: Option<PathBuf>,
    pending_send: Option<Receiver<Result<String>>>,
    event_form: Option<event_form::EventForm>,
    pending_event: Option<Receiver<Result<Option<Item>>>>,
    kept_event: Option<event_form::Kept>,
}

impl App {
    fn new(dir: &Path) -> Result<Self> {
        let cfg = if dir.join("config.json").exists() {
            Some(config::load(dir)?)
        } else {
            None
        };
        let mut app = Self {
            dir: dir.to_owned(),
            config: cfg,
            tab: 0,
            rows: vec![],
            collections: vec![],
            collection: 0,
            folders: ListState::default(),
            table: TableState::default(),
            page: 0,
            total: 0,
            query: String::new(),
            searching: false,
            detail: false,
            scroll: 0,
            help: false,
            status: "離線可瀏覽 · r 同步".into(),
            counts: serde_json::Value::Null,
            sync_state: serde_json::Value::Null,
            progress: crate::sync_progress::read(dir).ok().flatten(),
            progress_due: Instant::now(),
            pending: None,
            pending_directory: None,
            remote: vec![],
            remote_truncated: false,
            search_generation: 0,
            search_due: None,
            local_due: None,
            pending_search: None,
            body_generation: 0,
            pending_body: None,
            hits: HitAreas::default(),
            room_finder: rooms::RoomFinder::new(dir),
            calendar: calendar::Calendar::new(),
            reply: None,
            reply_editor: None,
            editor_request: None,
            pending_send: None,
            event_form: None,
            pending_event: None,
            kept_event: None,
        };
        app.reload()?;
        Ok(app)
    }

    fn reload(&mut self) -> Result<()> {
        if self.tab == 3 {
            return Ok(());
        }
        let store = Store::open_reader(&self.dir)?;
        if let Some(cfg) = &self.config {
            store.verify_account(cfg)?;
        }
        let kind = KINDS[self.tab];
        self.counts = store.counts()?;
        self.sync_state = store.sync_status()?;
        let mut collections = vec![(String::new(), "全部".into())];
        collections.extend(store.collections(kind)?.into_iter().map(|c| {
            let name = if kind == Kind::Mail {
                mail::display_collection(&c.name)
            } else {
                c.name
            };
            (c.id, name)
        }));
        self.collections = collections;
        self.collection = self
            .collection
            .min(self.collections.len().saturating_sub(1));
        self.folders.select(Some(self.collection));
        let selected = self
            .rows
            .get(self.table.selected().unwrap_or(0))
            .map(|r| r.id.clone());
        let loaded = if self.detail || kind == Kind::Calendar {
            self.rows
                .get_mut(self.table.selected().unwrap_or(0))
                .map(|row| {
                    let text = std::mem::take(&mut row.text);
                    let raw = std::mem::take(&mut row.raw);
                    let mut loaded = row.clone();
                    loaded.text = text;
                    loaded.raw = raw;
                    loaded
                })
        } else {
            None
        };
        let collection = self.selected_collection().map(str::to_owned);
        let query = Some(self.query.as_str()).filter(|value| !value.trim().is_empty());
        let mut page = if kind == Kind::Contacts {
            directory::contact_summaries_page(
                &store,
                query,
                collection.as_deref(),
                PAGE_SIZE,
                self.page * PAGE_SIZE,
            )?
        } else if kind == Kind::Calendar {
            let mut page = store.hybrid_summaries_page(
                kind,
                query,
                collection.as_deref(),
                &[],
                PAGE_SIZE,
                0,
            )?;
            if page.total > page.items.len() {
                page = store.hybrid_summaries_page(
                    kind,
                    query,
                    collection.as_deref(),
                    &[],
                    page.total,
                    0,
                )?;
            }
            page.items.retain(|item| self.calendar.contains(item));
            page.items.sort_by_key(|item| {
                (
                    calendar::Calendar::interval(item).map(|(start, _)| start),
                    item.id.clone(),
                )
            });
            page.total = page.items.len();
            self.page = 0;
            page
        } else {
            store.hybrid_summaries_page(
                kind,
                Some(&self.query),
                collection.as_deref(),
                &self.remote,
                PAGE_SIZE,
                self.page * PAGE_SIZE,
            )?
        };
        self.total = page.total;
        let last = self.total.saturating_sub(1) / PAGE_SIZE;
        if self.page > last {
            self.page = last;
            page = if kind == Kind::Contacts {
                directory::contact_summaries_page(
                    &store,
                    query,
                    collection.as_deref(),
                    PAGE_SIZE,
                    self.page * PAGE_SIZE,
                )?
            } else {
                store.hybrid_summaries_page(
                    kind,
                    Some(&self.query),
                    collection.as_deref(),
                    &self.remote,
                    PAGE_SIZE,
                    self.page * PAGE_SIZE,
                )?
            };
        }
        self.rows = page.items;
        if kind == Kind::Mail {
            for item in &mut self.rows {
                item.title = mail::display_header(&item.title);
                item.subtitle = mail::display_header(&item.subtitle);
            }
        }
        let index = selected
            .as_ref()
            .and_then(|id| self.rows.iter().position(|r| &r.id == id))
            .unwrap_or(self.table.selected().unwrap_or(0))
            .min(self.rows.len().saturating_sub(1));
        self.table.select(if self.rows.is_empty() {
            None
        } else {
            Some(index)
        });
        if self.detail || kind == Kind::Calendar {
            if let Some(index) = selected
                .as_ref()
                .and_then(|id| self.rows.iter().position(|r| &r.id == id))
            {
                if let Some(item) = loaded.filter(|item| {
                    item.id == self.rows[index].id
                        && item.etag == self.rows[index].etag
                        && !item.raw.is_empty()
                }) {
                    let summary = &mut self.rows[index];
                    summary.title = item.title;
                    summary.subtitle = item.subtitle;
                    summary.text = item.text;
                    summary.raw = item.raw;
                    summary.emails = item.emails;
                    summary.phones = item.phones;
                    summary.attachments = item.attachments;
                } else if let Ok(mut item) = store.get(&self.rows[index].id) {
                    if let Err(error) = normalize_detail(&mut item) {
                        item.raw.clear();
                        self.status = format!("快取正文損毀：{error:#} · Enter 重新載入");
                    }
                    self.rows[index] = item;
                }
            } else {
                self.detail = false;
            }
        }
        if kind == Kind::Calendar {
            self.load_selected_item()?;
        }
        Ok(())
    }

    fn selected_collection(&self) -> Option<&str> {
        (self.collection != 0).then(|| self.collections[self.collection].0.as_str())
    }

    fn invalidate_search(&mut self) {
        self.search_generation = self.search_generation.wrapping_add(1);
        self.remote.clear();
        self.remote_truncated = false;
        self.search_due =
            (!self.query.trim().is_empty() && self.tab == 0).then(|| Instant::now() + SEARCH_DELAY);
        self.local_due = None;
        self.page = 0;
        self.table = TableState::default();
        self.close_detail();
    }

    fn close_detail(&mut self) {
        self.detail = false;
        self.scroll = 0;
        self.body_generation = self.body_generation.wrapping_add(1);
        for row in &mut self.rows {
            if row.kind == Kind::Mail {
                row.text = String::new();
                row.raw = String::new();
            }
        }
    }

    fn load_selected_item(&mut self) -> Result<()> {
        let Some(index) = self.table.selected() else {
            return Ok(());
        };
        let Some(summary) = self.rows.get(index) else {
            return Ok(());
        };
        if summary.kind == Kind::Contacts {
            return Ok(());
        }
        if !summary.raw.is_empty() || !summary.text.trim().is_empty() {
            return Ok(());
        }
        let mut item = summary.clone();
        let mut cache_error = None;
        let store = Store::open_reader(&self.dir)?;
        if let Some(cfg) = &self.config {
            store.verify_account(cfg)?;
        }
        if let Ok(mut cached) = store.get(&item.id) {
            if let Err(error) = normalize_detail(&mut cached) {
                cache_error = Some(format!("快取正文損毀：{error:#}"));
                cached.raw.clear();
                self.rows[index] = cached.clone();
            }
            if cached.kind != Kind::Mail || !cached.raw.is_empty() || !cached.text.trim().is_empty()
            {
                self.rows[index] = cached;
                return Ok(());
            }
            item = cached;
        }
        if item.kind == Kind::Calendar {
            return Ok(());
        }
        if let Some(job) = &mut self.pending_body {
            if job.id == item.id {
                job.generation = self.body_generation;
            }
            self.status = "郵件正文仍在載入；完成後會載入目前選取的郵件".into();
            return Ok(());
        }
        let Some(cfg) = self
            .config
            .clone()
            .filter(|_| !self.dir.join(".demo").exists())
        else {
            self.status = cache_error
                .map(|error| format!("{error} · 離線，請連線後重新同步"))
                .unwrap_or_else(|| "離線：此郵件只有伺服器摘要，尚無本機正文".into());
            return Ok(());
        };
        let id = item.id.clone();
        let dir = self.dir.clone();
        let generation = self.body_generation;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = online_mail(&dir, &cfg, |password| {
                mail::fetch_item(&cfg, password, &item)
            });
            let _ = tx.send(result);
        });
        self.pending_body = Some(BodyJob {
            generation,
            id,
            receiver: rx,
        });
        self.status = cache_error
            .map(|error| format!("{error} · 正在重新下載正文..."))
            .unwrap_or_else(|| "正在載入郵件正文...".into());
        Ok(())
    }

    fn open_detail(&mut self) -> Result<()> {
        if self.rows.is_empty() || (self.tab == 1 && self.table.selected().is_none()) {
            return Ok(());
        }
        self.body_generation = self.body_generation.wrapping_add(1);
        let selected = self.table.selected();
        for (index, row) in self.rows.iter_mut().enumerate() {
            if Some(index) != selected && row.kind != Kind::Contacts {
                row.text = String::new();
                row.raw = String::new();
            }
        }
        self.detail = true;
        self.searching = false;
        self.scroll = 0;
        self.load_selected_item()
    }

    fn start_reply(&mut self, editor: Option<String>) {
        let selected = self
            .rows
            .get(self.table.selected().unwrap_or(0))
            .filter(|row| self.detail && row.kind == Kind::Mail && !row.raw.is_empty());
        let Some(item) = selected else {
            self.status = "先按 Enter 開啟郵件、等正文載入後再按 R 回覆".into();
            return;
        };
        let text = match crate::mail_reply::compose_draft(item) {
            Ok(text) => text,
            Err(error) => {
                self.status = format!("無法建立回覆草稿：{error:#}");
                return;
            }
        };
        let editor = editor.filter(|value| !value.trim().is_empty());
        let mut flow = reply::ReplyFlow::new(
            item.id.clone(),
            text,
            editor.is_some(),
            self.dir.join(".demo").exists(),
        );
        if editor.is_some() {
            match crate::mail_reply::save_draft_file(&self.dir, None, &flow.text) {
                Ok(path) => {
                    self.editor_request = Some(path.clone());
                    flow.draft_path = Some(path);
                }
                Err(error) => {
                    self.status = format!("無法建立草稿檔：{error:#}");
                    return;
                }
            }
        }
        self.reply_editor = editor;
        self.reply = Some(flow);
    }

    /// 保存目前草稿；失敗時留在流程內顯示原因，不丟文字。
    fn save_reply(&mut self) -> bool {
        let Some(flow) = self.reply.as_mut() else {
            return false;
        };
        match crate::mail_reply::save_draft_file(&self.dir, flow.draft_path.as_deref(), &flow.text)
        {
            Ok(path) => {
                flow.draft_path = Some(path);
                true
            }
            Err(error) => {
                flow.notice = Some(format!("草稿保存失敗：{error:#}"));
                false
            }
        }
    }

    fn autosave_reply(&mut self) {
        if self
            .reply
            .as_ref()
            .is_some_and(|flow| !flow.text.trim().is_empty())
        {
            self.save_reply();
        }
    }

    fn reply_key(&mut self, key: KeyEvent) {
        let Some(flow) = self.reply.as_mut() else {
            return;
        };
        match flow.key(key) {
            reply::Action::None => {}
            reply::Action::Edit => self.editor_request = flow.draft_path.clone(),
            reply::Action::Save { close } => {
                if self.save_reply() && close {
                    let path = self.reply.take().and_then(|flow| flow.draft_path);
                    self.status = path
                        .map(|path| format!("草稿已保存：{}", path.display()))
                        .unwrap_or_default();
                }
            }
            reply::Action::Send => {
                if !self.save_reply() {
                    if let Some(flow) = self.reply.as_mut() {
                        flow.phase = reply::Phase::Confirm;
                    }
                    return;
                }
                let flow = self.reply.as_mut().expect("reply flow");
                let Ok((headers, body)) = flow.check() else {
                    flow.phase = reply::Phase::Confirm;
                    return;
                };
                let (dir, id) = (self.dir.clone(), flow.mail_id.clone());
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let result = crate::mail_reply::send_reply(
                        &dir,
                        &id,
                        || Ok(body),
                        vec![],
                        Some(headers),
                    )
                    .map_err(|error| crate::mail_reply::masked_error(&dir, error));
                    let _ = tx.send(result);
                });
                self.pending_send = Some(rx);
                self.status = "正在寄出回覆...".into();
            }
        }
    }

    fn poll_send(&mut self) -> bool {
        let Some(result) = self.pending_send.as_ref().and_then(receive) else {
            return false;
        };
        self.pending_send = None;
        match result {
            Ok(message_id) => {
                if let Some(path) = self.reply.take().and_then(|flow| flow.draft_path) {
                    let _ = std::fs::remove_file(path);
                }
                self.status = format!("已寄出回覆 {message_id}");
            }
            Err(error) => {
                let reason = format!("{error:#}");
                if let Some(flow) = self.reply.as_mut() {
                    flow.send_failed(&reason);
                }
                self.status = "回覆寄送失敗，草稿保留".into();
            }
        }
        true
    }

    /// 外部編輯器結束後讀回草稿；失敗時關閉流程，草稿檔保留。
    fn editor_finished(&mut self, result: io::Result<std::process::ExitStatus>) {
        let Some(flow) = self.reply.as_mut() else {
            return;
        };
        let kept = flow
            .draft_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let failure = match result {
            Ok(code) if code.success() => match flow
                .draft_path
                .as_ref()
                .map(std::fs::read_to_string)
                .transpose()
            {
                Ok(Some(text)) => {
                    flow.editor_finished(text);
                    return;
                }
                Ok(None) => "沒有草稿檔".to_string(),
                Err(error) => format!("無法讀回草稿：{error}"),
            },
            Ok(code) => format!("編輯器異常結束（{code}）"),
            Err(error) => format!("無法啟動 $EDITOR：{error}"),
        };
        self.reply = None;
        self.status = format!("{failure}；草稿保留於 {kept}");
    }

    fn demo(&self) -> bool {
        self.dir.join(".demo").exists()
    }

    /// 新增事件：預填選取活動當天（或視窗內今天／第一天）的第一個空白整點。
    fn start_event_create(&mut self) {
        let today = Utc::now()
            .with_timezone(&chrono_tz::Asia::Taipei)
            .date_naive();
        let days = self.calendar.days();
        let day = self
            .table
            .selected()
            .and_then(|index| self.rows.get(index))
            .and_then(calendar::Calendar::interval)
            .map(|(start, _)| start.date_naive())
            .or_else(|| days.contains(&today).then_some(today))
            .or_else(|| days.first().copied())
            .unwrap_or(today);
        let busy = self
            .rows
            .iter()
            .filter_map(calendar::Calendar::interval)
            .map(|(start, end)| (start.with_timezone(&Utc), end.with_timezone(&Utc)))
            .collect::<Vec<_>>();
        let (start, end) = event_form::free_slot(day, &busy, Utc::now());
        let mut form = event_form::EventForm::create(start, end, self.demo());
        if let Some(kept) = self.kept_event.take_if(|kept| kept.id.is_none()) {
            form.restore(kept);
        }
        self.event_form = Some(form);
    }

    /// 編輯或刪除選取事件；不能修改的事件只在狀態列說明原因。
    fn start_event(&mut self, mode: event_form::Mode) {
        let Some(item) = self
            .table
            .selected()
            .and_then(|index| self.rows.get(index))
            .filter(|item| item.kind == Kind::Calendar)
            .cloned()
        else {
            self.status = "先用 j/k 或滑鼠選取一個活動".into();
            return;
        };
        let Some(cfg) = &self.config else {
            self.status = "尚未設定帳號 · 請執行 m2k init --username 你的帳號".into();
            return;
        };
        if let Err(reason) = crate::calendar_event::check_editable(cfg, &item) {
            self.status = format!(
                "無法{}：{reason}",
                if mode == event_form::Mode::Delete {
                    "刪除"
                } else {
                    "編輯"
                }
            );
            return;
        }
        let demo = self.demo();
        let id = item.id.clone();
        let mut form = match mode {
            event_form::Mode::Delete => event_form::EventForm::delete(item, demo),
            _ => match crate::calendar_event::fields(&item.raw) {
                Ok(fields) => event_form::EventForm::edit(item, fields, demo),
                Err(error) => {
                    self.status = format!("無法編輯：{error:#}");
                    return;
                }
            },
        };
        if mode == event_form::Mode::Edit {
            if let Some(kept) = self
                .kept_event
                .take_if(|kept| kept.id.as_deref() == Some(id.as_str()))
            {
                form.restore(kept);
            }
        }
        self.event_form = Some(form);
    }

    fn event_key(&mut self, key: KeyEvent) {
        let Some(form) = self.event_form.as_mut() else {
            return;
        };
        match form.key(key, Utc::now()) {
            event_form::Action::None => {}
            event_form::Action::Close => {
                if let Some(kept) = self.event_form.take().and_then(|form| form.kept()) {
                    self.status = if kept.id.is_some() {
                        "輸入已保留 · 按 r 更新後再按 e 帶回".into()
                    } else {
                        "輸入已保留 · 再按 n 帶回".into()
                    };
                    self.kept_event = Some(kept);
                }
            }
            event_form::Action::Write => {
                let dir = self.dir.clone();
                let (mode, base, raw) = (form.mode, form.base.clone(), form.raw.clone());
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let result = match (mode, base, raw) {
                        (event_form::Mode::Delete, Some(base), _) => {
                            crate::dav_write::delete(&dir, || Ok(base)).map(|_| None)
                        }
                        (event_form::Mode::Edit, Some(base), Some(raw)) => {
                            crate::dav_write::update(&dir, || Ok(base), |_, _| Ok(raw)).map(Some)
                        }
                        (event_form::Mode::Create, _, Some(raw)) => {
                            crate::dav_write::create(&dir, Kind::Calendar, None, |_, _| Ok(raw))
                                .map(Some)
                        }
                        _ => Err(anyhow::anyhow!("沒有可寫入的內容")),
                    };
                    let _ = tx.send(result);
                });
                self.pending_event = Some(rx);
                self.status = "正在寫入行事曆...".into();
            }
        }
    }

    fn poll_event(&mut self) -> Result<bool> {
        let Some(result) = self.pending_event.as_ref().and_then(receive) else {
            return Ok(false);
        };
        self.pending_event = None;
        let mode = self.event_form.as_ref().map(|form| form.mode);
        match result {
            Ok(item) => {
                self.event_form = None;
                self.status = match (&item, mode) {
                    (Some(item), Some(event_form::Mode::Create)) => {
                        format!("已新增「{}」", item.title)
                    }
                    (Some(item), _) => format!("已更新「{}」", item.title),
                    (None, _) => "已刪除活動".into(),
                };
                if self.tab == 1 {
                    if let Some(start) = item
                        .as_ref()
                        .filter(|item| !self.calendar.contains(item))
                        .and_then(calendar::Calendar::interval)
                    {
                        self.calendar.start = start.0.date_naive();
                    }
                    self.table.select(None);
                    self.reload()?;
                    if let Some(index) =
                        item.and_then(|item| self.rows.iter().position(|row| row.id == item.id))
                    {
                        self.select_calendar_event(index)?;
                    }
                }
            }
            Err(error) => {
                let accepted = error
                    .downcast_ref::<crate::provider::dav::RemoteWritePendingReconcile>()
                    .is_some();
                let reason = format!("{error:#}");
                if let Some(form) = self.event_form.as_mut() {
                    form.write_failed(&reason, accepted);
                }
                self.status = "行事曆寫入失敗，輸入已保留".into();
            }
        }
        Ok(true)
    }

    fn switch(&mut self, tab: usize) -> Result<()> {
        self.tab = tab % NAMES.len();
        self.collection = 0;
        self.query.clear();
        self.searching = false;
        self.invalidate_search();
        self.reload()?;
        if self.tab == 3 {
            self.room_finder.enter();
        }
        Ok(())
    }

    fn change_collection(&mut self, index: usize) -> Result<()> {
        self.collection = index.min(self.collections.len().saturating_sub(1));
        self.searching = false;
        self.invalidate_search();
        self.reload()
    }

    fn change_page(&mut self, delta: i32) -> Result<()> {
        if self.tab == 1 {
            self.calendar.shift(delta);
            self.table = TableState::default();
            self.close_detail();
            return self.reload();
        }
        let last = self.total.saturating_sub(1) / PAGE_SIZE;
        let next = (self.page as i64 + i64::from(delta)).clamp(0, last as i64) as usize;
        if next != self.page {
            self.page = next;
            self.table = TableState::default();
            self.close_detail();
            self.reload()?;
        }
        Ok(())
    }

    fn change_calendar_view(&mut self, view: calendar::CalendarView) -> Result<()> {
        if self.calendar.view == view {
            return Ok(());
        }
        let window_start = self.calendar.window().map(|(start, _)| start);
        self.calendar.view = view;
        if let Some(item) = self
            .table
            .selected()
            .and_then(|index| self.rows.get(index))
            .filter(|item| !self.calendar.contains(item))
        {
            if let Some((start, _)) = calendar::Calendar::interval(item) {
                self.calendar.start = start
                    .date_naive()
                    .max(window_start.unwrap_or(self.calendar.start));
            }
        }
        self.close_detail();
        self.reload()
    }

    fn start_sync(&mut self) {
        if self.tab == 2 {
            if let Err(error) = self.reload() {
                self.status = format!("本機聯絡人讀取失敗：{error:#}");
            } else if self.pending_directory.is_none() {
                self.status = "已更新本機聯絡人 · Shift-R 同步全公司目錄".into();
            }
            return;
        }
        if self.pending.is_some() || self.pending_directory.is_some() {
            self.status = "同步已在背景執行，完成前仍可瀏覽本機資料".into();
            return;
        }
        if self.config.is_none() {
            self.status = "尚未設定帳號 · 請執行 m2k init --username 你的帳號".into();
            return;
        }
        let dir = self.dir.clone();
        let kind = KINDS[self.tab];
        let calendar_center = self.calendar_center();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = if kind == Kind::Calendar {
                sync::refresh_calendar(&dir, calendar_center)
            } else {
                sync::run(&dir, &[kind])
            };
            let _ = tx.send(result);
        });
        self.pending = Some(rx);
        self.status = if kind == Kind::Calendar {
            "正在更新選取日期前後 30 天的行事曆...".into()
        } else {
            format!("正在同步{}...", NAMES[self.tab])
        };
    }

    fn calendar_center(&self) -> DateTime<Utc> {
        self.table
            .selected()
            .and_then(|index| self.rows.get(index))
            .and_then(calendar::Calendar::interval)
            .map(|(date, _)| date.with_timezone(&Utc))
            .unwrap_or_else(|| self.calendar.center())
    }

    fn start_directory_sync(&mut self) {
        if self.pending_directory.is_some() || self.pending.is_some() {
            self.status = "同步已在背景執行，完成前仍可瀏覽本機資料".into();
            return;
        }
        if self.dir.join(".demo").exists() {
            self.status = "展示模式 · 聯絡人使用本機資料".into();
            return;
        }
        if self.config.is_none() {
            self.status = "本機尚無聯絡人 · 請先設定帳號；Shift-R 同步公司目錄".into();
            return;
        }
        let lock = match sync::lock(&self.dir) {
            Ok(lock) => lock,
            Err(error) => {
                self.status = format!("公司目錄未啟動：{error:#} · 可稍後按 Shift-R");
                return;
            }
        };
        let dir = self.dir.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(directory::sync_contacts_locked(
                &dir,
                SyncArgs {
                    scope: DirectoryScope::Company,
                    addressbook: None,
                },
                lock,
            ));
        });
        self.pending_directory = Some(rx);
        self.status = "正在背景同步全公司聯絡人 · 完成前可繼續瀏覽".into();
    }

    fn start_search(&mut self) {
        if self.pending_search.is_some() || self.tab != 0 {
            return;
        }
        self.search_due = None;
        if self.query.trim().is_empty() {
            return;
        }
        let Some(cfg) = self.config.clone() else {
            self.status = "本機搜尋 · 離線仍可使用".into();
            return;
        };
        if self.dir.join(".demo").exists() {
            self.status = "展示模式 · 本機搜尋".into();
            return;
        }
        let dir = self.dir.clone();
        let query = self.query.clone();
        let collection = self.selected_collection().map(str::to_owned);
        let generation = self.search_generation;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = online_mail(&dir, &cfg, |password| {
                mail::search(&cfg, password, &query, collection.as_deref(), REMOTE_LIMIT)
            });
            let _ = tx.send(result);
        });
        self.pending_search = Some(SearchJob {
            generation,
            receiver: rx,
        });
        self.status = "本機結果已載入 · 正在搜尋 IMAP...".into();
    }

    fn poll_background(&mut self) -> Result<bool> {
        let mut dirty = self.room_finder.poll();
        dirty |= self.poll_send();
        dirty |= self.poll_event()?;
        if Instant::now() >= self.progress_due {
            self.progress_due = Instant::now() + Duration::from_millis(500);
            let progress = match crate::sync_progress::read(&self.dir) {
                Ok(progress) => progress,
                Err(error) => {
                    self.status = format!("同步進度無法讀取：{error:#}");
                    dirty = true;
                    None
                }
            };
            if self.progress != progress {
                self.progress = progress;
                dirty = true;
            }
        }
        if self.local_due.is_some_and(|due| Instant::now() >= due) {
            self.local_due = None;
            self.reload()?;
            self.status = "本機搜尋完成".into();
            dirty = true;
        }
        if let Some(response) = self.pending.as_ref().and_then(receive) {
            self.pending = None;
            self.status = match response {
                Ok(results) => results
                    .iter()
                    .map(|r| {
                        r.error
                            .as_ref()
                            .map(|e| format!("{} 失敗：{e}", r.kind))
                            .unwrap_or_else(|| format!("{} {} 筆", r.kind, r.count))
                    })
                    .collect::<Vec<_>>()
                    .join(" · "),
                Err(error) => format!("同步失敗：{error:#}"),
            };
            self.reload()?;
            dirty = true;
        }
        if let Some(response) = self.pending_directory.as_ref().and_then(receive) {
            self.pending_directory = None;
            self.status = match response {
                Ok(result) => format!(
                    "公司目錄同步完成 · {} 筆 · {} 個分支 · Shift-R 可重新同步",
                    result.count,
                    result.collections.len()
                ),
                Err(error) => format!("公司目錄同步失敗：{error:#} · Shift-R 可重試"),
            };
            self.reload()?;
            dirty = true;
        }
        if let Some(response) = self
            .pending_search
            .as_ref()
            .and_then(|job| receive(&job.receiver))
        {
            let job = self.pending_search.take().unwrap();
            if job.generation == self.search_generation && self.tab == 0 {
                match response {
                    Ok(result) => {
                        self.remote = result.items;
                        self.remote_truncated = result.truncated;
                        self.status = format!(
                            "本機 + IMAP 合併去重 · {} 筆伺服器摘要{}",
                            self.remote.len(),
                            if self.remote_truncated {
                                " · 伺服器結果達上限，請縮小條件"
                            } else {
                                ""
                            }
                        );
                        self.reload()?;
                    }
                    Err(error) => self.status = format!("IMAP 搜尋失敗，本機結果可用：{error:#}"),
                }
                dirty = true;
            }
        }
        if let Some(response) = self
            .pending_body
            .as_ref()
            .and_then(|job| receive(&job.receiver))
        {
            let job = self.pending_body.take().unwrap();
            if job.generation == self.body_generation && self.detail {
                if let Some(index) = self
                    .table
                    .selected()
                    .filter(|&i| self.rows.get(i).is_some_and(|r| r.id == job.id))
                {
                    match response {
                        Ok(mut item) => match normalize_detail(&mut item) {
                            Ok(()) => {
                                self.rows[index] = item;
                                self.status = "郵件正文已載入 · 未變更已讀狀態".into();
                            }
                            Err(error) => self.status = format!("正文解析失敗：{error:#}"),
                        },
                        Err(error) => self.status = format!("正文載入失敗：{error:#}"),
                    }
                    dirty = true;
                }
            }
            if self.detail
                && self
                    .rows
                    .get(self.table.selected().unwrap_or(0))
                    .is_some_and(|item| item.id != job.id && item.raw.is_empty())
            {
                self.load_selected_item()?;
                dirty = true;
            }
        }
        if self.search_due.is_some_and(|due| Instant::now() >= due) && self.pending_search.is_none()
        {
            self.start_search();
            dirty = true;
        }
        Ok(dirty)
    }

    fn key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self
                .reply
                .as_ref()
                .is_some_and(|flow| flow.phase == reply::Phase::Sending)
            {
                let hint = "回覆寄送中，請等結果再離開（避免重複寄出）";
                self.status = hint.into();
                if let Some(flow) = self.reply.as_mut() {
                    flow.notice = Some(hint.into());
                }
                return Ok(false);
            }
            if self
                .event_form
                .as_ref()
                .is_some_and(|form| form.phase == event_form::Phase::Writing)
            {
                self.status = "行事曆寫入中，請等結果再離開".into();
                return Ok(false);
            }
            if let Some(form) = self.event_form.as_mut().filter(|form| {
                form.dirty()
                    && matches!(
                        form.phase,
                        event_form::Phase::Editing | event_form::Phase::Confirm
                    )
            }) {
                let name = format!(
                    "event-{}-{}.txt",
                    chrono::Local::now().format("%Y%m%d-%H%M%S"),
                    &uuid::Uuid::new_v4().simple().to_string()[..8]
                );
                let root = self.dir.join("drafts");
                let saved = crate::local_state::private_directory(&root).and_then(|()| {
                    crate::local_state::write_atomic(
                        &root.join(&name),
                        form.draft_text().as_bytes(),
                    )
                });
                if let Err(error) = saved {
                    form.notice = Some(format!("草稿保存失敗，未離開：{error:#}"));
                    return Ok(false);
                }
            }
            self.autosave_reply();
            return Ok(true);
        }
        if self.reply.is_some() {
            self.reply_key(key);
            return Ok(false);
        }
        if self.event_form.is_some() {
            self.event_key(key);
            return Ok(false);
        }
        if self.help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.help = false;
            }
            return Ok(key.code == KeyCode::Char('q'));
        }
        if self.tab == 3 {
            if key.code == KeyCode::Char('?') {
                self.help = true;
            } else if matches!(key.code, KeyCode::Esc | KeyCode::F(1)) {
                if !self.room_finder.consume_escape() {
                    self.switch(0)?;
                }
            } else {
                self.room_finder.key(key);
            }
            return Ok(false);
        }
        if self.searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.searching = false;
                    if self.local_due.take().is_some() {
                        self.reload()?;
                        self.status = "本機搜尋完成".into();
                    }
                    if key.code == KeyCode::Enter {
                        self.start_search();
                    }
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.invalidate_search();
                    self.local_due = Some(Instant::now() + Duration::from_millis(200));
                    self.status = "輸入中 · 停止輸入後搜尋，或 Enter 立即套用".into();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.query.push(c);
                    self.invalidate_search();
                    self.local_due = Some(Instant::now() + Duration::from_millis(200));
                    self.status = "輸入中 · 停止輸入後搜尋，或 Enter 立即套用".into();
                }
                _ => {}
            }
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('R') if self.tab == 2 => self.start_directory_sync(),
            KeyCode::Char('r') if self.tab == 0 && key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.start_reply(std::env::var("EDITOR").ok())
            }
            KeyCode::Char('R') if self.tab == 0 => self.start_reply(std::env::var("EDITOR").ok()),
            KeyCode::Char('r') if self.tab == 2 && key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.start_directory_sync()
            }
            KeyCode::Char('r') => self.start_sync(),
            KeyCode::Char('/') => {
                self.searching = true;
                self.close_detail();
            }
            KeyCode::Char('1') => self.switch(0)?,
            KeyCode::Char('2') => self.switch(1)?,
            KeyCode::Char('3') => self.switch(2)?,
            KeyCode::Char('4') => self.switch(3)?,
            KeyCode::Char('v') if self.tab == 1 => {
                self.change_calendar_view(self.calendar.view.next())?
            }
            KeyCode::Char('d') if self.tab == 1 => {
                self.change_calendar_view(calendar::CalendarView::ThreeDay)?
            }
            KeyCode::Char('w') if self.tab == 1 => {
                self.change_calendar_view(calendar::CalendarView::Week)?
            }
            KeyCode::Char('m') if self.tab == 1 => {
                self.change_calendar_view(calendar::CalendarView::Month)?
            }
            KeyCode::Char('n') if self.tab == 1 => self.start_event_create(),
            KeyCode::Char('e') if self.tab == 1 => self.start_event(event_form::Mode::Edit),
            KeyCode::Char('x') if self.tab == 1 => self.start_event(event_form::Mode::Delete),
            KeyCode::Tab => self.switch(self.tab + 1)?,
            KeyCode::BackTab => self.switch(self.tab + NAMES.len() - 1)?,
            KeyCode::Enter => self.open_detail()?,
            KeyCode::Esc if self.tab == 1 => {
                if self.detail {
                    self.close_detail();
                } else {
                    self.table.select(None);
                    self.close_detail();
                }
            }
            KeyCode::Esc => self.close_detail(),
            KeyCode::Char('l') | KeyCode::Char(']') if self.tab == 1 => self.change_page(1)?,
            KeyCode::Char('h') | KeyCode::Char('[') if self.tab == 1 => self.change_page(-1)?,
            KeyCode::Right if self.tab == 1 => self.move_selection(1)?,
            KeyCode::Left if self.tab == 1 => self.move_selection(-1)?,
            KeyCode::Right | KeyCode::Char('l') if !self.detail => {
                self.change_collection((self.collection + 1) % self.collections.len())?
            }
            KeyCode::Left | KeyCode::Char('h') if !self.detail => self.change_collection(
                (self.collection + self.collections.len() - 1) % self.collections.len(),
            )?,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1)?,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1)?,
            KeyCode::PageUp if self.detail => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown if self.detail => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageUp | KeyCode::Char('[') => self.change_page(-1)?,
            KeyCode::PageDown | KeyCode::Char(']') => self.change_page(1)?,
            KeyCode::Home | KeyCode::Char('g') if self.tab == 1 && !self.detail => {
                if !self.rows.is_empty() {
                    self.select_calendar_event(0)?;
                }
            }
            KeyCode::End | KeyCode::Char('G') if self.tab == 1 && !self.detail => {
                if let Some(index) = self.rows.len().checked_sub(1) {
                    self.select_calendar_event(index)?;
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                if self.detail {
                    self.scroll = 0;
                } else {
                    self.page = 0;
                    self.table = TableState::default();
                    self.reload()?;
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if self.detail {
                    self.scroll = self
                        .rows
                        .get(self.table.selected().unwrap_or(0))
                        .map(|r| {
                            r.text
                                .lines()
                                .count()
                                .saturating_sub(1)
                                .min(u16::MAX as usize) as u16
                        })
                        .unwrap_or(0);
                } else {
                    self.page = self.total.saturating_sub(1) / PAGE_SIZE;
                    self.reload()?;
                    self.table.select(self.rows.len().checked_sub(1));
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn move_selection(&mut self, delta: i32) -> Result<()> {
        if self.tab == 1 {
            if !self.rows.is_empty() {
                let index = self.table.selected().map_or_else(
                    || {
                        if delta < 0 {
                            self.rows.len() as i64 - 1
                        } else {
                            0
                        }
                    },
                    |index| index as i64 + i64::from(delta),
                );
                self.select_calendar_event(index.clamp(0, self.rows.len() as i64 - 1) as usize)?;
            }
            return Ok(());
        }
        if self.detail {
            self.scroll = (i32::from(self.scroll) + delta).clamp(0, u16::MAX as i32) as u16;
            return Ok(());
        }
        if self.rows.is_empty() {
            return Ok(());
        }
        let index = self.table.selected().unwrap_or(0) as i64 + i64::from(delta);
        if index >= self.rows.len() as i64 && self.page * PAGE_SIZE + self.rows.len() < self.total {
            self.change_page(1)?;
        } else if index < 0 && self.page > 0 {
            self.change_page(-1)?;
            self.table.select(self.rows.len().checked_sub(1));
        } else {
            self.table
                .select(Some(index.clamp(0, self.rows.len() as i64 - 1) as usize));
        }
        Ok(())
    }

    fn select_calendar_event(&mut self, index: usize) -> Result<()> {
        self.close_detail();
        self.table.select(Some(index));
        for (other, row) in self.rows.iter_mut().enumerate() {
            if other != index {
                row.text = String::new();
                row.raw = String::new();
            }
        }
        self.searching = false;
        self.load_selected_item()
    }

    fn mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if self.reply.is_some() || self.event_form.is_some() {
            return Ok(());
        }
        if self.hits.tabs.is_empty() {
            return Ok(());
        }
        if self.help {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                self.help = false;
            }
            return Ok(());
        }
        let point = Position::new(mouse.column, mouse.row);
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(tab) = self
                .hits
                .tabs
                .iter()
                .find(|(area, _)| area.contains(point))
                .map(|(_, tab)| *tab)
            {
                return self.switch(tab);
            }
            if self.tab == 3 {
                self.room_finder.mouse(mouse);
                return Ok(());
            }
            if self.hits.search.contains(point) {
                self.searching = true;
                self.close_detail();
            } else if self.hits.previous.contains(point) {
                self.change_page(-1)?;
            } else if self.hits.next.contains(point) {
                self.change_page(1)?;
            } else if self.hits.folders.contains(point) {
                let index = self.folders.offset() + usize::from(mouse.row - self.hits.folders.y);
                if index < self.collections.len() {
                    self.change_collection(index)?;
                }
            } else if self.hits.list.contains(point) {
                if self.tab == 1 {
                    if let Some(index) = self
                        .hits
                        .calendar_events
                        .iter()
                        .rev()
                        .find(|(area, _)| area.contains(point))
                        .map(|(_, index)| *index)
                    {
                        self.select_calendar_event(index)?;
                    }
                    return Ok(());
                }
                let index = self.table.offset() + usize::from(mouse.row - self.hits.list.y);
                if index < self.rows.len() {
                    self.table.select(Some(index));
                    self.open_detail()?;
                }
            } else if self.hits.detail.contains(point) {
                self.detail =
                    !self.rows.is_empty() && (self.tab != 1 || self.table.selected().is_some());
                self.searching = false;
            }
        } else if self.tab == 3 {
            self.room_finder.mouse(mouse);
        } else if matches!(
            mouse.kind,
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
        ) {
            let delta = if mouse.kind == MouseEventKind::ScrollDown {
                3
            } else {
                -3
            };
            if self.hits.folders.contains(point) {
                let index = (self.collection as i64 + i64::from(delta))
                    .clamp(0, self.collections.len().saturating_sub(1) as i64)
                    as usize;
                self.change_collection(index)?;
            } else if self.hits.detail.contains(point) && self.detail {
                self.scroll = (i32::from(self.scroll) + delta).clamp(0, u16::MAX as i32) as u16;
            } else if self.hits.list.contains(point) {
                self.close_detail();
                self.move_selection(delta)?;
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        self.hits = HitAreas::default();
        let area = frame.area();
        if area.width < 40 || area.height < if self.tab == 1 { 20 } else { 14 } {
            frame.render_widget(
                Paragraph::new(if self.tab == 1 {
                    "m2k · 離線模式可瀏覽\n行事曆至少需要 40 欄 × 20 列\nq 離開 · 1/2/3/4 切換"
                } else {
                    "m2k · 離線模式可瀏覽\n終端至少需要 40 欄 × 14 列\nq 離開 · 1/2/3/4 切換"
                })
                .block(block("工作台")),
                area,
            );
            return;
        }
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(3),
            ])
            .split(area);
        let user = self
            .config
            .as_ref()
            .map(|c| c.email.as_str())
            .unwrap_or("離線模式");
        let header = block(format!(" m2k · {user} "));
        let inner = header.inner(layout[0]);
        frame.render_widget(header, layout[0]);
        let tabs = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(inner);
        for (index, title) in NAMES.iter().enumerate() {
            self.hits.tabs.push((tabs[index], index));
            frame.render_widget(
                Paragraph::new(format!("{} {title}", index + 1)).style(if self.tab == index {
                    Style::default()
                        .fg(ACCENT)
                        .bg(SELECTED)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                }),
                tabs[index],
            );
        }
        if self.tab == 3 {
            self.room_finder.draw(
                frame,
                Rect::new(
                    layout[1].x,
                    layout[1].y,
                    layout[1].width,
                    layout[1].height + layout[2].height,
                ),
            );
        } else {
            self.hits.search = layout[1];
            let source = if self.tab == 0 {
                "本機 + IMAP"
            } else {
                "本機"
            };
            frame.render_widget(
                Paragraph::new(format!(
                    "{}{}",
                    self.query,
                    if self.searching { "|" } else { "" }
                ))
                .block(
                    block(format!(" 搜尋 · {source} · / 或點擊輸入 · Enter 套用 ")).border_style(
                        Style::default().fg(if self.searching { ACCENT } else { MUTED }),
                    ),
                ),
                layout[1],
            );
            self.draw_workspace(frame, layout[2]);
        }
        let progress_status = self
            .progress
            .as_ref()
            .filter(|progress| progress.state == "running")
            .map(|progress| {
                format!(
                    "同步中 · {} · {} · {} 筆",
                    progress
                        .kind
                        .map(|kind| kind.to_string())
                        .unwrap_or_default(),
                    progress.phase,
                    progress.count
                )
            });
        let status = if self.tab == 3 {
            self.room_finder.status.as_str()
        } else if self.tab == 2 && self.pending_directory.is_some() {
            "正在背景同步全公司聯絡人 · 完成前可繼續瀏覽"
        } else if let Some(progress) = &progress_status {
            progress.as_str()
        } else {
            self.status.as_str()
        };
        let count = if self.tab < 3 {
            self.counts
                .get(KINDS[self.tab].to_string())
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        } else {
            0
        };
        let last = if self.tab < 3 {
            self.sync_state
                .get(KINDS[self.tab].to_string())
                .and_then(|s| s.get("last_success_at"))
                .and_then(|v| v.as_str())
                .map(display_time)
                .unwrap_or_else(|| "尚未同步".into())
        } else {
            String::new()
        };
        let hint = if self.tab == 3 {
            "Tab 欄位 · Enter 查詢／段內起迄 · F5 更新 · Esc 返回 · Ctrl-C 離開 · ? 說明".into()
        } else if self.tab == 1 {
            let unit = match self.calendar.view {
                calendar::CalendarView::ThreeDay => "日",
                calendar::CalendarView::Week => "週",
                calendar::CalendarView::Month => "月",
            };
            format!("v 切視圖 · d/w/m 直選 · [ ] / h l 移{unit} · j/k 選活動 · n 新增 · e 編輯 · x 刪除 · ? 說明")
        } else if self.tab == 2 {
            "q 離開 · 1/2/3/4 頁籤 · / 搜尋 · [ ] 翻頁 · r 讀本機 · Shift-R 同步公司 · ? 說明"
                .into()
        } else {
            "q 離開 · 1/2/3/4 頁籤 · ↑↓ 選取 · Enter 詳情 · R 回覆 · / 搜尋 · [ ] 翻頁 · r 更新 · ? 說明"
                .into()
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(hint),
                Line::from(Span::styled(status, Style::default().fg(ACCENT))),
                Line::from(Span::styled(
                    if self.tab == 1 {
                        format!("{count} 筆本機資料 · 最後成功 {last} · 台北時間 · 滑鼠可選活動與行事曆")
                    } else if self.tab < 3 {
                        format!(
                            "{count} 筆本機資料 · 最後成功 {last} · 滑鼠可選列、資料夾、頁籤與分頁"
                        )
                    } else {
                        "自然語言以台北時間解讀 · 未知狀態會獨立列出".into()
                    },
                    Style::default().fg(MUTED),
                )),
            ]),
            layout[3],
        );
        if let Some(flow) = &self.reply {
            draw_reply(
                frame,
                area,
                flow,
                self.pending.is_some() || self.pending_directory.is_some(),
            );
        }
        if let Some(form) = &self.event_form {
            draw_event_form(frame, area, form);
        }
        if self.help {
            self.draw_help(frame, area);
        }
    }

    fn draw_workspace(&mut self, frame: &mut Frame, area: Rect) {
        if self.tab == 1 {
            self.draw_calendar_workspace(frame, area);
            return;
        }
        let wide = area.width >= 112;
        let folder_width = if area.width >= 64 { 18 } else { 0 };
        let cols = Layout::horizontal([
            Constraint::Length(folder_width),
            Constraint::Percentage(if wide { 43 } else { 100 }),
            Constraint::Min(0),
        ])
        .split(area);
        if folder_width > 0 {
            let panel = block(" 資料夾 ");
            self.hits.folders = panel.inner(cols[0]);
            let list = List::new(
                self.collections
                    .iter()
                    .map(|(_, name)| ListItem::new(name.clone())),
            )
            .block(panel)
            .highlight_style(
                Style::default()
                    .fg(ACCENT)
                    .bg(SELECTED)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
            frame.render_stateful_widget(list, cols[0], &mut self.folders);
        }
        let list_area = if wide {
            cols[1]
        } else {
            Rect::new(
                cols[1].x,
                cols[1].y,
                area.width.saturating_sub(folder_width),
                area.height,
            )
        };
        self.draw_list(frame, list_area);
        if wide {
            self.draw_detail(frame, cols[2]);
        } else if self.detail {
            self.hits.list = Rect::default();
            self.hits.previous = Rect::default();
            self.hits.next = Rect::default();
            frame.render_widget(Clear, list_area);
            self.draw_detail(frame, list_area);
        }
    }

    fn draw_calendar_workspace(&mut self, frame: &mut Frame, area: Rect) {
        let folder_width = if area.width >= 64 { 18 } else { 0 };
        let columns =
            Layout::horizontal([Constraint::Length(folder_width), Constraint::Min(0)]).split(area);
        if folder_width > 0 {
            let panel = block(" 行事曆 ");
            self.hits.folders = panel.inner(columns[0]);
            let list = List::new(
                self.collections
                    .iter()
                    .map(|(_, name)| ListItem::new(name.clone())),
            )
            .block(panel)
            .highlight_style(
                Style::default()
                    .fg(ACCENT)
                    .bg(SELECTED)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");
            frame.render_stateful_widget(list, columns[0], &mut self.folders);
        }
        let vertical = Layout::vertical([
            Constraint::Min(4),
            Constraint::Length((area.height / 3).clamp(6, 10)),
        ])
        .split(columns[1]);
        let top = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).split(vertical[0]);
        self.hits.list = top[0];
        self.hits.calendar_events =
            self.calendar
                .draw_grid(frame, top[0], &self.rows, self.table.selected());
        let navigation = Layout::horizontal([
            Constraint::Length(8),
            Constraint::Min(0),
            Constraint::Length(8),
        ])
        .split(top[1]);
        self.hits.previous = navigation[0];
        self.hits.next = navigation[2];
        let (previous, next) = match self.calendar.view {
            calendar::CalendarView::ThreeDay => ("< 前一天", "後一天 >"),
            calendar::CalendarView::Week => ("< 前一週", "後一週 >"),
            calendar::CalendarView::Month => ("< 前一月", "後一月 >"),
        };
        frame.render_widget(
            Paragraph::new(previous).style(Style::default().fg(ACCENT)),
            navigation[0],
        );
        frame.render_widget(
            Paragraph::new(format!(
                "{} · {} 筆活動",
                self.calendar.view.label(),
                self.rows.len()
            ))
            .alignment(ratatui::layout::Alignment::Center)
            .style(Style::default().fg(MUTED)),
            navigation[1],
        );
        frame.render_widget(
            Paragraph::new(next).style(Style::default().fg(ACCENT)),
            navigation[2],
        );
        self.draw_calendar_detail(frame, vertical[1]);
    }

    fn draw_calendar_detail(&mut self, frame: &mut Frame, area: Rect) {
        self.hits.detail = area;
        let panel = block(if self.detail {
            " 活動詳情 · PageUp/Down 捲動 · Esc 收起 "
        } else {
            " 活動詳情 · Enter 閱讀 · Esc 清除選取 "
        });
        let Some(item) = self.table.selected().and_then(|index| self.rows.get(index)) else {
            frame.render_widget(
                Paragraph::new("選取上方活動以查看詳情。")
                    .block(panel)
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        };
        let time = calendar::Calendar::interval(item)
            .map(|(start, end)| {
                format!(
                    "{} – {} · 台北時間",
                    start.format("%Y-%m-%d %H:%M"),
                    end.format("%Y-%m-%d %H:%M")
                )
            })
            .unwrap_or_default();
        let body = if !item.text.is_empty() {
            item.text.as_str()
        } else if !item.raw.is_empty() {
            "（沒有活動說明）"
        } else {
            "按 Enter 載入活動說明。"
        };
        let mut lines = vec![
            Line::from(Span::styled(
                item.title.as_str(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(time),
        ];
        if !item.subtitle.is_empty() {
            lines.push(Line::from(item.subtitle.as_str()));
        }
        lines.extend(body.lines().map(Line::from));
        frame.render_widget(
            Paragraph::new(lines)
                .block(panel)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let name = self
            .collections
            .get(self.collection)
            .map(|c| c.1.as_str())
            .unwrap_or("全部");
        let panel = block(format!(
            " {} · {name} · {} 筆{} ",
            NAMES[self.tab],
            self.total,
            if self.remote_truncated { "+" } else { "" }
        ));
        let inner = panel.inner(area);
        frame.render_widget(panel, area);
        let body = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
        let paging = Layout::horizontal([
            Constraint::Length(8),
            Constraint::Min(1),
            Constraint::Length(8),
        ])
        .split(body[1]);
        self.hits.previous = paging[0];
        self.hits.next = paging[2];
        frame.render_widget(
            Paragraph::new("< 上一頁").style(Style::default().fg(if self.page > 0 {
                ACCENT
            } else {
                MUTED
            })),
            paging[0],
        );
        frame.render_widget(
            Paragraph::new(format!(
                "第 {}/{} 頁 · 每頁 {PAGE_SIZE}",
                self.page + 1,
                self.total.max(1).div_ceil(PAGE_SIZE)
            ))
            .alignment(ratatui::layout::Alignment::Center)
            .style(Style::default().fg(MUTED)),
            paging[1],
        );
        frame.render_widget(
            Paragraph::new("下一頁 >").style(Style::default().fg(
                if (self.page + 1) * PAGE_SIZE < self.total {
                    ACCENT
                } else {
                    MUTED
                },
            )),
            paging[2],
        );
        if self.rows.is_empty() {
            let empty = if self.tab == 2 {
                if self.pending_directory.is_some() {
                    "正在背景載入公司目錄。\n\n可切換頁籤，完成後會更新聯絡人。"
                } else {
                    "本機尚無符合條件的聯絡人。\n\n按 Shift-R 同步公司目錄，或 / 搜尋。"
                }
            } else {
                "尚無符合條件的資料。\n\n按 r 同步此頁，或 / 搜尋。\n展示：m2k demo"
            };
            frame.render_widget(Paragraph::new(empty).wrap(Wrap { trim: false }), body[0]);
            return;
        }
        self.hits.list = Rect::new(
            body[0].x,
            body[0].y.saturating_add(1),
            body[0].width,
            body[0].height.saturating_sub(1),
        );
        let contact_tab = self.tab == 2;
        let rows = self.rows.iter().map(|item| {
            let indicator = if item.flags.iter().any(|f| f == "\\Flagged") {
                "*"
            } else if item.kind == Kind::Mail && !item.flags.iter().any(|f| f == "\\Seen") {
                "o"
            } else {
                " "
            };
            let third = if item.kind == Kind::Contacts {
                contact_list_meta(item)
            } else {
                item.date.as_deref().map(display_time).unwrap_or_default()
            };
            Row::new(vec![
                Cell::from(indicator),
                Cell::from(item.title.clone()),
                Cell::from(third),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Min(8),
                Constraint::Length(if contact_tab { 16 } else { 11 }),
            ],
        )
        .header(
            Row::new([
                "",
                "主旨 / 姓名",
                if contact_tab {
                    "電話 / 資料夾"
                } else {
                    "時間"
                },
            ])
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(Style::default().bg(SELECTED).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
        frame.render_stateful_widget(table, body[0], &mut self.table);
    }

    fn draw_detail(&mut self, frame: &mut Frame, area: Rect) {
        self.hits.detail = area;
        let Some(item) = self.rows.get(self.table.selected().unwrap_or(0)) else {
            frame.render_widget(
                Paragraph::new(
                    "選取郵件或資料後，按 Enter 閱讀。

點擊列表可開啟詳情。",
                )
                .block(block(" 詳情 "))
                .wrap(Wrap { trim: false }),
                area,
            );
            return;
        };
        let text = if item.kind == Kind::Contacts {
            if item.text.trim().is_empty() {
                format_contact_item_detail(item)
            } else {
                item.text.clone()
            }
        } else {
            let body = if self.detail && !item.text.is_empty() {
                item.text.as_str()
            } else if self.detail && !item.raw.is_empty() {
                "（本文沒有可顯示的文字）"
            } else if self
                .pending_body
                .as_ref()
                .is_some_and(|job| job.id == item.id)
            {
                "正在載入正文..."
            } else {
                "按 Enter 或點擊列表載入正文。"
            };
            format!(
                "{}
{}
{}
資料夾：{}
{}{}
{}",
                item.title,
                item.subtitle,
                item.date.as_deref().map(display_time).unwrap_or_default(),
                if item.kind == Kind::Mail {
                    mail::display_collection(&item.collection)
                } else {
                    item.collection.clone()
                },
                if item.emails.is_empty() {
                    String::new()
                } else {
                    format!(
                        "Email: {}
",
                        item.emails.join(", ")
                    )
                },
                if item.attachments.is_empty() {
                    String::new()
                } else {
                    format!(
                        "附件: {}
",
                        item.attachments
                            .iter()
                            .map(|a| a.filename.as_str())
                            .collect::<Vec<_>>()
                            .join(" · ")
                    )
                },
                body
            )
        };
        frame.render_widget(
            Paragraph::new(text)
                .block(block(if self.detail {
                    " 詳情 · Esc 返回列表 · ↑↓ 捲動 "
                } else {
                    " 詳情預覽 · Enter 閱讀 "
                }))
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.min(80);
        let height = area.height.min(20);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        if self.tab == 1 {
            frame.render_widget(Paragraph::new("行事曆：預設 3-day，所有視圖下方保留活動詳情（台北時間）\nv                     3-day → week → month → 3-day\nd / w / m             直選三天／週／月視圖\nweek / month          七天時段格＋時間軸／月份日期格\n↑ / ↓ / ← / →、j / k   選取視窗內活動\n[ / ]、h / l           移一天／一週／一月（依視圖）\nPageUp / PageDown      移視窗；閱讀時捲動詳情\ng / G、Home / End      首／末活動；閱讀時捲至首／末\nEnter                 閱讀活動說明\nn                     新增事件（預填當天第一個空檔）\ne / x                 編輯／刪除選取事件（確認後才寫入）\nEsc                   收起閱讀；再次按下清除選取\n/ 或點搜尋框          搜尋本機活動\nr                     更新選取日期／視窗中心前後 30 天\n滑鼠／滾輪            點活動選取／在活動區選取、詳情捲動\n1 / 2 / 3 / 4、Tab     切換頁籤\n? / Esc               關閉說明\nq / Ctrl-C            離開")
                .block(block(" 行事曆操作說明 ")).wrap(Wrap { trim: false }), popup);
            return;
        }
        frame.render_widget(Paragraph::new("1 / 2 / 3 / 4、Tab     郵件、行事曆、聯絡人、找會議室\n↑ / ↓、j / k           選取資料 / 捲動詳情\n← / →、h / l           切換資料夾\nEnter / Esc            開啟詳情 / 返回列表\n郵件詳情內 R          回覆草稿（$EDITOR；確認後才寄出）\n/ 或點搜尋框           搜尋中文或英文\nEnter 或停止輸入       合併本機 + IMAP 搜尋（離線保留本機結果）\n[ / ]、PageUp/Down     前後頁 / 捲動詳情\ng / G、Home / End      第一頁 / 最後一頁\nr                      郵件增量；行事曆 ±30 天；聯絡人讀本機\n聯絡人 Shift-R         手動背景同步全公司目錄\n滑鼠                   點頁籤、資料夾、列、搜尋框、上一頁/下一頁\n滾輪                   捲動游標所在資料夾、列表或詳情\n會議室時間             明天 14:00 / 下週一 下午兩點（台北；RFC3339 亦可）\n會議室預設             6 人 · 120 分 · 09:30-17:30\n結束留空               從開始找當日空檔；清空分鐘需填結束\n會議室 Enter           查詢；分鐘結果再 Enter 於空檔內挑實際起迄\n? / Esc                關閉說明\nq / Ctrl-C             離開（表單內 Ctrl-C）").block(block(" 操作說明 ")).wrap(Wrap{trim:false}),popup);
    }
}

fn normalize_detail(item: &mut Item) -> Result<()> {
    if item.kind == Kind::Mail {
        mail::normalize_cached(item)?;
        item.title = mail::display_header(&item.title);
        item.subtitle = mail::display_header(&item.subtitle);
    }
    Ok(())
}

fn contact_list_meta(item: &Item) -> String {
    if let Some(phone) = item
        .phones
        .iter()
        .map(String::as_str)
        .find(|value| !value.trim().is_empty())
    {
        return phone.to_owned();
    }
    if item.collection.contains("個資料夾") {
        return item.collection.clone();
    }
    if let Some(email) = item
        .emails
        .iter()
        .map(String::as_str)
        .find(|value| !value.trim().is_empty())
    {
        return email.to_owned();
    }
    item.collection.clone()
}

fn format_contact_item_detail(item: &Item) -> String {
    let mut lines = vec![item.title.clone()];
    if !item.subtitle.trim().is_empty()
        && item.subtitle.trim() != ";;"
        && item.subtitle.trim() != item.title.trim()
    {
        lines.push(item.subtitle.clone());
    }
    if !item.emails.is_empty() {
        lines.push(format!("Email：{}", item.emails.join(", ")));
    }
    if !item.phones.is_empty() {
        lines.push(format!("電話：{}", item.phones.join(", ")));
    }
    if !item.collection.trim().is_empty() && !item.collection.starts_with("http") {
        lines.push(format!("資料夾／群組：{}", item.collection));
    }
    lines.join("\n")
}

fn receive<T>(receiver: &Receiver<Result<T>>) -> Option<Result<T>> {
    match receiver.try_recv() {
        Ok(result) => Some(result),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(mpsc::TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!("背景工作意外結束"))),
    }
}

fn online_mail<T>(dir: &Path, cfg: &Config, work: impl FnOnce(&str) -> Result<T>) -> Result<T> {
    if dir.join(".demo").exists() {
        bail!("展示資料不連線");
    }
    let password = config::password(dir, cfg)?;
    work(&password)
        .map_err(|error| anyhow::anyhow!(format!("{error:#}").replace(&password, "[已隱藏]")))
}

fn block<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_set(ratatui::symbols::border::Set {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            vertical_left: "|",
            vertical_right: "|",
            horizontal_top: "-",
            horizontal_bottom: "-",
        })
        .border_style(Style::default().fg(MUTED))
        .title(title)
}

fn display_time(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|date| {
            date.with_timezone(&chrono_tz::Asia::Taipei)
                .format("%m/%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| value.into())
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

pub fn run(data_dir: &Path) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("TUI 需要互動式終端；可用 m2k tui --snapshot 檢視畫面");
    }
    let mut app = App::new(data_dir)?;
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        crossterm::cursor::Hide
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut dirty = true;
    loop {
        dirty |= app.poll_background()?;
        if dirty {
            terminal.draw(|frame| app.draw(frame))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if app.key(key)? {
                        break;
                    }
                    dirty = true;
                }
                Event::Mouse(mouse) => {
                    app.mouse(mouse)?;
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
        if let Some(path) = app.editor_request.take() {
            let editor = app.reply_editor.clone().unwrap_or_default();
            let result = run_editor(&mut terminal, &editor, &path)?;
            app.editor_finished(result);
            dirty = true;
        }
    }
    Ok(())
}

/// 暫時交出終端給 `$EDITOR`，無論編輯器成敗都還原 raw mode、alternate screen 與滑鼠捕捉。
fn run_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    editor: &str,
    path: &Path,
) -> Result<io::Result<std::process::ExitStatus>> {
    static IGNORE_SIGINT: std::sync::Once = std::sync::Once::new();
    // 編輯器在前景時 Ctrl-C 會同時送給 m2k；吞掉它，讓 m2k 一定能走到下面的終端還原。
    IGNORE_SIGINT.call_once(|| {
        let _ = ctrlc::set_handler(|| {});
    });
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    #[cfg(unix)]
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status();
    #[cfg(not(unix))]
    let status = std::process::Command::new(editor).arg(path).status();
    let restored = enable_raw_mode()
        .map_err(anyhow::Error::from)
        .and_then(|_| {
            execute!(
                io::stdout(),
                EnterAlternateScreen,
                EnableMouseCapture,
                crossterm::cursor::Hide
            )
            .map_err(anyhow::Error::from)
        });
    restored?;
    terminal.clear()?;
    Ok(status)
}

fn draw_reply(frame: &mut Frame, area: Rect, flow: &reply::ReplyFlow, syncing: bool) {
    frame.render_widget(Clear, area);
    let inner_height = area.height.saturating_sub(2) as usize;
    let notice = |flow: &reply::ReplyFlow| flow.notice.clone().unwrap_or_default();
    let (title, text) = match flow.phase {
        reply::Phase::Editing => {
            let lines = flow.text.split('\n').count();
            let mut shown = flow.text.clone();
            shown.push('_');
            let skip = lines.saturating_sub(inner_height.saturating_sub(2));
            let body = shown.split('\n').skip(skip).collect::<Vec<_>>().join("\n");
            (
                " 回覆草稿 · 內建輸入只能在文末追加（改收件人／主旨請設 $EDITOR）· Ctrl-D 完成 · Esc 存草稿 ",
                format!("{body}\n\n{}", notice(flow)),
            )
        }
        reply::Phase::External => (" 回覆草稿 ", "編輯器執行中…".to_string()),
        reply::Phase::Sending => (
            " 回覆草稿 ",
            format!("正在寄出，請稍候…\n\n{}", notice(flow)),
        ),
        reply::Phase::Confirm => {
            let (head, preview) = match flow.check() {
                Ok((headers, body)) => (
                    format!(
                        "To: {}\nSubject: {}",
                        headers.to.join(", "),
                        headers.subject
                    ),
                    body,
                ),
                Err(reason) => (format!("（草稿尚不能寄出：{reason}）"), flow.text.clone()),
            };
            let quoted = preview
                .lines()
                .filter(|line| line.trim_start().starts_with('>'))
                .count();
            let mut preview = preview
                .lines()
                .filter(|line| {
                    let line = line.trim();
                    !line.starts_with('>') && !(line.starts_with("On ") && line.ends_with("wrote:"))
                })
                .take(inner_height.saturating_sub(8))
                .collect::<Vec<_>>()
                .join("\n");
            if quoted > 0 {
                preview.push_str(&format!("\n\n（另附 {quoted} 行引用原文）"));
            }
            let busy = if syncing && !flow.demo {
                "\n（同步進行中：此時寄送會因 account_busy 失敗，請等同步結束）"
            } else {
                ""
            };
            let keys = if flow.demo {
                "展示資料只能存草稿 · d 存草稿 · e 繼續編輯 · Esc 存草稿離開"
            } else {
                "y 確認寄出 · d 只存草稿 · e 繼續編輯 · Esc 存草稿離開"
            };
            (
                " 確認回覆 ",
                format!("{head}\n\n{preview}\n\n{}\n{keys}{busy}", notice(flow)),
            )
        }
    };
    let paragraph = Paragraph::new(text).block(block(title));
    frame.render_widget(
        if flow.phase == reply::Phase::Editing {
            paragraph
        } else {
            paragraph.wrap(Wrap { trim: false })
        },
        area,
    );
}

fn draw_event_form(frame: &mut Frame, area: Rect, form: &event_form::EventForm) {
    use event_form::{Mode, Phase};
    let width = area.width.min(76);
    let height = area.height.min(18);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let notice = form.notice.clone().unwrap_or_default();
    let base_title = form
        .base
        .as_ref()
        .map(|item| item.title.clone())
        .unwrap_or_default();
    let (title, mut lines) = match (form.phase, form.mode) {
        (Phase::Editing, mode) => {
            let mut lines = event_form::LABELS
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    let value = if index == 5 {
                        if form.all_day {
                            "[x] 是（Space 切換）"
                        } else {
                            "[ ] 否（Space 切換）"
                        }
                        .to_owned()
                    } else if form.locked(index) {
                        "（唯讀）".to_owned()
                    } else if index == form.focus {
                        format!("{}_", form.fields[index])
                    } else {
                        form.fields[index].clone()
                    };
                    let style = if index == form.focus {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    Line::from(vec![
                        Span::styled(
                            format!("{} {label}  ", if index == form.focus { ">" } else { " " }),
                            style,
                        ),
                        Span::raw(value),
                    ])
                })
                .collect::<Vec<_>>();
            lines.push(Line::from(""));
            if let Some(reason) = &form.time_locked {
                lines.push(Line::from(Span::styled(
                    format!("時間欄位唯讀：{reason}"),
                    Style::default().fg(Color::Yellow),
                )));
            }
            for hint in [
                "時間（台北）：明天 14:00、下週一 下午兩點、2026-10-22 09:30",
                "全天事件只填日期，結束為最後一天（含）",
            ] {
                lines.push(Line::from(Span::styled(hint, Style::default().fg(MUTED))));
            }
            lines.push(Line::from("Tab/↑↓ 欄位 · Enter 預覽確認 · Esc 取消"));
            (
                if mode == Mode::Create {
                    " 新增事件 "
                } else {
                    " 編輯事件 "
                },
                lines,
            )
        }
        (Phase::Writing, _) => (" 行事曆寫入 ", vec![Line::from("正在寫入，請稍候…")]),
        (Phase::Accepted, _) => (
            " 行事曆寫入未確認 ",
            vec![Line::from("Esc／Enter 關閉後按 r 更新")],
        ),
        (Phase::Confirm, Mode::Delete) => (
            " 確認刪除 ",
            vec![
                Line::from(format!("刪除「{base_title}」？")),
                Line::from("會刪除伺服器上的事件（使用目前 ETag，伺服器版本已變更時拒絕）。"),
                Line::from(""),
                Line::from(if form.demo {
                    "展示資料不會寫入 · Esc 返回"
                } else {
                    "y 確認刪除 · Esc 返回"
                }),
            ],
        ),
        (Phase::Confirm, mode) => {
            let mut lines = vec![Line::from(if mode == Mode::Create {
                "將建立以下事件：".to_owned()
            } else {
                format!("將更新「{base_title}」（保留 UID 與其他屬性）：")
            })];
            let preview = form
                .raw
                .as_deref()
                .and_then(|raw| crate::calendar_event::fields(raw).ok());
            if let Some(fields) = preview {
                let (start, end) = fields.when.map(|when| when.form_text()).unwrap_or_default();
                let all_day = if fields.when.is_some_and(|when| when.is_all_day()) {
                    "（全天）"
                } else {
                    ""
                };
                lines.push(Line::from(format!("  標題  {}", fields.title)));
                lines.push(Line::from(format!("  時間  {start} → {end}{all_day}")));
                lines.push(Line::from(format!("  地點  {}", fields.location)));
                lines.push(Line::from(format!(
                    "  說明  {}",
                    fields.description.replace('\n', " / ")
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(if form.demo {
                "展示資料不會寫入 · e 繼續編輯 · Esc 返回"
            } else {
                "y 確認寫入 · e 繼續編輯 · Esc 返回"
            }));
            (
                if mode == Mode::Create {
                    " 確認新增 "
                } else {
                    " 確認更新 "
                },
                lines,
            )
        }
    };
    if !notice.is_empty() {
        lines.push(Line::from(""));
        lines.extend(notice.lines().map(|line| {
            Line::from(Span::styled(
                line.to_owned(),
                Style::default().fg(Color::Yellow),
            ))
        }));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block(title))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

pub fn render_snapshot(data_dir: &Path, width: u16, height: u16) -> Result<String> {
    if width == 0 || height == 0 {
        return Ok(String::new());
    }
    let mut app = App::new(data_dir)?;
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| app.draw(frame))?;
    let buffer = terminal.backend().buffer();
    let mut text = String::new();
    for y in 0..height {
        let mut x = 0;
        while x < width {
            let cell = &buffer[(x, y)];
            text.push_str(cell.symbol());
            x += unicode_width::UnicodeWidthStr::width(cell.symbol()).max(1) as u16;
        }
        text.push('\n');
    }
    Ok(text)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Collection, Item, Snapshot};
    use chrono::Utc;
    use tempfile::tempdir;

    /// 會設定 `M2K_PASSWORD` 的測試互斥執行，避免互相覆蓋環境變數。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn empty_contacts_tab_stays_local_and_shift_r_explicitly_refreshes_once() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            sync::{
                atomic::{AtomicUsize, Ordering},
                Arc,
            },
        };
        struct PasswordEnv(Option<std::ffi::OsString>);
        impl Drop for PasswordEnv {
            fn drop(&mut self) {
                if let Some(value) = &self.0 {
                    std::env::set_var("M2K_PASSWORD", value);
                } else {
                    std::env::remove_var("M2K_PASSWORD");
                }
            }
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let temp = tempdir().unwrap();
        let mut cfg = Config::new("user@example.test".into(), &base).unwrap();
        cfg.carddav_url = format!("{base}/principals/user@example.test");
        config::save(temp.path(), &cfg).unwrap();
        Store::open(temp.path())
            .unwrap()
            .ensure_account(&cfg)
            .unwrap();
        let principal = "<D:response><D:href>/principals/user@example.test</D:href><D:propstat><D:prop><CR:addressbook-home-set><D:href>/company/</D:href></CR:addressbook-home-set></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>";
        let home = "<D:response><D:href>/company/people/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype><D:displayname>People(/People)</D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>";
        let (release_tx, release_rx) = mpsc::channel();
        let request_count = Arc::new(AtomicUsize::new(0));
        let server_count = Arc::clone(&request_count);
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for node in [principal, home, "", ""] {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "company worker did not request expected fixture"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("mock accept failed: {error}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut one = [0; 1];
                while !bytes.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut one).unwrap();
                    bytes.push(one[0]);
                    assert!(bytes.len() < 32_768);
                }
                let headers = String::from_utf8(bytes).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("Content-Length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                requests.push(headers.lines().next().unwrap().to_owned());
                server_count.fetch_add(1, Ordering::SeqCst);
                if requests.len() == 1 {
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                let body = format!("<?xml version=\"1.0\"?><D:multistatus xmlns:D=\"DAV:\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\">{node}</D:multistatus>");
                write!(stream, "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            }
            requests
        });
        let _env = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let _password_env = PasswordEnv(std::env::var_os("M2K_PASSWORD"));
        std::env::set_var("M2K_PASSWORD", "isolated-fixture-password");
        let mut app = App::new(temp.path()).unwrap();
        for _ in 0..3 {
            app.switch(2).unwrap();
            assert!(
                app.pending_directory.is_none(),
                "empty tab entry must remain local"
            );
            assert_eq!(app.total, 0);
            app.switch(0).unwrap();
        }
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            0,
            "local tab entries must make no HTTP requests"
        );
        app.switch(2).unwrap();
        app.key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(app.pending_directory.is_some());
        assert!(app.status.contains("背景同步全公司"));
        app.status = "本機搜尋完成".into();
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let loading = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .replace(' ', "");
        assert!(
            loading.contains("正在背景同步全公司聯絡人"),
            "local search must not hide the running company sync"
        );
        assert!(
            sync::lock(temp.path()).is_err(),
            "worker must keep its acquired lock"
        );
        app.key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT))
            .unwrap();
        release_tx.send(()).unwrap();
        let finish = |app: &mut App| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while app.pending_directory.is_some() {
                app.poll_background().unwrap();
                assert!(
                    Instant::now() < deadline,
                    "company worker did not finish: {}",
                    app.status
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(app.status.contains("公司目錄同步完成"), "{}", app.status);
        };
        finish(&mut app);
        assert_eq!(app.total, 0);
        for _ in 0..3 {
            app.switch(0).unwrap();
            app.switch(2).unwrap();
            assert!(
                app.pending_directory.is_none(),
                "returning to the tab must remain local"
            );
        }
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(request_count.load(Ordering::SeqCst), 4);
        assert!(requests[2].starts_with("PROPFIND /company/people/"));
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.starts_with("REPORT /company/people/"))
                .count(),
            1
        );
        assert!(requests
            .iter()
            .all(|request| request.starts_with("PROPFIND ") || request.starts_with("REPORT ")));
    }

    #[test]
    fn contacts_search_dedupes_demo_contact_a_and_detail_shows_fields_not_empty_body() {
        let temp = tempdir().unwrap();
        let cfg = Config::new("user@example.test".into(), "http://127.0.0.1:9").unwrap();
        config::save(temp.path(), &cfg).unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        store.ensure_account(&cfg).unwrap();
        let collections = vec![
            Collection {
                kind: Kind::Contacts,
                id: "https://mail.example.test/carddav/ACME/".into(),
                name: "ACME(/ACME)".into(),
            },
            Collection {
                kind: Kind::Contacts,
                id: "https://mail.example.test/carddav/ACME/MKT/".into(),
                name: "MKT(/ACME/MKT)".into(),
            },
        ];
        let raw = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:uid-contact-a\r\nFN:demo_contact_a\r\nNICKNAME:demo_contact_a\r\nN:;Demo Contact A (範例聯絡人甲);;;\r\nORG:;;\r\nEMAIL;TYPE=INTERNET:demo_contact_a@example.test\r\nTEL;TYPE=WORK:(00)55501\r\nEND:VCARD\r\n";
        let a = Item {
            id: "contact-a".into(),
            kind: Kind::Contacts,
            collection: collections[0].id.clone(),
            remote_id: "a.vcf".into(),
            etag: None,
            uid_validity: None,
            title: "demo_contact_a".into(),
            subtitle: ";;".into(),
            date: None,
            end_date: None,
            text: String::new(),
            raw: raw.into(),
            flags: vec![],
            emails: vec!["demo_contact_a@example.test".into()],
            phones: vec!["(00)55501".into()],
            attachments: vec![],
        };
        let mut b = a.clone();
        b.id = "contact-b".into();
        b.collection = collections[1].id.clone();
        store
            .apply_snapshot(
                &Snapshot {
                    kind: Kind::Contacts,
                    collections,
                    items: vec![a, b],
                },
                Utc::now(),
            )
            .unwrap();
        let mut app = App::new(temp.path()).unwrap();
        app.switch(2).unwrap();
        app.query = "demo_contact_a".into();
        app.reload().unwrap();
        assert_eq!(app.total, 1, "same person across folders should collapse");
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].title, "demo_contact_a");
        assert_eq!(app.rows[0].collection, "2 個資料夾");
        assert!(app.rows[0].text.contains("電話：(00)55501"));
        assert!(app.rows[0]
            .text
            .contains("Email：demo_contact_a@example.test"));
        assert!(app.rows[0]
            .text
            .contains("顯示名：Demo Contact A (範例聯絡人甲)"));
        assert!(app.rows[0].text.contains("資料夾／群組："));
        assert!(!app.rows[0].text.contains("本文沒有可顯示的文字"));
        assert!(!app.rows[0].text.contains(";;"));
        app.table.select(Some(0));
        app.open_detail().unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("(00)55501"));
        assert!(screen.contains("demo_contact_a"));
        assert!(!screen.contains("本文沒有可顯示的文字"));
        assert!(!screen.contains("https://mail.example.test"));
    }

    #[test]
    fn contacts_with_local_data_stay_local_and_lowercase_r_only_reloads() {
        let (_temp, mut app) = populated_app(2);
        app.switch(2).unwrap();
        assert!(app.total > 0);
        assert!(app.pending_directory.is_none());
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            .unwrap();
        assert!(app.pending.is_none());
        assert!(app.pending_directory.is_none());
        assert!(app.status.contains("本機聯絡人"));
        app.switch(0).unwrap();
        app.switch(2).unwrap();
        assert!(app.pending_directory.is_none());
    }

    #[test]
    fn calendar_refresh_uses_selected_event_timezone_and_empty_window_center() {
        let (_temp, mut app) = populated_app(2);
        app.switch(1).unwrap();
        app.rows[0].date = Some("2026-11-07T09:30:00+08:00".into());
        app.table.select(Some(0));
        assert_eq!(
            app.calendar_center().to_rfc3339(),
            "2026-11-07T01:30:00+00:00"
        );
        let mut malformed_event = app.rows[0].clone();
        malformed_event.date = Some("invalid".into());
        app.rows.clear();
        assert_eq!(app.calendar_center(), app.calendar.center());
        app.rows.push(malformed_event);
        assert_eq!(app.calendar_center(), app.calendar.center());
    }

    #[test]
    fn empty_contacts_tab_stays_local_and_manual_refresh_respects_existing_sync_lock() {
        let temp = tempdir().unwrap();
        let cfg = Config::new("user@example.test".into(), "http://127.0.0.1:9").unwrap();
        config::save(temp.path(), &cfg).unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        store.ensure_account(&cfg).unwrap();
        let lock = sync::lock(temp.path()).unwrap();
        let mut app = App::new(temp.path()).unwrap();
        app.switch(2).unwrap();
        assert!(app.pending_directory.is_none());
        assert_eq!(app.total, 0);
        drop(lock);
        app.switch(0).unwrap();
        app.switch(2).unwrap();
        assert!(app.pending_directory.is_none());
        let _lock = sync::lock(temp.path()).unwrap();
        app.key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(app.pending_directory.is_none());
        assert!(app.status.contains("Shift-R"));
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(app.pending_directory.is_none());
        assert!(app.status.contains("另一個同步"));
    }

    #[test]
    fn company_completion_refreshes_local_contacts_and_failure_keeps_them_visible() {
        let (temp, mut app) = populated_app(2);
        app.switch(2).unwrap();
        let previous_count = app.total;
        let mut store = Store::open(temp.path()).unwrap();
        let mut item = store.list(Kind::Contacts, None, None, 1).unwrap().remove(0);
        item.id = "new-company-person".into();
        item.title = "公司目錄新增成員".into();
        item.raw = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:new-company-person\r\nFN:公司目錄新增成員\r\nEMAIL:new-company@example.test\r\nEND:VCARD\r\n".into();
        item.emails = vec!["new-company@example.test".into()];
        store.upsert(&item).unwrap();
        let (tx, rx) = mpsc::channel();
        app.pending_directory = Some(rx);
        tx.send(Ok(DirectorySyncResult {
            scope: DirectoryScope::Company,
            scope_paths: vec!["公司目錄".into()],
            collections: vec![item.collection],
            count: previous_count + 1,
        }))
        .unwrap();
        assert!(app.poll_background().unwrap());
        assert!(app.pending_directory.is_none());
        assert_eq!(app.total, previous_count + 1);
        assert!(app.status.contains("公司目錄同步完成"));
        let (tx, rx) = mpsc::channel();
        app.pending_directory = Some(rx);
        tx.send(Err(anyhow::anyhow!("mock CardDAV 403 Forbidden")))
            .unwrap();
        app.poll_background().unwrap();
        assert_eq!(app.total, previous_count + 1);
        assert!(app.status.contains("403 Forbidden"));
        assert!(app.status.contains("Shift-R"));
    }

    #[test]
    fn historical_encoded_headers_and_mailbox_are_displayed_without_loading_body() {
        let temp = tempdir().unwrap();
        let item = Item {
            id: "legacy-html-mail".into(),
            kind: Kind::Mail,
            collection: "INBOX/&ZeVnLIqe-".into(),
            remote_id: "1".into(),
            etag: None,
            uid_validity: Some(1),
            title: "=?UTF-8?B?5ris6Kmm?=".into(),
            subtitle: "=?UTF-8?B?546L5bCP5piO?= <wang@example.test>".into(),
            date: None,
            end_date: None,
            text: String::new(),
            raw: "From: =?UTF-8?B?546L5bCP5piO?= <wang@example.test>\r\nSubject: =?UTF-8?B?5ris6Kmm?=\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<h1>歷史正文</h1><p>第二段</p>".into(),
            flags: vec!["\\Seen".into()],
            emails: vec![],
            phones: vec![],
            attachments: vec![],
        };
        let mut store = Store::open(temp.path()).unwrap();
        store
            .apply_snapshot(
                &Snapshot {
                    kind: Kind::Mail,
                    collections: vec![Collection {
                        kind: Kind::Mail,
                        id: item.collection.clone(),
                        name: item.collection.clone(),
                    }],
                    items: vec![item.clone()],
                },
                Utc::now(),
            )
            .unwrap();
        let mut app = App::new(temp.path()).unwrap();
        assert_eq!(app.rows[0].title, "測試");
        assert!(app.rows[0].subtitle.contains("王小明"));
        assert!(app.rows[0].raw.is_empty());
        assert!(app.rows[0].text.is_empty());
        assert_eq!(app.collections[1].0, item.collection);
        assert_eq!(app.collections[1].1, "INBOX/日本語");
        let mut terminal = Terminal::new(TestBackend::new(150, 32)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let preview = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(preview.replace(' ', "").contains("資料夾：INBOX/日本語"));
        app.open_detail().unwrap();
        assert!(app.rows[0].text.contains("歷史正文"));
        assert!(app.rows[0].text.contains("第二段"));
        assert_eq!(app.rows[0].collection, item.collection);
        assert_eq!(app.rows[0].flags, item.flags);
        let persisted = store.get(&item.id).unwrap();
        assert!(
            persisted.text.is_empty(),
            "display repair must not rewrite the cache"
        );
        assert_eq!(persisted.title, item.title);
    }

    #[test]
    fn unicode_search_uses_summary_and_enter_loads_detail_body() {
        let temp = tempdir().unwrap();
        let item = Item {
            id: "mail-tui".into(),
            kind: Kind::Mail,
            collection: "INBOX".into(),
            remote_id: "1".into(),
            etag: None,
            uid_validity: Some(1),
            title: "王小明的郵件".into(),
            subtitle: "王小明 <wang@example.com>".into(),
            date: Some("2026-10-07T00:00:00Z".into()),
            end_date: None,
            text: "詳細頁本文".into(),
            raw: "完整 RFC822".into(),
            flags: vec!["\\Seen".into()],
            emails: vec!["wang@example.com".into()],
            phones: Vec::new(),
            attachments: Vec::new(),
        };
        let mut store = Store::open(temp.path()).unwrap();
        store
            .apply_snapshot(
                &Snapshot {
                    kind: Kind::Mail,
                    collections: vec![Collection {
                        kind: Kind::Mail,
                        id: "INBOX".into(),
                        name: "收件匣".into(),
                    }],
                    items: vec![item],
                },
                Utc::now(),
            )
            .unwrap();

        let mut app = App::new(temp.path()).unwrap();
        assert_eq!(app.rows[0].text, "");
        assert_eq!(app.rows[0].raw, "");
        app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Char('王'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.rows.len(), 1);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.detail);
        assert_eq!(app.rows[0].text, "詳細頁本文");
        assert_eq!(app.rows[0].raw, "完整 RFC822");
        app.reload().unwrap();
        assert_eq!(app.rows[0].text, "詳細頁本文");
    }

    #[test]
    fn persisted_header_only_mail_accepts_lazy_body_completion_without_writing_cache() {
        let (temp, mut app) = populated_app(1);
        let mut store = Store::open(temp.path()).unwrap();
        let mut header = store.get("paged-0000").unwrap();
        header.text.clear();
        header.raw.clear();
        store.upsert(&header).unwrap();
        app.reload().unwrap();
        app.table
            .select(app.rows.iter().position(|row| row.id == header.id));

        let (tx, rx) = mpsc::channel();
        app.pending_body = Some(BodyJob {
            generation: app.body_generation,
            id: header.id.clone(),
            receiver: rx,
        });
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(
            app.pending_body.as_ref().unwrap().generation,
            app.body_generation,
            "header-only cache must enter the lazy-body path"
        );
        let mut fetched = header.clone();
        fetched.text = "Synthetic body from read-only IMAP fetch".into();
        fetched.raw = "Subject: synthetic\r\n\r\nSynthetic body from read-only IMAP fetch".into();
        tx.send(Ok(fetched)).unwrap();
        app.poll_background().unwrap();
        let loaded = &app.rows[app.table.selected().unwrap()];
        assert_eq!(loaded.id, header.id);
        assert_eq!(loaded.collection, header.collection);
        assert_eq!(loaded.remote_id, header.remote_id);
        assert_eq!(loaded.uid_validity, header.uid_validity);
        assert_eq!(loaded.flags, header.flags);
        assert!(loaded.text.contains("read-only IMAP fetch"));
        assert!(!loaded.raw.is_empty());
        assert!(app.pending_body.is_none());
        let persisted = store.get(&header.id).unwrap();
        assert!(persisted.raw.is_empty());
        assert!(persisted.text.is_empty());
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(app.pending_body.is_none(), "loaded body is reused");

        let mut text_only = persisted;
        text_only.text = "Useful cached text without raw MIME".into();
        store.upsert(&text_only).unwrap();
        app.close_detail();
        app.open_detail().unwrap();
        assert_eq!(app.rows[app.table.selected().unwrap()].text, text_only.text);
        assert!(app.pending_body.is_none(), "usable cached text stays local");
    }

    #[test]
    fn malformed_cached_body_remains_recoverable_without_overwriting_storage() {
        let (temp, mut app) = populated_app(2);
        let mut store = Store::open(temp.path()).unwrap();
        let mut item = store.get("paged-0000").unwrap();
        item.raw = "base64:invalid".into();
        item.text.clear();
        store.upsert(&item).unwrap();
        let mut summary = item.clone();
        summary.raw.clear();
        app.rows = vec![summary];
        app.table.select(Some(0));
        app.config = None;
        app.open_detail().unwrap();
        assert!(app.status.contains("快取正文損毀"));
        assert!(app.rows[0].raw.is_empty());
        assert!(app.pending_body.is_none());
        assert_eq!(store.get(&item.id).unwrap().raw, "base64:invalid");
    }

    #[test]
    fn external_sync_progress_redraws_without_keyboard_input() {
        let (temp, mut app) = populated_app(0);
        let lock = crate::sync::lock(temp.path()).unwrap();
        let mut reporter = crate::sync_progress::Reporter::start(temp.path()).unwrap();
        reporter
            .update("fetching", Some(Kind::Calendar), 8)
            .unwrap();
        app.progress_due = Instant::now();
        assert!(app.poll_background().unwrap());
        assert_eq!(app.progress.as_ref().unwrap().state, "running");
        drop(lock);
        app.progress_due = Instant::now();
        assert!(app.poll_background().unwrap());
        assert_eq!(app.progress.as_ref().unwrap().state, "interrupted");
    }

    fn populated_app(count: usize) -> (tempfile::TempDir, App) {
        let temp = tempdir().unwrap();
        crate::demo::seed_without_rooms(temp.path()).unwrap();
        let mut store = Store::open(temp.path()).unwrap();
        let mut item = store.list(Kind::Mail, None, None, 1).unwrap().remove(0);
        item.date = Some("2026-10-07T00:00:00Z".into());
        item.text = "按需載入的測試本文".repeat(512);
        item.raw = "cached body".repeat(512);
        for index in 0..count {
            item.id = format!("paged-{index:04}");
            item.title = format!("分頁郵件 {index:04}");
            store.upsert(&item).unwrap();
        }
        drop(store);
        let app = App::new(temp.path()).unwrap();
        (temp, app)
    }

    fn render(app: &mut App, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }

    fn click(app: &mut App, area: Rect) {
        assert!(area.width > 0 && area.height > 0);
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();
    }

    fn demo_reply_app() -> (tempfile::TempDir, App) {
        let temp = tempdir().unwrap();
        crate::demo::seed_without_rooms(temp.path()).unwrap();
        let mut app = App::new(temp.path()).unwrap();
        app.open_detail().unwrap();
        (temp, app)
    }

    fn type_keys(app: &mut App, keys: &[KeyCode]) {
        for code in keys {
            app.key(KeyEvent::new(*code, KeyModifiers::NONE)).unwrap();
        }
    }

    #[test]
    fn reply_requires_opened_mail_and_does_not_steal_other_tabs_r() {
        let (_temp, mut app) = demo_reply_app();
        app.close_detail();
        type_keys(&mut app, &[KeyCode::Char('R')]);
        assert!(app.reply.is_none());
        assert!(app.status.contains("按 Enter"));
    }

    #[test]
    fn demo_reply_builtin_editor_saves_draft_and_never_sends() {
        let (temp, mut app) = demo_reply_app();
        type_keys(&mut app, &[KeyCode::Char('R')]);
        let flow = app.reply.as_ref().expect("reply flow");
        assert!(flow.demo && !flow.external);
        assert!(flow
            .text
            .starts_with("To: ming@example.test\nSubject: Re: "));
        assert!(app.editor_request.is_none());
        render(&mut app, 120, 32);
        for c in "週三見".chars() {
            type_keys(&mut app, &[KeyCode::Char(c)]);
        }
        app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .unwrap();
        render(&mut app, 120, 32);
        type_keys(&mut app, &[KeyCode::Char('y')]);
        assert!(app.pending_send.is_none(), "demo 不可寄送");
        assert!(app
            .reply
            .as_ref()
            .unwrap()
            .notice
            .as_deref()
            .unwrap()
            .contains("只能存草稿"));
        type_keys(&mut app, &[KeyCode::Char('d')]);
        assert!(app.reply.is_none());
        let drafts = std::fs::read_dir(temp.path().join("drafts"))
            .unwrap()
            .count();
        assert_eq!(drafts, 1);
        assert!(app.status.starts_with("草稿已保存："));
    }

    #[test]
    fn editor_env_selects_external_flow_and_failure_keeps_draft() {
        let (temp, mut app) = demo_reply_app();
        app.start_reply(Some("  ".into()));
        assert!(!app.reply.as_ref().unwrap().external);
        app.reply = None;
        app.start_reply(Some("true".into()));
        let path = app.editor_request.take().expect("editor request");
        assert!(path.starts_with(temp.path().join("drafts")));
        assert!(path.exists());
        app.editor_finished(Err(io::Error::other("找不到編輯器")));
        assert!(app.reply.is_none());
        assert!(app.status.contains("找不到編輯器") && app.status.contains("reply-"));
        assert!(path.exists(), "草稿必須保留");
    }

    #[test]
    fn ctrl_c_during_reply_saves_draft_and_quits() {
        let (temp, mut app) = demo_reply_app();
        type_keys(&mut app, &[KeyCode::Char('R'), KeyCode::Char('x')]);
        let quit = app
            .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(quit);
        let drafts = std::fs::read_dir(temp.path().join("drafts"))
            .unwrap()
            .count();
        assert_eq!(drafts, 1);
    }

    #[test]
    fn ctrl_c_while_sending_is_held_back_and_shift_r_replies() {
        let (_temp, mut app) = demo_reply_app();
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::SHIFT))
            .unwrap();
        assert!(app.reply.is_some(), "kitty 協定的 Shift-R 也要開回覆");
        app.reply.as_mut().unwrap().phase = reply::Phase::Sending;
        let quit = app
            .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .unwrap();
        assert!(!quit && app.status.contains("寄送中"));
        // The footer also paints `status`; clear it so only the overlay can satisfy the check.
        app.status = "其他狀態".into();
        let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .replace(' ', "");
        assert!(screen.contains("避免重複寄出"), "提示必須畫在 overlay 上");
    }

    fn calendar_app() -> (tempfile::TempDir, App) {
        let (temp, mut app) = populated_app(0);
        let mut store = Store::open(temp.path()).unwrap();
        let mut event = store.get("demo-event-1").unwrap();
        store.remove(&event.id).unwrap();
        for (id, title, start, end) in [
            (
                "standup",
                "Daily standup",
                "2026-10-07T09:00:00+08:00",
                "2026-10-07T10:30:00+08:00",
            ),
            (
                "review",
                "Design review",
                "2026-10-07T10:00:00+08:00",
                "2026-10-07T12:00:00+08:00",
            ),
            (
                "planning",
                "Sprint planning",
                "2026-10-09T14:00:00+08:00",
                "2026-10-09T16:00:00+08:00",
            ),
            (
                "outside",
                "Next window",
                "2026-10-10T09:00:00+08:00",
                "2026-10-10T10:00:00+08:00",
            ),
        ] {
            event.id = id.into();
            event.title = title.into();
            event.subtitle = "North meeting room".into();
            event.date = Some(start.into());
            event.end_date = Some(end.into());
            event.text = format!("Agenda for {title}");
            store.upsert(&event).unwrap();
        }
        app.calendar.start = chrono::NaiveDate::from_ymd_opt(2026, 10, 7).unwrap();
        app.switch(1).unwrap();
        (temp, app)
    }

    fn rendered_text(app: &mut App, width: u16, height: u16, detail_only: bool) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let area = if detail_only {
            app.hits.detail
        } else {
            Rect::new(0, 0, width, height)
        };
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in area.y..area.bottom() {
            let mut x = area.x;
            while x < area.right() {
                let symbol = buffer[(x, y)].symbol();
                text.push_str(symbol);
                x += unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn calendar_draws_three_day_headers_and_detail_below_at_wide_and_narrow_sizes() {
        let (_temp, mut app) = calendar_app();
        for (width, height) in [(120, 32), (80, 24), (40, 24)] {
            let text = rendered_text(&mut app, width, height, false);
            for day in ["2026-10-07", "2026-10-08", "2026-10-09"] {
                assert!(text.contains(day), "missing {day} at width {width}");
            }
            assert!(text.contains("活動詳情"));
            assert!(app.hits.detail.y >= app.hits.list.bottom());
            assert_eq!(app.hits.detail.x, app.hits.list.x);
            assert_eq!(app.hits.detail.width, app.hits.list.width);
            assert!(!app.hits.calendar_events.is_empty());
        }
    }

    #[test]
    fn calendar_selection_shows_title_time_location_and_cached_body_without_enter() {
        let (_temp, mut app) = calendar_app();
        let text = rendered_text(&mut app, 120, 32, true);
        assert!(text.contains("Daily standup"));
        assert!(text.contains("09:00") && text.contains("10:30"));
        assert!(text.contains("North meeting room"));
        assert!(text.contains("Agenda for Daily standup"));
        assert!(!app.detail);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.rows[app.table.selected().unwrap()].id, "review");
        assert!(rendered_text(&mut app, 120, 32, true).contains("Agenda for Design review"));
        assert_eq!(
            app.rows.iter().filter(|item| !item.text.is_empty()).count(),
            1
        );
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(app.table.selected().is_some());
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.table.selected(), None);
        assert!(!rendered_text(&mut app, 120, 32, true).contains("Daily standup"));
        app.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.table.selected(), Some(0));
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
            .unwrap();
        assert!(rendered_text(&mut app, 120, 32, true).contains("Agenda for Sprint planning"));
        app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))
            .unwrap();
        assert!(rendered_text(&mut app, 120, 32, true).contains("Agenda for Daily standup"));
    }

    #[test]
    fn calendar_day_navigation_filters_window_and_empty_refresh_follows_visible_days() {
        let (_temp, mut app) = calendar_app();
        let initial = app.calendar.start;
        app.key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.calendar.start, initial + chrono::Duration::days(1));
        assert_eq!(
            app.rows
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["planning", "outside"]
        );
        app.key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.calendar.start, initial);
        app.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
            .unwrap();
        app.key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.calendar.start, initial);
        app.change_page(10).unwrap();
        assert!(app.rows.is_empty());
        assert_eq!(app.calendar_center(), app.calendar.center());
        assert!(rendered_text(&mut app, 120, 32, false).contains("這 3 天沒有符合條件的活動"));
        render(&mut app, 120, 32);
        let previous = app.hits.previous;
        click(&mut app, previous);
        assert_eq!(app.calendar.start, initial + chrono::Duration::days(9));
    }

    #[test]
    fn calendar_mouse_selects_overlapping_blocks_and_resizing_clears_hits() {
        let (_temp, mut app) = calendar_app();
        render(&mut app, 120, 40);
        for index in 0..app.rows.len() {
            render(&mut app, 120, 40);
            let area = app
                .hits
                .calendar_events
                .iter()
                .find(|(_, row)| *row == index)
                .unwrap()
                .0;
            click(&mut app, area);
            assert_eq!(app.table.selected(), Some(index));
            assert!(rendered_text(&mut app, 120, 40, true).contains(&app.rows[index].title));
        }
        render(&mut app, 39, 12);
        assert!(app.hits.calendar_events.is_empty());
        assert_eq!(app.hits.detail, Rect::default());
    }

    #[test]
    fn calendar_window_is_not_truncated_by_summary_page_size() {
        let (temp, mut app) = calendar_app();
        let mut store = Store::open(temp.path()).unwrap();
        let mut event = store.get("outside").unwrap();
        for index in 0..PAGE_SIZE + 5 {
            event.id = format!("later-{index}");
            store.upsert(&event).unwrap();
        }
        app.reload().unwrap();
        assert_eq!(app.rows.len(), 3);
        assert!(app.rows.iter().any(|item| item.id == "standup"));
        for view in [calendar::CalendarView::Week, calendar::CalendarView::Month] {
            app.change_calendar_view(view).unwrap();
            assert_eq!(app.rows.len(), PAGE_SIZE + 9);
            app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
                .unwrap();
            render(&mut app, 120, 32);
            let selected = app.table.selected().unwrap();
            assert!(app
                .hits
                .calendar_events
                .iter()
                .any(|(_, index)| *index == selected));
        }
    }

    #[test]
    fn calendar_view_keys_cycle_filter_and_preserve_selection_and_search() {
        use calendar::CalendarView::{Month, ThreeDay, Week};
        let (_temp, mut app) = calendar_app();
        assert_eq!(app.calendar.view, ThreeDay);
        for (view, count) in [(Week, 4), (Month, 4), (ThreeDay, 3)] {
            app.key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
                .unwrap();
            assert_eq!(app.calendar.view, view);
            assert_eq!(app.rows.len(), count);
            assert_eq!(app.rows[app.table.selected().unwrap()].id, "standup");
        }
        for (key, view) in [('m', Month), ('d', ThreeDay), ('w', Week)] {
            app.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
                .unwrap();
            assert_eq!(app.calendar.view, view);
        }
        app.query = "Design".into();
        app.reload().unwrap();
        app.open_detail().unwrap();
        app.scroll = 10;
        app.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.query, "Design");
        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.rows[0].id, "review");
        assert_eq!(app.scroll, 0);
        assert!(rendered_text(&mut app, 120, 32, true).contains("Agenda for Design review"));

        app.query.clear();
        app.searching = true;
        for key in ['v', 'd', 'w', 'm'] {
            app.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(app.query, "vdwm");
        assert_eq!(app.calendar.view, Month);
        app.switch(0).unwrap();
        for key in ['v', 'd', 'w', 'm'] {
            app.key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(app.tab, 0);
        assert_eq!(app.calendar.view, Month);
        app.key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.tab, 1);
        assert_eq!(app.calendar.view, Month);
    }

    #[test]
    fn calendar_week_and_month_navigation_uses_window_units_and_empty_center() {
        use calendar::CalendarView::{Month, Week};
        let (_temp, mut app) = calendar_app();
        let initial = app.calendar.start;
        for (view, forward) in [
            (Week, initial + chrono::Duration::weeks(1)),
            (Month, chrono::NaiveDate::from_ymd_opt(2026, 11, 7).unwrap()),
        ] {
            app.calendar.start = initial;
            app.change_calendar_view(view).unwrap();
            for (next, previous) in [(']', '['), ('l', 'h')] {
                app.key(KeyEvent::new(KeyCode::Char(next), KeyModifiers::NONE))
                    .unwrap();
                assert_eq!(app.calendar.start, forward);
                assert!(app.rows.is_empty());
                assert_eq!(app.table.selected(), None);
                assert_eq!(app.calendar_center(), app.calendar.center());
                app.key(KeyEvent::new(KeyCode::Char(previous), KeyModifiers::NONE))
                    .unwrap();
                assert_eq!(app.calendar.start, initial);
                assert_eq!(app.rows.len(), 4);
            }
            render(&mut app, 120, 32);
            let next = app.hits.next;
            click(&mut app, next);
            assert_eq!(app.calendar.start, forward);
            let text = rendered_text(&mut app, 120, 32, false);
            assert!(text.contains(view.label()));
            assert!(text.contains("選取上方活動以查看詳情"));
            let previous = app.hits.previous;
            click(&mut app, previous);
            assert_eq!(app.calendar.start, initial);
        }
    }

    #[test]
    fn calendar_week_and_month_details_and_mouse_selection_work_at_narrow_sizes() {
        use calendar::CalendarView::{Month, Week};
        let (_temp, mut app) = calendar_app();
        for view in [Week, Month] {
            app.change_calendar_view(view).unwrap();
            for (width, height) in [(120, 32), (80, 24), (40, 24), (40, 20)] {
                let text = rendered_text(&mut app, width, height, false);
                assert!(text.contains(view.label()));
                assert!(text.contains("活動詳情"));
                assert!(app.hits.detail.y >= app.hits.list.bottom());
                assert!(!app.hits.calendar_events.is_empty());
                app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
                    .unwrap();
                render(&mut app, width, height);
                let index = app.table.selected().unwrap();
                let hit = app
                    .hits
                    .calendar_events
                    .iter()
                    .find(|(_, row)| *row == index)
                    .map(|(rect, _)| *rect);
                assert!(
                    hit.is_some(),
                    "selected event hidden in {view:?} at {width}x{height}"
                );
                click(&mut app, hit.unwrap());
                assert_eq!(app.rows[app.table.selected().unwrap()].id, "outside");
                app.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE))
                    .unwrap();
                let detail = rendered_text(&mut app, 120, 32, true);
                assert!(detail.contains("Sprint planning"));
                assert!(detail.contains("14:00") && detail.contains("16:00"));
                assert!(detail.contains("North meeting room"));
                assert!(detail.contains("Agenda for Sprint planning"));
                app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))
                    .unwrap();
            }
        }
    }

    #[test]
    fn calendar_week_overlap_lanes_mouse_select_their_own_detail_at_wide_and_narrow_sizes() {
        let (_temp, mut app) = calendar_app();
        app.change_calendar_view(calendar::CalendarView::Week)
            .unwrap();
        for (width, height) in [(120, 32), (80, 24), (40, 24)] {
            let text = rendered_text(&mut app, width, height, false);
            assert!(text.contains("08:00"), "missing time axis at width {width}");
            let hit = |id: &str| {
                let index = app.rows.iter().position(|item| item.id == id).unwrap();
                app.hits
                    .calendar_events
                    .iter()
                    .find(|(_, row)| *row == index)
                    .map(|(rect, _)| (*rect, index))
                    .unwrap()
            };
            let (standup, standup_index) = hit("standup");
            let (review, review_index) = hit("review");
            assert_ne!(standup.x, review.x, "overlap lanes merged at width {width}");
            assert!(standup.right() <= review.x || review.right() <= standup.x);
            assert!(standup.y < review.bottom() && review.y < standup.bottom());
            for (area, index, agenda) in [
                (standup, standup_index, "Agenda for Daily standup"),
                (review, review_index, "Agenda for Design review"),
            ] {
                render(&mut app, width, height);
                click(&mut app, area);
                assert_eq!(app.table.selected(), Some(index));
                assert_eq!(app.rows[index].text, agenda);
                let detail = rendered_text(&mut app, width, height, true);
                assert!(
                    detail.contains(&app.rows[index].title),
                    "wrong detail title at width {width}"
                );
                assert!(rendered_text(&mut app, 120, 32, true).contains(agenda));
            }
        }
    }

    #[test]
    fn calendar_shrinking_view_keeps_late_and_overnight_selected_events_visible() {
        use calendar::CalendarView::{Month, ThreeDay};
        let (temp, mut app) = calendar_app();
        let mut store = Store::open(temp.path()).unwrap();
        let mut event = store.get("outside").unwrap();
        for (id, start, end) in [
            (
                "late-month",
                "2026-10-31T09:00:00+08:00",
                "2026-10-31T10:00:00+08:00",
            ),
            (
                "overnight",
                "2026-09-30T23:00:00+08:00",
                "2026-10-01T01:00:00+08:00",
            ),
        ] {
            event.id = id.into();
            event.date = Some(start.into());
            event.end_date = Some(end.into());
            store.upsert(&event).unwrap();
        }
        app.change_calendar_view(Month).unwrap();
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.rows[app.table.selected().unwrap()].id, "late-month");
        app.change_calendar_view(ThreeDay).unwrap();
        assert_eq!(app.calendar.start.to_string(), "2026-10-31");
        assert_eq!(app.rows[app.table.selected().unwrap()].id, "late-month");
        app.change_calendar_view(Month).unwrap();
        app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.rows[app.table.selected().unwrap()].id, "overnight");
        app.change_calendar_view(ThreeDay).unwrap();
        assert_eq!(app.calendar.start.to_string(), "2026-10-01");
        assert_eq!(app.rows[app.table.selected().unwrap()].id, "overnight");
    }

    #[test]
    fn calendar_six_week_month_keeps_last_event_reachable_in_compact_grid() {
        let (temp, mut app) = calendar_app();
        let mut store = Store::open(temp.path()).unwrap();
        let mut event = store.get("outside").unwrap();
        event.id = "month-end".into();
        event.date = Some("2026-03-31T09:00:00+08:00".into());
        event.end_date = Some("2026-03-31T10:00:00+08:00".into());
        store.upsert(&event).unwrap();
        app.calendar.start = chrono::NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        app.table.select(None);
        app.change_calendar_view(calendar::CalendarView::Month)
            .unwrap();
        for (width, height) in [(120, 32), (40, 24), (40, 20)] {
            app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
                .unwrap();
            let text = rendered_text(&mut app, width, height, false);
            assert!(text.contains("2026-03"));
            assert!(text.contains("31"));
            if height == 20 {
                assert!(text.contains("週 6–6/6"));
            } else if height == 24 {
                assert!(text.contains("週 2–6/6"));
            } else {
                assert!(!text.contains("週 2–6/6") && !text.contains("週 6–6/6"));
            }
            assert_eq!(app.rows[app.table.selected().unwrap()].id, "month-end");
            assert!(app
                .hits
                .calendar_events
                .iter()
                .any(|(_, index)| *index == 0));
        }
    }

    #[test]
    fn pages_and_detail_bodies_stay_bounded_across_navigation() {
        let (_temp, mut app) = populated_app(205);
        assert_eq!(app.total, 207);
        assert_eq!(app.rows.len(), PAGE_SIZE);
        assert!(app
            .rows
            .iter()
            .all(|item| item.raw.is_empty() && item.text.is_empty()));
        let selected = app
            .rows
            .iter()
            .position(|row| row.id.starts_with("paged-"))
            .unwrap();
        app.table.select(Some(selected));
        app.open_detail().unwrap();
        assert!(!app.rows[selected].raw.is_empty());
        app.table.select(Some(selected + 1));
        app.open_detail().unwrap();
        assert!(app.rows[selected].raw.is_empty());
        assert_eq!(app.rows.iter().filter(|row| !row.raw.is_empty()).count(), 1);
        app.close_detail();
        assert!(app
            .rows
            .iter()
            .all(|row| row.raw.capacity() == 0 && row.text.capacity() == 0));
        app.change_page(1).unwrap();
        assert_eq!(app.page, 1);
        assert_eq!(app.rows.len(), PAGE_SIZE);
        assert!(app
            .rows
            .iter()
            .all(|item| item.raw.is_empty() && item.text.is_empty()));
        app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.page, 2);
        assert_eq!(app.rows.len(), 7);
        app.change_page(1).unwrap();
        assert_eq!(app.page, 2);
        app.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.page, 0);
    }

    #[test]
    fn mouse_uses_rendered_offsets_and_hits_pages_folders_tabs_search() {
        let (_temp, mut app) = populated_app(205);
        app.table.select(Some(80));
        render(&mut app, 120, 32);
        let offset = app.table.offset();
        assert!(offset > 0);
        let area = app.hits.list;
        click(&mut app, area);
        assert_eq!(app.table.selected(), Some(offset));
        assert!(app.detail);
        app.close_detail();
        render(&mut app, 120, 32);
        let next = app.hits.next;
        click(&mut app, next);
        assert_eq!(app.page, 1);
        render(&mut app, 120, 32);
        let search = app.hits.search;
        click(&mut app, search);
        assert!(app.searching);
        app.key(KeyEvent::new(KeyCode::Char('分'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.page, 0);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.total, 205);
        render(&mut app, 120, 32);
        let folder = Rect {
            y: app.hits.folders.y + 1,
            ..app.hits.folders
        };
        click(&mut app, folder);
        assert_eq!(app.collection, 1);
        render(&mut app, 120, 32);
        let tab = app.hits.tabs[3].0;
        click(&mut app, tab);
        assert_eq!(app.tab, 3);
        render(&mut app, 120, 32);
        let tab = app.hits.tabs[0].0;
        click(&mut app, tab);
        assert_eq!(app.tab, 0);
        assert!(app.query.is_empty());
    }

    #[test]
    fn late_search_results_are_discarded_and_errors_keep_local_results() {
        let (_temp, mut app) = populated_app(2);
        app.query = "分頁".into();
        app.invalidate_search();
        app.reload().unwrap();
        assert_eq!(app.total, 2);
        let (tx, rx) = mpsc::channel();
        app.pending_search = Some(SearchJob {
            generation: app.search_generation,
            receiver: rx,
        });
        app.query = "不存在".into();
        app.invalidate_search();
        app.reload().unwrap();
        tx.send(Ok(mail::SearchResults {
            items: vec![],
            truncated: false,
        }))
        .unwrap();
        app.poll_background().unwrap();
        assert!(app.remote.is_empty());
        assert_eq!(app.total, 0);
        app.query = "分頁".into();
        app.invalidate_search();
        app.reload().unwrap();
        let (tx, rx) = mpsc::channel();
        app.pending_search = Some(SearchJob {
            generation: app.search_generation,
            receiver: rx,
        });
        tx.send(Err(anyhow::anyhow!("network unavailable")))
            .unwrap();
        app.poll_background().unwrap();
        assert_eq!(app.total, 2);
        assert!(app.status.contains("本機結果可用"));
    }

    #[test]
    fn resizing_clears_invisible_hit_regions_in_every_tab() {
        let (_temp, mut app) = populated_app(2);
        for tab in 0..4 {
            app.switch(tab).unwrap();
            for (width, height) in [(120, 32), (80, 24), (64, 12), (40, 12), (39, 11)] {
                render(&mut app, width, height);
            }
            assert!(app.hits.tabs.is_empty());
            assert_eq!(app.hits.list, Rect::default());
            assert_eq!(app.hits.detail, Rect::default());
        }
    }

    #[test]
    fn hybrid_completion_merges_cached_and_uncached_hits_without_bodies() {
        let (temp, mut app) = populated_app(2);
        app.query = "分頁".into();
        app.invalidate_search();
        app.reload().unwrap();
        let store = Store::open(temp.path()).unwrap();
        let mut cached = store.get("paged-0000").unwrap();
        cached.text = String::new();
        cached.raw = String::new();
        let mut remote = cached.clone();
        remote.id = "mail:INBOX:1:900".into();
        remote.remote_id = "900".into();
        remote.title = "伺服器依正文匹配的郵件".into();
        let remote_id = remote.id.clone();
        let (tx, rx) = mpsc::channel();
        app.pending_search = Some(SearchJob {
            generation: app.search_generation,
            receiver: rx,
        });
        tx.send(Ok(mail::SearchResults {
            items: vec![cached, remote],
            truncated: true,
        }))
        .unwrap();
        app.poll_background().unwrap();
        assert_eq!(app.total, 3);
        assert!(app.remote_truncated);
        assert!(app
            .rows
            .iter()
            .all(|row| row.text.is_empty() && row.raw.is_empty()));
        assert!(!store.get("paged-0000").unwrap().raw.is_empty());
        assert!(store.get(&remote_id).is_err());
        app.config = None;
        app.table
            .select(app.rows.iter().position(|row| row.id == remote_id));
        app.open_detail().unwrap();
        assert!(app.status.contains("離線"));
        assert!(app.pending_body.is_none());
    }

    #[test]
    fn typing_debounces_local_scan_and_enter_flushes_it() {
        let (_temp, mut app) = populated_app(2);
        let total = app.total;
        app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        for c in "不存在".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(app.total, total);
        assert!(app.local_due.is_some());
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.total, 0);
        assert!(app.local_due.is_none());
        assert!(app.status.contains("展示模式"));
    }

    #[test]
    fn closing_detail_discards_late_body_and_tiny_room_mouse_is_ignored() {
        let (temp, mut app) = populated_app(2);
        app.table
            .select(app.rows.iter().position(|row| row.id == "paged-0000"));
        app.open_detail().unwrap();
        let item = Store::open(temp.path()).unwrap().get("paged-0000").unwrap();
        let (tx, rx) = mpsc::channel();
        app.pending_body = Some(BodyJob {
            generation: app.body_generation,
            id: item.id.clone(),
            receiver: rx,
        });
        app.close_detail();
        tx.send(Ok(item)).unwrap();
        app.poll_background().unwrap();
        assert!(app
            .rows
            .iter()
            .all(|row| row.raw.capacity() == 0 && row.text.capacity() == 0));
        app.switch(3).unwrap();
        render(&mut app, 120, 32);
        render(&mut app, 39, 11);
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 9,
            modifiers: KeyModifiers::NONE,
        })
        .unwrap();
        assert_eq!(app.tab, 3);
        assert!(app.hits.tabs.is_empty());
    }

    fn demo_calendar_app() -> (tempfile::TempDir, App) {
        let temp = tempdir().unwrap();
        crate::demo::seed(temp.path()).unwrap();
        let mut app = App::new(temp.path()).unwrap();
        type_keys(&mut app, &[KeyCode::Char('2')]);
        (temp, app)
    }

    fn select_titled(app: &mut App, title: &str) {
        let index = app.rows.iter().position(|row| row.title == title).unwrap();
        app.select_calendar_event(index).unwrap();
    }

    #[test]
    fn calendar_keys_n_e_x_do_not_collide_and_need_the_calendar_tab() {
        let (_temp, mut app) = demo_calendar_app();
        type_keys(
            &mut app,
            &[KeyCode::Char('1'), KeyCode::Char('n'), KeyCode::Char('e')],
        );
        assert!(app.event_form.is_none(), "mail tab keeps n/e/x free");
        type_keys(&mut app, &[KeyCode::Char('2')]);
        app.table.select(None);
        type_keys(&mut app, &[KeyCode::Char('e')]);
        assert!(app.event_form.is_none() && app.status.contains("先用 j/k"));
        type_keys(&mut app, &[KeyCode::Char('n')]);
        let form = app.event_form.as_ref().expect("create form");
        assert_eq!(form.mode, event_form::Mode::Create);
        type_keys(&mut app, &[KeyCode::Char('d'), KeyCode::Char('w')]);
        assert_eq!(
            app.calendar.view,
            calendar::CalendarView::ThreeDay,
            "form swallows view keys"
        );
        assert_eq!(app.event_form.as_ref().unwrap().fields[0], "dw");
        type_keys(&mut app, &[KeyCode::Esc]);
        assert!(app.event_form.is_none());
    }

    #[test]
    fn room_calendar_events_are_refused_with_a_reason() {
        let (_temp, mut app) = demo_calendar_app();
        select_titled(&mut app, "下午簡報");
        type_keys(&mut app, &[KeyCode::Char('e')]);
        assert!(app.event_form.is_none());
        assert!(
            app.status.contains("無法編輯") && app.status.contains("會議室"),
            "{}",
            app.status
        );
        type_keys(&mut app, &[KeyCode::Char('x')]);
        assert!(app.event_form.is_none() && app.status.contains("無法刪除"));
    }

    #[test]
    fn demo_edit_and_delete_reach_confirm_but_never_write() {
        let (temp, mut app) = demo_calendar_app();
        select_titled(&mut app, "m2k 骨架驗收");
        type_keys(&mut app, &[KeyCode::Char('e')]);
        assert_eq!(app.event_form.as_ref().unwrap().fields[0], "m2k 骨架驗收");
        type_keys(&mut app, &[KeyCode::Char('!'), KeyCode::Enter]);
        render(&mut app, 120, 32);
        assert_eq!(
            app.event_form.as_ref().unwrap().phase,
            event_form::Phase::Confirm
        );
        type_keys(&mut app, &[KeyCode::Char('y')]);
        let form = app.event_form.as_ref().unwrap();
        assert!(app.pending_event.is_none(), "demo must not start a write");
        assert!(form.notice.as_deref().unwrap().contains("展示資料"));
        type_keys(&mut app, &[KeyCode::Esc, KeyCode::Esc]);
        assert!(app.event_form.is_none());
        type_keys(&mut app, &[KeyCode::Char('x'), KeyCode::Char('y')]);
        assert!(app.pending_event.is_none());
        type_keys(&mut app, &[KeyCode::Esc]);
        let stored = Store::open_reader(temp.path())
            .unwrap()
            .get("demo-event-1")
            .unwrap();
        assert_eq!(stored.title, "m2k 骨架驗收", "nothing changed locally");
    }

    /// 行事曆寫入完成前持續 poll，模擬事件迴圈。
    fn settle_event(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while app.pending_event.is_some() {
            assert!(Instant::now() < deadline, "calendar write did not finish");
            app.poll_event().unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn fixture_events(info: &fixture::Info, kind: &str) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&info.events)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["kind"] == kind)
            .collect()
    }

    fn put_body(info: &fixture::Info, event: &serde_json::Value) -> String {
        std::fs::read_to_string(info.data_dir.join(event["file"].as_str().unwrap())).unwrap()
    }

    #[test]
    fn tui_creates_edits_and_deletes_against_fixture_with_etag_guard() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        const PASSWORD: &str = "isolated-fixture-password";
        std::env::set_var("M2K_PASSWORD", PASSWORD);
        let root = tempdir().unwrap();
        let info = fixture::start(fixture::Options {
            data_dir: root.path().join("fixture"),
            user: "fixture@example.test".into(),
            password: PASSWORD.into(),
            imap_port: 0,
            smtp_port: 0,
            http_port: 0,
            sync_collection: false,
        })
        .unwrap();
        let dir = root.path().join("client");
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config::with_site_profile(info.user.clone(), &info.server, None).unwrap();
        config::save(&dir, &cfg).unwrap();
        Store::open(&dir).unwrap().ensure_account(&cfg).unwrap();
        sync::run(&dir, &[Kind::Calendar]).unwrap();
        let mut app = App::new(&dir).unwrap();
        app.switch(1).unwrap();

        // 新增：表單 → 確認 → y，伺服器收到不含 METHOD 的新 ICS。
        type_keys(&mut app, &[KeyCode::Char('n')]);
        for c in "TUI 新增".chars() {
            type_keys(&mut app, &[KeyCode::Char(c)]);
        }
        type_keys(&mut app, &[KeyCode::Enter]);
        assert_eq!(
            app.event_form.as_ref().unwrap().phase,
            event_form::Phase::Confirm
        );
        assert!(
            fixture_events(&info, "dav.put").is_empty(),
            "nothing written before y"
        );
        type_keys(&mut app, &[KeyCode::Char('y')]);
        settle_event(&mut app);
        assert!(app.event_form.is_none(), "{}", app.status);
        assert!(app.status.contains("已新增「TUI 新增」"), "{}", app.status);
        let puts = fixture_events(&info, "dav.put");
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0]["created"], true);
        let created = put_body(&info, &puts[0]);
        assert!(created.contains("SUMMARY:TUI 新增") && !created.contains("METHOD"));
        let uid = puts[0]["uid"].as_str().unwrap().to_owned();
        let selected = app.rows[app.table.selected().expect("new event selected")].clone();
        assert_eq!(selected.title, "TUI 新增");

        // 編輯：只改地點，保留 UID 與標題，用剛取得的 ETag。
        type_keys(
            &mut app,
            &[KeyCode::Char('e'), KeyCode::Tab, KeyCode::Tab, KeyCode::Tab],
        );
        for c in "B 室".chars() {
            type_keys(&mut app, &[KeyCode::Char(c)]);
        }
        type_keys(&mut app, &[KeyCode::Enter, KeyCode::Char('y')]);
        settle_event(&mut app);
        assert!(app.status.contains("已更新"), "{}", app.status);
        let puts = fixture_events(&info, "dav.put");
        assert_eq!(puts.len(), 2);
        assert_eq!(
            (puts[1]["created"].clone(), puts[1]["uid"].as_str()),
            (false.into(), Some(uid.as_str()))
        );
        let updated = put_body(&info, &puts[1]);
        assert!(updated.contains("LOCATION:B 室") && updated.contains("SUMMARY:TUI 新增"));
        assert!(updated.contains(
            &created
                .lines()
                .find(|l| l.starts_with("DTSTART"))
                .unwrap()
                .to_owned()
        ));

        // 別的客戶端先改了伺服器版本：刪除用舊 ETag，必須被拒絕且保留表單。
        let current = Store::open_reader(&dir).unwrap().get(&selected.id).unwrap();
        let response = reqwest::blocking::Client::new()
            .put(&current.remote_id)
            .basic_auth(&info.user, Some(PASSWORD))
            .header("If-Match", current.etag.clone().unwrap())
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(updated.replace("B 室", "C 室"))
            .send()
            .unwrap();
        assert!(response.status().is_success());

        // 舊 ETag 的編輯被拒絕：輸入保留，更新後再按 e 只帶回自己改的標題。
        let puts_before = fixture_events(&info, "dav.put").len();
        type_keys(&mut app, &[KeyCode::Char('e')]);
        for c in "改".chars() {
            type_keys(&mut app, &[KeyCode::Char(c)]);
        }
        type_keys(&mut app, &[KeyCode::Enter, KeyCode::Char('y')]);
        settle_event(&mut app);
        let notice = app.event_form.as_ref().unwrap().notice.clone().unwrap();
        assert!(
            notice.contains("412") && notice.contains("再按 e"),
            "{notice}"
        );
        assert_eq!(
            fixture_events(&info, "dav.put").len(),
            puts_before,
            "no PUT went through"
        );
        type_keys(&mut app, &[KeyCode::Esc, KeyCode::Esc]);
        assert!(
            app.event_form.is_none() && app.status.contains("輸入已保留"),
            "{}",
            app.status
        );
        sync::run(&dir, &[Kind::Calendar]).unwrap();
        app.reload().unwrap();
        let index = app
            .rows
            .iter()
            .position(|row| row.id == selected.id)
            .unwrap();
        app.select_calendar_event(index).unwrap();
        type_keys(&mut app, &[KeyCode::Char('e')]);
        let form = app.event_form.as_ref().unwrap();
        assert_eq!(form.fields[0], "TUI 新增改");
        assert_eq!(
            form.fields[3], "C 室",
            "the other client's location is not overwritten"
        );
        type_keys(&mut app, &[KeyCode::Esc]);
        assert!(app.kept_event.is_none());

        // 刪除用過期 ETag 同樣被拒絕。
        let current = Store::open_reader(&dir).unwrap().get(&selected.id).unwrap();
        reqwest::blocking::Client::new()
            .put(&current.remote_id)
            .basic_auth(&info.user, Some(PASSWORD))
            .header("If-Match", current.etag.clone().unwrap())
            .body(current.raw.replace("C 室", "D 室"))
            .send()
            .unwrap()
            .error_for_status()
            .unwrap();
        type_keys(&mut app, &[KeyCode::Char('x'), KeyCode::Char('y')]);
        settle_event(&mut app);
        let form = app.event_form.as_ref().expect("form kept after failure");
        assert_eq!(form.phase, event_form::Phase::Confirm);
        assert!(
            form.notice.as_deref().unwrap().contains("412"),
            "{:?}",
            form.notice
        );
        assert!(fixture_events(&info, "dav.delete").is_empty());
        type_keys(&mut app, &[KeyCode::Esc]);

        // 更新後再刪除：伺服器收到 DELETE，本機快取移除。
        sync::run(&dir, &[Kind::Calendar]).unwrap();
        app.reload().unwrap();
        let index = app
            .rows
            .iter()
            .position(|row| row.id == selected.id)
            .unwrap();
        app.select_calendar_event(index).unwrap();
        type_keys(&mut app, &[KeyCode::Char('x'), KeyCode::Char('y')]);
        settle_event(&mut app);
        assert!(app.status.contains("已刪除"), "{}", app.status);
        assert_eq!(fixture_events(&info, "dav.delete").len(), 1);
        assert!(Store::open_reader(&dir).unwrap().get(&selected.id).is_err());
        std::env::remove_var("M2K_PASSWORD");
    }

    #[test]
    fn open_event_form_blocks_mouse_and_ctrl_c_waits_for_write_or_saves_a_draft() {
        let (temp, mut app) = demo_calendar_app();
        render(&mut app, 120, 32);
        type_keys(&mut app, &[KeyCode::Char('n')]);
        let tab = app.hits.tabs.iter().find(|(_, tab)| *tab == 0).unwrap().0;
        click(&mut app, tab);
        assert_eq!(app.tab, 1, "clicks do not reach the tabs behind the form");
        assert!(app.event_form.is_some());

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.event_form.as_mut().unwrap().phase = event_form::Phase::Writing;
        assert!(!app.key(ctrl_c).unwrap(), "Ctrl-C must wait while writing");
        assert!(app.status.contains("寫入中"));

        app.event_form.as_mut().unwrap().phase = event_form::Phase::Editing;
        type_keys(&mut app, &[KeyCode::Char('草'), KeyCode::Char('稿')]);
        assert!(app.key(ctrl_c).unwrap(), "Ctrl-C quits after saving");
        let drafts = std::fs::read_dir(temp.path().join("drafts"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("event-")
            })
            .collect::<Vec<_>>();
        assert_eq!(drafts.len(), 1);
        assert!(std::fs::read_to_string(&drafts[0])
            .unwrap()
            .contains("標題: 草稿"));
    }
}
