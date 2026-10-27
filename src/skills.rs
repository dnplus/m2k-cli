use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const M2K_SKILL: &str = include_str!("../skills/m2k/SKILL.md");

#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub path: PathBuf,
    pub changed: bool,
}

pub fn install(destination: Option<&Path>) -> Result<InstallResult> {
    let root = match destination {
        Some(path) => path.to_owned(),
        None => dirs::home_dir()
            .context("找不到使用者家目錄；請以 --path 指定 skills 目錄")?
            .join(".agents/skills"),
    };
    let folder = root.join("m2k");
    let path = folder.join("SKILL.md");
    match fs::read(&path) {
        Ok(content) if content == M2K_SKILL.as_bytes() => {
            return Ok(InstallResult {
                path,
                changed: false,
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("無法讀取 {}", path.display())),
    }
    fs::create_dir_all(&folder).with_context(|| format!("無法建立 {}", folder.display()))?;
    fs::write(&path, M2K_SKILL).with_context(|| format!("無法安裝 {}", path.display()))?;
    Ok(InstallResult {
        path,
        changed: true,
    })
}
