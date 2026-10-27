use super::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MovePath {
    Move,
    Uidplus,
    GuardedCopyFallback,
}

impl MovePath {
    fn from_capabilities(has_move: bool, has_uidplus: bool) -> Self {
        if has_move {
            Self::Move
        } else if has_uidplus {
            Self::Uidplus
        } else {
            Self::GuardedCopyFallback
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct MovePlan {
    pub path: MovePath,
    pub source: String,
    pub target: String,
    pub uids: BTreeSet<u32>,
    pub message_ids: Vec<String>,
    pub target_existing: BTreeMap<String, BTreeSet<u32>>,
    pub dry_run: bool,
    pub refusal: Option<String>,
}

struct MessageIdentity {
    uid: u32,
    message_id: String,
    deleted: bool,
}

trait FallbackImap {
    async fn open(&mut self, folder: &str, readonly: bool) -> Result<u32>;
    async fn fetch_identities(&mut self, uids: &BTreeSet<u32>) -> Result<Vec<MessageIdentity>>;
    async fn deleted(&mut self) -> Result<BTreeSet<u32>>;
    async fn copy(&mut self, uids: &BTreeSet<u32>, target: &str) -> Result<()>;
    async fn search_message_id(&mut self, message_id: &str) -> Result<BTreeSet<u32>>;
    async fn store_deleted(&mut self, uids: &BTreeSet<u32>, add: bool) -> Result<()>;
    async fn expunge_all(&mut self) -> Result<()>;
}

fn uid_set(uids: &BTreeSet<u32>) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

impl<T: MailTransport> FallbackImap for Session<T> {
    async fn open(&mut self, folder: &str, readonly: bool) -> Result<u32> {
        let mailbox = timeout(IMAP_TIMEOUT, async {
            if readonly {
                self.examine(folder).await
            } else {
                self.select(folder).await
            }
        })
        .await
        .context("開啟郵件資料夾逾時")??;
        if !readonly && !mailbox.permanent_flags.contains(&Flag::Deleted) {
            bail!("來源資料夾不允許永久設定 \\Deleted，拒絕移動");
        }
        mailbox
            .uid_validity
            .ok_or_else(|| anyhow!("郵件資料夾缺少 UIDVALIDITY"))
    }

    async fn fetch_identities(&mut self, uids: &BTreeSet<u32>) -> Result<Vec<MessageIdentity>> {
        let mut stream = timeout(
            IMAP_TIMEOUT,
            self.uid_fetch(
                uid_set(uids),
                "(UID FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])",
            ),
        )
        .await
        .context("讀取 Message-ID 逾時")??;
        let mut identities = Vec::new();
        while let Some(fetch) = timeout(IMAP_TIMEOUT, stream.try_next())
            .await
            .context("讀取 Message-ID 回應逾時")??
        {
            let uid = fetch
                .uid
                .ok_or_else(|| anyhow!("Message-ID 回應缺少 UID"))?;
            if !uids.contains(&uid) {
                continue;
            }
            let header = fetch
                .header()
                .ok_or_else(|| anyhow!("UID {uid} 缺少 Message-ID 標頭"))?;
            let (headers, _) = parse_headers(header)?;
            let ids = headers.get_all_values("Message-ID");
            if ids.len() != 1 || ids[0].trim().is_empty() || ids[0].contains(['\r', '\n', '\0']) {
                bail!("UID {uid} 缺少有效且唯一的 Message-ID");
            }
            identities.push(MessageIdentity {
                uid,
                message_id: ids[0].trim().to_owned(),
                deleted: fetch.flags().any(|flag| flag == Flag::Deleted),
            });
        }
        Ok(identities)
    }

    async fn deleted(&mut self) -> Result<BTreeSet<u32>> {
        Ok(timeout(IMAP_TIMEOUT, self.uid_search("DELETED"))
            .await
            .context("搜尋已刪除郵件逾時")??
            .into_iter()
            .collect())
    }

    async fn copy(&mut self, uids: &BTreeSet<u32>, target: &str) -> Result<()> {
        timeout(IMAP_TIMEOUT, self.uid_copy(uid_set(uids), target))
            .await
            .context("UID COPY 逾時")??;
        Ok(())
    }

    async fn search_message_id(&mut self, message_id: &str) -> Result<BTreeSet<u32>> {
        let quoted = message_id.replace('\\', "\\\\").replace('"', "\\\"");
        let hits = timeout(
            IMAP_TIMEOUT,
            self.uid_search(format!("HEADER Message-ID \"{quoted}\"")),
        )
        .await
        .context("驗證目標 Message-ID 逾時")??;
        Ok(hits.into_iter().collect())
    }

    async fn store_deleted(&mut self, uids: &BTreeSet<u32>, add: bool) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let query = if add {
            "+FLAGS.SILENT (\\Deleted)"
        } else {
            "-FLAGS.SILENT (\\Deleted)"
        };
        let updates = timeout(IMAP_TIMEOUT, self.uid_store(uid_set(uids), query))
            .await
            .context("修改來源刪除標記逾時")??;
        drain_fetches(updates, "修改來源刪除標記").await
    }

    async fn expunge_all(&mut self) -> Result<()> {
        let removed = timeout(IMAP_TIMEOUT, self.expunge())
            .await
            .context("EXPUNGE 逾時")??;
        pin_mut!(removed);
        while timeout(IMAP_TIMEOUT, removed.try_next())
            .await
            .context("EXPUNGE 回應逾時")??
            .is_some()
        {}
        Ok(())
    }
}

async fn check_source(imap: &mut impl FallbackImap, plan: &MovePlan, expected: u32) -> Result<()> {
    let actual = imap.open(&plan.source, plan.dry_run).await?;
    if actual != expected {
        bail!("郵件已過期：UIDVALIDITY 從 {expected} 變成 {actual}");
    }
    Ok(())
}

async fn preflight(
    imap: &mut impl FallbackImap,
    plan: &mut MovePlan,
    expected: u32,
) -> Result<(BTreeSet<u32>, u32)> {
    check_source(imap, plan, expected).await?;
    let identities = imap.fetch_identities(&plan.uids).await?;
    let found: BTreeSet<_> = identities.iter().map(|item| item.uid).collect();
    if found != plan.uids || identities.len() != plan.uids.len() {
        bail!("移動集合中有 UID 不存在或回應重複，拒絕移動");
    }
    let mut ids = HashSet::new();
    for item in &identities {
        if item.message_id.is_empty() || !ids.insert(item.message_id.clone()) {
            bail!("移動集合中有缺少或重複的 Message-ID，無法驗證");
        }
    }
    plan.message_ids = identities
        .iter()
        .map(|item| item.message_id.clone())
        .collect();
    let deleted = imap.deleted().await?;
    if !deleted.is_subset(&plan.uids) {
        bail!("來源有移動集合以外的郵件已標記 \\Deleted，拒絕移動以避免誤刪");
    }
    let already_deleted: BTreeSet<_> = deleted
        .into_iter()
        .chain(
            identities
                .iter()
                .filter(|item| item.deleted)
                .map(|item| item.uid),
        )
        .collect();
    let target_validity = imap.open(&plan.target, true).await?;
    for id in &plan.message_ids {
        plan.target_existing
            .insert(id.clone(), imap.search_message_id(id).await?);
    }
    Ok((
        plan.uids.difference(&already_deleted).copied().collect(),
        target_validity,
    ))
}

async fn guarded_copy(
    imap: &mut impl FallbackImap,
    plan: &mut MovePlan,
    expected: u32,
) -> Result<()> {
    let (ours, target_validity) = match preflight(imap, plan, expected).await {
        Ok(ours) => ours,
        Err(error) if plan.dry_run => {
            plan.refusal = Some(format!("{error:#}"));
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if plan.dry_run {
        return Ok(());
    }
    check_source(imap, plan, expected).await?;
    let copied = async {
        imap.copy(&plan.uids, &plan.target).await?;
        if imap.open(&plan.target, true).await? != target_validity {
            bail!("目標 UIDVALIDITY 已變更，無法比較複製前後的 UID");
        }
        for id in &plan.message_ids {
            let hits = imap.search_message_id(id).await?;
            let existing = &plan.target_existing[id];
            if hits.len() <= existing.len() || hits.difference(existing).next().is_none() {
                bail!("目標 Message-ID {id} 的命中數未增加或沒有新增 UID");
            }
        }
        check_source(imap, plan, expected).await
    }
    .await;
    copied.context("複製或驗證失敗；目標可能保留副本，來源未新增刪除標記、未執行 EXPUNGE")?;
    let flagged = async {
        imap.store_deleted(&plan.uids, true).await?;
        if imap.deleted().await? != plan.uids {
            bail!("標記後的 DELETED 集合與移動集合不符");
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = flagged {
        let rollback = imap.store_deleted(&ours, false).await;
        return match rollback {
            Ok(()) => {
                Err(error.context("已回復本次新增的刪除標記，未執行 EXPUNGE；目標可能保留副本"))
            }
            Err(rollback) => Err(error.context(format!(
                "回復刪除標記失敗：{rollback:#}；未執行 EXPUNGE，目標可能保留副本"
            ))),
        };
    }
    imap.expunge_all()
        .await
        .context("EXPUNGE 失敗，來源狀態待確認；目標可能保留副本")
}

fn prepare(items: &[Item], target: &str, dry_run: bool) -> Result<(MovePlan, u32)> {
    let first = items.first().ok_or_else(|| anyhow!("至少需要一封郵件"))?;
    let target = require(target, "移動目標資料夾")?;
    if target.eq_ignore_ascii_case(first.collection.trim()) {
        bail!("移動目標與來源相同");
    }
    let expected = first
        .uid_validity
        .ok_or_else(|| anyhow!("郵件缺少 UIDVALIDITY，拒絕變更"))?;
    let mut uids = BTreeSet::new();
    for item in items {
        if item.collection != first.collection || item.uid_validity != Some(expected) {
            bail!("批次移動必須使用相同來源資料夾與 UIDVALIDITY");
        }
        let uid = parse_uid(item)?;
        if uid == 0 {
            bail!("郵件 UID 不可為 0");
        }
        uids.insert(uid);
    }
    Ok((
        MovePlan {
            path: MovePath::GuardedCopyFallback,
            source: first.collection.clone(),
            target,
            uids,
            message_ids: Vec::new(),
            target_existing: BTreeMap::new(),
            dry_run,
            refusal: None,
        },
        expected,
    ))
}

pub fn move_many(
    config: &Config,
    password: &str,
    items: &[Item],
    target: &str,
    dry_run: bool,
) -> Result<MovePlan> {
    let (mut plan, expected) = prepare(items, target, dry_run)?;
    run_async(async {
        let mut session = connect_imap(config, password).await?;
        let result = async {
            if !dry_run {
                for item in items {
                    select_and_check(&mut session, item).await?;
                }
            }
            let has_move = server_has(&mut session, "MOVE").await?;
            let has_uidplus = !has_move && server_has(&mut session, "UIDPLUS").await?;
            plan.path = MovePath::from_capabilities(has_move, has_uidplus);
            if dry_run || plan.path == MovePath::GuardedCopyFallback {
                guarded_copy(&mut session, &mut plan, expected).await?;
            } else {
                move_supported_set(&mut session, &uid_set(&plan.uids), &plan.target, has_move)
                    .await?;
            }
            Ok(plan)
        }
        .await;
        logout(session).await;
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        source: BTreeSet<u32>,
        deleted: BTreeSet<u32>,
        seen: BTreeSet<u32>,
        target: BTreeMap<String, BTreeSet<u32>>,
        commands: Vec<String>,
        lose_copy: bool,
        race: bool,
        changed_validity: bool,
        duplicate_id: bool,
        missing_id: bool,
        search_error: bool,
        rollback_error: bool,
        source_opens: usize,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                source: [1, 2, 3, 99].into(),
                deleted: BTreeSet::new(),
                seen: [2, 99].into(),
                target: BTreeMap::new(),
                commands: Vec::new(),
                lose_copy: false,
                race: false,
                changed_validity: false,
                duplicate_id: false,
                missing_id: false,
                search_error: false,
                rollback_error: false,
                source_opens: 0,
            }
        }
    }

    impl FallbackImap for Fake {
        async fn open(&mut self, folder: &str, readonly: bool) -> Result<u32> {
            self.commands.push(format!(
                "{} {folder}",
                if readonly { "EXAMINE" } else { "SELECT" }
            ));
            if folder == "INBOX" {
                self.source_opens += 1;
                if self.changed_validity && self.source_opens > 2 {
                    return Ok(8);
                }
            }
            Ok(7)
        }
        async fn fetch_identities(&mut self, uids: &BTreeSet<u32>) -> Result<Vec<MessageIdentity>> {
            self.commands
                .push("UID FETCH FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)]".into());
            Ok(uids
                .intersection(&self.source)
                .map(|uid| MessageIdentity {
                    uid: *uid,
                    message_id: if self.missing_id {
                        String::new()
                    } else if self.duplicate_id {
                        "<duplicate>".into()
                    } else {
                        format!("<{uid}>")
                    },
                    deleted: self.deleted.contains(uid),
                })
                .collect())
        }
        async fn deleted(&mut self) -> Result<BTreeSet<u32>> {
            self.commands.push("UID SEARCH DELETED".into());
            Ok(self.deleted.clone())
        }
        async fn copy(&mut self, uids: &BTreeSet<u32>, _: &str) -> Result<()> {
            self.commands.push(format!("UID COPY {}", uid_set(uids)));
            if !self.lose_copy {
                for uid in uids {
                    let hits = self.target.entry(format!("<{uid}>")).or_default();
                    let next = hits.last().copied().unwrap_or(0) + 1;
                    hits.insert(next);
                }
            }
            Ok(())
        }
        async fn search_message_id(&mut self, message_id: &str) -> Result<BTreeSet<u32>> {
            self.commands
                .push(format!("UID SEARCH HEADER Message-ID {message_id}"));
            if self.search_error && self.commands.iter().any(|cmd| cmd.starts_with("UID COPY")) {
                bail!("模擬搜尋失敗");
            }
            Ok(self.target.get(message_id).cloned().unwrap_or_default())
        }
        async fn store_deleted(&mut self, uids: &BTreeSet<u32>, add: bool) -> Result<()> {
            if uids.is_empty() {
                return Ok(());
            }
            self.commands.push(format!(
                "UID STORE {} {}FLAGS.SILENT (\\Deleted)",
                uid_set(uids),
                if add { "+" } else { "-" }
            ));
            if add {
                self.deleted.extend(uids);
                if self.race {
                    self.deleted.insert(99);
                }
            } else {
                if self.rollback_error {
                    bail!("模擬回復失敗");
                }
                self.deleted.retain(|uid| !uids.contains(uid));
            }
            Ok(())
        }
        async fn expunge_all(&mut self) -> Result<()> {
            self.commands.push("EXPUNGE".into());
            self.source.retain(|uid| !self.deleted.contains(uid));
            self.deleted.clear();
            Ok(())
        }
    }

    fn plan() -> MovePlan {
        MovePlan {
            path: MovePath::GuardedCopyFallback,
            source: "INBOX".into(),
            target: "Archive".into(),
            uids: [1, 2, 3].into(),
            message_ids: Vec::new(),
            target_existing: BTreeMap::new(),
            dry_run: false,
            refusal: None,
        }
    }

    fn execute(fake: &mut Fake, plan: &mut MovePlan) -> Result<()> {
        run_async(guarded_copy(fake, plan, 7))
    }

    fn assert_no_mutations(fake: &Fake) {
        assert!(fake.commands.iter().all(|cmd| !cmd.starts_with("UID COPY")
            && !cmd.starts_with("UID STORE")
            && cmd != "EXPUNGE"));
    }

    #[test]
    fn unrelated_deleted_refuses_before_copy() {
        let mut fake = Fake {
            deleted: [99].into(),
            ..Fake::default()
        };
        assert!(execute(&mut fake, &mut plan())
            .unwrap_err()
            .to_string()
            .contains("移動集合以外"));
        assert_no_mutations(&fake);
    }

    #[test]
    fn failed_verification_never_flags_or_expunges() {
        for search_error in [false, true] {
            let mut fake = Fake {
                lose_copy: !search_error,
                search_error,
                ..Fake::default()
            };
            let error = execute(&mut fake, &mut plan()).unwrap_err();
            assert!(error.to_string().contains("可能保留副本"));
            assert!(fake
                .commands
                .iter()
                .all(|cmd| !cmd.starts_with("UID STORE") && cmd != "EXPUNGE"));
            assert_eq!(fake.source, [1, 2, 3, 99].into());
        }
    }

    #[test]
    fn existing_target_hits_cannot_hide_silently_failed_copy() {
        let mut fake = Fake {
            target: [("<1>".into(), [41].into())].into(),
            lose_copy: true,
            ..Fake::default()
        };
        let mut plan = MovePlan {
            uids: [1].into(),
            ..plan()
        };
        let error = execute(&mut fake, &mut plan).unwrap_err();
        assert!(format!("{error:#}").contains("命中數未增加"));
        assert_eq!(plan.target_existing["<1>"], [41].into());
        assert!(fake.commands.iter().any(|cmd| cmd.starts_with("UID COPY")));
        assert!(fake
            .commands
            .iter()
            .all(|cmd| !cmd.starts_with("UID STORE") && cmd != "EXPUNGE"));
        assert_eq!(fake.source, [1, 2, 3, 99].into());
    }

    #[test]
    fn dry_run_reports_existing_target_uids_without_mutations() {
        let mut fake = Fake {
            target: [("<1>".into(), [41, 42].into())].into(),
            ..Fake::default()
        };
        let mut plan = MovePlan {
            dry_run: true,
            ..plan()
        };
        execute(&mut fake, &mut plan).unwrap();
        let output = serde_json::to_value(&plan).unwrap();
        assert_eq!(
            output["target_existing"]["<1>"],
            serde_json::json!([41, 42])
        );
        assert_eq!(output["target_existing"]["<2>"], serde_json::json!([]));
        assert_no_mutations(&fake);
        assert!(fake.commands.iter().all(|cmd| !cmd.starts_with("SELECT")));
    }

    #[test]
    fn post_store_mismatch_rolls_back_only_our_flags() {
        let mut fake = Fake {
            deleted: [2].into(),
            race: true,
            ..Fake::default()
        };
        assert!(execute(&mut fake, &mut plan()).is_err());
        assert_eq!(fake.deleted, [2, 99].into());
        assert!(fake
            .commands
            .contains(&"UID STORE 1,3 -FLAGS.SILENT (\\Deleted)".into()));
        assert!(!fake.commands.contains(&"EXPUNGE".into()));
    }

    #[test]
    fn rollback_failure_is_reported_without_expunge() {
        let mut fake = Fake {
            race: true,
            rollback_error: true,
            ..Fake::default()
        };
        assert!(execute(&mut fake, &mut plan())
            .unwrap_err()
            .to_string()
            .contains("回復刪除標記失敗"));
        assert!(!fake.commands.contains(&"EXPUNGE".into()));
    }

    #[test]
    fn batch_copies_flags_and_expunges_once_preserving_unrelated_and_seen() {
        let mut fake = Fake::default();
        execute(&mut fake, &mut plan()).unwrap();
        assert_eq!(fake.source, [99].into());
        assert_eq!(fake.seen, [2, 99].into());
        assert_eq!(fake.target.len(), 3);
        assert_eq!(
            fake.commands
                .iter()
                .filter(|cmd| cmd.starts_with("UID COPY"))
                .count(),
            1
        );
        assert_eq!(
            fake.commands
                .iter()
                .filter(|cmd| cmd.starts_with("UID STORE"))
                .count(),
            1
        );
        assert_eq!(
            fake.commands
                .iter()
                .filter(|cmd| cmd.as_str() == "EXPUNGE")
                .count(),
            1
        );
        assert_eq!(
            fake.commands
                .iter()
                .filter(|cmd| cmd.as_str() == "UID SEARCH DELETED")
                .count(),
            2
        );
        assert!(fake.commands.iter().all(|cmd| !cmd.contains("\\Seen")
            && !cmd.contains("BODY[")
            && !cmd.contains("RFC822")));
        assert_eq!(fake.commands[0], "SELECT INBOX");
        let target = fake
            .commands
            .iter()
            .position(|cmd| cmd == "EXAMINE Archive")
            .unwrap();
        let source = fake
            .commands
            .iter()
            .rposition(|cmd| cmd == "SELECT INBOX")
            .unwrap();
        assert!(source > target);
    }

    #[test]
    fn dry_run_is_readonly_on_every_capability_path_and_reports_refusal() {
        for path in [
            MovePath::Move,
            MovePath::Uidplus,
            MovePath::GuardedCopyFallback,
        ] {
            for refused in [false, true] {
                let mut fake = Fake::default();
                if refused {
                    fake.deleted.insert(99);
                }
                let mut plan = MovePlan {
                    path,
                    dry_run: true,
                    ..plan()
                };
                execute(&mut fake, &mut plan).unwrap();
                assert_eq!(plan.refusal.is_some(), refused);
                assert_eq!(plan.message_ids.len(), 3);
                assert_eq!(fake.commands[0], "EXAMINE INBOX");
                assert!(fake.commands.iter().all(|cmd| !cmd.starts_with("SELECT")));
                assert_no_mutations(&fake);
            }
        }
    }

    #[test]
    fn capability_selection_prefers_move_then_uidplus() {
        assert_eq!(MovePath::from_capabilities(true, true), MovePath::Move);
        assert_eq!(MovePath::from_capabilities(true, false), MovePath::Move);
        assert_eq!(MovePath::from_capabilities(false, true), MovePath::Uidplus);
        assert_eq!(
            MovePath::from_capabilities(false, false),
            MovePath::GuardedCopyFallback
        );
    }

    #[test]
    fn invalid_identities_refuse_before_copy() {
        for mut fake in [
            Fake {
                source: [1, 2, 99].into(),
                ..Fake::default()
            },
            Fake {
                duplicate_id: true,
                ..Fake::default()
            },
            Fake {
                missing_id: true,
                ..Fake::default()
            },
        ] {
            assert!(execute(&mut fake, &mut plan()).is_err());
            assert_no_mutations(&fake);
        }
    }

    #[test]
    fn uidvalidity_change_after_copy_never_flags_source() {
        let mut fake = Fake {
            changed_validity: true,
            ..Fake::default()
        };
        assert!(execute(&mut fake, &mut plan()).is_err());
        assert!(fake
            .commands
            .iter()
            .all(|cmd| !cmd.starts_with("UID STORE") && cmd != "EXPUNGE"));
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn real_adapter_uses_peek_readonly_verification_and_deleted_only_store() {
        run_async(async {
            let (client, server) = tokio::io::duplex(8192);
            let serve = async {
                let mut server = BufReader::new(server);
                server.write_all(b"* OK ready\r\n").await.unwrap();
                let source = "* OK [UIDVALIDITY 7] valid\r\n* OK [PERMANENTFLAGS (\\Deleted \\Seen)] flags\r\n";
                let header = "Message-ID: <one@example.test>\r\n\r\n";
                let fetch = format!("* 1 FETCH (UID 1 FLAGS (\\Seen) BODY[HEADER.FIELDS (MESSAGE-ID)] {{{}}}\r\n{header})\r\n", header.len());
                let steps = [
                    ("LOGIN \"test\" \"password\"", ""),
                    ("SELECT \"INBOX\"", source),
                    ("UID FETCH 1 (UID FLAGS BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])", fetch.as_str()),
                    ("UID SEARCH DELETED", "* SEARCH\r\n"),
                    ("EXAMINE \"Archive\"", "* OK [UIDVALIDITY 8] valid\r\n"),
                    ("UID SEARCH HEADER Message-ID \"<one@example.test>\"", "* SEARCH 41\r\n"),
                    ("SELECT \"INBOX\"", source),
                    ("UID COPY 1 \"Archive\"", ""),
                    ("EXAMINE \"Archive\"", "* OK [UIDVALIDITY 8] valid\r\n"),
                    ("UID SEARCH HEADER Message-ID \"<one@example.test>\"", "* SEARCH 41 42\r\n"),
                    ("SELECT \"INBOX\"", source),
                    ("UID STORE 1 +FLAGS.SILENT (\\Deleted)", ""),
                    ("UID SEARCH DELETED", "* SEARCH 1\r\n"),
                    ("EXPUNGE", "* 1 EXPUNGE\r\n"),
                ];
                for (expected, response) in steps {
                    let mut line = String::new();
                    server.read_line(&mut line).await.unwrap();
                    let (tag, command) = line.trim_end().split_once(' ').unwrap();
                    assert_eq!(command, expected);
                    server.write_all(format!("{response}{tag} OK complete\r\n").as_bytes()).await.unwrap();
                }
            };
            let execute = async {
                let mut client = Client::new(client);
                client.read_response().await.unwrap().unwrap();
                let mut session = client.login("test", "password").await.unwrap();
                let mut plan = MovePlan { path: MovePath::GuardedCopyFallback, source: "INBOX".into(), target: "Archive".into(), uids: [1].into(), message_ids: Vec::new(), target_existing: BTreeMap::new(), dry_run: false, refusal: None };
                guarded_copy(&mut session, &mut plan, 7).await.unwrap();
            };
            futures_util::future::join(serve, execute).await;
            Ok(())
        }).unwrap();
    }

    fn item(uid: u32) -> Item {
        serde_json::from_value(serde_json::json!({
            "id": format!("mail:INBOX:7:{uid}"), "kind": "mail", "collection": "INBOX",
            "remote_id": uid.to_string(), "uid_validity": 7, "title": "", "subtitle": "", "text": "", "raw": ""
        })).unwrap()
    }

    #[test]
    fn batch_validation_rejects_mixed_source_validity_empty_and_zero_uid() {
        assert!(prepare(&[], "Archive", false).is_err());
        assert!(prepare(&[item(0)], "Archive", false).is_err());
        assert!(prepare(&[item(1)], "INBOX", false).is_err());
        let mut other = item(2);
        other.collection = "Other".into();
        assert!(prepare(&[item(1), other], "Archive", false).is_err());
        let mut other = item(2);
        other.uid_validity = Some(8);
        assert!(prepare(&[item(1), other], "Archive", false).is_err());
        let (plan, expected) = prepare(&[item(1), item(2), item(1)], "Archive", false).unwrap();
        assert_eq!(expected, 7);
        assert_eq!(plan.uids, [1, 2].into());
    }
}
