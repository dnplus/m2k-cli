use crate::{
    directory::{self, DirectoryNode},
    model::{Collection, Item, Kind, Snapshot},
    store::Store,
    sync,
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::Args;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub const FORMAT: &str = "m2k-directory/1";
pub const ENV_VAR: &str = "M2K_DIRECTORY_CACHE";

#[derive(Debug, Args)]
pub struct ExportArgs {
    #[arg(long, help = "輸出的 JSON 檔路徑；以暫存檔加 rename 原子寫入")]
    pub output: PathBuf,
    #[arg(
        long,
        visible_alias = "path",
        help = "要匯出的通訊錄分支（含子部門）；可填 path、名稱或 URL，預設為站台設定的公司通訊錄根"
    )]
    pub addressbook: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryCacheFile {
    pub format: String,
    pub generated_at: DateTime<Utc>,
    pub collections: Vec<Collection>,
    pub items: Vec<Item>,
}

#[derive(Deserialize)]
struct Header {
    format: String,
    generated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct ExportResult {
    pub output: String,
    pub generated_at: DateTime<Utc>,
    pub collections: usize,
    pub items: usize,
}

pub fn execute_export(data_dir: &Path, args: ExportArgs) -> Result<()> {
    let result = export(data_dir, &args.output, args.addressbook.as_deref())?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub fn export(data_dir: &Path, output: &Path, selector: Option<&str>) -> Result<ExportResult> {
    let selector = match selector {
        Some(selector) => selector.to_owned(),
        None => {
            let config = crate::config::load(data_dir)?;
            crate::site::SiteProfile::for_config(Some(&config))?
                .company_directory_root
                .context("站台設定沒有公司通訊錄根；請以 --addressbook 指定分支")?
        }
    };
    let selector = selector.as_str();
    let store = directory::local_store(data_dir)?;
    let all_collections = store.collections(Kind::Contacts)?;
    let nodes = directory::address_book_tree(&all_collections, None);
    let selected = directory::select_node(&nodes, selector)?;
    let ids = nodes
        .iter()
        .filter(|node| in_branch(node, selected))
        .map(|node| node.id.as_str())
        .collect::<HashSet<_>>();
    let collections = all_collections
        .into_iter()
        .filter(|collection| ids.contains(collection.id.as_str()))
        .collect::<Vec<_>>();
    let mut items = store
        .list(Kind::Contacts, None, None, usize::MAX)?
        .into_iter()
        .filter(|item| ids.contains(item.collection.as_str()))
        .collect::<Vec<_>>();
    if items.is_empty() {
        bail!("本機快取沒有「{selector}」的聯絡人；請先執行 directory sync --scope company");
    }
    items.sort_by(|left, right| left.id.cmp(&right.id));
    let file = DirectoryCacheFile {
        format: FORMAT.to_owned(),
        generated_at: Utc::now(),
        collections,
        items,
    };
    write_atomically(output, &serde_json::to_vec(&file)?)?;
    Ok(ExportResult {
        output: output.display().to_string(),
        generated_at: file.generated_at,
        collections: file.collections.len(),
        items: file.items.len(),
    })
}

fn in_branch(node: &DirectoryNode, selected: &DirectoryNode) -> bool {
    directory::same_or_descendant_path(&node.path, &selected.path)
        || directory::same_or_descendant_url(&node.id, &selected.id)
}

fn write_atomically(output: &Path, bytes: &[u8]) -> Result<()> {
    let name = output
        .file_name()
        .context("輸出路徑必須是檔案")?
        .to_string_lossy();
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp_path = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .with_context(|| format!("無法建立暫存檔：{}", temp_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("無法寫入暫存檔：{}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("無法同步暫存檔：{}", temp_path.display()))?;
        drop(file);
        fs::rename(&temp_path, output)
            .with_context(|| format!("無法更新輸出檔：{}", output.display()))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    written
}

/// Imports the file named by `M2K_DIRECTORY_CACHE` when it is newer than the
/// last import into this data dir. Lookups keep working on local data when the
/// file is missing or bad, so every failure is only a warning on stderr.
pub fn import_from_env(data_dir: &Path) {
    let Some(path) = std::env::var_os(ENV_VAR).filter(|value| !value.is_empty()) else {
        return;
    };
    let path = PathBuf::from(path);
    match import_if_newer(data_dir, &path) {
        Ok(Some(count)) => eprintln!("已匯入共用通訊錄快取：{count} 筆聯絡人"),
        Ok(None) => {}
        Err(error) => eprintln!(
            "警告：無法匯入共用通訊錄快取 {}：{error:#}；改用本機快取",
            path.display()
        ),
    }
}

/// Returns the number of imported contacts, or `None` when already current.
pub fn import_if_newer(data_dir: &Path, path: &Path) -> Result<Option<usize>> {
    let bytes = fs::read(path).context("無法讀取檔案")?;
    let header: Header = serde_json::from_slice(&bytes).context("檔案格式錯誤")?;
    if header.format != FORMAT {
        bail!("不支援的格式：{}", header.format);
    }
    let _lock = sync::lock(data_dir)?;
    let mut store = Store::open(data_dir)?;
    if data_dir.join("config.json").exists() {
        store.ensure_account(&crate::config::load(data_dir)?)?;
    }
    if store
        .directory_cache_imported()?
        .is_some_and(|imported| imported >= header.generated_at)
    {
        return Ok(None);
    }
    let file: DirectoryCacheFile = serde_json::from_slice(&bytes).context("檔案格式錯誤")?;
    if file
        .collections
        .iter()
        .any(|collection| collection.kind != Kind::Contacts)
        || file.items.iter().any(|item| item.kind != Kind::Contacts)
    {
        bail!("檔案只能包含聯絡人");
    }
    let count = file.items.len();
    store
        .apply_collection_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections: file.collections,
                items: file.items,
            },
            Utc::now(),
        )
        .context("無法寫入通訊錄快取")?;
    store.mark_directory_cache_imported(file.generated_at)?;
    Ok(Some(count))
}
