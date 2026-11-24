//! Bidirectional draft snapshots. Never use mailbox-wide EXPUNGE.
use super::*;
use crate::mail_draft::{Draft, RemoteDraft};

pub fn sync_draft(
    cfg: &Config,
    password: &str,
    draft: &Draft,
    bytes: &[u8],
) -> Result<Option<RemoteDraft>> {
    run_async(async {
        let mut session = connect_imap(cfg, password).await?;
        let result = timeout(IMAP_TIMEOUT, sync_session(&mut session, cfg, draft, bytes))
            .await
            .context("同步 IMAP 草稿逾時；可重試")?;
        logout(session).await;
        result
    })
}

/// CREATE fails if the mailbox already exists. Keep this empty mailbox forever:
/// unlike a local lock this also arbitrates copies of a draft on separate hosts.
pub fn claim_draft(cfg: &Config, password: &str, id: &str, message_id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id)?;
    run_async(async {
        let mut session = connect_imap(cfg, password).await?;
        let result = timeout(IMAP_TIMEOUT, async {
            claim_session(&mut session, id).await?;
            claim_session(
                &mut session,
                &format!("message-{}", fingerprint(message_id.as_bytes())),
            )
            .await
        })
        .await
        .context("取得防重寄標記逾時")?
        .context("無法取得永久防重寄標記；可能已提交，請人工查證，勿刪標記重試");
        logout(session).await;
        result
    })
}

async fn claim_session<S: MailTransport>(session: &mut Session<S>, id: &str) -> Result<()> {
    session.create(format!("m2k-submission-{id}")).await?;
    Ok(())
}

async fn folder_optional<S: MailTransport>(
    session: &mut Session<S>,
    cfg: &Config,
) -> Result<Option<String>> {
    let mut names = session.list(None, Some("*")).await?;
    let mut special = Vec::new();
    let mut fallback = Vec::new();
    while let Some(name) = names.try_next().await? {
        if name.attributes().contains(&NameAttribute::NoSelect) {
            continue;
        }
        if name.attributes().contains(&NameAttribute::Drafts) {
            special.push(name.name().to_owned());
        }
        fallback.push(name.name().to_owned());
    }
    if special.len() > 1 {
        bail!("找到多個 SPECIAL-USE 草稿匣，拒絕猜測");
    }
    if let Some(name) = special.pop() {
        return Ok(Some(name));
    }
    if let Some(name) = &cfg.drafts_folder {
        return Ok(Some(name.clone()));
    }
    Ok(fallback.into_iter().find(|name| {
        let decoded = display_collection(name);
        decoded.eq_ignore_ascii_case("Drafts") || decoded == "草稿匣"
    }))
}

async fn folder<S: MailTransport>(session: &mut Session<S>, cfg: &Config) -> Result<String> {
    folder_optional(session, cfg)
        .await?
        .context("找不到草稿匣；請設定 config.json drafts_folder（IMAP 原始名稱）")
}

pub(crate) struct RemoteSnapshot {
    pub folder: String,
    pub uid_validity: u32,
    pub messages: Vec<(u32, Vec<u8>)>,
}

pub(crate) fn read_drafts(cfg: &Config, password: &str) -> Result<Option<RemoteSnapshot>> {
    run_async(async {
        let mut session = connect_imap(cfg, password).await?;
        let result = timeout(IMAP_TIMEOUT, read_session(&mut session, cfg))
            .await
            .context("讀取遠端草稿逾時")?;
        logout(session).await;
        result
    })
}

async fn read_session<S: MailTransport>(
    session: &mut Session<S>,
    cfg: &Config,
) -> Result<Option<RemoteSnapshot>> {
    let Some(folder) = folder_optional(session, cfg).await? else {
        return Ok(None);
    };
    let mailbox = session.examine(&folder).await?;
    let uid_validity = mailbox.uid_validity.context("草稿匣沒有 UIDVALIDITY")?;
    let mut uids = session
        .uid_search("UNDELETED")
        .await?
        .into_iter()
        .collect::<Vec<_>>();
    uids.sort_unstable();
    let mut messages = Vec::new();
    for uid in uids {
        let mut fetched = session
            .uid_fetch(uid.to_string(), "(UID FLAGS BODY.PEEK[])")
            .await?;
        let mut found = false;
        while let Some(message) = fetched.try_next().await? {
            if message.uid != Some(uid) {
                continue;
            }
            if message.flags().any(|flag| flag == Flag::Deleted) {
                found = true;
                continue;
            }
            messages.push((uid, message.body().context("草稿 FETCH 缺少正文")?.to_vec()));
            found = true;
        }
        // Incomplete snapshots must never be mistaken for remote deletion.
        if !found {
            bail!("遠端草稿在讀取中改變，請重試");
        }
    }
    Ok(Some(RemoteSnapshot {
        folder,
        uid_validity,
        messages,
    }))
}

pub(crate) fn fingerprint(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

async fn matching<S: MailTransport>(
    session: &mut Session<S>,
    message_id: &str,
) -> Result<Vec<u32>> {
    if message_id.chars().any(char::is_control) {
        bail!("不合法的 Message-ID");
    }
    let escaped = message_id.replace('\\', "\\\\").replace('"', "\\\"");
    let candidates = session
        .uid_search(format!("HEADER Message-ID \"{escaped}\""))
        .await?;
    let mut exact = Vec::new();
    for uid in candidates {
        // SEARCH HEADER is substring matching: verify the exact header before deleting.
        let mut fetched = session
            .uid_fetch(
                uid.to_string(),
                "(UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])",
            )
            .await?;
        while let Some(message) = fetched.try_next().await? {
            if message.uid != Some(uid) {
                continue;
            }
            let header = message.header().context("IMAP 未回傳 Message-ID 標頭")?;
            let (headers, _) = mailparse::parse_headers(header)?;
            if headers.get_first_value("Message-ID").as_deref() == Some(message_id) {
                exact.push(uid);
            }
        }
    }
    exact.sort_unstable();
    Ok(exact)
}

async fn sync_session<S: MailTransport>(
    session: &mut Session<S>,
    cfg: &Config,
    draft: &Draft,
    bytes: &[u8],
) -> Result<Option<RemoteDraft>> {
    let folder = match &draft.remote {
        Some(remote) => remote.folder.clone(),
        None => folder(session, cfg).await?,
    };
    let selected = session.select(&folder).await?;
    let validity = selected.uid_validity.context("草稿匣沒有 UIDVALIDITY")?;
    // Always search the fixed ID, including after UIDVALIDITY changes or an
    // ambiguous APPEND response. This recovers without appending a second copy.
    let mut uids =
        if let (Some(remote), Some(expected)) = (&draft.remote, &draft.remote_fingerprint) {
            if remote.uid_validity == validity {
                let mut fetched = session
                    .uid_fetch(remote.uid.to_string(), "(UID BODY.PEEK[])")
                    .await?;
                let mut matches = Vec::new();
                while let Some(message) = fetched.try_next().await? {
                    if message.uid != Some(remote.uid) {
                        continue;
                    }
                    let raw = message.body().context("草稿 FETCH 缺少正文")?;
                    if fingerprint(raw) != *expected {
                        bail!("遠端草稿已修改，保留兩邊內容；請先 draft sync 檢查衝突");
                    }
                    matches.push(remote.uid);
                }
                matches
            } else if let Some(message_id) = &draft.remote_message_id {
                matching(session, message_id).await?
            } else {
                bail!("UIDVALIDITY 已變更且遠端草稿沒有 Message-ID，拒絕猜測刪除目標");
            }
        } else {
            matching(session, &draft.message_id).await?
        };
    if draft.delete_pending || draft.state == "sent" {
        if uids.is_empty() {
            return Ok(None);
        }
        let uidplus = session.capabilities().await?.has_str("UIDPLUS");
        for uid in uids {
            drain_fetches(
                session
                    .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
                    .await?,
                "標記草稿刪除",
            )
            .await?;
            if uidplus {
                let removed = session.uid_expunge(uid.to_string()).await?;
                pin_mut!(removed);
                while removed.try_next().await?.is_some() {}
            }
        }
        if !uidplus {
            bail!(
                "伺服器沒有 UIDPLUS；只標記自己的草稿為 Deleted，拒絕全量 EXPUNGE；保留本機供重試"
            );
        }
        return Ok(None);
    }
    if uids.is_empty() {
        session
            .append(&folder, Some("(\\Draft)"), None, bytes)
            .await?;
        uids = matching(session, &draft.message_id).await?;
    }
    if uids.len() != 1 {
        bail!("Message-ID 對應 {} 份草稿，請人工查證", uids.len());
    }
    Ok(Some(RemoteDraft {
        folder,
        uid: uids[0],
        uid_validity: validity,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{mock_imap, mock_login};
    use super::*;

    const MIME: &str = "Message-ID: <fixed@test>\r\nSubject: test\r\n\r\nbody\r\n";
    fn draft(state: &str) -> Draft {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1, "draft_id": "a", "kind": "mail", "account": "test",
            "created_at": "", "message_id": "<fixed@test>", "from": "a@test", "to": ["b@test"],
            "sha256": "", "state": state, "mime_base64": ""
        }))
        .unwrap()
    }
    fn select() -> Vec<(String, String)> {
        vec![
            (
                "LIST \"\" *".into(),
                "* LIST (\\Drafts) \"/\" \"Special\"\r\n".into(),
            ),
            (
                "SELECT \"Special\"".into(),
                "* 1 EXISTS\r\n* OK [UIDVALIDITY 42] valid\r\n".into(),
            ),
        ]
    }
    fn search(uid: Option<u32>) -> Vec<(String, String)> {
        let mut steps = vec![(
            "UID SEARCH HEADER Message-ID \"<fixed@test>\"".into(),
            format!(
                "* SEARCH{}\r\n",
                uid.map(|n| format!(" {n}")).unwrap_or_default()
            ),
        )];
        if let Some(uid) = uid {
            let header = "Message-ID: <fixed@test>\r\n\r\n";
            steps.push((
                format!("UID FETCH {uid} (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])"),
                format!(
                    "* 1 FETCH (UID {uid} BODY[HEADER.FIELDS (MESSAGE-ID)] {{{}}}\r\n{header})\r\n",
                    header.len()
                ),
            ));
        }
        steps
    }
    fn execute(steps: Vec<(String, String)>, draft: &Draft) -> Result<Option<RemoteDraft>> {
        let (address, server) = mock_imap(steps);
        let result = run_async(async {
            let mut session = mock_login(address).await;
            sync_session(
                &mut session,
                &Config::new("a@test".into(), "localhost")?,
                draft,
                MIME.as_bytes(),
            )
            .await
        });
        server.join().unwrap();
        result
    }
    #[test]
    fn append_and_recover_ambiguous_response_without_duplicate() {
        let mut steps = select();
        steps.extend(search(None));
        steps.push((
            format!("APPEND \"Special\" (\\Draft) {{{}}}", MIME.len()),
            "+ ready\r\n".into(),
        ));
        steps.push((format!("LITERAL:{MIME}"), String::new()));
        steps.extend(search(Some(7)));
        let remote = execute(steps, &draft("draft")).unwrap().unwrap();
        assert_eq!((remote.uid, remote.uid_validity), (7, 42));
        // A previous successful APPEND with a lost response is found by ID.
        let mut retry = select();
        retry.extend(search(Some(7)));
        assert_eq!(execute(retry, &draft("draft")).unwrap().unwrap().uid, 7);
    }
    #[test]
    fn append_failure_can_retry() {
        let mut steps = select();
        steps.extend(search(None));
        steps.push((
            format!("APPEND \"Special\" (\\Draft) {{{}}}", MIME.len()),
            "{tag} NO full\r\n".into(),
        ));
        assert!(execute(steps, &draft("draft")).is_err());
        let mut retry = select();
        retry.extend(search(None));
        retry.push((
            format!("APPEND \"Special\" (\\Draft) {{{}}}", MIME.len()),
            "+ ready\r\n".into(),
        ));
        retry.push((format!("LITERAL:{MIME}"), String::new()));
        retry.extend(search(Some(9)));
        assert_eq!(execute(retry, &draft("draft")).unwrap().unwrap().uid, 9);
    }
    #[test]
    fn sent_and_deleted_only_expunge_exact_uid() {
        for state in ["sent", "draft"] {
            let mut draft = draft(state);
            draft.delete_pending = state == "draft";
            let mut steps = select();
            steps.extend(search(Some(7)));
            steps.extend([
                (
                    "CAPABILITY".into(),
                    "* CAPABILITY IMAP4rev1 UIDPLUS\r\n".into(),
                ),
                (
                    "UID STORE 7 +FLAGS.SILENT (\\Deleted)".into(),
                    String::new(),
                ),
                ("UID EXPUNGE 7".into(), "* 1 EXPUNGE\r\n".into()),
            ]);
            assert!(execute(steps, &draft).unwrap().is_none());
        }
    }
    #[test]
    fn without_uidplus_never_expunge_other_mail() {
        let mut steps = select();
        steps.extend(search(Some(7)));
        steps.extend([
            ("CAPABILITY".into(), "* CAPABILITY IMAP4rev1\r\n".into()),
            (
                "UID STORE 7 +FLAGS.SILENT (\\Deleted)".into(),
                String::new(),
            ),
        ]);
        assert!(execute(steps, &draft("sent"))
            .unwrap_err()
            .to_string()
            .contains("UIDPLUS"));
    }
    #[test]
    fn permanent_claim_refuses_second_submission() {
        let id = uuid::Uuid::new_v4().to_string();
        let command = format!("CREATE \"m2k-submission-{id}\"");
        let (address, server) = mock_imap(vec![
            (command.clone(), String::new()),
            (command, "{tag} NO already exists\r\n".into()),
        ]);
        run_async(async {
            let mut session = mock_login(address).await;
            claim_session(&mut session, &id).await?;
            assert!(claim_session(&mut session, &id).await.is_err());
            Ok(())
        })
        .unwrap();
        server.join().unwrap();
    }

    #[test]
    fn substring_search_never_deletes_another_message() {
        let mut steps = select();
        let mut found = search(Some(7));
        let other = "Message-ID: <fixed@test>.other\r\n\r\n";
        found[1].1 = format!(
            "* 1 FETCH (UID 7 BODY[HEADER.FIELDS (MESSAGE-ID)] {{{}}}\r\n{other})\r\n",
            other.len()
        );
        steps.extend(found);
        assert!(execute(steps, &draft("sent")).unwrap().is_none());
    }

    #[test]
    fn stale_uid_is_not_deleted_after_uidvalidity_changes() {
        let mut draft = draft("sent");
        draft.remote = Some(RemoteDraft {
            folder: "Special".into(),
            uid: 7,
            uid_validity: 1,
        });
        let mut steps = select();
        steps.remove(0); // persisted folder, no discovery
        steps.extend(search(None));
        assert!(execute(steps, &draft).unwrap().is_none());
    }
    #[test]
    fn unsolicited_fetch_cannot_authorize_deletion_of_candidate() {
        let mut steps = select();
        let mut found = search(Some(7));
        found[1].1 = found[1].1.replace("UID 7", "UID 99");
        steps.extend(found);
        assert!(execute(steps, &draft("sent")).unwrap().is_none());
    }
    #[test]
    fn pull_requires_complete_uid_snapshot_before_deletion_reconciliation() {
        let (address, server) = mock_imap(vec![
            (
                "LIST \"\" *".into(),
                "* LIST (\\Drafts) \"/\" \"Drafts\"\r\n".into(),
            ),
            (
                "EXAMINE \"Drafts\"".into(),
                "* 1 EXISTS\r\n* OK [UIDVALIDITY 42] valid\r\n".into(),
            ),
            ("UID SEARCH UNDELETED".into(), "* SEARCH 7\r\n".into()),
            ("UID FETCH 7 (UID FLAGS BODY.PEEK[])".into(), String::new()),
        ]);
        let result = run_async(async {
            let mut session = mock_login(address).await;
            read_session(&mut session, &Config::new("a@test".into(), "localhost")?).await
        });
        assert!(result.err().unwrap().to_string().contains("重試"));
        server.join().unwrap();
    }
}
