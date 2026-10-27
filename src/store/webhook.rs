use super::*;
use crate::{
    mail_webhook::Subscription,
    provider::mail::watch::{Cursor, Delta},
};

pub(super) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS mail_webhook_subscriptions (
        id TEXT PRIMARY KEY NOT NULL, folder TEXT NOT NULL, url TEXT NOT NULL,
        uid_validity INTEGER NOT NULL, last_uid INTEGER NOT NULL,
        delivered INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS mail_webhook_events (
        id TEXT NOT NULL, subscription_id TEXT NOT NULL REFERENCES mail_webhook_subscriptions(id),
        payload TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
        next_attempt_at INTEGER NOT NULL, last_error TEXT, PRIMARY KEY(subscription_id,id));
        CREATE INDEX IF NOT EXISTS mail_webhook_pending_idx ON mail_webhook_events(subscription_id,next_attempt_at,id);")?;
    Ok(())
}

fn cursor(conn: &Connection, id: &str) -> Result<Option<Cursor>> {
    Ok(conn
        .query_row(
            "SELECT uid_validity,last_uid FROM mail_webhook_subscriptions WHERE id=?1",
            [id],
            |row| {
                Ok(Cursor {
                    uid_validity: row.get(0)?,
                    last_uid: row.get(1)?,
                })
            },
        )
        .optional()?)
}

impl Store {
    pub fn mail_webhook_cursor(&self, subscription: &Subscription) -> Result<Option<Cursor>> {
        cursor(&self.conn, &subscription.id)
    }

    pub fn record_mail_delta(
        &mut self,
        subscription: &Subscription,
        expected: Option<Cursor>,
        delta: &Delta,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if delta.cursor.uid_validity == 0
            || (delta.baseline && (!delta.items.is_empty() || !delta.parse_errors.is_empty()))
            || delta.parse_errors.keys().any(|uid| {
                !delta
                    .items
                    .iter()
                    .any(|item| item.remote_id == uid.to_string())
            })
        {
            anyhow::bail!("監看增量資料格式錯誤");
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if cursor(&tx, &subscription.id)? != expected {
            anyhow::bail!("監看進度已由其他程序更新，請重新查詢");
        }
        if !delta.baseline
            && expected.is_none_or(|old| {
                old.uid_validity != delta.cursor.uid_validity
                    || old.last_uid > delta.cursor.last_uid
            })
        {
            anyhow::bail!("監看 UID 進度無效");
        }
        if delta.baseline
            && expected.is_some_and(|old| old.uid_validity == delta.cursor.uid_validity)
        {
            anyhow::bail!("既有監看基準不可跳過新 UID");
        }
        if !delta.baseline
            && delta.cursor.last_uid
                != delta
                    .items
                    .iter()
                    .map(|item| item.remote_id.parse::<u32>())
                    .collect::<std::result::Result<Vec<_>, _>>()?
                    .into_iter()
                    .max()
                    .unwrap_or_else(|| expected.expect("existing cursor").last_uid)
        {
            anyhow::bail!("監看進度缺少對應的新郵件");
        }
        tx.execute("INSERT INTO mail_webhook_subscriptions(id,folder,url,uid_validity,last_uid,updated_at)
            VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET uid_validity=excluded.uid_validity,last_uid=excluded.last_uid,updated_at=excluded.updated_at",
            params![subscription.id,subscription.folder,subscription.url,delta.cursor.uid_validity,delta.cursor.last_uid,now.timestamp()])?;
        if !delta.items.is_empty() {
            tx.execute("INSERT INTO collections(kind,id,name) VALUES ('mail',?1,?1) ON CONFLICT(kind,id) DO NOTHING", [&subscription.folder])?;
        }
        for item in &delta.items {
            let uid = item.remote_id.parse::<u32>()?;
            if item.kind != Kind::Mail
                || item.collection != subscription.folder
                || item.uid_validity != Some(delta.cursor.uid_validity)
                || uid <= expected.map_or(0, |old| old.last_uid)
                || uid > delta.cursor.last_uid
            {
                anyhow::bail!("新郵件 UID 與監看來源不符");
            }
            let encoded = encode_item(item)?;
            insert_or_update_item(&tx, &encoded, now.timestamp())?;
            let payload = crate::mail_webhook::event(
                subscription,
                item,
                delta.parse_errors.get(&uid).map(String::as_str),
                now,
            )?;
            let id = payload["event_id"].as_str().context("事件缺少識別碼")?;
            tx.execute("INSERT OR IGNORE INTO mail_webhook_events(id,subscription_id,payload,next_attempt_at) VALUES (?1,?2,?3,?4)", params![id,subscription.id,payload.to_string(),now.timestamp()])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mail_webhook_pending(
        &self,
        subscription: &Subscription,
        now: DateTime<Utc>,
    ) -> Result<Option<(String, String, u32)>> {
        Ok(self.conn.query_row("SELECT id,payload,attempts FROM mail_webhook_events WHERE subscription_id=?1 AND next_attempt_at<=?2 ORDER BY next_attempt_at,id LIMIT 1",
            params![subscription.id,now.timestamp()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?)
    }

    pub fn mail_webhook_delivered(&mut self, subscription: &Subscription, id: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM mail_webhook_events WHERE id=?1 AND subscription_id=?2",
            params![id, subscription.id],
        )?;
        tx.execute(
            "UPDATE mail_webhook_subscriptions SET delivered=delivered+?1 WHERE id=?2",
            params![deleted, subscription.id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn mail_webhook_failed(
        &self,
        subscription: &Subscription,
        id: &str,
        error: &str,
        next_attempt: DateTime<Utc>,
    ) -> Result<()> {
        self.conn.execute("UPDATE mail_webhook_events SET attempts=attempts+1,last_error=?1,next_attempt_at=?2 WHERE id=?3 AND subscription_id=?4",
            params![error,next_attempt.timestamp(),id,subscription.id])?;
        Ok(())
    }

    pub fn mail_webhook_due(&self, subscription: &Subscription) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT MIN(next_attempt_at) FROM mail_webhook_events WHERE subscription_id=?1",
            [&subscription.id],
            |row| row.get(0),
        )?)
    }

    pub fn mail_webhook_status(&self) -> Result<Value> {
        let mut query = self.conn.prepare(
            "SELECT s.id,s.folder,s.uid_validity,s.last_uid,s.delivered,s.updated_at,
            (SELECT COUNT(*) FROM mail_webhook_events e WHERE e.subscription_id=s.id),
            (SELECT MAX(attempts) FROM mail_webhook_events e WHERE e.subscription_id=s.id),
            (SELECT MIN(next_attempt_at) FROM mail_webhook_events e WHERE e.subscription_id=s.id),
            (SELECT last_error FROM mail_webhook_events e WHERE e.subscription_id=s.id AND last_error IS NOT NULL ORDER BY next_attempt_at DESC,id LIMIT 1)
            FROM mail_webhook_subscriptions s ORDER BY s.id",
        )?;
        let rows = query.query_map([], |row| Ok(json!({"subscription_id":row.get::<_,String>(0)?,"folder":row.get::<_,String>(1)?,
            "uid_validity":row.get::<_,u32>(2)?,"last_uid":row.get::<_,u32>(3)?,"delivered":row.get::<_,u64>(4)?,"updated_at":row.get::<_,i64>(5)?,
            "pending":row.get::<_,u64>(6)?,"max_attempts":row.get::<_,Option<u32>>(7)?,"next_attempt_at":row.get::<_,Option<i64>>(8)?,"last_error":row.get::<_,Option<String>>(9)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!(rows))
    }
}
