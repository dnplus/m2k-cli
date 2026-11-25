//! Pull first, preserve prior snapshots, never authorize submission from remote data.
use super::*;
use mailparse::{MailAddr, MailHeaderMap};
use provider::mail::{fingerprint, RemoteSnapshot};

fn remote_id(raw: &[u8]) -> Result<Option<String>> {
    let (headers, _) = mailparse::parse_headers(raw)?;
    Ok(headers
        .get_first_value("Message-ID")
        .filter(|id| !id.trim().is_empty()))
}

fn deterministic_id(key: &str) -> String {
    let hash = Sha256::digest(key.as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash[..16]);
    // UUID v8: application-defined, deterministic identity shared across hosts.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

fn addresses(headers: &[mailparse::MailHeader<'_>], names: &[&str]) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for name in names {
        for value in headers.get_all_values(name) {
            for address in mailparse::addrparse(&value)?.iter() {
                match address {
                    MailAddr::Single(address) => result.push(address.addr.clone()),
                    MailAddr::Group(group) => {
                        result.extend(group.addrs.iter().map(|a| a.addr.clone()))
                    }
                }
            }
        }
    }
    for address in &result {
        address.parse::<lettre::Address>()?;
    }
    result.sort();
    result.dedup();
    Ok(result)
}

fn imported(cfg: &Config, remote: RemoteDraft, raw: &[u8], id: String) -> Result<Draft> {
    let (headers, body_start) = mailparse::parse_headers(raw)?;
    let original_id = remote_id(raw)?;
    let message_id = original_id
        .clone()
        .unwrap_or_else(|| format!("<{id}@m2k.local>"));
    let mut issues = Vec::new();
    let from = match addresses(&headers, &["From"]) {
        Ok(from) if from.len() == 1 && from[0].eq_ignore_ascii_case(&cfg.email) => from[0].clone(),
        _ => {
            issues.push("網頁草稿寄件者缺少、無效或不是目前帳號");
            cfg.email.clone()
        }
    };
    let to = match addresses(&headers, &["To", "Cc", "Bcc"]) {
        Ok(to) if !to.is_empty() => to,
        _ => {
            issues.push("網頁草稿收件者缺少或無效");
            Vec::new()
        }
    };
    if headers.get_all_values("Message-ID").len() > 1 || message_id.chars().any(char::is_control) {
        issues.push("網頁草稿 Message-ID 不明確或無效");
    }
    // Preserve every byte of the body/attachments. Remove Bcc headers from the
    // submission snapshot while retaining Bcc envelope recipients and original MIME.
    let mut bytes = Vec::new();
    let mut omit = false;
    for line in raw[..body_start].split_inclusive(|b| *b == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let content = content.strip_suffix(b"\r").unwrap_or(content);
        if content.is_empty() {
            continue;
        }
        if !matches!(content.first(), Some(b' ' | b'\t')) {
            omit = content.split(|b| *b == b':').next().is_some_and(|name| {
                name.eq_ignore_ascii_case(b"bcc")
                    || (original_id.is_none() && name.eq_ignore_ascii_case(b"message-id"))
            });
        }
        if !omit {
            bytes.extend_from_slice(content);
            bytes.extend_from_slice(b"\r\n");
        }
    }
    if original_id.is_none() {
        bytes.extend_from_slice(format!("Message-ID: {message_id}\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(&raw[body_start..]);
    if !bytes.ends_with(b"\r\n") {
        bytes.extend_from_slice(b"\r\n");
    }
    Ok(Draft {
        schema_version: 1,
        draft_id: id,
        kind: "mail".into(),
        account: account(cfg)?,
        created_at: chrono::Utc::now().to_rfc3339(),
        message_id,
        from: from.clone(),
        to: to.clone(),
        sha256: digest(&from, &to, &bytes)?,
        state: "draft".into(),
        mime_base64: STANDARD.encode(bytes),
        remote_sync: "synced".into(),
        remote: Some(remote),
        remote_error: None,
        scheduled_at: None,
        delete_pending: false,
        edit_pending: false,
        source: "remote".into(),
        remote_fingerprint: Some(fingerprint(raw)),
        remote_message_id: original_id,
        remote_deleted: false,
        remote_conflict: false,
        import_error: if issues.is_empty() {
            None
        } else {
            Some(issues.join("；"))
        },
        remote_mime_base64: Some(STANDARD.encode(raw)),
    })
}

pub(super) fn archive(dir: &Path, id: &str, value: &impl Serialize) -> Result<()> {
    let history = dir.join("outgoing-draft-history");
    if history.try_exists()? {
        regular(&history, true)?;
    }
    local_state::private_directory(&history)?;
    let folder = history.join(id);
    if folder.try_exists()? {
        regular(&folder, true)?;
    }
    local_state::private_directory(&folder)?;
    let bytes = serde_json::to_vec_pretty(value)?;
    write_state(
        &folder.join(format!("{}.json", fingerprint(&bytes))),
        &bytes,
    )
}

fn persist_new(dir: &Path, draft: &Draft) -> Result<()> {
    let folder = root(dir)?.join(&draft.draft_id);
    fs::create_dir(&folder)?;
    local_state::private_directory(&folder)?;
    write_state(
        &folder.join("manifest.json"),
        &serde_json::to_vec_pretty(draft)?,
    )?;
    #[cfg(unix)]
    fs::File::open(root(dir)?)?.sync_all()?;
    Ok(())
}

pub(super) fn apply(dir: &Path, cfg: &Config, snapshot: RemoteSnapshot) -> Result<()> {
    // Validate the complete response before interpreting anything as deleted.
    let remote_ids = snapshot
        .messages
        .iter()
        .map(|(_, raw)| remote_id(raw))
        .collect::<Result<Vec<_>>>()?;
    let mut known = ids(dir)?
        .iter()
        .map(|id| load(dir, id, cfg).map(|(path, draft, _)| (path, draft)))
        .collect::<Result<Vec<_>>>()?;
    let mut seen = std::collections::HashSet::new();
    for ((uid, raw), message_id) in snapshot.messages.iter().zip(&remote_ids) {
        let remote = RemoteDraft {
            folder: snapshot.folder.clone(),
            uid: *uid,
            uid_validity: snapshot.uid_validity,
        };
        let matches = known
            .iter()
            .enumerate()
            .filter_map(|(index, (_, draft))| {
                let same_uid = draft.remote.as_ref().is_some_and(|r| {
                    r.folder == remote.folder
                        && r.uid_validity == remote.uid_validity
                        && r.uid == remote.uid
                });
                let same_id = message_id.as_ref().is_some_and(|id| {
                    draft
                        .remote_message_id
                        .as_ref()
                        .unwrap_or(&draft.message_id)
                        == id
                });
                (same_uid
                    || same_id
                    || (draft.edit_pending && message_id.as_deref() == Some(&draft.message_id)))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        let duplicate_id = message_id.is_some()
            && remote_ids
                .iter()
                .filter(|other| *other == message_id)
                .count()
                > 1;
        if matches.is_empty() {
            let key = message_id.clone().unwrap_or_else(|| {
                format!("{}|{}|{}", remote.folder, remote.uid_validity, remote.uid)
            });
            let id = deterministic_id(&format!("{}|{}", account(cfg)?, key));
            let mut draft = imported(cfg, remote, raw, id)?;
            if duplicate_id {
                draft.import_error = Some("多份遠端草稿使用相同 Message-ID，請先在網頁整理".into());
            }
            persist_new(dir, &draft)?;
            seen.insert(draft.draft_id.clone());
            known.push((path(dir, &draft.draft_id)?, draft));
            continue;
        }
        let ambiguous = matches.len() != 1 || duplicate_id;
        for index in matches {
            let (path, draft) = &mut known[index];
            seen.insert(draft.draft_id.clone());
            if draft.edit_pending {
                continue;
            }
            let unchanged = draft.remote_fingerprint.as_deref() == Some(fingerprint(raw).as_str())
                || (draft.remote_fingerprint.is_none()
                    && STANDARD.decode(&draft.mime_base64)? == *raw);
            if ambiguous
                || (!unchanged
                    && (matches!(draft.state.as_str(), "sending" | "unknown" | "sent")
                        || draft.delete_pending))
            {
                archive(
                    dir,
                    &draft.draft_id,
                    &serde_json::json!({"remote": remote, "mime_base64": STANDARD.encode(raw)}),
                )?;
                draft.remote_sync = "failed".into();
                draft.remote_conflict = true;
                if draft.state == "scheduled" {
                    draft.state = "draft".into();
                    draft.scheduled_at = None;
                }
                draft.remote_error = Some("遠端修改與本機寄送／刪除狀態衝突或 Message-ID 重複；已保留遠端版本於 outgoing-draft-history，請人工處理".into());
            } else if unchanged {
                draft.remote = Some(remote.clone());
                draft.remote_fingerprint = Some(fingerprint(raw));
                draft.remote_message_id = message_id.clone();
                draft.remote_sync = "synced".into();
                draft.remote_conflict = false;
                if draft.source == "remote" || draft.remote_mime_base64.is_some() {
                    draft.import_error =
                        imported(cfg, remote.clone(), raw, draft.draft_id.clone())?.import_error;
                }
                if !draft.remote_deleted {
                    draft.remote_error = None;
                }
                // Tombstones are deliberately not resurrected automatically.
            } else {
                archive(dir, &draft.draft_id, draft)?;
                let mut updated = imported(cfg, remote.clone(), raw, draft.draft_id.clone())?;
                updated.created_at = draft.created_at.clone();
                updated.source = draft.source.clone();
                updated.remote_error =
                    Some("網頁草稿已修改；舊版本已保存，原排程已取消，請重新預覽".into());
                *draft = updated;
            }
            write_state(path, &serde_json::to_vec_pretty(draft)?)?;
        }
    }
    for (path, mut draft) in known {
        if draft.edit_pending
            || draft.delete_pending
            || draft.state == "sent"
            || seen.contains(&draft.draft_id)
            || !draft
                .remote
                .as_ref()
                .is_some_and(|r| r.folder == snapshot.folder)
            || draft.remote_deleted
        {
            continue;
        }
        archive(dir, &draft.draft_id, &draft)?;
        draft.remote_deleted = true;
        draft.scheduled_at = None;
        if draft.state == "scheduled" {
            draft.state = "draft".into();
        }
        draft.remote_sync = "synced".into();
        draft.remote_error = Some(
            "網頁草稿已刪除或 UIDVALIDITY 改變後無法辨識；保留本機快照，禁止自動復原／寄送".into(),
        );
        write_state(&path, &serde_json::to_vec_pretty(&draft)?)?;
    }
    Ok(())
}

pub(super) fn pull(dir: &Path, cfg: &Config, password: &str) -> Result<bool> {
    let snapshot = match provider::mail::read_drafts(cfg, password) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            for id in ids(dir)? {
                let (path, mut draft, _) = load(dir, &id, cfg)?;
                if draft.remote.is_some() {
                    draft.remote_sync = "failed".into();
                    draft.remote_error = Some(format!("遠端草稿拉取失敗，保留本機快照：{error:#}"));
                    write_state(&path, &serde_json::to_vec_pretty(&draft)?)?;
                }
            }
            return Err(error);
        }
    };
    if let Some(snapshot) = snapshot {
        apply(dir, cfg, snapshot)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::new("me@example.test".into(), "localhost").unwrap();
        config::save(dir.path(), &cfg).unwrap();
        (dir, cfg)
    }
    fn raw(subject: &str) -> Vec<u8> {
        format!("From: me@example.test\r\nTo: you@example.test\r\nMessage-ID: <web@example.test>\r\nSubject: {subject}\r\n\r\nbody\r\n").into_bytes()
    }
    fn snapshot(uid: u32, bytes: Vec<u8>) -> RemoteSnapshot {
        RemoteSnapshot {
            folder: "Drafts".into(),
            uid_validity: 7,
            messages: vec![(uid, bytes)],
        }
    }
    #[test]
    fn pending_edit_pull_preserves_new_content_schedule_and_identity() {
        let (dir, cfg) = setup();
        apply(dir.path(), &cfg, snapshot(1, raw("old"))).unwrap();
        let id = ids(dir.path()).unwrap().pop().unwrap();
        let (path, mut draft, _) = load(dir.path(), &id, &cfg).unwrap();
        let new_bytes = String::from_utf8(raw("new"))
            .unwrap()
            .replace("<web@example.test>", "<new@example.test>")
            .into_bytes();
        draft.message_id = "<new@example.test>".into();
        draft.mime_base64 = STANDARD.encode(&new_bytes);
        draft.sha256 = digest(&draft.from, &draft.to, &new_bytes).unwrap();
        draft.edit_pending = true;
        draft.remote_sync = "failed".into();
        draft.state = "scheduled".into();
        draft.scheduled_at = Some("2099-01-01T09:00:00+08:00".into());
        write_state(&path, &serde_json::to_vec(&draft).unwrap()).unwrap();
        for remote in [
            snapshot(1, raw("old")),
            snapshot(2, new_bytes.clone()),
            RemoteSnapshot {
                folder: "Drafts".into(),
                uid_validity: 7,
                messages: vec![],
            },
        ] {
            apply(dir.path(), &cfg, remote).unwrap();
            let (_, after, bytes) = load(dir.path(), &id, &cfg).unwrap();
            assert_eq!(bytes, new_bytes);
            assert!(after.edit_pending);
            assert!(!after.remote_deleted);
            assert_eq!(after.scheduled_at, draft.scheduled_at);
            assert_eq!(after.remote.unwrap().uid, 1);
            assert_eq!(ids(dir.path()).unwrap().len(), 1);
        }
    }

    #[test]
    fn import_updates_cancel_schedule_and_remote_delete_preserves_history() {
        let (dir, cfg) = setup();
        apply(dir.path(), &cfg, snapshot(1, raw("old"))).unwrap();
        let id = ids(dir.path()).unwrap().pop().unwrap();
        let draft = show(dir.path(), &id).unwrap();
        assert_eq!(draft.source, "remote");
        assert_eq!(draft.state, "draft");
        assert!(draft.scheduled_at.is_none());
        schedule(dir.path(), &id, "2099-01-01").unwrap();
        apply(dir.path(), &cfg, snapshot(2, raw("new"))).unwrap();
        assert_eq!(ids(dir.path()).unwrap(), std::slice::from_ref(&id));
        let updated = show(dir.path(), &id).unwrap();
        assert_eq!(updated.state, "draft");
        assert!(updated.scheduled_at.is_none());
        assert_eq!(preview(&updated).unwrap()["subject"], "new");
        assert_eq!(updated.remote.as_ref().unwrap().uid, 2);
        let history = dir.path().join("outgoing-draft-history").join(&id);
        let old = fs::read_dir(&history)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let archived: Draft = serde_json::from_slice(&fs::read(old).unwrap()).unwrap();
        assert_eq!(preview(&archived).unwrap()["subject"], "old");
        apply(
            dir.path(),
            &cfg,
            RemoteSnapshot {
                folder: "Drafts".into(),
                uid_validity: 7,
                messages: vec![],
            },
        )
        .unwrap();
        let deleted = show(dir.path(), &id).unwrap();
        assert!(deleted.remote_deleted);
        assert_eq!(preview(&deleted).unwrap()["subject"], "new");
        assert!(send(dir.path(), &id).is_err());
        assert!(schedule(dir.path(), &id, "2099-01-01").is_err());
        assert!(run(dir.path(), false)
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[test]
    fn terminal_states_and_pending_delete_keep_both_versions_without_rescheduling() {
        for state in ["sending", "unknown", "sent", "draft"] {
            let (dir, cfg) = setup();
            apply(dir.path(), &cfg, snapshot(1, raw("old"))).unwrap();
            let id = ids(dir.path()).unwrap().pop().unwrap();
            let (path, mut draft, _) = load(dir.path(), &id, &cfg).unwrap();
            draft.state = state.into();
            draft.delete_pending = state == "draft";
            write_state(&path, &serde_json::to_vec(&draft).unwrap()).unwrap();
            apply(dir.path(), &cfg, snapshot(2, raw("changed"))).unwrap();
            let after = show(dir.path(), &id).unwrap();
            assert_eq!(after.state, state);
            assert_eq!(after.remote_sync, "failed");
            assert_eq!(preview(&after).unwrap()["subject"], "old");
            assert!(after.scheduled_at.is_none());
            assert!(dir.path().join("outgoing-draft-history").join(&id).exists());
        }
    }
    #[test]
    fn missing_message_id_and_bcc_import_safely_and_deduplicate_across_hosts() {
        let (a, cfg) = setup();
        let (b, _) = setup();
        let raw = b"From: me@example.test\r\nTo: you@example.test\r\nMessage-ID: \r\nBcc: secret@example.test,\r\n other@example.test\r\nSubject: test\r\n\r\nbody\r\n";
        for dir in [&a, &b] {
            apply(dir.path(), &cfg, snapshot(1, raw.to_vec())).unwrap();
        }
        let id = ids(a.path()).unwrap().pop().unwrap();
        assert_eq!(ids(b.path()).unwrap(), std::slice::from_ref(&id));
        apply(a.path(), &cfg, snapshot(1, raw.to_vec())).unwrap();
        assert_eq!(ids(a.path()).unwrap(), std::slice::from_ref(&id));
        let draft = show(a.path(), &id).unwrap();
        let bytes = STANDARD.decode(&draft.mime_base64).unwrap();
        let parsed = mailparse::parse_mail(&bytes).unwrap();
        assert!(parsed.headers.get_first_value("Bcc").is_none());
        assert_eq!(draft.to.len(), 3);
        assert_eq!(
            parsed.headers.get_first_value("Message-ID").unwrap(),
            draft.message_id
        );
        assert_eq!(
            STANDARD.decode(draft.remote_mime_base64.unwrap()).unwrap(),
            raw
        );
    }
    #[test]
    fn incomplete_and_ambiguous_web_drafts_are_visible_but_not_sendable() {
        let (dir, cfg) = setup();
        apply(
            dir.path(),
            &cfg,
            snapshot(1, b"Subject: unfinished\r\n\r\nbody".to_vec()),
        )
        .unwrap();
        let id = ids(dir.path()).unwrap().pop().unwrap();
        assert!(show(dir.path(), &id).unwrap().import_error.is_some());
        assert!(schedule(dir.path(), &id, "2099-01-01").is_err());
        let (dir, cfg) = setup();
        apply(
            dir.path(),
            &cfg,
            RemoteSnapshot {
                folder: "Drafts".into(),
                uid_validity: 7,
                messages: vec![(1, raw("one")), (2, raw("two"))],
            },
        )
        .unwrap();
        let id = ids(dir.path()).unwrap().pop().unwrap();
        assert!(show(dir.path(), &id).unwrap().import_error.is_some());
        assert!(schedule(dir.path(), &id, "2099-01-01").is_err());
        apply(dir.path(), &cfg, snapshot(1, raw("one"))).unwrap();
        assert!(show(dir.path(), &id).unwrap().import_error.is_none());
        assert!(!show(dir.path(), &id).unwrap().remote_conflict);
    }
    #[test]
    fn uidvalidity_reset_matches_message_id_without_duplicates() {
        let (dir, cfg) = setup();
        apply(dir.path(), &cfg, snapshot(1, raw("one"))).unwrap();
        let id = ids(dir.path()).unwrap().pop().unwrap();
        let mut changed = snapshot(99, raw("one"));
        changed.uid_validity = 42;
        apply(dir.path(), &cfg, changed).unwrap();
        assert_eq!(ids(dir.path()).unwrap(), std::slice::from_ref(&id));
        let draft = show(dir.path(), &id).unwrap();
        assert_eq!(draft.remote.unwrap().uid_validity, 42);
        assert!(!draft.remote_deleted);
    }
}
