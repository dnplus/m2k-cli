use crate::{
    config::{self, Config},
    model::Item,
    provider::mail::watch::{Cursor, Watcher},
    store::Store,
    sync,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Args;
use fs2::FileExt;
use hmac::{Hmac, Mac};
use reqwest::{blocking::Client, redirect::Policy, Url};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Debug, Args)]
pub struct WatchArgs {
    #[arg(long, help = "收到新郵件後 POST JSON 的 HTTP(S) 網址")]
    pub webhook_url: String,
    #[arg(long, default_value = "INBOX")]
    pub folder: String,
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=1740), help = "IDLE 最長等待／不支援 IDLE 時的查詢間隔，秒")]
    pub poll_seconds: u64,
    #[arg(long, help = "執行一輪基準／增量查詢與待送事件，然後結束")]
    pub once: bool,
}

#[derive(Debug)]
pub struct Subscription {
    pub id: String,
    pub folder: String,
    pub url: String,
    account_key: String,
    email: String,
}

impl Subscription {
    pub fn new(cfg: &Config, folder: &str, endpoint: &str) -> Result<Self> {
        if folder.trim().is_empty() || folder.chars().any(char::is_control) {
            bail!("監看資料夾不可為空白或包含控制字元");
        }
        let url = Url::parse(endpoint).context("webhook 網址格式錯誤")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            bail!("webhook 必須是 HTTP(S) 網址，不可包含帳密或 fragment");
        }
        let account_key = config::credential_key(cfg)?;
        let url = url.to_string();
        let id = digest(&serde_json::to_vec(&(&account_key, folder, &url))?);
        Ok(Self {
            id,
            folder: folder.into(),
            url,
            account_key,
            email: cfg.email.clone(),
        })
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn event(
    subscription: &Subscription,
    item: &Item,
    parse_error: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Value> {
    let id = digest(&serde_json::to_vec(&(
        &subscription.account_key,
        &item.collection,
        item.uid_validity,
        &item.remote_id,
    ))?);
    let mut mail = item.clone();
    mail.raw.clear();
    Ok(
        json!({"schema_version":1,"event_id":id,"event":"mail.received","account":subscription.email,"observed_at":now,
        "folder":item.collection,"uid_validity":item.uid_validity,"uid":item.remote_id.parse::<u32>()?,"mail":mail,
        "parse_error":parse_error.map(|message| json!({"code":"mail_parse_failed","message":message}))}),
    )
}

pub struct Delivery {
    client: Client,
    secret: Option<Vec<u8>>,
}

/// 簽章內容為 `{timestamp}.` 加上原始 body 位元組；回傳小寫十六進位 HMAC-SHA256。
pub fn sign(secret: &[u8], timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC 接受任意長度金鑰");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("{:x}", mac.finalize().into_bytes())
}

/// 簽章金鑰只從環境變數 `M2K_WEBHOOK_SECRET` 讀取；未設定或空字串視為不簽章。
pub fn signing_secret() -> Result<Option<Vec<u8>>> {
    match std::env::var("M2K_WEBHOOK_SECRET") {
        Ok(secret) => Ok(Some(secret.into_bytes()).filter(|secret| !secret.is_empty())),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => bail!("M2K_WEBHOOK_SECRET 必須是 UTF-8 文字"),
    }
}

impl Delivery {
    pub fn new(token: Option<String>) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(token) = token {
            let mut header = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .context("M2K_WEBHOOK_TOKEN 不是有效的 HTTP 憑證")?;
            header.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, header);
        }
        Ok(Self {
            client: Client::builder()
                .default_headers(headers)
                .timeout(Duration::from_secs(10))
                .redirect(Policy::none())
                .build()?,
            secret: None,
        })
    }

    pub fn with_secret(mut self, secret: Option<Vec<u8>>) -> Self {
        self.secret = secret;
        self
    }

    pub fn drain(
        &self,
        store: &mut Store,
        subscription: &Subscription,
        stop: &AtomicBool,
    ) -> Result<Value> {
        let mut delivered = 0;
        let mut failure = None;
        for _ in 0..32 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let Some((id, payload, attempts)) =
                store.mail_webhook_pending(subscription, Utc::now())?
            else {
                break;
            };
            let mut request = self
                .client
                .post(&subscription.url)
                .header("Content-Type", "application/json")
                .header("Idempotency-Key", &id)
                .header("X-M2K-Event-ID", &id)
                .body(payload.clone());
            if let Some(secret) = &self.secret {
                // 每次嘗試以當下時間重新簽章；body 與 event_id 維持不變。
                let timestamp = Utc::now().timestamp();
                request = request
                    .header("X-M2K-Timestamp", timestamp.to_string())
                    .header(
                        "X-M2K-Signature",
                        format!("sha256={}", sign(secret, timestamp, payload.as_bytes())),
                    );
            }
            let result = request
                .send()
                .map_err(|error| error.without_url().to_string())
                .and_then(|response| {
                    if response.status().is_success() {
                        Ok(())
                    } else {
                        Err(format!("webhook HTTP {}", response.status().as_u16()))
                    }
                });
            match result {
                Ok(()) => {
                    store.mail_webhook_delivered(subscription, &id)?;
                    delivered += 1;
                }
                Err(error) => {
                    let seconds = (5_i64 * (1_i64 << attempts.min(6))).min(300);
                    store.mail_webhook_failed(
                        subscription,
                        &id,
                        &error,
                        Utc::now() + chrono::Duration::seconds(seconds),
                    )?;
                    failure = Some(
                        json!({"code":"webhook_delivery_failed","event_id":id,"message":error,"retryable":true,"retry_after_seconds":seconds}),
                    );
                    continue;
                }
            }
        }
        Ok(json!({"delivered":delivered,"error":failure}))
    }
}

fn pause(duration: Duration, stop: &AtomicBool) {
    let start = Instant::now();
    while start.elapsed() < duration && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn watch_error(error: &anyhow::Error, password: &str) -> Value {
    let detail = crate::error::ErrorOutput::from_error(error).error;
    json!({"code":if detail.code == "operation_failed" {"imap_watch_failed"} else {detail.code},
        "message":detail.message.replace(password,"[已隱藏]"),"retryable":true})
}

fn watcher_lock(dir: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(dir.join("mail-watch.lock"))?;
    file.try_lock_exclusive()
        .context("此帳號已有郵件 webhook 監看程序")?;
    Ok(file)
}

pub fn run(dir: &Path, args: WatchArgs) -> Result<()> {
    if dir.join(".demo").exists() {
        bail!("展示資料不提供線上郵件 webhook 監看");
    }
    let cfg = config::load(dir)?;
    let subscription = Subscription::new(&cfg, &args.folder, &args.webhook_url)?;
    let mut store = Store::open(dir)?;
    store.ensure_account(&cfg)?;
    let _watcher = watcher_lock(dir)?;
    let token = match std::env::var("M2K_WEBHOOK_TOKEN") {
        Ok(token) => Some(token).filter(|token| !token.is_empty()),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => bail!("M2K_WEBHOOK_TOKEN 必須是 UTF-8 文字"),
    };
    let delivery = Delivery::new(token)?.with_secret(signing_secret()?);
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
    let mut watcher = None;
    let mut previous_cursor: Option<Cursor> = store.mail_webhook_cursor(&subscription)?;
    let mut failures = 0;
    while !stop.load(Ordering::Relaxed) {
        let receipt = delivery.drain(&mut store, &subscription, &stop)?;
        let password = config::password(dir, &cfg)?;
        let cycle = (|| -> Result<_> {
            if watcher.is_none() {
                watcher = Some(Watcher::connect(&cfg, &password, &stop)?);
            }
            let delta = watcher.as_mut().expect("connected watcher").poll(
                &subscription.folder,
                previous_cursor,
                &stop,
            )?;
            let _lock = sync::lock(dir)?;
            store.record_mail_delta(&subscription, previous_cursor, &delta, Utc::now())?;
            previous_cursor = Some(delta.cursor);
            Ok(delta)
        })();
        match cycle {
            Ok(delta) => {
                failures = 0;
                let delivered = delivery.drain(&mut store, &subscription, &stop)?;
                println!(
                    "{}",
                    json!({"state":"watching","subscription_id":subscription.id,"folder":subscription.folder,
                    "transport":if watcher.as_ref().is_some_and(|watcher| watcher.idle_supported) {"idle"} else {"poll"},
                    "baseline":delta.baseline,"cursor":delta.cursor,"new_mail":delta.items.len(),"parse_errors":delta.parse_errors,"delivery":[receipt,delivered],"time":Utc::now()})
                );
                if args.once {
                    break;
                }
                if delta.has_more {
                    continue;
                }
                let retry = store
                    .mail_webhook_due(&subscription)?
                    .map(|due| (due - Utc::now().timestamp()).max(0) as u64);
                let seconds = retry.map_or(args.poll_seconds, |retry| retry.min(args.poll_seconds));
                if seconds == 0 {
                    continue;
                }
                if let Err(error) = watcher
                    .as_mut()
                    .expect("connected watcher")
                    .wait(Duration::from_secs(seconds), &stop)
                {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    println!(
                        "{}",
                        json!({"state":"reconnecting","error":watch_error(&error,&password),"time":Utc::now()})
                    );
                    watcher = None;
                    pause(Duration::from_secs(5), &stop);
                }
            }
            Err(error) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if args.once {
                    let detail = crate::error::ErrorOutput::from_error(&error).error;
                    return Err(crate::error::Failure {
                        code: detail.code,
                        message: detail.message.replace(&password, "[已隱藏]"),
                        retryable: detail.retryable,
                    }
                    .into());
                }
                failures += 1;
                println!(
                    "{}",
                    json!({"state":"reconnecting","error":watch_error(&error,&password),"delivery":receipt,"time":Utc::now()})
                );
                watcher = None;
                pause(
                    Duration::from_secs((5 * (1_u64 << failures.min(6))).min(300)),
                    &stop,
                );
            }
        }
    }
    println!(
        "{}",
        json!({"state":"stopped","subscription_id":subscription.id,"time":Utc::now()})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_errors_preserve_account_busy_and_redact_passwords() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = sync::lock(dir.path()).unwrap();
        let error = sync::lock(dir.path()).unwrap_err();
        let output = watch_error(&error, "fixture-password");
        assert_eq!(output["code"], "account_busy");
        assert_eq!(output["retryable"], true);
        let output = watch_error(
            &anyhow::anyhow!("failed fixture-password"),
            "fixture-password",
        );
        assert_eq!(output["code"], "imap_watch_failed");
        assert_eq!(output["message"], "failed [已隱藏]");
    }

    #[test]
    fn signature_matches_fixed_vector() {
        // 以 openssl 獨立計算：printf '1700000000.{"a":1}' | openssl dgst -sha256 -hmac key
        assert_eq!(
            sign(b"key", 1_700_000_000, br#"{"a":1}"#),
            "a438e398bfafc57e4396bb7fc2304422f0f768e965d073ca313cb52e22e6ad03"
        );
    }
}
