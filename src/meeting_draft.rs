use crate::{
    config, local_state,
    meeting::{self, MeetingRoom, TimeRange},
    store::Store,
};
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub uid: String,
    pub title: String,
    pub attendees: Vec<String>,
    pub start: String,
    pub end: String,
    pub room_url: String,
    pub data_dir: PathBuf,
    pub invitation: PathBuf,
    pub manifest: PathBuf,
    pub actions: BTreeMap<String, String>,
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn save_draft(
    data_dir: &Path,
    room: &MeetingRoom,
    range: &TimeRange,
    title: &str,
    attendees: &[String],
) -> Result<Draft> {
    let cfg = config::load(data_dir)?;
    let store = Store::open_reader(data_dir)?;
    store.verify_account(&cfg)?;
    let mut attendees = meeting::resolve_attendees(&store, &cfg, attendees)?;
    attendees.sort();
    let identity = serde_json::to_vec(&(
        config::credential_key(&cfg)?,
        &room.calendar_url,
        range.start.timestamp(),
        range.end.timestamp(),
        title.trim(),
        &attendees,
    ))?;
    let hash = format!("{:x}", Sha256::digest(identity));
    let uid = format!("{hash}@m2k");
    let (ics, _) = meeting::build_invite_ics_with_uid(
        &uid,
        title,
        range,
        Some(&room.name),
        None,
        (&cfg.email, &cfg.email),
        &attendees,
    )?;
    let data_dir = data_dir.canonicalize()?;
    let root = data_dir.join("drafts");
    local_state::private_directory(&root)?;
    let folder = root.join(&hash);
    local_state::private_directory(&folder)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(folder.join("draft.lock"))?;
    lock.try_lock_exclusive()
        .context("另一個程序正在保存相同草稿，請稍後重試")?;
    let invitation = folder.join("invite.ics");
    match fs::read_to_string(&invitation) {
        Ok(existing) => {
            let stable = |value: &str| {
                value
                    .lines()
                    .filter(|line| !line.starts_with("DTSTAMP:"))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            };
            if stable(&existing) != stable(&ics) {
                bail!("既有草稿內容已修改，保留原檔：{}", invitation.display());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            local_state::write_atomic(&invitation, ics.as_bytes())?
        }
        Err(error) => return Err(error.into()),
    }
    let prefix = format!(
        "m2k --data-dir {}",
        quote(data_dir.to_str().context("資料目錄路徑不是 UTF-8")?)
    );
    let file = quote(invitation.to_str().context("草稿檔案路徑不是 UTF-8")?);
    let book = format!(
        "{prefix} meeting book --file {file} --url {}",
        quote(&room.calendar_url)
    );
    let invite = format!("{prefix} meeting invite --file {file}");
    let actions = BTreeMap::from([
        ("check_booking".into(), format!("{book} --dry-run")),
        ("book".into(), book.clone()),
        ("repair_room".into(), format!("{book} --target room")),
        ("check_invitation".into(), format!("{invite} --dry-run")),
        ("send_invitation".into(), format!("{invite} --send")),
    ]);
    let manifest = folder.join("draft.json");
    let draft = Draft {
        uid,
        title: title.trim().into(),
        attendees,
        start: range.start.to_rfc3339(),
        end: range.end.to_rfc3339(),
        room_url: room.calendar_url.clone(),
        data_dir,
        invitation,
        manifest,
        actions,
    };
    local_state::write_atomic(&draft.manifest, &serde_json::to_vec_pretty(&draft)?)?;
    Ok(draft)
}
