//! `mail send|forward|flag|move|delete|attachment` 的核心，CLI 與 TUI 共用。
//! 線上寫入一律先取同步鎖、讀本機郵件、拒絕展示資料，最後才連線；不重試。
use crate::{
    config::{self, Config},
    model::{Item, Kind, OutgoingMail},
    provider::{self, mail::MovePlan},
    store::Store,
    sync,
};
use anyhow::{bail, Context, Result};
use std::path::Path;

pub const DEMO_OFFLINE: &str = "展示資料不連線；請指定正式帳號的 --data-dir";

pub fn checked_store(dir: &Path) -> Result<Store> {
    let mut store = Store::open(dir)?;
    let cfg = config::load(dir)?;
    store.ensure_account(&cfg)?;
    Ok(store)
}

pub fn get(dir: &Path, id: &str, kind: Kind) -> Result<Item> {
    let item = checked_store(dir)?.get(id)?;
    if item.kind != kind {
        bail!("資料類型不符：{}", item.kind);
    }
    Ok(item)
}

pub fn online(dir: &Path) -> Result<(Config, String)> {
    if dir.join(".demo").exists() {
        bail!(DEMO_OFFLINE);
    }
    let cfg = config::load(dir)?;
    Store::open(dir)?.ensure_account(&cfg)?;
    let pass = config::password(&cfg)?;
    Ok((cfg, pass))
}

/// 建立新檔寫入；檔案已存在就失敗，不覆寫。
pub fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("無法建立檔案 {}；不覆寫既有檔案", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// `mail` 在確認可連線後才呼叫，讀不到正文時不會連線。
pub fn send(dir: &Path, mail: impl FnOnce() -> Result<OutgoingMail>) -> Result<String> {
    let _lock = sync::lock(dir)?;
    let (cfg, pass) = online(dir)?;
    provider::mail::send(&cfg, &pass, &mail()?)
}

pub fn forward(
    dir: &Path,
    id: &str,
    to: &[String],
    introduction: impl FnOnce() -> Result<String>,
) -> Result<String> {
    let _lock = sync::lock(dir)?;
    let item = get(dir, id, Kind::Mail)?;
    let (cfg, pass) = online(dir)?;
    let introduction = introduction()?;
    provider::mail::forward(&cfg, &pass, &item, to, &introduction)
}

/// `flag` 是 IMAP 旗標（`\Seen`、`\Flagged`、`\Answered`）；回傳已寫回本機的郵件。
pub fn flag(dir: &Path, id: &str, flag: &str, enabled: bool) -> Result<Item> {
    let _lock = sync::lock(dir)?;
    let mut item = get(dir, id, Kind::Mail)?;
    let (cfg, pass) = online(dir)?;
    provider::mail::flag(&cfg, &pass, &item, flag, enabled)?;
    item.flags.retain(|f| f != flag);
    if enabled {
        item.flags.push(flag.into());
    }
    Store::open(dir)?.upsert(&item)?;
    Ok(item)
}

/// 非 dry-run 時移動成功後從本機移除，下次同步會在新資料夾出現。
pub fn move_mail(dir: &Path, ids: &[String], folder: &str, dry_run: bool) -> Result<MovePlan> {
    let _lock = sync::lock(dir)?;
    let items = ids
        .iter()
        .map(|id| get(dir, id, Kind::Mail))
        .collect::<Result<Vec<_>>>()?;
    let (cfg, pass) = online(dir)?;
    let plan = provider::mail::move_many(&cfg, &pass, &items, folder, dry_run)?;
    if !dry_run {
        let mut store = Store::open(dir)?;
        for id in ids {
            store.remove(id)?;
        }
    }
    Ok(plan)
}

pub fn delete(dir: &Path, id: &str) -> Result<()> {
    let _lock = sync::lock(dir)?;
    let item = get(dir, id, Kind::Mail)?;
    let (cfg, pass) = online(dir)?;
    provider::mail::delete(&cfg, &pass, &item)?;
    Store::open(dir)?.remove(id)?;
    Ok(())
}

/// 從本機原文取出附件寫到 `output`（不覆寫）；回傳原始檔名與位元組數。
pub fn save_attachment(
    dir: &Path,
    id: &str,
    index: usize,
    output: &Path,
) -> Result<(String, usize)> {
    let item = get(dir, id, Kind::Mail)?;
    let (name, bytes) = provider::mail::attachment(&item, index)?;
    write_new(output, &bytes)?;
    Ok((name, bytes.len()))
}
