use crate::{local_state, model::Kind};
use anyhow::Result;
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub state: String,
    pub phase: String,
    pub kind: Option<Kind>,
    pub count: usize,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub(crate) struct Reporter {
    path: PathBuf,
    progress: Progress,
}

impl Reporter {
    pub fn start(dir: &Path) -> Result<Self> {
        let now = Utc::now();
        let reporter = Self {
            path: dir.join("sync_progress.json"),
            progress: Progress {
                state: "running".into(),
                phase: "preparing".into(),
                kind: None,
                count: 0,
                started_at: now,
                updated_at: now,
            },
        };
        reporter.persist()?;
        Ok(reporter)
    }

    pub fn update(&mut self, phase: &str, kind: Option<Kind>, count: usize) -> Result<()> {
        self.progress.phase = phase.into();
        self.progress.kind = kind;
        self.progress.count = count;
        self.progress.updated_at = Utc::now();
        self.persist()
    }

    pub fn finish(&mut self, success: bool) -> Result<()> {
        self.progress.state = if success { "completed" } else { "failed" }.into();
        self.update("finished", self.progress.kind, self.progress.count)
    }

    fn persist(&self) -> Result<()> {
        local_state::write_atomic(&self.path, &serde_json::to_vec(&self.progress)?)
    }
}

pub fn read(dir: &Path) -> Result<Option<Progress>> {
    let bytes = match fs::read(dir.join("sync_progress.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut progress: Progress = serde_json::from_slice(&bytes)?;
    if progress.state == "running" {
        let active = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("sync.lock"))
        {
            Ok(file) => match file.try_lock_exclusive() {
                Ok(()) => false,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
                Err(error) => return Err(error.into()),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if !active {
            progress.state = "interrupted".into();
        }
    }
    Ok(Some(progress))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_requires_a_live_lock_and_finished_state_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        assert!(read(temp.path()).unwrap().is_none());
        let lock = crate::sync::lock(temp.path()).unwrap();
        let mut reporter = Reporter::start(temp.path()).unwrap();
        reporter
            .update("fetching", Some(Kind::Calendar), 10)
            .unwrap();
        assert_eq!(read(temp.path()).unwrap().unwrap().state, "running");
        drop(lock);
        assert_eq!(read(temp.path()).unwrap().unwrap().state, "interrupted");
        reporter.finish(false).unwrap();
        let progress = read(temp.path()).unwrap().unwrap();
        assert_eq!(progress.state, "failed");
        assert_eq!(progress.count, 10);
    }
}
