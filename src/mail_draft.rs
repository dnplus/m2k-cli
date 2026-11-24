//! Versioned MIME snapshots. Only `send` can obtain a Submission capability.
mod remote;
use crate::{
    config::{self, Config},
    local_state, provider, sync,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use lettre::{address::Envelope, Message};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const MIGRATION: &str = "已停用直接寄信；請用 mail draft create/reply/forward/invite/rsvp 建立草稿、mail draft show <draft-id> 預覽，再用 mail send <draft-id> 寄送";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub schema_version: u32,
    pub draft_id: String,
    pub kind: String,
    pub account: String,
    pub created_at: String,
    pub message_id: String,
    pub from: String,
    pub to: Vec<String>,
    pub sha256: String,
    pub state: String,
    /// Exact RFC822 bytes, including attachment snapshots, encoded as base64.
    pub mime_base64: String,
    #[serde(default = "pending")]
    pub remote_sync: String,
    #[serde(default)]
    pub remote: Option<RemoteDraft>,
    #[serde(default)]
    pub remote_error: Option<String>,
    #[serde(default)]
    pub scheduled_at: Option<String>,
    #[serde(default)]
    pub delete_pending: bool,
    #[serde(default = "local_source")]
    pub source: String,
    #[serde(default)]
    pub remote_fingerprint: Option<String>,
    #[serde(default)]
    pub remote_message_id: Option<String>,
    #[serde(default)]
    pub remote_deleted: bool,
    #[serde(default)]
    pub remote_conflict: bool,
    #[serde(default)]
    pub import_error: Option<String>,
    /// Original remote bytes, preserved even when submission headers are normalized.
    #[serde(default)]
    pub remote_mime_base64: Option<String>,
}

fn local_source() -> String {
    "local".into()
}

fn pending() -> String {
    "pending".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteDraft {
    pub folder: String,
    pub uid: u32,
    pub uid_validity: u32,
}

fn write_state(path: &Path, bytes: &[u8]) -> Result<()> {
    local_state::write_atomic(path, bytes)?;
    #[cfg(unix)]
    fs::File::open(path.parent().context("草稿缺少父目錄")?)?.sync_all()?;
    Ok(())
}

fn digest(from: &str, to: &[String], bytes: &[u8]) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(&(from, to))?);
    hash.update(bytes);
    Ok(format!("{:x}", hash.finalize()))
}

fn account(cfg: &Config) -> Result<String> {
    Ok(format!(
        "{}|{}|{}:{}",
        config::credential_key(cfg)?,
        cfg.email,
        cfg.smtp_host,
        cfg.smtp_port
    ))
}

fn regular(path: &Path, directory: bool) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink()
        || (directory && !meta.is_dir())
        || (!directory && !meta.is_file())
    {
        bail!(
            "草稿路徑不是一般{}，拒絕符號連結",
            if directory { "目錄" } else { "檔案" }
        );
    }
    Ok(())
}

fn root(dir: &Path) -> Result<PathBuf> {
    let root = dir.join("outgoing-drafts");
    if root.try_exists()? {
        regular(&root, true)?;
    }
    local_state::private_directory(&root)?;
    Ok(root)
}

fn path(dir: &Path, id: &str) -> Result<PathBuf> {
    let parsed = uuid::Uuid::parse_str(id).context("無效的草稿 ID")?;
    if parsed.to_string() != id {
        bail!("無效的草稿 ID");
    }
    let folder = root(dir)?.join(id);
    regular(&folder, true)?;
    let path = folder.join("manifest.json");
    regular(&path, false)?;
    Ok(path)
}

pub(crate) fn save(dir: &Path, cfg: &Config, kind: &str, message: Message) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let mut bytes = message.formatted();
    // Persist the final RFC822 line ending that SMTP's DATA terminator supplies.
    if !bytes.ends_with(b"\r\n") {
        bytes.extend_from_slice(b"\r\n");
    }
    let parsed = mailparse::parse_mail(&bytes)?;
    use mailparse::MailHeaderMap;
    let mut draft = Draft {
        schema_version: 1,
        draft_id: id.clone(),
        kind: kind.into(),
        account: account(cfg)?,
        created_at: chrono::Utc::now().to_rfc3339(),
        message_id: parsed
            .headers
            .get_first_value("Message-ID")
            .context("草稿缺少 Message-ID")?,
        from: message
            .envelope()
            .from()
            .context("草稿缺少寄件者")?
            .to_string(),
        to: message
            .envelope()
            .to()
            .iter()
            .map(ToString::to_string)
            .collect(),
        sha256: String::new(),
        state: "draft".into(),
        mime_base64: STANDARD.encode(&bytes),
        remote_sync: pending(),
        remote: None,
        remote_error: None,
        scheduled_at: None,
        delete_pending: false,
        source: local_source(),
        remote_fingerprint: None,
        remote_message_id: None,
        remote_deleted: false,
        remote_conflict: false,
        import_error: None,
        remote_mime_base64: None,
    };
    draft.sha256 = digest(&draft.from, &draft.to, &bytes)?;
    let folder = root(dir)?.join(&id);
    fs::create_dir(&folder)?;
    local_state::private_directory(&folder)?;
    write_state(
        &folder.join("manifest.json"),
        &serde_json::to_vec_pretty(&draft)?,
    )?;
    #[cfg(unix)]
    fs::File::open(root(dir)?)?.sync_all()?;
    reconcile(dir, cfg, &folder.join("manifest.json"), &mut draft, &bytes)?;
    Ok(id)
}

fn load(dir: &Path, id: &str, cfg: &Config) -> Result<(PathBuf, Draft, Vec<u8>)> {
    let path = path(dir, id)?;
    let draft: Draft = serde_json::from_slice(&fs::read(&path)?)?;
    if draft.schema_version != 1 || draft.draft_id != id || draft.account != account(cfg)? {
        bail!("草稿版本、ID 或帳號不符");
    }
    let bytes = STANDARD.decode(&draft.mime_base64)?;
    if digest(&draft.from, &draft.to, &bytes)? != draft.sha256 {
        bail!("草稿內容雜湊不符");
    }
    use mailparse::MailHeaderMap;
    let mail = mailparse::parse_mail(&bytes)?;
    if mail.headers.get_first_value("Message-ID").as_deref() != Some(draft.message_id.as_str()) {
        bail!("草稿 Message-ID 不符");
    }
    Ok((path, draft, bytes))
}

pub fn show(dir: &Path, id: &str) -> Result<Draft> {
    let _lock = sync::lock(dir)?;
    let (_, draft, _) = load(dir, id, &config::load(dir)?)?;
    Ok(draft)
}

pub(crate) struct Submission {
    envelope: Envelope,
    bytes: Vec<u8>,
}
impl Submission {
    pub(crate) fn envelope(&self) -> &Envelope {
        &self.envelope
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub fn send(dir: &Path, id: &str) -> Result<String> {
    let _lock = sync::lock(dir)?;
    send_locked(dir, id, &config::load(dir)?, false)
}

fn send_locked(dir: &Path, id: &str, cfg: &Config, scheduled: bool) -> Result<String> {
    let (_, before, _) = load(dir, id, cfg)?;
    if matches!(before.state.as_str(), "draft" | "scheduled")
        && before.remote.is_some()
        && !before.remote_deleted
        && !before.delete_pending
    {
        if !remote::pull(dir, cfg, &config::password(cfg)?)? {
            bail!("無法確認遠端草稿狀態，拒絕寄送");
        }
        let (_, after, _) = load(dir, id, cfg)?;
        if before.sha256 != after.sha256 || before.state != after.state {
            bail!("遠端草稿已修改，排程已取消；請重新預覽並明確寄送或排程");
        }
    }
    let (path, mut draft, bytes) = load(dir, id, cfg)?;
    if draft.remote_conflict {
        bail!("草稿有同步衝突，請先人工處理");
    }
    if draft.delete_pending
        || draft.remote_deleted
        || draft.remote_conflict
        || draft.import_error.is_some()
        || !(draft.state == "draft" || (scheduled && due(&draft, chrono::Utc::now())?))
    {
        bail!(
            "草稿狀態為 {}，禁止重寄；sending/unknown 請先人工查證寄件備份",
            draft.state
        );
    }
    if dir.join(".demo").exists() {
        bail!(crate::mail_action::DEMO_OFFLINE);
    }
    crate::store::Store::open(dir)?.ensure_account(cfg)?;
    let password = config::password(cfg)?;
    let envelope = Envelope::new(
        Some(draft.from.parse()?),
        draft
            .to
            .iter()
            .map(|s| s.parse())
            .collect::<std::result::Result<Vec<_>, _>>()?,
    )?;
    let message_id = draft.message_id.clone();
    submit_saved(&path, &mut draft, || {
        provider::mail::claim_draft(cfg, &password, id, &message_id)?;
        provider::mail::submit_draft(
            cfg,
            &password,
            &Submission {
                envelope,
                bytes: bytes.clone(),
            },
        )
    })?;
    reconcile(dir, cfg, &path, &mut draft, &bytes)?;
    Ok(draft.message_id)
}

fn submit_saved(path: &Path, draft: &mut Draft, submit: impl FnOnce() -> Result<()>) -> Result<()> {
    if !matches!(draft.state.as_str(), "draft" | "scheduled") || draft.delete_pending {
        bail!("禁止重寄");
    }
    draft.state = "sending".into();
    write_state(path, &serde_json::to_vec_pretty(draft)?)?;
    let result = submit();
    draft.state = if result.is_ok() { "sent" } else { "unknown" }.into();
    write_state(path, &serde_json::to_vec_pretty(draft)?)?;
    result
}

pub fn invite(dir: &Path, file: &Path) -> Result<String> {
    let _lock = sync::lock(dir)?;
    let cfg = config::load(dir)?;
    let raw = fs::read_to_string(file)?;
    save(
        dir,
        &cfg,
        "invite",
        provider::mail::build_itip(&cfg, &raw, "REQUEST", "會議邀請")?,
    )
}

/// Decode the saved snapshot for review, without rebuilding the message.
pub fn preview(draft: &Draft) -> Result<serde_json::Value> {
    use mailparse::MailHeaderMap;
    fn parts(mail: &mailparse::ParsedMail<'_>, output: &mut Vec<serde_json::Value>) -> Result<()> {
        if mail.subparts.is_empty() {
            let disposition = mail.get_content_disposition();
            let filename = disposition
                .params
                .get("filename")
                .or_else(|| mail.ctype.params.get("name"));
            let bytes = mail.get_body_raw()?;
            output.push(serde_json::json!({
                "content_type": mail.ctype.mimetype,
                "filename": filename,
                "bytes": bytes.len(),
                "text": if mail.ctype.mimetype.starts_with("text/") { Some(mail.get_body()?) } else { None },
            }));
        } else {
            for part in &mail.subparts {
                parts(part, output)?;
            }
        }
        Ok(())
    }
    let bytes = STANDARD.decode(&draft.mime_base64)?;
    let mail = mailparse::parse_mail(&bytes)?;
    let mut decoded = Vec::new();
    parts(&mail, &mut decoded)?;
    let mut output = serde_json::to_value(draft)?;
    output["subject"] = serde_json::json!(mail.headers.get_first_value("Subject"));
    output["header_to"] = serde_json::json!(mail.headers.get_first_value("To"));
    output["cc"] = serde_json::json!(mail.headers.get_first_value("Cc"));
    output["in_reply_to"] = serde_json::json!(mail.headers.get_first_value("In-Reply-To"));
    output["parts"] = serde_json::json!(decoded);
    Ok(output)
}

fn reconcile_with(
    path: &Path,
    draft: &mut Draft,
    operation: impl FnOnce(&Draft) -> Result<Option<RemoteDraft>>,
) -> Result<()> {
    draft.remote_sync = pending();
    draft.remote_error = None;
    write_state(path, &serde_json::to_vec_pretty(draft)?)?;
    match operation(draft) {
        Ok(remote) => {
            draft.remote = remote;
            draft.remote_sync = "synced".into();
        }
        Err(error) => {
            draft.remote_sync = "failed".into();
            draft.remote_error = Some(format!("{error:#}"));
        }
    }
    write_state(path, &serde_json::to_vec_pretty(draft)?)
}

fn reconcile(dir: &Path, cfg: &Config, path: &Path, draft: &mut Draft, bytes: &[u8]) -> Result<()> {
    reconcile_with(path, draft, |draft| {
        if dir.join(".demo").exists() {
            bail!(crate::mail_action::DEMO_OFFLINE);
        }
        provider::mail::sync_draft(cfg, &config::password(cfg)?, draft, bytes)
    })?;
    if draft.remote_sync == "synced" && draft.remote.is_some() && draft.remote_fingerprint.is_none()
    {
        draft.remote_fingerprint = Some(provider::mail::fingerprint(bytes));
        draft.remote_message_id = Some(draft.message_id.clone());
        write_state(path, &serde_json::to_vec_pretty(draft)?)?;
    }
    Ok(())
}

fn ids(dir: &Path) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    for entry in fs::read_dir(root(dir)?)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if uuid::Uuid::parse_str(&name).is_ok() {
            ids.push(name);
        }
    }
    ids.sort();
    Ok(ids)
}

fn summary(draft: &Draft, bytes: &[u8]) -> Result<serde_json::Value> {
    use mailparse::MailHeaderMap;
    let mail = mailparse::parse_mail(bytes)?;
    Ok(serde_json::json!({
        "draft_id": draft.draft_id, "to": draft.to,
        "subject": mail.headers.get_first_value("Subject"),
        "created_at": draft.created_at, "state": draft.state,
        "remote_sync": draft.remote_sync, "remote": draft.remote,
        "remote_error": draft.remote_error,
        "scheduled_at": draft.scheduled_at, "delete_pending": draft.delete_pending,
        "source": draft.source, "remote_deleted": draft.remote_deleted, "remote_conflict": draft.remote_conflict, "import_error": draft.import_error,
    }))
}

pub fn list(dir: &Path, scheduled_only: bool) -> Result<serde_json::Value> {
    let _lock = sync::lock(dir)?;
    let cfg = config::load(dir)?;
    let mut drafts = Vec::new();
    for id in ids(dir)? {
        let (_, draft, bytes) = load(dir, &id, &cfg)?;
        if !scheduled_only || draft.state == "scheduled" {
            drafts.push(summary(&draft, &bytes)?);
        }
    }
    Ok(serde_json::json!(drafts))
}

pub fn sync_remote(dir: &Path, id: Option<&str>) -> Result<serde_json::Value> {
    let _lock = sync::lock(dir)?;
    let cfg = config::load(dir)?;
    if dir.join(".demo").exists() {
        bail!(crate::mail_action::DEMO_OFFLINE);
    }
    sync_locked(dir, &cfg, &config::password(&cfg)?, id)
}

/// Caller holds sync.lock. This routine never calls the submission boundary.
pub(crate) fn sync_locked(
    dir: &Path,
    cfg: &Config,
    password: &str,
    id: Option<&str>,
) -> Result<serde_json::Value> {
    if let Some(id) = id {
        load(dir, id, cfg)?;
    }
    // Pull before pushing so web edits/deletions cannot be overwritten or resurrected.
    remote::pull(dir, cfg, password)?;
    let ids = match id {
        Some(id) => vec![id.to_owned()],
        None => ids(dir)?,
    };
    let mut results = Vec::new();
    for id in ids {
        let (path, mut draft, bytes) = load(dir, &id, cfg)?;
        let conflicted = draft.remote_conflict;
        if !matches!(draft.state.as_str(), "sending" | "unknown")
            && !conflicted
            && (draft.delete_pending
                || draft.state == "sent"
                || (!draft.remote_deleted && draft.source == "local" && draft.remote.is_none()))
        {
            reconcile(dir, cfg, &path, &mut draft, &bytes)?;
        }
        let mut output = summary(&draft, &bytes)?;
        if draft.delete_pending && draft.remote_sync == "synced" {
            remove_local(&path)?;
            output["deleted"] = true.into();
        }
        results.push(output);
    }
    Ok(serde_json::json!(results))
}

fn remove_local(path: &Path) -> Result<()> {
    fs::remove_file(path)?;
    fs::remove_dir(path.parent().context("草稿缺少父目錄")?)?;
    Ok(())
}

pub fn delete(dir: &Path, id: &str) -> Result<serde_json::Value> {
    let _lock = sync::lock(dir)?;
    let cfg = config::load(dir)?;
    let (path, mut draft, bytes) = load(dir, id, &cfg)?;
    if !matches!(draft.state.as_str(), "draft" | "scheduled" | "sent") {
        bail!("草稿狀態為 {}，拒絕刪除", draft.state);
    }
    draft.delete_pending = true;
    draft.scheduled_at = None;
    if draft.state == "scheduled" {
        draft.state = "draft".into();
    }
    reconcile(dir, &cfg, &path, &mut draft, &bytes)?;
    let deleted = draft.remote_sync == "synced";
    if deleted {
        remove_local(&path)?;
    }
    Ok(
        serde_json::json!({"draft_id": id, "deleted": deleted, "remote_sync": draft.remote_sync, "remote_error": draft.remote_error}),
    )
}

pub fn schedule(dir: &Path, id: &str, at: &str) -> Result<serde_json::Value> {
    let _lock = sync::lock(dir)?;
    let (path, mut draft, _) = load(dir, id, &config::load(dir)?)?;
    if draft.state != "draft"
        || draft.delete_pending
        || draft.remote_deleted
        || draft.remote_conflict
        || draft.import_error.is_some()
    {
        bail!("只有 draft 狀態可排程");
    }
    let now = chrono::Utc::now();
    let at = crate::human_time::parse(at, now, chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap())?;
    if at <= now {
        bail!("排程時間必須在未來");
    }
    draft.state = "scheduled".into();
    draft.scheduled_at = Some(at.to_rfc3339());
    write_state(&path, &serde_json::to_vec_pretty(&draft)?)?;
    Ok(serde_json::json!({"draft_id": id, "scheduled_at": draft.scheduled_at, "sent": false}))
}

pub fn cancel(dir: &Path, id: &str) -> Result<serde_json::Value> {
    let _lock = sync::lock(dir)?;
    let (path, mut draft, _) = load(dir, id, &config::load(dir)?)?;
    if draft.state != "scheduled" {
        bail!("草稿沒有排程");
    }
    draft.state = "draft".into();
    draft.scheduled_at = None;
    write_state(&path, &serde_json::to_vec_pretty(&draft)?)?;
    Ok(serde_json::json!({"draft_id": id, "cancelled": true}))
}

fn due(draft: &Draft, now: chrono::DateTime<chrono::Utc>) -> Result<bool> {
    Ok(draft.state == "scheduled"
        && !draft.delete_pending
        && !draft.remote_deleted
        && !draft.remote_conflict
        && draft.import_error.is_none()
        && chrono::DateTime::parse_from_rfc3339(
            draft.scheduled_at.as_deref().context("排程缺少時間")?,
        )? <= now)
}

pub fn run(dir: &Path, dry_run: bool) -> Result<serde_json::Value> {
    let _lock = sync::lock(dir)?;
    let cfg = config::load(dir)?;
    let now = chrono::Utc::now();
    let mut results = Vec::new();
    for id in ids(dir)? {
        let (_, draft, _) = load(dir, &id, &cfg)?;
        if !due(&draft, now)? {
            continue;
        }
        if dry_run {
            results.push(serde_json::json!({"draft_id": id, "due": true, "sent": false}));
        } else {
            match send_locked(dir, &id, &cfg, true) {
                Ok(message_id) => results.push(serde_json::json!({"draft_id": id, "message_id": message_id, "sent": true})),
                Err(error) => results.push(serde_json::json!({"draft_id": id, "sent": false, "error": format!("{error:#}")})),
            }
        }
    }
    Ok(serde_json::json!(results))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Config, String) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::new("me@example.test".into(), "localhost").unwrap();
        config::save(dir.path(), &cfg).unwrap();
        let message = Message::builder()
            .from("me@example.test".parse().unwrap())
            .to("you@example.test".parse().unwrap())
            .message_id(Some("<fixed@example.test>".into()))
            .subject("草稿")
            .body("正文".to_owned())
            .unwrap();
        let id = save(dir.path(), &cfg, "mail", message).unwrap();
        (dir, cfg, id)
    }

    #[test]
    fn list_schedule_cancel_and_no_early_submission() {
        let (dir, _, id) = setup();
        assert!(schedule(dir.path(), &id, "2000-01-01").is_err());
        let at = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert_eq!(schedule(dir.path(), &id, &at).unwrap()["sent"], false);
        assert_eq!(list(dir.path(), true).unwrap().as_array().unwrap().len(), 1);
        let rows = list(dir.path(), false).unwrap();
        assert_eq!(rows[0]["draft_id"], id);
        assert_eq!(rows[0]["subject"], "草稿");
        assert_eq!(rows[0]["state"], "scheduled");
        assert!(run(dir.path(), false)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
        assert!(send(dir.path(), &id).is_err());
        cancel(dir.path(), &id).unwrap();
        assert_eq!(show(dir.path(), &id).unwrap().state, "draft");
        assert!(list(dir.path(), true)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn persisted_send_state_blocks_repeat_and_unknown() {
        let (dir, cfg, id) = setup();
        let (path, mut draft, _) = load(dir.path(), &id, &cfg).unwrap();
        draft.state = "scheduled".into();
        draft.scheduled_at = Some("2000-01-01T00:00:00+08:00".into());
        assert!(due(&draft, chrono::Utc::now()).unwrap());
        write_state(&path, &serde_json::to_vec(&draft).unwrap()).unwrap();
        assert_eq!(run(dir.path(), true).unwrap()[0]["sent"], false);
        let mut submissions = 0;
        submit_saved(&path, &mut draft, || {
            submissions += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(show(dir.path(), &id).unwrap().state, "sent");
        assert!(!due(&draft, chrono::Utc::now()).unwrap());
        assert!(run(dir.path(), false)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
        assert!(submit_saved(&path, &mut draft, || {
            submissions += 1;
            Ok(())
        })
        .is_err());
        assert_eq!(submissions, 1);
        draft.state = "draft".into();
        assert!(submit_saved(&path, &mut draft, || bail!("SMTP unknown")).is_err());
        assert_eq!(show(dir.path(), &id).unwrap().state, "unknown");
        assert!(delete(dir.path(), &id).is_err());
        draft.state = "sending".into();
        write_state(&path, &serde_json::to_vec(&draft).unwrap()).unwrap();
        assert!(delete(dir.path(), &id).is_err());
    }

    #[test]
    fn failed_sync_preserves_snapshot_and_retries_and_old_schema_defaults() {
        let (dir, cfg, id) = setup();
        let (path, mut draft, bytes) = load(dir.path(), &id, &cfg).unwrap();
        reconcile_with(&path, &mut draft, |_| bail!("APPEND failed")).unwrap();
        assert_eq!(show(dir.path(), &id).unwrap().remote_sync, "failed");
        assert_eq!(load(dir.path(), &id, &cfg).unwrap().2, bytes);
        reconcile_with(&path, &mut draft, |_| {
            Ok(Some(RemoteDraft {
                folder: "Drafts".into(),
                uid: 7,
                uid_validity: 42,
            }))
        })
        .unwrap();
        assert_eq!(show(dir.path(), &id).unwrap().remote_sync, "synced");
        let mut old = serde_json::to_value(&draft).unwrap();
        for key in [
            "remote_sync",
            "remote",
            "remote_error",
            "scheduled_at",
            "delete_pending",
        ] {
            old.as_object_mut().unwrap().remove(key);
        }
        write_state(&path, &serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(show(dir.path(), &id).unwrap().remote_sync, "pending");
    }

    #[test]
    fn failed_delete_cancels_schedule_keeps_retryable_tombstone() {
        let (dir, _, id) = setup();
        schedule(dir.path(), &id, "2099-01-01").unwrap();
        fs::write(dir.path().join(".demo"), "").unwrap();
        assert_eq!(delete(dir.path(), &id).unwrap()["deleted"], false);
        let draft = show(dir.path(), &id).unwrap();
        assert!(draft.delete_pending);
        assert_eq!(draft.state, "draft");
        assert!(draft.scheduled_at.is_none());
        assert!(send(dir.path(), &id).is_err());
        assert!(schedule(dir.path(), &id, "2099-01-01").is_err());
    }

    #[test]
    fn concurrent_workers_must_hold_the_same_lock() {
        let (dir, _, _) = setup();
        let _lock = sync::lock(dir.path()).unwrap();
        assert!(run(dir.path(), false).is_err());
    }

    #[test]
    fn invalid_id_account_content_envelope_and_schema_fail_closed() {
        let (dir, mut cfg, id) = setup();
        for invalid in ["../escape", "/tmp/file", "", "manifest.json"] {
            assert!(show(dir.path(), invalid).is_err());
            assert!(send(dir.path(), invalid).is_err());
        }
        cfg.email = "other@example.test".into();
        config::save(dir.path(), &cfg).unwrap();
        assert!(show(dir.path(), &id)
            .unwrap_err()
            .to_string()
            .contains("帳號不符"));
        cfg.email = "me@example.test".into();
        config::save(dir.path(), &cfg).unwrap();
        let file = path(dir.path(), &id).unwrap();
        let original = fs::read(&file).unwrap();
        for (key, value) in [
            (
                "mime_base64",
                serde_json::json!(STANDARD.encode(b"tampered")),
            ),
            ("to", serde_json::json!(["changed@example.test"])),
            ("schema_version", serde_json::json!(999)),
            (
                "draft_id",
                serde_json::json!(uuid::Uuid::new_v4().to_string()),
            ),
        ] {
            let mut changed: serde_json::Value = serde_json::from_slice(&original).unwrap();
            changed[key] = value;
            fs::write(&file, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert!(show(dir.path(), &id).is_err(), "{key}");
            assert!(send(dir.path(), &id).is_err(), "{key}");
        }
        fs::write(&file, b"{broken").unwrap();
        assert!(send(dir.path(), &id).is_err());
        fs::remove_file(&file).unwrap();
        assert!(send(dir.path(), &id).is_err());
    }

    #[test]
    fn terminal_or_interrupted_states_refuse_before_credentials() {
        let (dir, _, id) = setup();
        let file = path(dir.path(), &id).unwrap();
        let mut draft = show(dir.path(), &id).unwrap();
        for state in ["sending", "sent", "unknown", "invalid"] {
            draft.state = state.into();
            write_state(&file, &serde_json::to_vec(&draft).unwrap()).unwrap();
            assert!(send(dir.path(), &id)
                .unwrap_err()
                .to_string()
                .contains("禁止重寄"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_files_and_symlink_rejection() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let (dir, _, id) = setup();
        let file = path(dir.path(), &id).unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let target = dir.path().join("saved.json");
        fs::rename(&file, &target).unwrap();
        symlink(&target, &file).unwrap();
        assert!(show(dir.path(), &id)
            .unwrap_err()
            .to_string()
            .contains("符號連結"));
        fs::remove_file(&file).unwrap();
        fs::remove_dir(file.parent().unwrap()).unwrap();
        symlink(dir.path(), file.parent().unwrap()).unwrap();
        assert!(show(dir.path(), &id).is_err());
    }
}
