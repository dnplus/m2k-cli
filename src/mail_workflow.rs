use crate::{
    config::{self, Config},
    model::{Item, Kind, Snapshot},
    provider,
    store::Store,
    sync,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Subcommand, ValueEnum};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const PLAN_VERSION: u8 = 1;
const PLAN_TTL_SECONDS: i64 = 30 * 60;
const SIGNING_KEY_FILE: &str = ".mail-cleanup-signing-key";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum CleanupAction {
    Delete,
    Move,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanupCriteria {
    pub query: Option<String>,
    pub folder: Option<String>,
    pub before: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanupIdentity {
    pub username: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub caldav_url: String,
    pub carddav_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanupSelection {
    pub folder: String,
    pub uid_validity: u32,
    pub selected_ids: Vec<String>,
    pub selected_uids: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CleanupPlan {
    pub version: u8,
    pub created_at: String,
    pub expires_at: String,
    pub account: CleanupIdentity,
    pub folder: Option<String>,
    pub uid_validity: Option<u32>,
    pub selected_ids: Vec<String>,
    pub selected_uids: Vec<u32>,
    pub criteria: CleanupCriteria,
    pub matched_count: usize,
    pub action: CleanupAction,
    pub destination: Option<String>,
    pub selections: Vec<CleanupSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SignedCleanupPlan {
    payload: CleanupPlan,
    signature: String,
}

#[derive(Subcommand, Debug)]
pub enum MailWorkflowCommand {
    #[command(
        name = "thread",
        about = "讀取同主旨最新郵件；--full 包含歷史郵件與引用"
    )]
    Thread {
        #[arg(long, conflicts_with = "subject", required_unless_present = "subject")]
        id: Option<String>,
        #[arg(long, conflicts_with = "id", required_unless_present = "id")]
        subject: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, help = "顯示歷史郵件及完整引用正文；最多 --limit 封")]
        full: bool,
    },
    #[command(name = "cleanup-preview", about = "在本機預覽郵件清理並建立簽署方案")]
    CleanupPreview {
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        before: Option<String>,
        #[arg(long, value_enum, default_value_t = CleanupAction::Delete)]
        action: CleanupAction,
        #[arg(long)]
        destination: Option<String>,
        #[arg(long, required = true)]
        output: PathBuf,
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    #[command(name = "cleanup-execute", about = "驗證簽署方案後執行郵件清理")]
    CleanupExecute {
        #[arg(long, required = true)]
        plan: PathBuf,
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Serialize)]
struct CleanupPreviewOutput<'a> {
    plan: &'a Path,
    criteria: &'a CleanupCriteria,
    action: CleanupAction,
    destination: Option<&'a String>,
    matched_count: usize,
    selected_count: usize,
    folders: Vec<CleanupSelectionOutput>,
}

#[derive(Debug, Serialize)]
struct CleanupSelectionOutput {
    folder: String,
    uid_validity: u32,
    count: usize,
    uids: Vec<u32>,
}

#[derive(Debug, Serialize)]
struct CleanupExecuteOutput {
    action: CleanupAction,
    destination: Option<String>,
    matched_count: usize,
    planned_count: usize,
    affected_count: usize,
    affected_remote_count: usize,
    failed_count: usize,
    cache_error_count: usize,
    partial: bool,
    folders: Vec<CleanupExecuteFolderOutput>,
    failures: Vec<CleanupFailureOutput>,
    cache_errors: Vec<CleanupCacheErrorOutput>,
}

#[derive(Debug, Serialize)]
struct CleanupExecuteFolderOutput {
    folder: String,
    planned_count: usize,
    affected_count: usize,
    affected_remote_count: usize,
    failed_count: usize,
    cache_error_count: usize,
}

#[derive(Debug, Serialize)]
struct CleanupFailureOutput {
    id: String,
    folder: String,
    uid: u32,
    error: String,
}

#[derive(Debug, Serialize)]
struct CleanupCacheErrorOutput {
    id: String,
    folder: String,
    uid: u32,
    error: String,
    remediation: &'static str,
}

pub fn execute(dir: &Path, command: MailWorkflowCommand, json: bool) -> Result<()> {
    match command {
        MailWorkflowCommand::Thread {
            id,
            subject,
            limit,
            full,
        } => execute_thread(dir, id.as_deref(), subject.as_deref(), limit, full, json),
        MailWorkflowCommand::CleanupPreview {
            query,
            folder,
            before,
            action,
            destination,
            output,
            limit,
        } => execute_cleanup_preview(
            dir,
            CleanupPreviewArgs {
                query,
                folder,
                before,
                action,
                destination,
                output,
                limit,
            },
            json,
        ),
        MailWorkflowCommand::CleanupExecute { plan, apply } => {
            execute_cleanup(dir, &plan, apply, json)
        }
    }
}

struct CleanupPreviewArgs {
    query: Option<String>,
    folder: Option<String>,
    before: Option<String>,
    action: CleanupAction,
    destination: Option<String>,
    output: PathBuf,
    limit: usize,
}

fn execute_thread(
    dir: &Path,
    id: Option<&str>,
    subject: Option<&str>,
    limit: usize,
    full: bool,
    json: bool,
) -> Result<()> {
    if limit == 0 {
        bail!("thread limit 必須大於 0");
    }
    let store = Store::open(dir)?;
    let target = if let Some(id) = id {
        let item = store.get(id.trim())?;
        if item.kind != Kind::Mail {
            bail!("項目不是郵件：{id}");
        }
        provider::mail::display_header(&item.title)
    } else {
        subject.unwrap_or_default().trim().to_owned()
    };
    let normalized = normalize_thread_subject(&target);
    if normalized.is_empty() {
        bail!("thread 主旨不可為空白");
    }
    let mut items = store
        .list_summaries(Kind::Mail, None, None, usize::MAX)?
        .into_iter()
        .filter(|item| {
            normalize_thread_subject(&provider::mail::display_header(&item.title)) == normalized
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        let left_date = left.date.as_deref().and_then(parse_timestamp);
        let right_date = right.date.as_deref().and_then(parse_timestamp);
        right_date
            .cmp(&left_date)
            .then_with(|| right.collection.cmp(&left.collection))
            .then_with(|| right.uid_validity.cmp(&left.uid_validity))
            .then_with(|| {
                right
                    .remote_id
                    .parse::<u32>()
                    .ok()
                    .cmp(&left.remote_id.parse::<u32>().ok())
            })
            .then_with(|| right.id.cmp(&left.id))
    });
    items.truncate(if full { limit } else { 1 });
    for item in &mut items {
        *item = store.get(&item.id)?;
        provider::mail::normalize_cached(item)?;
        if !full {
            item.text = crate::mail_read::latest_reply(&item.text);
            item.raw.clear();
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "subject": target,
                "normalized_subject": normalized,
                "count": items.len(),
                "first_date": items.iter().filter_map(|item| item.date.as_deref())
                    .filter(|date| parse_timestamp(date).is_some()).min_by_key(|date| parse_timestamp(date)),
                "last_date": items.iter().filter_map(|item| item.date.as_deref())
                    .filter(|date| parse_timestamp(date).is_some()).max_by_key(|date| parse_timestamp(date)),
                "items": items,
            }))?
        );
    } else {
        for heading in [
            format!("主旨：{target}"),
            format!("正規化主旨：{normalized}"),
            format!("thread 共 {} 封", items.len()),
        ] {
            println!("{}", crate::plain_output::row(&[(&heading, 118)]));
        }
        for (index, item) in items.iter().enumerate() {
            println!(
                "{}",
                crate::plain_output::row(&[
                    (&(index + 1).to_string(), 4),
                    (item.date.as_deref().unwrap_or("無日期"), 25),
                    (&item.subtitle, 30),
                    (&item.title, 56),
                ])
            );
            if !item.text.trim().is_empty() {
                println!("   {}", item.text.trim());
            }
        }
    }
    Ok(())
}

pub fn normalize_thread_subject(value: &str) -> String {
    let mut subject = value.trim().to_owned();
    loop {
        let lower = subject.to_ascii_lowercase();
        let prefix_length = if lower.starts_with("re:") {
            Some(3)
        } else if lower.starts_with("fwd:") {
            Some(4)
        } else if lower.starts_with("fw:") {
            Some(3)
        } else {
            None
        };
        let Some(prefix_length) = prefix_length else {
            break;
        };
        subject = subject[prefix_length..].trim_start().to_owned();
    }
    subject
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn execute_cleanup_preview(dir: &Path, args: CleanupPreviewArgs, _json: bool) -> Result<()> {
    let _lock = sync::lock(dir)?;
    let config = config::load(dir)?;
    let mut store = Store::open(dir)?;
    store.ensure_account(&config)?;
    let criteria = normalize_criteria(args.query, args.folder, args.before, args.limit)?;
    validate_action(args.action, args.destination.as_deref())?;
    let items = store.list(Kind::Mail, None, None, usize::MAX)?;
    let mut candidates = items
        .iter()
        .filter(|item| item_matches_criteria(item, &criteria))
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by(compare_dates_asc);
    let matched_count = candidates.len();
    if criteria.limit > 0 {
        candidates.truncate(criteria.limit);
    }
    let selections = build_selections(&candidates)?;
    if args.action == CleanupAction::Move {
        let destination = args.destination.as_deref().unwrap_or_default();
        if selections
            .iter()
            .any(|selection| selection.folder.eq_ignore_ascii_case(destination))
        {
            bail!("move destination 不可與來源資料夾相同");
        }
    }
    let now = Utc::now();
    let plan = CleanupPlan {
        version: PLAN_VERSION,
        created_at: now.to_rfc3339(),
        expires_at: (now + chrono::Duration::seconds(PLAN_TTL_SECONDS)).to_rfc3339(),
        account: CleanupIdentity::from_config(&config),
        folder: single_folder(&selections),
        uid_validity: single_uid_validity(&selections),
        selected_ids: selections
            .iter()
            .flat_map(|selection| selection.selected_ids.iter().cloned())
            .collect(),
        selected_uids: selections
            .iter()
            .flat_map(|selection| selection.selected_uids.iter().copied())
            .collect(),
        criteria,
        matched_count,
        action: args.action,
        destination: args.destination,
        selections,
    };
    let signed = sign_plan(dir, plan.clone())?;
    write_new_plan(&args.output, &signed)?;
    let output = CleanupPreviewOutput {
        plan: &args.output,
        criteria: &plan.criteria,
        action: plan.action,
        destination: plan.destination.as_ref(),
        matched_count: plan.matched_count,
        selected_count: plan.selected_ids.len(),
        folders: plan
            .selections
            .iter()
            .map(|selection| CleanupSelectionOutput {
                folder: selection.folder.clone(),
                uid_validity: selection.uid_validity,
                count: selection.selected_ids.len(),
                uids: selection.selected_uids.clone(),
            })
            .collect(),
    };
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn execute_cleanup(dir: &Path, plan_path: &Path, apply: bool, _json: bool) -> Result<()> {
    let plan = read_cleanup_plan(dir, plan_path)?;
    if !apply {
        bail!("清理執行需要明確指定 --apply");
    }
    let _lock = sync::lock(dir)?;
    if dir.join(".demo").exists() {
        bail!("展示資料可以離線預覽，但拒絕執行遠端清理");
    }
    let config = config::load(dir)?;
    let mut store = Store::open(dir)?;
    store.ensure_account(&config)?;
    validate_cleanup_plan_account(&plan, &config)?;
    let password = config::password(dir, &config)?;
    let existing = store.list(Kind::Mail, None, None, usize::MAX)?;
    let snapshot = provider::mail::sync(&config, &password, &existing)
        .map_err(|error| sanitize_error(error, &password))?;
    validate_cleanup_snapshot(&plan, &snapshot)?;
    let mut result = CleanupExecuteOutput {
        action: plan.action,
        destination: plan.destination.clone(),
        matched_count: plan.matched_count,
        planned_count: plan.selected_ids.len(),
        affected_count: 0,
        affected_remote_count: 0,
        failed_count: 0,
        cache_error_count: 0,
        partial: false,
        folders: Vec::new(),
        failures: Vec::new(),
        cache_errors: Vec::new(),
    };
    let items_by_id = snapshot
        .items
        .iter()
        .map(|item| (item.id.clone(), item))
        .collect::<HashMap<_, _>>();
    for selection in &plan.selections {
        let mut folder_result = CleanupExecuteFolderOutput {
            folder: selection.folder.clone(),
            planned_count: selection.selected_ids.len(),
            affected_count: 0,
            affected_remote_count: 0,
            failed_count: 0,
            cache_error_count: 0,
        };
        for id in &selection.selected_ids {
            let item = items_by_id
                .get(id)
                .ok_or_else(|| anyhow!("驗證後找不到郵件：{id}"))?;
            let operation = match plan.action {
                CleanupAction::Delete => provider::mail::delete(&config, &password, item),
                CleanupAction::Move => provider::mail::move_to(
                    &config,
                    &password,
                    item,
                    plan.destination.as_deref().unwrap_or_default(),
                ),
            };
            match operation {
                Ok(()) => {
                    result.affected_count += 1;
                    result.affected_remote_count += 1;
                    folder_result.affected_count += 1;
                    folder_result.affected_remote_count += 1;
                    if let Err(error) = store.remove(id) {
                        result.cache_error_count += 1;
                        folder_result.cache_error_count += 1;
                        result.cache_errors.push(CleanupCacheErrorOutput {
                            id: id.clone(),
                            folder: selection.folder.clone(),
                            uid: item.remote_id.parse().unwrap_or_default(),
                            error: error.to_string(),
                            remediation:
                                "遠端已變更；請先重新 sync，再檢查本機快取後手動移除殘留項目",
                        });
                    }
                }
                Err(error) => {
                    let safe_error = sanitize_error(error, &password);
                    result.failed_count += 1;
                    folder_result.failed_count += 1;
                    result.failures.push(CleanupFailureOutput {
                        id: id.clone(),
                        folder: selection.folder.clone(),
                        uid: item.remote_id.parse().unwrap_or_default(),
                        error: safe_error.to_string(),
                    });
                }
            }
        }
        result.folders.push(folder_result);
    }
    result.partial = result.failed_count > 0 || result.cache_error_count > 0;
    println!("{}", serde_json::to_string_pretty(&result)?);
    if result.failed_count > 0 || result.cache_error_count > 0 {
        bail!(
            "清理部分完成：遠端影響 {} 封，遠端失敗 {} 封，本機快取移除失敗 {} 封；未自動重試",
            result.affected_remote_count,
            result.failed_count,
            result.cache_error_count
        );
    }
    Ok(())
}

impl CleanupIdentity {
    fn from_config(config: &Config) -> Self {
        Self {
            username: config.username.clone(),
            email: config.email.clone(),
            imap_host: config.imap_host.clone(),
            imap_port: config.imap_port,
            smtp_host: config.smtp_host.clone(),
            smtp_port: config.smtp_port,
            caldav_url: config.caldav_url.clone(),
            carddav_url: config.carddav_url.clone(),
        }
    }
}

pub fn validate_cleanup_plan_account(plan: &CleanupPlan, config: &Config) -> Result<()> {
    if plan.account != CleanupIdentity::from_config(config) {
        bail!("清理方案帳號或 IMAP 端點與目前設定不符，請重新 preview");
    }
    Ok(())
}

pub fn validate_cleanup_snapshot(plan: &CleanupPlan, snapshot: &Snapshot) -> Result<()> {
    if snapshot.kind != Kind::Mail {
        bail!("遠端快照不是郵件資料");
    }
    validate_cleanup_plan(plan)?;
    let mut candidates = snapshot
        .items
        .iter()
        .filter(|item| item_matches_criteria(item, &plan.criteria))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| compare_dates_asc(left, right));
    if candidates.len() != plan.matched_count {
        bail!(
            "清理符合數量已變更：方案 {}，目前 {}；請重新 preview",
            plan.matched_count,
            candidates.len()
        );
    }
    if plan.criteria.limit > 0 {
        candidates.truncate(plan.criteria.limit);
    }
    let current_ids = candidates
        .iter()
        .map(|item| item.id.as_str())
        .collect::<BTreeSet<_>>();
    let planned_ids = plan
        .selected_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if current_ids != planned_ids {
        bail!("清理郵件 UID 或範圍已變更，請重新 preview");
    }
    let current_uids = candidates
        .iter()
        .map(|item| item.remote_id.parse::<u32>())
        .collect::<std::result::Result<BTreeSet<_>, _>>()
        .context("遠端快照包含無效郵件 UID")?;
    let planned_uids = plan.selected_uids.iter().copied().collect::<BTreeSet<_>>();
    if current_uids != planned_uids {
        bail!("清理郵件 UID 集合已變更，請重新 preview");
    }
    let mut current_by_id = HashMap::new();
    for item in candidates {
        current_by_id.insert(item.id.as_str(), item);
    }
    for selection in &plan.selections {
        let mut ids = BTreeSet::new();
        let mut uids = BTreeSet::new();
        for id in &selection.selected_ids {
            let item = current_by_id
                .get(id.as_str())
                .ok_or_else(|| anyhow!("清理郵件範圍已變更，找不到方案項目：{id}"))?;
            if item.collection != selection.folder {
                bail!("清理資料夾範圍已變更，請重新 preview");
            }
            if item.uid_validity != Some(selection.uid_validity) {
                bail!(
                    "資料夾 {} 的 UIDVALIDITY 已變更，請重新 preview",
                    selection.folder
                );
            }
            let uid = item
                .remote_id
                .parse::<u32>()
                .context("清理方案包含無效 UID")?;
            ids.insert(id.as_str());
            uids.insert(uid);
        }
        let planned_selection_ids = selection
            .selected_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if ids != planned_selection_ids {
            bail!("清理方案項目重複或格式錯誤");
        }
        let planned_selection_uids = selection
            .selected_uids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if uids != planned_selection_uids {
            bail!(
                "清理資料夾 {} 的 UID 集合已變更，請重新 preview",
                selection.folder
            );
        }
    }
    Ok(())
}

pub fn validate_cleanup_plan(plan: &CleanupPlan) -> Result<()> {
    if plan.version != PLAN_VERSION {
        bail!("清理方案版本不支援，請重新 preview");
    }
    let created = parse_timestamp(&plan.created_at).context("清理方案建立時間無效")?;
    let expires = parse_timestamp(&plan.expires_at).context("清理方案到期時間無效")?;
    if expires <= created || Utc::now() >= expires {
        bail!("清理方案已過期，請重新 preview");
    }
    if plan.criteria.query.is_none()
        && plan.criteria.folder.is_none()
        && plan.criteria.before.is_none()
    {
        bail!("清理方案至少需要 query、folder 或 before 一項篩選條件");
    }
    validate_action(plan.action, plan.destination.as_deref())?;
    let selected_ids = plan
        .selections
        .iter()
        .flat_map(|selection| selection.selected_ids.iter().cloned())
        .collect::<Vec<_>>();
    let selected_uids = plan
        .selections
        .iter()
        .flat_map(|selection| selection.selected_uids.iter().copied())
        .collect::<Vec<_>>();
    if selected_ids != plan.selected_ids || selected_uids != plan.selected_uids {
        bail!("清理方案選取集合不一致，請重新 preview");
    }
    if plan.selected_ids.len() > plan.matched_count {
        bail!("清理方案選取數量超過符合數量");
    }
    if plan.selections.is_empty() && !plan.selected_ids.is_empty() {
        bail!("清理方案缺少資料夾選取集合");
    }
    let mut all_ids = BTreeSet::new();
    let mut all_uids = BTreeSet::new();
    for selection in &plan.selections {
        if selection.folder.trim().is_empty() || selection.uid_validity == 0 {
            bail!("清理方案資料夾或 UIDVALIDITY 無效");
        }
        if plan.action == CleanupAction::Move
            && plan
                .destination
                .as_deref()
                .is_some_and(|destination| selection.folder.eq_ignore_ascii_case(destination))
        {
            bail!("move destination 不可與來源資料夾相同");
        }
        if selection.selected_ids.len() != selection.selected_uids.len() {
            bail!("清理方案資料夾 UID 數量不一致");
        }
        for id in &selection.selected_ids {
            if !all_ids.insert(id) {
                bail!("清理方案包含重複郵件");
            }
        }
        for uid in &selection.selected_uids {
            if !all_uids.insert((selection.folder.clone(), selection.uid_validity, *uid)) {
                bail!("清理方案包含重複 UID");
            }
        }
    }
    if plan.selections.len() == 1 {
        let selection = &plan.selections[0];
        if plan.folder.as_deref() != Some(selection.folder.as_str())
            || plan.uid_validity != Some(selection.uid_validity)
        {
            bail!("清理方案頂層資料夾身分不一致");
        }
    } else if plan.folder.is_some() || plan.uid_validity.is_some() {
        bail!("多資料夾清理方案不可指定單一 UIDVALIDITY");
    }
    Ok(())
}

fn normalize_criteria(
    query: Option<String>,
    folder: Option<String>,
    before: Option<String>,
    limit: usize,
) -> Result<CleanupCriteria> {
    let query = normalize_filter(query, "query")?;
    let folder = normalize_filter(folder, "folder")?;
    let before = before
        .map(|value| {
            parse_timestamp(&value)
                .ok_or_else(|| anyhow!("before 日期格式錯誤：{value}"))
                .map(|timestamp| timestamp.to_rfc3339())
        })
        .transpose()?;
    if query.is_none() && folder.is_none() && before.is_none() {
        bail!("cleanup-preview 至少需要 query、folder 或 before 一項篩選條件");
    }
    Ok(CleanupCriteria {
        query,
        folder,
        before,
        limit,
    })
}

fn normalize_filter(value: Option<String>, label: &str) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.chars().any(char::is_control) {
        bail!("{label} 不可包含控制字元");
    }
    Ok(Some(value.to_owned()))
}

fn validate_action(action: CleanupAction, destination: Option<&str>) -> Result<()> {
    match action {
        CleanupAction::Delete if destination.is_some_and(|value| !value.trim().is_empty()) => {
            bail!("delete 不接受 destination")
        }
        CleanupAction::Move => {
            let destination = destination.unwrap_or_default().trim();
            if destination.is_empty() || destination.chars().any(char::is_control) {
                bail!("move 必須指定有效 destination");
            }
        }
        CleanupAction::Delete => {}
    }
    Ok(())
}

fn item_matches_criteria(item: &Item, criteria: &CleanupCriteria) -> bool {
    if let Some(folder) = criteria.folder.as_deref() {
        if !item.collection.eq_ignore_ascii_case(folder) {
            return false;
        }
    }
    if let Some(before) = criteria.before.as_deref() {
        let Some(item_date) = item.date.as_deref().and_then(parse_timestamp) else {
            return false;
        };
        let Some(before) = parse_timestamp(before) else {
            return false;
        };
        if item_date >= before {
            return false;
        }
    }
    if let Some(query) = criteria.query.as_deref() {
        let query = query.to_lowercase();
        let values = [
            item.title.as_str(),
            item.subtitle.as_str(),
            item.text.as_str(),
            item.raw.as_str(),
            item.collection.as_str(),
        ];
        if !values
            .iter()
            .any(|value| value.to_lowercase().contains(&query))
            && !item
                .emails
                .iter()
                .any(|value| value.to_lowercase().contains(&query))
        {
            return false;
        }
    }
    true
}

fn build_selections(items: &[Item]) -> Result<Vec<CleanupSelection>> {
    let mut groups = BTreeMap::<(String, u32), (Vec<String>, Vec<u32>)>::new();
    for item in items {
        let uid = item
            .remote_id
            .parse::<u32>()
            .with_context(|| format!("郵件 {} 的 remote UID 無效", item.id))?;
        let uid_validity = item
            .uid_validity
            .ok_or_else(|| anyhow!("郵件 {} 缺少 UIDVALIDITY，拒絕建立清理方案", item.id))?;
        if uid_validity == 0 {
            bail!("郵件 {} 的 UIDVALIDITY 無效，拒絕建立清理方案", item.id);
        }
        let entry = groups
            .entry((item.collection.clone(), uid_validity))
            .or_default();
        entry.0.push(item.id.clone());
        entry.1.push(uid);
    }
    Ok(groups
        .into_iter()
        .map(
            |((folder, uid_validity), (mut selected_ids, mut selected_uids))| {
                selected_ids.sort();
                selected_uids.sort_unstable();
                CleanupSelection {
                    folder,
                    uid_validity,
                    selected_ids,
                    selected_uids,
                }
            },
        )
        .collect())
}

fn single_folder(selections: &[CleanupSelection]) -> Option<String> {
    (selections.len() == 1).then(|| selections[0].folder.clone())
}

fn single_uid_validity(selections: &[CleanupSelection]) -> Option<u32> {
    (selections.len() == 1).then(|| selections[0].uid_validity)
}

fn compare_dates_asc(left: &Item, right: &Item) -> std::cmp::Ordering {
    let left_date = left.date.as_deref().and_then(parse_timestamp);
    let right_date = right.date.as_deref().and_then(parse_timestamp);
    left_date
        .cmp(&right_date)
        .then_with(|| left.collection.cmp(&right.collection))
        .then_with(|| left.id.cmp(&right.id))
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(value) {
        return Some(timestamp.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return date
            .and_hms_opt(0, 0, 0)
            .map(|value| DateTime::from_naive_utc_and_offset(value, Utc));
    }
    None
}

fn signing_key(data_dir: &Path) -> Result<Vec<u8>> {
    fs::create_dir_all(data_dir)?;
    set_mode(data_dir, 0o700)?;
    let path = data_dir.join(SIGNING_KEY_FILE);
    match OpenOptions::new().read(true).open(&path) {
        Ok(mut file) => {
            set_file_mode(&file, 0o600)?;
            let mut key = Vec::new();
            file.read_to_end(&mut key)?;
            if key.len() < 16 {
                bail!("清理方案簽署金鑰格式錯誤");
            }
            Ok(key)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let key = Uuid::new_v4().into_bytes().to_vec();
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    set_file_mode(&file, 0o600)?;
                    file.write_all(&key)?;
                    file.sync_all()?;
                    Ok(key)
                }
                Err(create_error) if create_error.kind() == std::io::ErrorKind::AlreadyExists => {
                    signing_key(data_dir)
                }
                Err(create_error) => Err(create_error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn sign_plan(data_dir: &Path, payload: CleanupPlan) -> Result<SignedCleanupPlan> {
    let key = signing_key(data_dir)?;
    let bytes = serde_json::to_vec(&payload)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|_| anyhow!("建立方案簽章失敗"))?;
    mac.update(&bytes);
    Ok(SignedCleanupPlan {
        payload,
        signature: hex_encode(&mac.finalize().into_bytes()),
    })
}

pub fn read_cleanup_plan(data_dir: &Path, path: &Path) -> Result<CleanupPlan> {
    let bytes = fs::read(path).with_context(|| format!("無法讀取清理方案：{}", path.display()))?;
    let signed: SignedCleanupPlan = serde_json::from_slice(&bytes).context("清理方案格式錯誤")?;
    let key = signing_key(data_dir)?;
    let payload_bytes = serde_json::to_vec(&signed.payload)?;
    let signature = hex_decode(&signed.signature).context("清理方案簽章格式錯誤")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|_| anyhow!("建立方案簽章失敗"))?;
    mac.update(&payload_bytes);
    mac.verify_slice(&signature)
        .map_err(|_| anyhow!("清理方案簽章不正確，請重新 preview"))?;
    validate_cleanup_plan(&signed.payload)?;
    Ok(signed.payload)
}

fn write_new_plan(path: &Path, signed: &SignedCleanupPlan) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(signed)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("無法建立清理方案：{}；不覆寫既有檔案", path.display()))?;
    set_file_mode(&file, 0o600)?;
    file.write_all(&bytes)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn sanitize_error(error: anyhow::Error, password: &str) -> anyhow::Error {
    anyhow!("{}", format!("{error:#}").replace(password, "[已隱藏]"))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        bail!("奇數長度十六進位簽章");
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).context("簽章不是十六進位"))
        .collect()
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(file: &File, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_mode(mode);
    file.set_permissions(permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_mode(_file: &File, _mode: u32) -> Result<()> {
    Ok(())
}
