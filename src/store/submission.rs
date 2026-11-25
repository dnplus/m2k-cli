//! Durable, account-directory-local submission claims; never automatically removed.
use super::*;
use anyhow::bail;

pub(super) fn initialize(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS mail_submissions (
            draft_id TEXT PRIMARY KEY NOT NULL,
            message_id TEXT NOT NULL UNIQUE,
            state TEXT NOT NULL CHECK(state IN ('sending', 'sent', 'unknown')),
            updated_at TEXT NOT NULL
        );",
    )?;
    Ok(())
}

impl Store {
    pub(crate) fn check_submission(&self, draft_id: &str, message_id: &str) -> Result<()> {
        let state: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM mail_submissions WHERE draft_id = ?1 OR message_id = ?2 LIMIT 1",
                params![draft_id, message_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(state) = state {
            bail!("本機 SQLite 防重寄記號為 {state}，拒絕提交；請人工查證，勿刪記號重試");
        }
        Ok(())
    }

    pub(crate) fn claim_submission(&mut self, draft_id: &str, message_id: &str) -> Result<()> {
        // FULL ensures the claim reaches durable storage before SMTP, even in WAL mode.
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT INTO mail_submissions (draft_id, message_id, state, updated_at)
             VALUES (?1, ?2, 'sending', ?3) ON CONFLICT DO NOTHING",
            params![draft_id, message_id, Utc::now().to_rfc3339()],
        )?;
        if inserted != 1 {
            bail!("本機 SQLite 已有防重寄記號，拒絕再次提交");
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn finish_submission(
        &self,
        draft_id: &str,
        message_id: &str,
        state: &str,
    ) -> Result<()> {
        if !matches!(state, "sent" | "unknown") {
            bail!("無效的寄送結果");
        }
        let changed = self.conn.execute(
            "UPDATE mail_submissions SET state = ?3, updated_at = ?4
             WHERE draft_id = ?1 AND message_id = ?2 AND state = 'sending'",
            params![draft_id, message_id, state, Utc::now().to_rfc3339()],
        )?;
        if changed != 1 {
            bail!("無法更新防重寄記號；禁止自動重寄");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_claim_states_block_either_identity_after_reopening() {
        for state in ["sending", "sent", "unknown"] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = Store::open(dir.path()).unwrap();
            store.claim_submission("draft", "<message@test>").unwrap();
            if state != "sending" {
                store
                    .finish_submission("draft", "<message@test>", state)
                    .unwrap();
            }
            drop(store);
            let mut store = Store::open(dir.path()).unwrap();
            assert!(store.check_submission("draft", "<other@test>").is_err());
            assert!(store.check_submission("other", "<message@test>").is_err());
            assert!(store.claim_submission("draft", "<other@test>").is_err());
            assert!(store.claim_submission("other", "<message@test>").is_err());
            assert!(store.check_submission("new", "<other@test>").is_ok());
        }
    }

    #[test]
    fn concurrent_connections_admit_one_submission_for_either_key() {
        for same_draft in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            Store::open(dir.path()).unwrap();
            let barrier = std::sync::Barrier::new(2);
            let winners = std::thread::scope(|scope| {
                let threads: Vec<_> = (0..2)
                    .map(|n| {
                        let barrier = &barrier;
                        let dir = dir.path();
                        scope.spawn(move || {
                            let mut store = Store::open(dir).unwrap();
                            barrier.wait();
                            let id = if same_draft {
                                "same".into()
                            } else {
                                format!("draft-{n}")
                            };
                            let mid = if same_draft {
                                format!("<message-{n}@test>")
                            } else {
                                "<same@test>".into()
                            };
                            store.claim_submission(&id, &mid).is_ok()
                        })
                    })
                    .collect();
                threads
                    .into_iter()
                    .map(|t| usize::from(t.join().unwrap()))
                    .sum::<usize>()
            });
            assert_eq!(winners, 1);
        }
    }

    #[test]
    fn v4_database_upgrades_without_inventing_submission_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .conn
            .execute_batch("DROP TABLE mail_submissions; PRAGMA user_version=4;")
            .unwrap();
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        assert!(store.check_submission("old", "<old@test>").is_ok());
        let count: i64 = store
            .conn
            .query_row("SELECT count(*) FROM mail_submissions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
