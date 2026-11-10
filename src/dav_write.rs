//! 建立用 `If-None-Match: *`，更新與刪除用 `base` 的 ETag `If-Match`；不自動重試。
//! PUT 已接受但本機快取失敗時回傳 [`RemoteWritePendingReconcile`] receipt。

use crate::{
    config::{self, Config},
    model::{Item, Kind},
    provider::{
        self,
        dav::{DeleteOutcome, RemoteWritePendingReconcile},
    },
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
    let pass = config::password(&cfg)?;
    Ok((cfg, pass))
}

pub fn get(dir: &Path, id: &str, kind: Kind) -> Result<Item> {
    let mut store = Store::open(dir)?;
    store.ensure_account(&config::load(dir)?)?;
    let item = store.get(id)?;
    if item.kind != kind {
        bail!("資料類型不符：{}", item.kind);
    }
    Ok(item)
}

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

/// 本機沒有的資源（例如會議室 collection 裡的副本）不經快取，直接 live 刪除：
/// 先 live GET 取 ETag，再以 `If-Match` DELETE，不自動重試；成功後清掉指向同一 href 的快取。
pub fn delete_remote(dir: &Path, kind: Kind, url: &str) -> Result<()> {
    let _lock = sync::lock(dir)?;
    let (cfg, pass) = online(dir)?;
    match provider::dav::delete_resource(&cfg, &pass, kind, url, None)? {
        DeleteOutcome::Deleted => {}
        DeleteOutcome::NotFound => {
            return Err(crate::error::Failure {
                code: "not_found",
                message: format!("本機與遠端都找不到資源：{url}"),
                retryable: false,
            }
            .into())
        }
        DeleteOutcome::PreconditionFailed => {
            bail!("412：資源在取得 ETag 後已被修改，未刪除；請重新確認後再處理")
        }
    }
    let mut store = Store::open(dir)?;
    let target = crate::dav_url::identity(url);
    for item in store.list(kind, None, None, usize::MAX)? {
        if crate::dav_url::identity(&item.remote_id) == target {
            store.remove(&item.id)?;
        }
    }
    Ok(())
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
