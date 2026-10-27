//! CalDAV／CardDAV 寫入的唯一路徑：CLI 與 TUI 都經過這裡。
//! 建立用 `If-None-Match: *`，更新與刪除用 `base` 的 ETag `If-Match`；不自動重試。
//! PUT 已接受但本機快取失敗時回傳 [`RemoteWritePendingReconcile`] receipt。

use crate::{
    config::{self, Config},
    model::{Item, Kind},
    provider::{self, dav::RemoteWritePendingReconcile},
    store::Store,
    sync,
};
use anyhow::{bail, Result};
use std::path::Path;

fn online(dir: &Path) -> Result<(Config, String)> {
    if dir.join(".demo").exists() {
        bail!("展示資料不連線；請指定正式帳號的 --data-dir");
    }
    let cfg = config::load(dir)?;
    Store::open(dir)?.ensure_account(&cfg)?;
    let pass = config::password(dir, &cfg)?;
    Ok((cfg, pass))
}

/// 讀本機快取的資源，並確認類型相符。
pub fn get(dir: &Path, id: &str, kind: Kind) -> Result<Item> {
    let mut store = Store::open(dir)?;
    store.ensure_account(&config::load(dir)?)?;
    let item = store.get(id)?;
    if item.kind != kind {
        bail!("資料類型不符：{}", item.kind);
    }
    Ok(item)
}

// 錯誤順序固定為：sync.lock（account_busy）→ 本機資源 → 連線設定與密碼 → 內容。
// 內容由 closure 在這些檢查之後才產生，讀檔錯誤不會蓋掉忙碌或展示資料的錯誤。

/// 新增資源；`collection` 省略時用設定的 CalDAV／CardDAV URL。
/// `body` 收到實際要寫入的 collection URL，回傳要 PUT 的內容。
pub fn create(
    dir: &Path,
    kind: Kind,
    collection: Option<&str>,
    body: impl FnOnce(&Config, &str) -> Result<String>,
) -> Result<Item> {
    let _lock = sync::lock(dir)?;
    let (cfg, pass) = online(dir)?;
    let url = collection.map(str::to_owned).unwrap_or_else(|| {
        if kind == Kind::Calendar {
            cfg.caldav_url.clone()
        } else {
            cfg.carddav_url.clone()
        }
    });
    let raw = body(&cfg, &url)?;
    let item = provider::dav::put(&cfg, &pass, kind, &url, &raw, None)?;
    persist(dir, &item)?;
    Ok(item)
}

/// 以 `base` 的 ETag 條件更新；伺服器版本已變更時由伺服器拒絕。
/// `base` 在取得鎖之後才讀，`body` 依它產生新內容。
pub fn update(
    dir: &Path,
    base: impl FnOnce() -> Result<Item>,
    body: impl FnOnce(&Config, &Item) -> Result<String>,
) -> Result<Item> {
    let _lock = sync::lock(dir)?;
    let base = base()?;
    let (cfg, pass) = online(dir)?;
    let raw = body(&cfg, &base)?;
    let item = provider::dav::put(&cfg, &pass, base.kind, &base.collection, &raw, Some(&base))?;
    persist(dir, &item)?;
    Ok(item)
}

/// 以 `base` 的 ETag 條件刪除遠端資源，成功後移除本機快取。
pub fn delete(dir: &Path, base: impl FnOnce() -> Result<Item>) -> Result<Item> {
    let _lock = sync::lock(dir)?;
    let base = base()?;
    let (cfg, pass) = online(dir)?;
    provider::dav::delete(&cfg, &pass, &base)?;
    Store::open(dir)?.remove(&base.id)?;
    Ok(base)
}

fn persist(dir: &Path, item: &Item) -> Result<()> {
    Store::open(dir)
        .and_then(|mut store| store.upsert(item))
        .map_err(|error| {
            RemoteWritePendingReconcile {
                kind: item.kind,
                collection: item.collection.clone(),
                resource_url: item.remote_id.clone(),
                uid: None,
                operation: "PUT".to_owned(),
                status: "accepted".to_owned(),
                remote_accepted: true,
                reconcile_required: true,
                reason: format!("遠端寫入成功，本機快取失敗：{error:#}"),
            }
            .into()
        })
}
