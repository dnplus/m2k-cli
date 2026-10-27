use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

pub(super) const FILE_NAME: &str = "room_finder_prefs.json";
pub(super) const DEFAULT_WORKING_HOURS: &str = "09:30-17:30";

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub(super) struct Preferences {
    pub site: String,
    pub capacity: Option<u32>,
    pub minutes: Option<u32>,
    pub working_hours: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            site: String::new(),
            capacity: Some(6),
            minutes: Some(120),
            working_hours: DEFAULT_WORKING_HOURS.into(),
        }
    }
}

impl Preferences {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(FILE_NAME);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => {
                return Err(error).with_context(|| format!("無法讀取 {}", path.display()))
            }
        };
        serde_json::from_slice(&bytes).with_context(|| format!("無法解析 {}", path.display()))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir).context("無法建立會議室偏好目錄")?;
        let path = dir.join(FILE_NAME);
        let temporary = dir.join(format!(".room_finder_prefs-{}.tmp", uuid::Uuid::new_v4()));
        let result: Result<()> = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.with_context(|| format!("無法保存 {}", path.display()))
    }
}
