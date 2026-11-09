use crate::error::Failure;
use crate::site::{FileServer, SiteProfile};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Subcommand;
use futures_util::StreamExt;
use serde::Serialize;
use smb::{
    Client, ClientConfig, DirAccessMask, Directory, FileAccessMask, FileCreateArgs,
    FileFullDirectoryInformation, ReadAt, Resource, Status, UncPath,
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

const PATH_HELP: &str =
    r"SHARE/dir（profile 的伺服器）、\\server\share\dir 或 smb://server/share/dir";
const FIND_BUDGET: Duration = Duration::from_secs(120);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const READ_CHUNK: usize = 64 * 1024;

#[derive(Debug, Subcommand)]
pub enum FilesCommand {
    #[command(about = "列出 site profile 的檔案伺服器與共用資料夾；不連線")]
    Shares,
    #[command(about = "列出一個資料夾的檔案與子資料夾")]
    Ls {
        #[arg(help = PATH_HELP)]
        path: String,
    },
    #[command(about = "遞迴依名稱搜尋（不分大小寫的子字串）；最多約 120 秒")]
    Find {
        #[arg(help = PATH_HELP)]
        path: String,
        #[arg(long, help = "名稱包含的文字")]
        name: String,
        #[arg(long, default_value_t = 3, help = "搜尋層數；1 只看 PATH 本身的內容")]
        depth: usize,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    #[command(about = "下載單一檔案；不覆寫既有檔案")]
    Get {
        #[arg(help = PATH_HELP)]
        path: String,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmbPath {
    pub server: String,
    pub share: String,
    pub parts: Vec<String>,
}

impl SmbPath {
    /// `SHARE/dir` uses `default_server`; `\\server\share\dir` and
    /// `smb://server/share/dir` name their own server. Names are literal (no
    /// percent-decoding).
    pub fn parse(input: &str, default_server: Option<&str>) -> Result<Self> {
        let input = input.trim();
        let (server, rest) = match input
            .strip_prefix("smb://")
            .or_else(|| input.strip_prefix(r"\\"))
            .or_else(|| input.strip_prefix("//"))
        {
            Some(rest) => rest.split_once(['/', '\\']).unwrap_or((rest, "")),
            None => (
                default_server.context(
                    r"site profile 沒有 file_server；請在 profile 設定 file_server，或改用 \\server\share\dir 或 smb://server/share/dir",
                )?,
                input,
            ),
        };
        let mut segments = rest
            .split(['/', '\\'])
            .filter(|segment| !segment.is_empty())
            .map(ToOwned::to_owned);
        let share = segments.next().unwrap_or_default();
        if server.trim().is_empty() || share.is_empty() {
            bail!("路徑需要伺服器與共用資料夾名稱：{input}");
        }
        Ok(Self {
            server: server.to_owned(),
            share,
            parts: segments.collect(),
        })
    }

    fn share_root(&self) -> Result<UncPath> {
        Ok(UncPath::from_str(&format!(
            r"\\{}\{}",
            self.server, self.share
        ))?)
    }

    fn unc(&self) -> Result<UncPath> {
        let root = self.share_root()?;
        Ok(if self.parts.is_empty() {
            root
        } else {
            root.with_path(&self.parts.join(r"\"))
        })
    }

    fn join(&self, name: &str) -> Self {
        let mut child = self.clone();
        child.parts.push(name.to_owned());
        child
    }

    pub fn url(&self) -> String {
        let mut url = format!("smb://{}/{}", self.server, self.share);
        for part in &self.parts {
            url.push('/');
            url.push_str(part);
        }
        url
    }
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub kind: &'static str,
    pub size: u64,
    pub modified: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
struct FindOutput {
    matches: Vec<Entry>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated_reason: Option<&'static str>,
    scanned_dirs: usize,
    skipped_dirs: usize,
}

pub fn execute(data_dir: &Path, command: FilesCommand, json: bool) -> Result<()> {
    if let FilesCommand::Shares = command {
        let profile = SiteProfile::for_config(Some(&crate::config::load(data_dir)?))?;
        let file_server = profile.file_server.context(
            "site profile 沒有 file_server；請在 profile 加入 file_server（server、shares）",
        )?;
        return emit(&file_server);
    }
    let (cfg, password) = crate::mail_action::online(data_dir)?;
    let file_server = SiteProfile::for_config(Some(&cfg))?.file_server;
    let user = login_name(&cfg.username, file_server.as_ref());
    let server = file_server.as_ref().map(|files| files.server.as_str());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    match command {
        FilesCommand::Shares => unreachable!(),
        FilesCommand::Ls { path } => {
            let path = SmbPath::parse(&path, server)?;
            let entries = runtime.block_on(async {
                let client = connect(&path, &user, &password).await?;
                list(&client, &path).await
            })?;
            if json {
                return emit(&entries);
            }
            print_entries(&entries, |entry| &entry.name);
        }
        FilesCommand::Find {
            path,
            name,
            depth,
            limit,
        } => {
            let path = SmbPath::parse(&path, server)?;
            let output = runtime.block_on(async {
                let client = connect(&path, &user, &password).await?;
                find(&client, &path, &name, depth, limit, FIND_BUDGET).await
            })?;
            if json {
                return emit(&output);
            }
            print_entries(&output.matches, |entry| &entry.path);
            if let Some(reason) = output.truncated_reason {
                println!("已截斷（{reason}）；可縮小 PATH、提高 --limit 或降低 --depth");
            }
            if output.skipped_dirs > 0 {
                println!("略過 {} 個無法開啟的資料夾", output.skipped_dirs);
            }
        }
        FilesCommand::Get { path, output } => {
            let path = SmbPath::parse(&path, server)?;
            if output.try_exists()? {
                bail!("{} 已存在；不覆寫既有檔案", output.display());
            }
            let bytes = runtime.block_on(async {
                let client = connect(&path, &user, &password).await?;
                download(&client, &path, &output).await
            })?;
            emit(&serde_json::json!({"path":path.url(),"output":output,"bytes":bytes}))?;
        }
    }
    Ok(())
}

fn login_name(username: &str, file_server: Option<&FileServer>) -> String {
    let user = username.split('@').next().unwrap_or_default();
    match file_server.and_then(|files| files.domain.as_deref()) {
        Some(domain) if !domain.trim().is_empty() => format!(r"{domain}\{user}"),
        _ => user.to_owned(),
    }
}

async fn connect(path: &SmbPath, user: &str, password: &str) -> Result<Client> {
    let mut config = ClientConfig::default();
    config.connection.timeout = Some(REQUEST_TIMEOUT);
    let client = Client::new(config);
    client
        .share_connect(&path.share_root()?, user, password.to_owned())
        .await
        .map_err(|error| smb_error(error, &format!("{}@{}", user, path.server)))?;
    Ok(client)
}

async fn list(client: &Client, path: &SmbPath) -> Result<Vec<Entry>> {
    let access = DirAccessMask::new()
        .with_list_directory(true)
        .with_synchronize(true);
    let resource = client
        .create_file(
            &path.unc()?,
            &FileCreateArgs::make_open_existing(access.into()),
        )
        .await
        .map_err(|error| smb_error(error, &path.url()))?;
    let Resource::Directory(directory) = resource else {
        bail!("不是資料夾：{}", path.url());
    };
    let directory = Arc::new(directory);
    let mut entries = Vec::new();
    let mut stream = Directory::query::<FileFullDirectoryInformation>(&directory, "*")
        .await
        .map_err(|error| smb_error(error, &path.url()))?;
    while let Some(info) = stream.next().await {
        let info = info.map_err(|error| smb_error(error, &path.url()))?;
        let name = info.file_name.to_string();
        if name == "." || name == ".." {
            continue;
        }
        let dir = info.file_attributes.directory();
        entries.push(Entry {
            kind: if dir { "dir" } else { "file" },
            size: if dir { 0 } else { info.end_of_file },
            modified: DateTime::<Utc>::from(std::time::SystemTime::from(info.last_write_time))
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            path: path.join(&name).url(),
            name,
        });
    }
    drop(stream);
    directory.close().await.ok();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

async fn find(
    client: &Client,
    root: &SmbPath,
    name: &str,
    depth: usize,
    limit: usize,
    budget: Duration,
) -> Result<FindOutput> {
    let started = Instant::now();
    let needle = name.to_lowercase();
    let mut output = FindOutput {
        matches: Vec::new(),
        truncated: false,
        truncated_reason: None,
        scanned_dirs: 0,
        skipped_dirs: 0,
    };
    let mut queue = std::collections::VecDeque::from([(root.clone(), 1)]);
    while let Some((dir, level)) = queue.pop_front() {
        if started.elapsed() >= budget {
            output.truncated_reason = Some("time_limit");
            break;
        }
        let entries = match list(client, &dir).await {
            Ok(entries) => entries,
            Err(error) if output.scanned_dirs > 0 && skippable(&error) => {
                output.skipped_dirs += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        output.scanned_dirs += 1;
        for entry in entries {
            if entry.kind == "dir" && level < depth {
                queue.push_back((dir.join(&entry.name), level + 1));
            }
            if entry.name.to_lowercase().contains(&needle) {
                if output.matches.len() == limit {
                    output.truncated_reason = Some("limit");
                    break;
                }
                output.matches.push(entry);
            }
        }
        if output.truncated_reason.is_some() {
            break;
        }
    }
    output.truncated = output.truncated_reason.is_some();
    Ok(output)
}

fn skippable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<Failure>()
        .is_some_and(|failure| matches!(failure.code, "permission_denied" | "not_found"))
}

async fn download(client: &Client, path: &SmbPath, output: &Path) -> Result<u64> {
    let resource = client
        .create_file(
            &path.unc()?,
            &FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true)),
        )
        .await
        .map_err(|error| smb_error(error, &path.url()))?;
    let Resource::File(file) = resource else {
        bail!("不是檔案：{}", path.url());
    };
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = parent.join(format!(".m2k-download-{}", uuid::Uuid::new_v4()));
    let result = async {
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("無法建立暫存檔 {}", temp.display()))?;
        let mut buffer = vec![0; READ_CHUNK];
        let mut offset = 0u64;
        loop {
            let read = file
                .read_at(&mut buffer, offset)
                .await
                .map_err(|error| smb_error(error, &path.url()))?;
            if read == 0 {
                break;
            }
            target.write_all(&buffer[..read])?;
            offset += read as u64;
        }
        target.sync_all()?;
        std::fs::hard_link(&temp, output)
            .with_context(|| format!("無法建立檔案 {}；不覆寫既有檔案", output.display()))?;
        Ok(offset)
    }
    .await;
    std::fs::remove_file(&temp).ok();
    file.close().await.ok();
    result
}

fn smb_error(error: smb::Error, target: &str) -> anyhow::Error {
    let status = match &error {
        smb::Error::UnexpectedMessageStatus(status)
        | smb::Error::ReceivedErrorMessage(status, _) => Some(*status),
        _ => None,
    };
    let (code, message) = match (status, &error) {
        (Some(Status::U32_LOGON_FAILURE), _) => {
            return crate::error::auth_failed("SMB", format!("{target}：{error}")).into();
        }
        (
            Some(
                Status::U32_OBJECT_NAME_NOT_FOUND
                | Status::U32_OBJECT_PATH_NOT_FOUND
                | Status::U32_BAD_NETWORK_NAME,
            ),
            _,
        ) => ("not_found", format!("找不到 {target}（{error}）")),
        (Some(Status::U32_ACCESS_DENIED), _) => (
            "permission_denied",
            format!("沒有權限存取 {target}（{error}）"),
        ),
        (Some(Status::U32_IO_TIMEOUT), _) | (_, smb::Error::OperationTimeout(..)) => (
            "network_timeout",
            format!("SMB 連線逾時：{target}（{error}）"),
        ),
        (
            None,
            smb::Error::IoError(_) | smb::Error::TransportError(_) | smb::Error::ConnectionStopped,
        ) => (
            "network_error",
            format!("SMB 連線失敗：{target}（{error}）"),
        ),
        _ => (
            "operation_failed",
            format!("SMB 操作失敗：{target}（{error}）"),
        ),
    };
    Failure {
        code,
        message,
        retryable: false,
    }
    .into()
}

fn print_entries(entries: &[Entry], label: impl Fn(&Entry) -> &str) {
    println!("{}", entry_row("類型", "大小", "修改時間", "名稱 / 路徑"));
    for entry in entries {
        println!(
            "{}",
            entry_row(
                entry.kind,
                &entry.size.to_string(),
                &entry.modified,
                label(entry)
            )
        );
    }
}

fn entry_row(kind: &str, size: &str, modified: &str, name: &str) -> String {
    crate::plain_output::row(&[(kind, 4), (size, 12), (modified, 20), (name, 80)])
}

fn emit(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(server: &str, share: &str, parts: &[&str]) -> SmbPath {
        SmbPath {
            server: server.into(),
            share: share.into(),
            parts: parts.iter().map(|part| part.to_string()).collect(),
        }
    }

    #[test]
    fn three_path_forms_name_the_same_location() {
        let expected = path("files.example.test", "Shared", &["Team", "Q3 plan"]);
        for input in [
            "Shared/Team/Q3 plan",
            r"Shared\Team\Q3 plan\",
            r"\\files.example.test\Shared\Team\Q3 plan",
            "//files.example.test/Shared/Team/Q3 plan",
            "smb://files.example.test/Shared/Team/Q3 plan/",
        ] {
            assert_eq!(
                SmbPath::parse(input, Some("files.example.test")).unwrap(),
                expected,
                "{input}"
            );
        }
        assert_eq!(
            expected.url(),
            "smb://files.example.test/Shared/Team/Q3 plan"
        );
    }

    #[test]
    fn explicit_server_overrides_profile_and_share_root_has_no_parts() {
        assert_eq!(
            SmbPath::parse(r"\\other.example.test\Projects", Some("files.example.test")).unwrap(),
            path("other.example.test", "Projects", &[])
        );
        assert_eq!(
            SmbPath::parse("smb://127.0.0.1:4445/Shared", None).unwrap(),
            path("127.0.0.1:4445", "Shared", &[])
        );
    }

    #[test]
    fn share_relative_path_needs_profile_server_and_every_form_needs_a_share() {
        let error = SmbPath::parse("Shared/Team", None).unwrap_err().to_string();
        assert!(error.contains("file_server"), "{error}");
        for input in [
            "",
            "/",
            "smb://files.example.test",
            r"\\files.example.test\",
        ] {
            assert!(
                SmbPath::parse(input, Some("files.example.test")).is_err(),
                "{input}"
            );
        }
    }

    #[test]
    fn find_past_its_time_budget_reports_time_limit_instead_of_failing() {
        let output = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(find(
                &Client::new(ClientConfig::default()),
                &path("files.example.test", "Shared", &[]),
                "plan",
                3,
                100,
                Duration::ZERO,
            ))
            .unwrap();
        assert!(output.truncated);
        assert_eq!(output.truncated_reason, Some("time_limit"));
        assert!(output.matches.is_empty());
    }

    #[test]
    fn login_name_is_the_account_local_part_with_optional_domain() {
        let mut files = FileServer {
            server: "files.example.test".into(),
            shares: vec![],
            domain: None,
        };
        assert_eq!(login_name("alice@example.test", Some(&files)), "alice");
        files.domain = Some("EXAMPLE".into());
        assert_eq!(
            login_name("alice@example.test", Some(&files)),
            r"EXAMPLE\alice"
        );
        assert_eq!(login_name("alice", None), "alice");
    }
}
