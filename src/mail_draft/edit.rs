use super::*;
use mailparse::MailHeaderMap;

/// Omitted fields retain their saved value; --to/--cc/--bcc without values clear them.
#[derive(clap::Args, Default)]
pub struct EditInput {
    #[arg(long, num_args = 0.., value_delimiter = ',')]
    pub to: Option<Vec<String>>,
    #[arg(long, num_args = 0.., value_delimiter = ',')]
    pub cc: Option<Vec<String>>,
    #[arg(long, num_args = 0.., value_delimiter = ',')]
    pub bcc: Option<Vec<String>>,
    #[arg(long)]
    pub subject: Option<String>,
    #[arg(long)]
    pub body_file: Option<PathBuf>,
    /// Replace attachments (omit to preserve snapshots).
    #[arg(long, num_args = 0..)]
    pub attach: Option<Vec<PathBuf>>,
}

fn header_addresses(headers: &[mailparse::MailHeader<'_>], name: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for value in headers.get_all_values(name) {
        for address in mailparse::addrparse(&value)?.iter() {
            match address {
                mailparse::MailAddr::Single(a) => result.push(a.addr.clone()),
                mailparse::MailAddr::Group(g) => {
                    result.extend(g.addrs.iter().map(|a| a.addr.clone()))
                }
            }
        }
    }
    Ok(result)
}

fn plain<'a>(mail: &'a mailparse::ParsedMail<'a>) -> Option<&'a mailparse::ParsedMail<'a>> {
    if mail.ctype.mimetype == "text/plain"
        && mail.get_content_disposition().disposition != mailparse::DispositionType::Attachment
    {
        Some(mail)
    } else {
        mail.subparts.iter().find_map(plain)
    }
}

pub fn edit(dir: &Path, id: &str, input: EditInput) -> Result<serde_json::Value> {
    let _lock = sync::lock(dir)?;
    let cfg = config::load(dir)?;
    let (path, mut draft, bytes) = load(dir, id, &cfg)?;
    if !matches!(draft.state.as_str(), "draft" | "scheduled")
        || draft.delete_pending
        || draft.edit_pending
        || draft.remote_deleted
        || draft.remote_conflict
    {
        bail!("草稿狀態不允許編輯；未完成的編輯請先 draft sync 重試");
    }
    let mail = mailparse::parse_mail(&bytes)?;
    let old_to = header_addresses(&mail.headers, "To")?;
    let old_cc = header_addresses(&mail.headers, "Cc")?;
    let old_bcc = draft
        .to
        .iter()
        .filter(|a| !old_to.contains(a) && !old_cc.contains(a))
        .cloned()
        .collect();
    let bcc = input.bcc.unwrap_or(old_bcc);
    let body = if let Some(file) = &input.body_file {
        if file.as_os_str() == "-" {
            use std::io::Read;
            let mut body = String::new();
            std::io::stdin().read_to_string(&mut body)?;
            body
        } else {
            fs::read_to_string(file)?
        }
    } else {
        plain(&mail)
            .map(|part| part.get_body())
            .transpose()?
            .unwrap_or_default()
    };
    let outgoing = crate::model::OutgoingMail {
        to: input.to.unwrap_or(old_to),
        cc: input.cc.unwrap_or(old_cc),
        subject: input
            .subject
            .unwrap_or_else(|| mail.headers.get_first_value("Subject").unwrap_or_default()),
        body,
        attachments: input.attach.clone().unwrap_or_default(),
        in_reply_to: mail.headers.get_first_value("In-Reply-To"),
    };
    let message = provider::mail::build_mail_bcc(&cfg, &outgoing, &bcc)?;
    let fresh = message.formatted();
    let (fresh_headers, fresh_start) = mailparse::parse_headers(&fresh)?;
    let (_, old_start) = mailparse::parse_headers(&bytes)?;
    let mut updated = Vec::new();
    if input.attach.is_some() {
        updated = fresh.clone();
    } else {
        for header in &fresh_headers {
            if !header
                .get_key()
                .to_ascii_lowercase()
                .starts_with("content-")
            {
                updated.extend_from_slice(header.get_key_raw());
                updated.extend_from_slice(b": ");
                updated.extend_from_slice(header.get_value_raw());
                updated.extend_from_slice(b"\r\n");
            }
        }
        let mut mime = bytes.clone();
        if input.body_file.is_some() {
            let target =
                plain(&mail).context("草稿沒有可替換的純文字正文；請提供 --attach 重建 MIME")?;
            let start = target.raw_bytes.as_ptr() as usize - bytes.as_ptr() as usize;
            let end = start + target.raw_bytes.len();
            let mut replacement = Vec::new();
            for header in &fresh_headers {
                if header
                    .get_key()
                    .to_ascii_lowercase()
                    .starts_with("content-")
                {
                    replacement.extend_from_slice(header.get_key_raw());
                    replacement.extend_from_slice(b": ");
                    replacement.extend_from_slice(header.get_value_raw());
                    replacement.extend_from_slice(b"\r\n");
                }
            }
            replacement.extend_from_slice(b"\r\n");
            replacement.extend_from_slice(&fresh[fresh_start..]);
            if !replacement.ends_with(b"\r\n") {
                replacement.extend_from_slice(b"\r\n");
            }
            mime.splice(start..end, replacement);
        }
        let (headers, start) = mailparse::parse_headers(&mime)?;
        for header in &headers {
            if header
                .get_key()
                .to_ascii_lowercase()
                .starts_with("content-")
            {
                updated.extend_from_slice(header.get_key_raw());
                updated.extend_from_slice(b": ");
                updated.extend_from_slice(header.get_value_raw());
                updated.extend_from_slice(b"\r\n");
            }
        }
        updated.extend_from_slice(b"\r\n");
        updated.extend_from_slice(
            &mime[if input.body_file.is_some() {
                start
            } else {
                old_start
            }..],
        );
    }
    if !updated.ends_with(b"\r\n") {
        updated.extend_from_slice(b"\r\n");
    }
    // Archive before changing content. Keep old remote identity until APPEND + cleanup succeeds.
    remote::archive(dir, id, &draft)?;
    if draft.remote.is_none() && draft.remote_message_id.is_none() {
        draft.remote_message_id = Some(draft.message_id.clone());
    }
    draft.message_id = fresh_headers
        .get_first_value("Message-ID")
        .context("缺少 Message-ID")?;
    draft.to = message
        .envelope()
        .to()
        .iter()
        .map(ToString::to_string)
        .collect();
    draft.mime_base64 = STANDARD.encode(&updated);
    draft.sha256 = digest(&draft.from, &draft.to, &updated)?;
    draft.edit_pending = true;
    draft.remote_mime_base64 = None;
    draft.source = "local".into();
    draft.import_error = None;
    reconcile(dir, &cfg, &path, &mut draft, &updated)?;
    preview(&draft)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Config, String) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::new("me@example.test".into(), "localhost").unwrap();
        config::save(dir.path(), &cfg).unwrap();
        fs::write(dir.path().join(".demo"), "").unwrap();
        let attachment = dir.path().join("report.txt");
        fs::write(&attachment, "saved attachment").unwrap();
        let message = provider::mail::build_mail_bcc(
            &cfg,
            &crate::model::OutgoingMail {
                to: vec!["to@example.test".into()],
                cc: vec!["cc@example.test".into()],
                subject: "old".into(),
                body: "old body".into(),
                attachments: vec![attachment.clone()],
                in_reply_to: None,
            },
            &["hidden@example.test".into()],
        )
        .unwrap();
        let id = save(dir.path(), &cfg, "mail", message).unwrap();
        fs::remove_file(attachment).unwrap();
        (dir, cfg, id)
    }

    #[test]
    fn edit_preserves_schedule_attachment_and_local_content_after_failure() {
        let (dir, cfg, id) = setup();
        let at = "2099-01-01T09:00:00+08:00";
        schedule(dir.path(), &id, at).unwrap();
        let old = show(dir.path(), &id).unwrap();
        let body = dir.path().join("body.md");
        fs::write(&body, "# 新內容\n\n正文").unwrap();
        let output = edit(
            dir.path(),
            &id,
            EditInput {
                subject: Some("new".into()),
                body_file: Some(body),
                to: Some(vec!["new@example.test".into()]),
                cc: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output["state"], "scheduled");
        assert_eq!(output["scheduled_at"], at);
        assert_eq!(output["remote_sync"], "failed");
        assert_eq!(output["subject"], "new");
        let (path, mut updated, bytes) = load(dir.path(), &id, &cfg).unwrap();
        assert_ne!(old.message_id, updated.message_id);
        assert!(updated.to.contains(&"hidden@example.test".into()));
        assert!(!String::from_utf8_lossy(&bytes).contains("hidden@example.test"));
        assert!(output["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["text"] == "saved attachment"));
        assert!(output["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["text"].as_str().is_some_and(|s| s.contains("# 新內容"))));
        assert!(send(dir.path(), &id).is_err());
        reconcile_with(&path, &mut updated, |_| {
            Ok(Some(RemoteDraft {
                folder: "Drafts".into(),
                uid: 8,
                uid_validity: 42,
            }))
        })
        .unwrap();
        assert!(!updated.edit_pending);
        assert_eq!(
            updated.remote_message_id.as_deref(),
            Some(updated.message_id.as_str())
        );
        assert_eq!(updated.scheduled_at.as_deref(), Some(at));
        assert_eq!(load(dir.path(), &id, &cfg).unwrap().2, bytes);
    }

    #[test]
    fn subject_only_edit_preserves_mime_and_bcc_can_be_cleared() {
        let (dir, _, id) = setup();
        let output = edit(
            dir.path(),
            &id,
            EditInput {
                subject: Some("changed".into()),
                bcc: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(output["subject"], "changed");
        assert!(!show(dir.path(), &id)
            .unwrap()
            .to
            .contains(&"hidden@example.test".into()));
        assert!(output["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["text"] == "old body"));
        assert!(output["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["text"] == "saved attachment"));
    }

    #[test]
    fn edit_rejects_unsafe_states_without_changing_content() {
        let (dir, cfg, id) = setup();
        let (path, mut draft, _) = load(dir.path(), &id, &cfg).unwrap();
        for state in ["sending", "sent", "unknown"] {
            draft.state = state.into();
            write_state(&path, &serde_json::to_vec(&draft).unwrap()).unwrap();
            let before = fs::read(&path).unwrap();
            assert!(edit(
                dir.path(),
                &id,
                EditInput {
                    subject: Some("no".into()),
                    ..Default::default()
                }
            )
            .is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
    }
}
