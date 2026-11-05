use crate::site::SiteProfile;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

const DEFAULT_IMAP_PORT: u16 = 993;
const DEFAULT_SMTP_PORT: u16 = 587;
const DEFAULT_SYNC_INTERVAL_MINUTES: u64 = 360;
const DEFAULT_MAIL_RETENTION_DAYS: u32 = 180;
const DEFAULT_CALENDAR_RETENTION_DAYS: u32 = 365;
const DEFAULT_CONTACTS_RETENTION_DAYS: u32 = 0;
const CONFIG_FILE_NAME: &str = "config.json";
const KEYRING_SERVICE: &str = "m2k";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub username: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub caldav_url: String,
    pub carddav_url: String,
    pub sync_interval_minutes: u64,
    pub mail_retention_days: u32,
    pub calendar_retention_days: u32,
    pub contacts_retention_days: u32,
    /// Site profile name or path to a profile JSON file; see `site`. `init`
    /// always sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_profile: Option<String>,
    /// 沒有自動信標頭的系統寄件者：完整地址、`@網域` 或 local part（不分大小寫）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automated_senders: Vec<String>,
}

impl Config {
    pub fn new(username: String, server: &str) -> Result<Self> {
        Self::with_site_profile(username, server, None)
    }

    pub fn with_site_profile(
        username: String,
        server: &str,
        site_profile: Option<String>,
    ) -> Result<Self> {
        let username = username.trim().to_owned();
        if username.is_empty() {
            return Err(anyhow!("帳號不可為空白"));
        }
        let site_profile = site_profile.map(|spec| SiteProfile::stored_spec(&spec));
        let selected = site_profile
            .as_deref()
            .map(|spec| SiteProfile::load(spec).map(|profile| (spec.to_owned(), profile)))
            .transpose()?;
        let server = match (server.trim(), &selected) {
            ("", Some((_, profile))) => profile.server.as_deref().unwrap_or_default(),
            (server, _) => server,
        };
        let origin = normalize_origin(server)?;
        let host = reqwest::Url::parse(&origin)
            .ok()
            .and_then(|url| url.host_str().map(ToOwned::to_owned))
            .ok_or_else(|| anyhow!("無法解析伺服器主機"))?;
        let email = if username.contains('@') {
            username.to_lowercase()
        } else {
            let domain = host.strip_prefix("mail.").unwrap_or(&host);
            format!("{}@{domain}", username.to_lowercase())
        };
        validate_email(&email)?;
        let encoded_username = percent_encode_segment(&email);
        // Record the selection so later loads never depend on auto-selection.
        let (site_profile, profile) =
            selected.unwrap_or_else(|| SiteProfile::named_for_host(&host));
        let carddav_url = profile.carddav_principal_url(&origin, &email);
        let config = Self {
            username: username.clone(),
            email,
            imap_host: host.clone(),
            imap_port: DEFAULT_IMAP_PORT,
            smtp_host: host,
            smtp_port: DEFAULT_SMTP_PORT,
            caldav_url: format!(
                "{}/{encoded_username}/default/",
                profile.caldav_calendars_url(&origin)
            ),
            carddav_url,
            sync_interval_minutes: DEFAULT_SYNC_INTERVAL_MINUTES,
            mail_retention_days: DEFAULT_MAIL_RETENTION_DAYS,
            calendar_retention_days: DEFAULT_CALENDAR_RETENTION_DAYS,
            contacts_retention_days: DEFAULT_CONTACTS_RETENTION_DAYS,
            site_profile: Some(site_profile),
            automated_senders: Vec::new(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.username.trim().is_empty() {
            return Err(anyhow!("帳號不可為空白"));
        }
        if self.username.chars().any(char::is_control) {
            return Err(anyhow!("帳號格式錯誤"));
        }
        if self.email.trim().is_empty() {
            return Err(anyhow!("電子郵件不可為空白"));
        }
        if self.email.chars().any(char::is_control) {
            return Err(anyhow!("電子郵件格式錯誤"));
        }
        validate_email(&self.email)?;
        validate_host("IMAP", &self.imap_host)?;
        validate_host("SMTP", &self.smtp_host)?;
        if self.imap_port == 0 {
            return Err(anyhow!("IMAP 連接埠必須大於 0"));
        }
        if self.smtp_port == 0 {
            return Err(anyhow!("SMTP 連接埠必須大於 0"));
        }
        if self.sync_interval_minutes == 0 {
            return Err(anyhow!("同步週期必須大於 0 分鐘"));
        }
        validate_http_url("CalDAV", &self.caldav_url)?;
        if !self.carddav_url.trim().is_empty() {
            validate_http_url("CardDAV", &self.carddav_url)?;
        }
        if let Some(spec) = &self.site_profile {
            SiteProfile::load(spec)?;
        }
        Ok(())
    }
}

pub fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("m2k")
}

pub fn load(data_dir: &std::path::Path) -> Result<Config> {
    let path = data_dir.join(CONFIG_FILE_NAME);
    let bytes = fs::read(&path).with_context(|| format!("無法讀取設定檔：{}", path.display()))?;
    let config: Config = serde_json::from_slice(&bytes)
        .with_context(|| format!("設定檔格式錯誤：{}", path.display()))?;
    config.validate()?;
    Ok(config)
}

pub fn save(data_dir: &std::path::Path, config: &Config) -> Result<()> {
    config.validate()?;
    ensure_data_dir(data_dir)?;
    let path = data_dir.join(CONFIG_FILE_NAME);
    let temp_path = data_dir.join(format!(".{CONFIG_FILE_NAME}.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(config).context("無法序列化設定")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .with_context(|| format!("無法建立暫存設定檔：{}", temp_path.display()))?;
    set_file_mode(&file, 0o600)?;
    file.write_all(&bytes)
        .with_context(|| format!("無法寫入設定檔：{}", temp_path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("無法完成設定檔：{}", temp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("無法同步設定檔：{}", temp_path.display()))?;
    drop(file);
    fs::rename(&temp_path, &path).with_context(|| format!("無法更新設定檔：{}", path.display()))?;
    set_path_mode(&path, 0o600)?;
    Ok(())
}

const CREDENTIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub fn password(config: &Config) -> Result<String> {
    config.validate()?;
    match std::env::var_os("M2K_PASSWORD") {
        Some(value) if !value.is_empty() => value
            .into_string()
            .map_err(|_| anyhow!("M2K_PASSWORD 不是有效的 UTF-8")),
        _ => platform_get_password(config),
    }
}

pub fn save_password(config: &Config, value: &str) -> Result<()> {
    config.validate()?;
    if value.is_empty() {
        return Err(anyhow!("密碼不可為空白"));
    }
    platform_set_password(config, value).map_err(|error| anyhow!("無法儲存系統密碼：{error}"))
}

pub fn delete_password(config: &Config) -> Result<()> {
    config.validate()?;
    platform_delete_password(config).map_err(|error| anyhow!("無法刪除系統密碼：{error}"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn security_command() -> crate::keychain::SecurityCommand {
    crate::keychain::SecurityCommand::from_env(CREDENTIAL_TIMEOUT)
}

#[cfg(target_os = "macos")]
fn platform_get_password(config: &Config) -> Result<String> {
    let key = credential_key(config)?;
    crate::keychain::get(&security_command(), KEYRING_SERVICE, &key).map_err(|error| match error {
        crate::keychain::Error::Timeout(timeout) => anyhow!(
            "讀取系統密碼庫逾時（{} 秒）；{KEYCHAIN_HELP}",
            timeout.as_secs()
        ),
        error => anyhow!("無法讀取系統密碼庫：{error}；{KEYCHAIN_HELP}"),
    })
}

#[cfg(target_os = "macos")]
fn platform_set_password(config: &Config, value: &str) -> Result<()> {
    let key = credential_key(config)?;
    Ok(crate::keychain::set(
        &security_command(),
        KEYRING_SERVICE,
        &key,
        value,
    )?)
}

#[cfg(target_os = "macos")]
fn platform_delete_password(config: &Config) -> Result<()> {
    let key = credential_key(config)?;
    Ok(crate::keychain::delete(
        &security_command(),
        KEYRING_SERVICE,
        &key,
    )?)
}

#[cfg(not(target_os = "macos"))]
fn platform_get_password(config: &Config) -> Result<String> {
    let config = config.clone();
    bounded_credential_read(CREDENTIAL_TIMEOUT, move || {
        keyring_entry(&config)?
            .get_password()
            .map_err(|error| anyhow!("無法讀取系統密碼庫：{error}；{KEYCHAIN_HELP}"))
    })
}

#[cfg(not(target_os = "macos"))]
fn platform_set_password(config: &Config, value: &str) -> Result<()> {
    Ok(keyring_entry(config)?.set_password(value)?)
}

#[cfg(not(target_os = "macos"))]
fn platform_delete_password(config: &Config) -> Result<()> {
    match keyring_entry(config)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn credential_key(config: &Config) -> Result<String> {
    config.validate()?;
    let identity = (
        config.username.as_str(),
        canonical_email(config),
        config.imap_host.as_str(),
        config.imap_port,
        config.smtp_host.as_str(),
        config.smtp_port,
        config.caldav_url.as_str(),
        config.carddav_url.as_str(),
    );
    let bytes = serde_json::to_vec(&identity).context("無法序列化密碼庫身分")?;
    let digest = Sha256::digest(bytes);
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(not(target_os = "macos"))]
fn keyring_entry(config: &Config) -> Result<keyring::Entry> {
    let account = credential_key(config)?;
    keyring::Entry::new(KEYRING_SERVICE, &account)
        .map_err(|error| anyhow!("無法建立系統密碼項目：{error}"))
}

fn canonical_email(config: &Config) -> String {
    config.email.trim().to_lowercase()
}

fn normalize_origin(server: &str) -> Result<String> {
    let value = server.trim().to_owned();
    if value.is_empty() {
        return Err(anyhow!(
            "請指定 Mail2000 伺服器（--server）或含 server 的 --site-profile"
        ));
    }
    let with_scheme = if value.starts_with("http://") || value.starts_with("https://") {
        value
    } else {
        format!("https://{value}")
    };
    let url = reqwest::Url::parse(&with_scheme).map_err(|_| anyhow!("伺服器網址格式錯誤"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!("伺服器網址必須使用 HTTP 或 HTTPS"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("伺服器網址缺少主機"))?;
    let mut origin = format!("{}://{host}", url.scheme());
    if let Some(port) = url.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    Ok(origin)
}

fn validate_host(label: &str, value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow!("{label} 主機不可為空白"));
    }
    if value.chars().any(char::is_whitespace) || value.contains('/') {
        return Err(anyhow!("{label} 主機格式錯誤"));
    }
    Ok(())
}

fn validate_email(value: &str) -> Result<()> {
    let (local, domain) = value
        .trim()
        .split_once('@')
        .ok_or_else(|| anyhow!("電子郵件必須包含 @"))?;
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(anyhow!("電子郵件格式錯誤"));
    }
    if local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || local
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || domain.starts_with('.')
        || domain.ends_with('.')
        || domain.contains("..")
    {
        return Err(anyhow!("電子郵件格式錯誤"));
    }
    if !local.chars().all(|character| {
        character.is_ascii_alphanumeric() || "!#$%&'*+-/=?^_`{|}~.".contains(character)
    }) {
        return Err(anyhow!("電子郵件格式錯誤"));
    }
    if !domain
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-'))
    {
        return Err(anyhow!("電子郵件格式錯誤"));
    }
    if domain
        .split('.')
        .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
    {
        return Err(anyhow!("電子郵件格式錯誤"));
    }
    Ok(())
}

fn validate_http_url(label: &str, value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value.trim()).map_err(|_| anyhow!("{label} URL 格式錯誤"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(anyhow!("{label} URL 必須是有效的 HTTP 或 HTTPS 網址"));
    }
    Ok(())
}

fn percent_encode_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte as char);
        } else {
            encoded.push('%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
    }
    encoded
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + value - 10) as char,
        _ => unreachable!(),
    }
}

fn ensure_data_dir(data_dir: &std::path::Path) -> Result<()> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("無法建立資料目錄：{}", data_dir.display()))?;
    set_path_mode(data_dir, 0o700)?;
    Ok(())
}

#[cfg(unix)]
fn set_path_mode(path: &std::path::Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)
        .with_context(|| format!("無法讀取檔案權限：{}", path.display()))?
        .permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("無法設定檔案權限：{}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_path_mode(_path: &std::path::Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(file: &std::fs::File, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = file
        .metadata()
        .context("無法讀取暫存設定檔權限")?
        .permissions();
    permissions.set_mode(mode);
    file.set_permissions(permissions)
        .context("無法設定暫存設定檔權限")?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_mode(_file: &std::fs::File, _mode: u32) -> Result<()> {
    Ok(())
}

const KEYCHAIN_HELP: &str = "請在 macOS 授權視窗允許 /usr/bin/security 存取 m2k 項目（可能需要輸入登入密碼，請選「永遠允許」）；或重新執行 m2k auth login";

#[cfg_attr(target_os = "macos", allow(dead_code))]
// Do not join a blocked platform credential call: the CLI must be able to exit.
// The worker only reads; a timed-out result is dropped and never logged.
fn bounded_credential_read<T: Send + 'static>(
    timeout: std::time::Duration,
    read: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("credential-read".into())
        .spawn(move || {
            let _ = sender.send(read());
        })
        .context("無法啟動系統密碼庫讀取")?;
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(anyhow!(
            "讀取系統密碼庫逾時（{} 秒）；{KEYCHAIN_HELP}",
            timeout.as_secs()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(anyhow!("系統密碼庫讀取意外中止；{KEYCHAIN_HELP}"))
        }
    }
}

#[cfg(test)]
mod credential_timeout_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn blocked_reader_returns_actionable_timeout_without_joining() {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let error = bounded_credential_read(Duration::from_millis(10), move || {
            let _ = wait.recv();
            Ok(())
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("逾時"));
        assert!(error.contains("永遠允許"));
        assert!(error.contains("m2k auth login"));
        release.send(()).unwrap();
    }

    #[test]
    fn reader_success_and_failure_are_preserved() {
        assert_eq!(
            bounded_credential_read(Duration::from_secs(1), || Ok(42)).unwrap(),
            42
        );
        assert!(
            bounded_credential_read::<()>(Duration::from_secs(1), || Err(anyhow!(
                "fixture failure"
            )))
            .unwrap_err()
            .to_string()
            .contains("fixture failure")
        );
    }
}
