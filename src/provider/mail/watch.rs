use super::*;
use futures_util::future::{select, Either};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::runtime::Runtime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub uid_validity: u32,
    pub last_uid: u32,
}

pub struct Delta {
    pub cursor: Cursor,
    pub items: Vec<Item>,
    pub baseline: bool,
    pub has_more: bool,
    pub parse_errors: BTreeMap<u32, String>,
}

pub struct Watcher {
    runtime: Runtime,
    session: Option<ImapSession>,
    pub idle_supported: bool,
}

impl Watcher {
    pub fn connect(config: &Config, password: &str, stop: &AtomicBool) -> Result<Self> {
        let runtime = Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        let (session, idle_supported) = runtime.block_on(cancellable(
            async {
                let mut session = connect_imap(config, password).await?;
                let capabilities = timeout(IMAP_TIMEOUT, session.capabilities())
                    .await
                    .context("IMAP CAPABILITY 逾時")?
                    .context("IMAP CAPABILITY 失敗")?;
                Ok::<_, anyhow::Error>((session, capabilities.has_str("IDLE")))
            },
            stop,
        ))?;
        Ok(Self {
            runtime,
            session: Some(session),
            idle_supported,
        })
    }

    pub fn poll(
        &mut self,
        folder: &str,
        cursor: Option<Cursor>,
        stop: &AtomicBool,
    ) -> Result<Delta> {
        if let Some(session) = &self.session {
            while session.unsolicited_responses.try_recv().is_ok() {}
        }
        self.runtime.block_on(cancellable(
            poll_with_session(
                self.session.as_mut().context("IMAP 監看連線已中斷")?,
                folder,
                cursor,
            ),
            stop,
        ))
    }

    pub fn wait(&mut self, duration: StdDuration, stop: &AtomicBool) -> Result<()> {
        if !self.idle_supported {
            self.runtime.block_on(wait_or_stop(duration, stop));
            return Ok(());
        }
        let session = self.session.take().context("IMAP 監看連線已中斷")?;
        let session = self.runtime.block_on(wait_idle(session, duration, stop))?;
        self.session = Some(session);
        Ok(())
    }
}

async fn stopped(stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }
}

pub(super) async fn cancellable<T>(
    future: impl std::future::Future<Output = Result<T>>,
    stop: &AtomicBool,
) -> Result<T> {
    match select(Box::pin(future), Box::pin(stopped(stop))).await {
        Either::Left((result, _)) => result,
        Either::Right(_) => bail!("郵件監看已停止"),
    }
}

async fn wait_or_stop(duration: StdDuration, stop: &AtomicBool) {
    let _ = select(
        Box::pin(tokio::time::sleep(duration)),
        Box::pin(stopped(stop)),
    )
    .await;
}

pub(super) async fn wait_idle<S: MailTransport>(
    session: Session<S>,
    duration: StdDuration,
    stop: &AtomicBool,
) -> Result<Session<S>> {
    let mut idle = session.idle();
    timeout(IMAP_TIMEOUT, idle.init())
        .await
        .context("啟動 IMAP IDLE 逾時")?
        .context("啟動 IMAP IDLE 失敗")?;
    {
        let (waiting, interrupt) =
            idle.wait_with_timeout(duration.min(StdDuration::from_secs(29 * 60)));
        let outcome = select(
            Box::pin(waiting),
            Box::pin(wait_or_stop(
                duration.min(StdDuration::from_secs(29 * 60)),
                stop,
            )),
        )
        .await;
        match outcome {
            Either::Left((result, _)) => {
                result.context("等待 IMAP IDLE 失敗")?;
            }
            Either::Right(((), waiting)) => {
                drop(interrupt);
                waiting.await.context("取消 IMAP IDLE 失敗")?;
            }
        }
    }
    timeout(IMAP_TIMEOUT, idle.done())
        .await
        .context("結束 IMAP IDLE 逾時")?
        .context("結束 IMAP IDLE 失敗")
}

pub(super) async fn poll_with_session<S: MailTransport>(
    session: &mut Session<S>,
    folder: &str,
    previous: Option<Cursor>,
) -> Result<Delta> {
    let mailbox = timeout(IMAP_TIMEOUT, session.examine(folder))
        .await
        .context("IMAP EXAMINE 逾時")?
        .context("IMAP EXAMINE 失敗")?;
    let uid_validity = mailbox
        .uid_validity
        .filter(|value| *value > 0)
        .context("IMAP 缺少有效 UIDVALIDITY")?;
    if previous.is_none_or(|cursor| cursor.uid_validity != uid_validity) {
        let last_uid = match mailbox.uid_next.filter(|value| *value > 0) {
            Some(next) => next - 1,
            None => timeout(IMAP_TIMEOUT, session.uid_search("ALL"))
                .await
                .context("建立 IMAP 基準逾時")?
                .context("建立 IMAP 基準失敗")?
                .into_iter()
                .max()
                .unwrap_or(0),
        };
        return Ok(Delta {
            cursor: Cursor {
                uid_validity,
                last_uid,
            },
            items: Vec::new(),
            baseline: true,
            has_more: false,
            parse_errors: BTreeMap::new(),
        });
    }
    let previous = previous.expect("matching cursor");
    let Some(next_uid) = previous.last_uid.checked_add(1) else {
        return Ok(Delta {
            cursor: previous,
            items: Vec::new(),
            baseline: false,
            has_more: false,
            parse_errors: BTreeMap::new(),
        });
    };
    let query = format!("UID {next_uid}:*");
    let selected = timeout(IMAP_TIMEOUT, session.uid_search(query))
        .await
        .context("IMAP 增量搜尋逾時")?
        .context("IMAP 增量搜尋失敗")?;
    let mut uids = selected
        .into_iter()
        .filter(|uid| *uid > previous.last_uid)
        .collect::<Vec<_>>();
    uids.sort_unstable();
    let has_more = uids.len() > FETCH_BATCH_SIZE;
    uids.truncate(FETCH_BATCH_SIZE);
    let messages = fetch_messages(session, &uids, "(UID FLAGS INTERNALDATE BODY.PEEK[])").await?;
    ensure_fetched(
        &uids.iter().copied().collect(),
        &messages,
        "增量 BODY.PEEK FETCH",
    )?;
    let mut items = Vec::with_capacity(uids.len());
    let mut parse_errors = BTreeMap::new();
    for uid in &uids {
        let fetched = &messages[uid];
        match parse_item(folder, uid_validity, fetched) {
            Ok(item) => items.push(item),
            Err(error) => {
                parse_errors.insert(*uid, format!("{error:#}"));
                items.push(unparsed_item(folder, uid_validity, fetched)?);
            }
        }
    }
    Ok(Delta {
        cursor: Cursor {
            uid_validity,
            last_uid: uids.last().copied().unwrap_or(previous.last_uid),
        },
        items,
        baseline: false,
        has_more,
        parse_errors,
    })
}

fn unparsed_item(folder: &str, uid_validity: u32, fetched: &FetchedMessage) -> Result<Item> {
    let raw = fetched.raw.as_deref().context("IMAP FETCH 缺少正文")?;
    let parsed = parse_mail(raw).ok();
    let title = parsed
        .as_ref()
        .map(|mail| display_header(&first_header(mail, "Subject")))
        .filter(|subject| !subject.is_empty())
        .unwrap_or_else(|| "(無法解析郵件)".into());
    let subtitle = parsed
        .as_ref()
        .map(|mail| display_header(&first_header(mail, "From")))
        .unwrap_or_default();
    let date = parsed
        .as_ref()
        .and_then(|mail| parse_date(mail, fetched.internal_date.as_deref()))
        .or_else(|| fetched.internal_date.clone());
    Ok(Item {
        id: stable_id(folder, uid_validity, fetched.uid),
        kind: Kind::Mail,
        collection: folder.into(),
        remote_id: fetched.uid.to_string(),
        uid_validity: Some(uid_validity),
        etag: Some(format!("{uid_validity}:{}", fetched.uid)),
        title,
        subtitle,
        date,
        end_date: None,
        text: String::new(),
        raw: format!("{RAW_BASE64_PREFIX}{}", BASE64_STANDARD.encode(raw)),
        flags: fetched.flags.clone(),
        emails: parsed
            .as_ref()
            .map(|mail| {
                let mut emails = addresses(mail, "From");
                emails.extend(addresses(mail, "To"));
                emails.extend(addresses(mail, "Cc"));
                emails.sort();
                emails.dedup();
                emails
            })
            .unwrap_or_default(),
        phones: Vec::new(),
        attachments: Vec::new(),
    })
}
