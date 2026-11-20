//! Immutable MIME snapshots. Only `send` can obtain a Submission capability.
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

#[derive(Debug, Serialize, Deserialize)]
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
    let cfg = config::load(dir)?;
    let (path, mut draft, bytes) = load(dir, id, &cfg)?;
    if draft.state != "draft" {
        bail!(
            "草稿狀態為 {}，禁止重寄；sending/unknown 請先人工查證寄件備份",
            draft.state
        );
    }
    if dir.join(".demo").exists() {
        bail!(crate::mail_action::DEMO_OFFLINE);
    }
    crate::store::Store::open(dir)?.ensure_account(&cfg)?;
    let password = config::password(&cfg)?;
    let envelope = Envelope::new(
        Some(draft.from.parse()?),
        draft
            .to
            .iter()
            .map(|s| s.parse())
            .collect::<std::result::Result<Vec<_>, _>>()?,
    )?;
    draft.state = "sending".into();
    write_state(&path, &serde_json::to_vec_pretty(&draft)?)?;
    let result = provider::mail::submit_draft(&cfg, &password, &Submission { envelope, bytes });
    draft.state = if result.is_ok() { "sent" } else { "unknown" }.into();
    write_state(&path, &serde_json::to_vec_pretty(&draft)?)?;
    result?;
    Ok(draft.message_id)
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
