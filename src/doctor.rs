//! 唯讀環境診斷：回答「這台機器、這個帳號能不能開始正式驗收」。
//!
//! 只做讀取：不建立檔案或目錄、不寄信、不改旗標、不寫行事曆，
//! 錯誤訊息一律先遮蔽密碼。

use crate::{
    config::{self, Config},
    model::Kind,
    provider, sqlite, store,
};
use anyhow::{anyhow, Result};
use fs2::FileExt;
use serde::Serialize;
use std::{fmt, fs, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub state: &'static str,
    pub data_dir: String,
    pub demo: bool,
    pub version: &'static str,
    pub build_commit: &'static str,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn failed(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|check| check.status == Status::Fail)
            .collect()
    }

    pub fn ok(&self) -> bool {
        self.failed().is_empty()
    }

    pub fn render(&self) -> String {
        let mut out = format!(
            "m2k doctor · {}{}\n",
            self.data_dir,
            if self.demo {
                "（展示資料目錄）"
            } else {
                ""
            }
        );
        for check in &self.checks {
            let label = match check.status {
                Status::Pass => "PASS",
                Status::Warn => "WARN",
                Status::Fail => "FAIL",
                Status::Skip => "SKIP",
            };
            out.push_str(&format!("[{label}] {:<12} {}\n", check.name, check.detail));
        }
        let failed = self.failed().len();
        if failed == 0 {
            out.push_str("結果：全部通過（略過項目不計）\n");
        } else {
            out.push_str(&format!("結果：{failed} 項失敗\n"));
        }
        out
    }
}

/// `doctor` 有檢查失敗時回傳；`--json` 會輸出完整報告並沿用錯誤契約。
#[derive(Debug)]
pub struct DoctorFailed(pub Report);

impl fmt::Display for DoctorFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<_> = self.0.failed().iter().map(|check| check.name).collect();
        write!(f, "{} 項檢查失敗：{}", names.len(), names.join("、"))
    }
}

impl std::error::Error for DoctorFailed {}

fn check(name: &'static str, result: Result<String>) -> Check {
    match result {
        Ok(detail) => Check {
            name,
            status: Status::Pass,
            detail,
        },
        Err(error) => Check {
            name,
            status: Status::Fail,
            detail: format!("{error:#}"),
        },
    }
}

fn skip(name: &'static str, reason: &str) -> Check {
    Check {
        name,
        status: Status::Skip,
        detail: reason.to_owned(),
    }
}

fn redact(check: &mut Check, password: Option<&str>) {
    if let Some(password) = password.filter(|value| !value.is_empty()) {
        check.detail = check.detail.replace(password, "[已隱藏]");
    }
}

pub fn run(dir: &Path) -> Report {
    let demo = dir.join(".demo").exists();
    let mut checks = vec![Check {
        name: "build",
        status: Status::Pass,
        detail: format!(
            "m2k {} ({})",
            crate::version::VERSION,
            crate::version::COMMIT
        ),
    }];
    checks.push(check("data_dir", data_dir(dir)));
    let (config_check, cfg) = match config_file(dir) {
        Ok((detail, cfg)) => (check("config", Ok(detail)), Some(cfg)),
        Err(error) => (check("config", Err(error)), None),
    };
    checks.push(config_check);
    if let Some(cfg) = &cfg {
        checks.push(site_profile(cfg));
    }

    const ONLINE: [&str; 5] = ["credentials", "imap", "smtp", "caldav", "carddav"];
    match (&cfg, demo) {
        (_, true) => {
            for name in ONLINE {
                checks.push(skip(name, "展示資料目錄：不進行線上檢查"));
            }
        }
        (None, false) => {
            for name in ONLINE {
                checks.push(skip(name, "設定檔不可用，無法檢查"));
            }
        }
        (Some(cfg), false) => online(dir, cfg, &mut checks),
    }
    checks.push(check("database", database(dir)));
    checks.push(check("sync_lock", sync_lock(dir)));
    let ok = checks.iter().all(|check| check.status != Status::Fail);
    Report {
        state: if ok { "ok" } else { "error" },
        data_dir: dir.display().to_string(),
        demo,
        version: crate::version::VERSION,
        build_commit: crate::version::COMMIT,
        checks,
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o777)
}

fn require_mode(path: &Path, expected: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let actual = mode(path)?;
        if actual != expected {
            return Err(anyhow!(
                "{} 權限為 {actual:04o}，應為 {expected:04o}（chmod {expected:o} '{}'）",
                path.display(),
                path.display()
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (path, expected);
    Ok(())
}

fn data_dir(dir: &Path) -> Result<String> {
    let meta = fs::metadata(dir).map_err(|error| anyhow!("資料目錄不可用：{error}"))?;
    if !meta.is_dir() {
        return Err(anyhow!("{} 不是目錄", dir.display()));
    }
    require_mode(dir, 0o700)?;
    Ok("目錄存在，權限 0700".into())
}

fn config_file(dir: &Path) -> Result<(String, Config)> {
    let path = dir.join("config.json");
    if !path.exists() {
        return Err(anyhow!(
            "找不到設定檔 {}；請先執行 m2k init",
            path.display()
        ));
    }
    require_mode(&path, 0o600)?;
    let cfg = config::load(dir)?;
    Ok((
        format!("{} 有效，權限 0600；帳號 {}", path.display(), cfg.email),
        cfg,
    ))
}

fn online(dir: &Path, cfg: &Config, checks: &mut Vec<Check>) {
    let from_env = std::env::var_os("M2K_PASSWORD").is_some_and(|value| !value.is_empty());
    let password = config::read_password(dir, cfg);
    checks.push(check(
        "credentials",
        match &password {
            Ok(_) if from_env => Ok("使用 M2K_PASSWORD".to_owned()),
            Ok(_) => Ok("系統密碼庫有憑證".to_owned()),
            Err(error) => Err(anyhow!("{error:#}")),
        },
    ));
    let Ok(password) = password else {
        for name in ["imap", "smtp", "caldav", "carddav"] {
            checks.push(skip(name, "沒有可用憑證，無法登入"));
        }
        return;
    };
    let mut results = vec![
        check(
            "imap",
            provider::mail::verify_imap(cfg, &password)
                .map(|()| format!("{}:{} 登入成功", cfg.imap_host, cfg.imap_port)),
        ),
        check(
            "smtp",
            provider::mail::verify_smtp(cfg, &password).map(|fallback| match fallback {
                Some(port) => format!(
                    "{} 登入成功（587 不通，改用 {port}；建議 m2k config set --smtp-port {port}）",
                    cfg.smtp_host
                ),
                None => format!("{}:{} 登入成功", cfg.smtp_host, cfg.smtp_port),
            }),
        ),
    ];
    for (name, kind, url) in [
        ("caldav", Kind::Calendar, &cfg.caldav_url),
        ("carddav", Kind::Contacts, &cfg.carddav_url),
    ] {
        let mut result = dav(cfg, &password, kind, url);
        if kind == Kind::Calendar {
            result =
                result.map(|detail| format!("{detail}；{}", calendar_sync_token(cfg, &password)));
        }
        results.push(check(name, result));
    }
    for mut result in results {
        redact(&mut result, Some(&password));
        checks.push(result);
    }
}

fn dav(cfg: &Config, password: &str, kind: Kind, url: &str) -> Result<String> {
    if url.trim().is_empty() {
        return Err(anyhow!("設定檔沒有 {kind} URL"));
    }
    let collections = provider::dav::discover(cfg, password, kind, url)?;
    if collections.is_empty() {
        return Err(anyhow!("登入成功但找不到可用的 {kind} collection"));
    }
    let first = &collections[0];
    Ok(format!(
        "登入成功，探索到 {} 個 collection（首個：{}）",
        collections.len(),
        first.id
    ))
}

/// Informational only: a server without RFC 6578 keeps the ETag strategy.
fn calendar_sync_token(cfg: &Config, password: &str) -> String {
    match provider::dav::probe_calendar_sync_tokens(cfg, password) {
        Ok((supported, total)) if supported == total && total > 0 => {
            format!("sync-token：{total} 個 collection 都宣告支援 sync-collection")
        }
        Ok((supported, total)) => format!(
            "sync-token：{supported}/{total} 個 collection 宣告支援 sync-collection，週期同步沿用 ETag 近區間"
        ),
        Err(error) => format!("sync-token：探測失敗（{error:#}），週期同步沿用 ETag 近區間"),
    }
}

fn site_profile(cfg: &Config) -> Check {
    let profile = match crate::site::SiteProfile::for_config(Some(cfg)) {
        Ok(profile) => profile,
        Err(error) => return check("site_profile", Err(error)),
    };
    let source = match &cfg.site_profile {
        Some(spec) => format!("config 指定 {spec}"),
        None => format!("依伺服器 {} 自動選用", cfg.imap_host),
    };
    let detail = format!("{}（{source}）", profile.name);
    if profile.is_generic() && crate::site::user_profiles_missing() {
        return Check {
            name: "site_profile",
            status: Status::Warn,
            detail: format!("{detail}；{}", crate::site::migration_hint()),
        };
    }
    check("site_profile", Ok(detail))
}

fn database(dir: &Path) -> Result<String> {
    let path = dir.join(store::DATABASE_FILE);
    if !path.exists() {
        return Err(anyhow!(
            "找不到資料庫 {}；請先執行 m2k init 或 sync",
            path.display()
        ));
    }
    let version = user_version(&path)?;
    match version.cmp(&store::SCHEMA_VERSION) {
        std::cmp::Ordering::Equal => Ok(format!("可開啟，schema v{version} 為最新")),
        std::cmp::Ordering::Less => Err(anyhow!(
            "schema v{version} 過舊，需要 v{}；執行任一會寫入的指令（如 m2k sync）會自動升級",
            store::SCHEMA_VERSION
        )),
        std::cmp::Ordering::Greater => Err(anyhow!(
            "schema v{version} 比此 m2k（v{}）新，請更新 m2k",
            store::SCHEMA_VERSION
        )),
    }
}

/// 沒有 WAL 時直接讀檔頭，避免唯讀連線在關閉後重建 -wal/-shm 檔；
/// 有未合併的 WAL（其他程序正在使用）時才以唯讀連線讀取，檔案本來就存在。
fn user_version(path: &Path) -> Result<i64> {
    let wal = path.with_file_name(format!(
        "{}-wal",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    if fs::metadata(&wal).is_ok_and(|meta| meta.len() > 0) {
        let conn = sqlite::open_reader(path)?;
        return Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?);
    }
    let mut header = [0_u8; 100];
    fs::File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut header))
        .map_err(|error| anyhow!("無法讀取資料庫檔頭：{error}"))?;
    if &header[..16] != b"SQLite format 3\0" {
        return Err(anyhow!("{} 不是有效的 SQLite 資料庫", path.display()));
    }
    Ok(i64::from(u32::from_be_bytes([
        header[60], header[61], header[62], header[63],
    ])))
}

fn sync_lock(dir: &Path) -> Result<String> {
    let path = dir.join("sync.lock");
    // 不建立檔案；以共享鎖探測，不會干擾持鎖程序也不改檔案內容。
    let Ok(file) = fs::File::open(&path) else {
        return Ok("沒有鎖檔，未被占用".into());
    };
    match FileExt::try_lock_shared(&file) {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            Ok("未被占用".into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Err(anyhow!(
            "sync.lock 正被另一個同步或寫入程序持有（account_busy）；請稍後重試"
        )),
        Err(error) => Err(anyhow!("無法檢查 sync.lock：{error}")),
    }
}
