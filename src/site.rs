use crate::config::Config;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};

pub const DEFAULT_PROFILE: &str = "mail2000";
pub const USER_PROFILE_DIR: &str = "site-profiles";
/// Overrides the user profile directory (tests, unusual layouts).
pub const USER_PROFILE_DIR_ENV: &str = "M2K_SITE_PROFILE_DIR";

const BUILTIN_PROFILES: &[(&str, &str)] = &[
    ("mail2000", include_str!("site/mail2000.json")),
    ("demo", include_str!("site/demo.json")),
];

static USER_PROFILES: OnceLock<PathBuf> = OnceLock::new();

pub fn set_data_dir(data_dir: &Path) {
    let _ = USER_PROFILES.set(data_dir.join(USER_PROFILE_DIR));
}

/// `M2K_SITE_PROFILE_DIR`, else `<data-dir>/site-profiles` once `set_data_dir`
/// ran. Library callers that set neither see no user profiles, so they never
/// read the real data directory.
pub fn user_profile_dir() -> Option<PathBuf> {
    std::env::var_os(USER_PROFILE_DIR_ENV)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| USER_PROFILES.get().cloned())
}

fn user_profile_dir_text() -> String {
    user_profile_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_else(|| format!("<data-dir>/{USER_PROFILE_DIR}"))
}

fn is_profile_name(spec: &str) -> bool {
    !spec.is_empty()
        && spec
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn user_profile_path(name: &str) -> Option<PathBuf> {
    user_profile_dir().map(|dir| dir.join(format!("{name}.json")))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SiteProfile {
    pub name: String,
    /// Default server for `init` / `auth login`; also used to auto-select the profile.
    #[serde(default)]
    pub server: Option<String>,
    /// Path below the server origin that holds `<account>/<calendar>/` collections.
    #[serde(default = "default_caldav_calendars_path")]
    pub caldav_calendars_path: String,
    /// CardDAV principal path; `{domain}` is the account's email domain.
    #[serde(default = "default_carddav_principal_path")]
    pub carddav_principal_path: String,
    /// Top-level address book that holds the company directory tree.
    #[serde(default)]
    pub company_directory_root: Option<String>,
    /// CalDAV account that owns room calendars; `{domain}` is the account's email domain.
    #[serde(default = "default_room_provider")]
    pub room_provider: String,
    #[serde(default)]
    pub sites: Vec<SiteRule>,
    /// Site for rooms whose name matches no site prefix.
    #[serde(default)]
    pub default_site: Option<String>,
    /// Read a room's capacity from a trailing `_<number>` in its name.
    #[serde(default)]
    pub room_capacity_from_name_suffix: bool,
    /// Rooms listed even when DAV resource discovery is unavailable.
    #[serde(default)]
    pub rooms: Vec<RoomEntry>,
    /// SMB file server for `m2k files`, logged in with the mail account and password.
    #[serde(default)]
    pub file_server: Option<FileServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileServer {
    pub server: String,
    #[serde(default)]
    pub shares: Vec<String>,
    /// Logs in as `DOMAIN\user` when set, otherwise as the bare account name.
    #[serde(default)]
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SiteRule {
    pub name: String,
    #[serde(default)]
    pub prefixes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RoomEntry {
    pub name: String,
    pub access: String,
}

fn default_caldav_calendars_path() -> String {
    "/cgi-bin/cal/caldav/calendars".to_owned()
}

fn default_carddav_principal_path() -> String {
    "/cgi-bin/carddav/principals/mPA.000@{domain}".to_owned()
}

fn default_room_provider() -> String {
    "@{domain}".to_owned()
}

impl SiteProfile {
    pub fn builtin_names() -> impl Iterator<Item = &'static str> {
        BUILTIN_PROFILES.iter().map(|(name, _)| *name)
    }

    pub fn builtin(name: &str) -> Option<Self> {
        BUILTIN_PROFILES
            .iter()
            .find(|(builtin, _)| builtin.eq_ignore_ascii_case(name.trim()))
            .map(|(builtin, json)| {
                Self::parse(json).unwrap_or_else(|error| {
                    panic!("builtin site profile {builtin} is invalid: {error:#}")
                })
            })
    }

    /// Load a builtin profile by name, a user profile by name, or a JSON
    /// profile file by path.
    pub fn load(spec: &str) -> Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            bail!("site profile 不可為空白");
        }
        if let Some(profile) = Self::builtin(spec) {
            if user_profile_path(spec).is_some_and(|path| path.is_file()) {
                static SHADOWED: Once = Once::new();
                SHADOWED.call_once(|| {
                    eprintln!(
                        "警告：{}/{spec}.json 與內建 site profile 同名，使用內建版本；請改用其他名稱",
                        user_profile_dir_text()
                    )
                });
            }
            return Ok(profile);
        }
        let not_found = || {
            format!(
                "找不到 site profile：{spec}（內建：{}；使用者 profile 放在 {}/<name>.json，或指定 JSON 檔路徑）",
                Self::builtin_names().collect::<Vec<_>>().join("、"),
                user_profile_dir_text()
            )
        };
        let path = if is_profile_name(spec) {
            match user_profile_path(spec).filter(|path| path.is_file()) {
                Some(path) => path,
                None => bail!(not_found()),
            }
        } else {
            PathBuf::from(spec)
        };
        let bytes = std::fs::read(&path).with_context(not_found)?;
        let text = String::from_utf8(bytes).context("site profile 不是有效的 UTF-8")?;
        Self::parse(&text).with_context(|| format!("site profile 格式錯誤：{}", path.display()))
    }

    /// Normalize a `--site-profile` value for storing in config.json: builtin
    /// names are lowercased, user profile names kept, file paths made absolute.
    pub fn stored_spec(spec: &str) -> String {
        let spec = spec.trim();
        if let Some(name) = Self::builtin_names().find(|name| name.eq_ignore_ascii_case(spec)) {
            return name.to_owned();
        }
        if is_profile_name(spec) {
            return spec.to_owned();
        }
        std::fs::canonicalize(spec)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| spec.to_owned())
    }

    pub fn parse(json: &str) -> Result<Self> {
        let profile: Self = serde_json::from_str(json)?;
        profile.validate()?;
        Ok(profile)
    }

    /// The builtin or user profile whose server is `host`, with the name that
    /// selects it in `site_profile`, otherwise the generic profile. Unreadable
    /// or invalid user profiles are skipped here; selecting one by name reports
    /// its error.
    pub fn named_for_host(host: &str) -> (String, Self) {
        BUILTIN_PROFILES
            .iter()
            .filter_map(|(name, _)| Some(((*name).to_owned(), Self::builtin(name)?)))
            .chain(Self::user_profiles())
            .find(|(_, profile)| {
                profile
                    .server
                    .as_deref()
                    .is_some_and(|server| server.eq_ignore_ascii_case(host.trim()))
            })
            .unwrap_or_else(|| (DEFAULT_PROFILE.to_owned(), Self::generic()))
    }

    pub fn for_config(config: Option<&Config>) -> Result<Self> {
        match config {
            Some(config) => match config.site_profile.as_deref() {
                Some(spec) => Self::load(spec),
                None => Ok(Self::generic()),
            },
            None => Ok(Self::generic()),
        }
    }

    fn user_profile_files() -> Vec<PathBuf> {
        let Some(dir) = user_profile_dir() else {
            return Vec::new();
        };
        let mut paths = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    fn user_profiles() -> Vec<(String, Self)> {
        Self::user_profile_files()
            .iter()
            .filter_map(|path| {
                let name = path.file_stem()?.to_str()?.to_owned();
                let profile = Self::parse(&std::fs::read_to_string(path).ok()?).ok()?;
                Some((name, profile))
            })
            .collect()
    }

    pub fn is_generic(&self) -> bool {
        self.name == DEFAULT_PROFILE
    }

    pub fn generic() -> Self {
        Self::builtin(DEFAULT_PROFILE).expect("generic site profile is builtin")
    }

    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("site profile 的 name 不可為空白");
        }
        for (label, path) in [
            ("caldav_calendars_path", &self.caldav_calendars_path),
            ("carddav_principal_path", &self.carddav_principal_path),
        ] {
            if !path.starts_with('/') || path.chars().any(char::is_whitespace) {
                bail!("site profile 的 {label} 必須是以 / 開頭的路徑");
            }
        }
        if self.room_provider.trim().is_empty() {
            bail!("site profile 的 room_provider 不可為空白");
        }
        if self.sites.iter().any(|site| site.name.trim().is_empty()) {
            bail!("site profile 的 sites 名稱不可為空白");
        }
        if let Some(default_site) = &self.default_site {
            if !self.sites.iter().any(|site| &site.name == default_site) {
                bail!("site profile 的 default_site 必須是 sites 之一：{default_site}");
            }
        }
        if self
            .rooms
            .iter()
            .any(|room| room.name.trim().is_empty() || room.access.trim().is_empty())
        {
            bail!("site profile 的 rooms 需要 name 與 access");
        }
        if let Some(files) = &self.file_server {
            if files.server.trim().is_empty() || files.shares.iter().any(|s| s.trim().is_empty()) {
                bail!("site profile 的 file_server 需要 server，shares 不可有空白名稱");
            }
        }
        Ok(())
    }

    pub fn caldav_calendars_url(&self, origin: &str) -> String {
        format!(
            "{origin}{}",
            self.caldav_calendars_path.trim_end_matches('/')
        )
    }

    pub fn carddav_principal_url(&self, origin: &str, email: &str) -> String {
        format!(
            "{origin}{}",
            self.carddav_principal_path
                .replace("{domain}", email_domain(email))
        )
    }

    pub fn room_provider(&self, email: &str) -> String {
        self.room_provider.replace("{domain}", email_domain(email))
    }

    pub fn site_for_room(&self, name: &str) -> String {
        self.sites
            .iter()
            .find(|site| {
                site.prefixes
                    .iter()
                    .any(|prefix| !prefix.is_empty() && name.starts_with(prefix.as_str()))
            })
            .map(|site| site.name.clone())
            .or_else(|| self.default_site.clone())
            .unwrap_or_default()
    }

    pub fn room_capacity(&self, name: &str) -> Option<u32> {
        if !self.room_capacity_from_name_suffix {
            return None;
        }
        name.rsplit_once('_')
            .and_then(|(_, suffix)| suffix.parse::<u32>().ok())
    }

    pub fn validate_site(&self, value: Option<&str>) -> Result<()> {
        let Some(value) = value else {
            return Ok(());
        };
        if self.sites.iter().any(|site| site.name == value.trim()) {
            return Ok(());
        }
        if self.sites.is_empty() {
            bail!(
                "site 必須是 site profile 定義的據點；目前的 profile（{}）沒有定義據點",
                self.name
            );
        }
        let names = self
            .sites
            .iter()
            .map(|site| site.name.as_str())
            .collect::<Vec<_>>();
        let listed = match names.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{}或{last}", rest.join("、")),
            _ => names.concat(),
        };
        bail!("site 必須是{listed}")
    }
}

fn email_domain(email: &str) -> &str {
    email
        .rsplit_once('@')
        .map(|(_, domain)| domain.trim())
        .unwrap_or_default()
}
