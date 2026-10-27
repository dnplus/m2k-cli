use clap::{ArgGroup, Args, Parser, Subcommand};
use m2k::model::Kind;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "m2k",
    version = m2k::version::DISPLAY,
    about = "Mail2000 原生 CLI / TUI · 郵件、行事曆、聯絡人與本機同步"
)]
pub struct Cli {
    #[arg(long, global = true, help = "獨立的設定與 SQLite 資料目錄")]
    pub data_dir: Option<PathBuf>,
    #[arg(long, global = true, help = "輸出 JSON，供腳本與代理程式使用")]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    #[command(
        about = "將英中日期、星期或週／月範圍解析成有時區的時間範圍",
        long_about = "將英中日期解析成當地日界線的 RFC3339 起迄。保留本週、下週、本月的多日範圍。\n\n範例：next tuesday、下週五、週六、兩天前、明天、2026-10-14、下週。\nnext tuesday 表示下一次週二（今天是週二則七天後）；tuesday／週二允許今天。下週五表示下一個曆週的週五。"
    )]
    DateRange {
        expression: String,
        #[arg(long, default_value = "Asia/Taipei")]
        timezone: String,
    },
    #[command(about = "設定單一帳號與服務端點")]
    Init(InitArgs),
    #[command(about = "登入、檢查連線或移除系統密碼庫憑證")]
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    #[command(about = "查看或調整端點、同步間隔與保存天數")]
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    #[command(about = "同步信件與行事曆至本機；--kind contacts 明確手動全公司同步")]
    Sync {
        #[arg(long)]
        kind: Option<Kind>,
    },
    #[command(about = "週期增量同步信件與鄰近日曆，遠處每日更新；聯絡人用手動全量")]
    Watch {
        #[arg(long)]
        kind: Option<Kind>,
    },
    #[command(
        about = "唯讀環境診斷：設定、權限、憑證、四種服務、SQLite 與同步鎖；任一失敗即非零結束"
    )]
    Doctor,
    #[command(about = "顯示本機數量與最近同步狀態")]
    Status,
    #[command(about = "依保存天數清理本機資料，不刪除伺服器資料")]
    Prune,
    #[command(about = "郵件資料夾、查詢、寄送、回覆、轉寄與管理")]
    Mail {
        #[command(subcommand)]
        command: MailCommand,
    },
    #[command(about = "行事曆查詢；以欄位或 ICS 建立、更新與刪除")]
    Calendar {
        #[command(subcommand)]
        command: CalendarCommand,
    },
    #[command(about = "聯絡人查詢、VCF 建立、更新與刪除")]
    Contacts {
        #[command(subcommand)]
        command: ContactsCommand,
    },
    #[command(about = "企業通訊錄、部門同步、名稱解析與按需查找")]
    Directory {
        #[command(subcommand)]
        command: m2k::directory::DirectoryCommand,
    },
    #[command(about = "共同空檔、會議室、邀請草稿與預約")]
    Meeting {
        #[command(subcommand)]
        command: m2k::meeting::MeetingCommand,
    },
    #[command(about = "安裝內附的 m2k 代理程式 skill")]
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    #[command(about = "開啟終端工作台；離線也可瀏覽")]
    Tui {
        #[arg(long, help = "以虛擬終端輸出畫面，適合無 TTY 驗證")]
        snapshot: bool,
        #[arg(long, default_value_t = 120)]
        width: u16,
        #[arg(long, default_value_t = 32)]
        height: u16,
    },
    #[command(about = "建立獨立虛構展示資料；不連線到 Mail2000")]
    Demo {
        #[arg(long)]
        no_tui: bool,
    },
}

#[derive(Subcommand)]
pub enum SkillsCommand {
    #[command(about = "安裝或更新 m2k/SKILL.md，預設放在 ~/.agents/skills")]
    Install {
        #[arg(
            long,
            visible_alias = "dest",
            value_name = "SKILLS_DIR",
            help = "skills 根目錄；安裝至 <SKILLS_DIR>/m2k/SKILL.md"
        )]
        path: Option<PathBuf>,
    },
}

#[derive(Args)]
pub struct InitArgs {
    #[arg(long)]
    pub username: String,
    #[arg(long, help = "Mail2000 伺服器；省略時使用 --site-profile 的 server")]
    pub server: Option<String>,
    #[arg(
        long,
        help = "site profile：內建名稱或 JSON 檔路徑；省略時依伺服器自動選擇"
    )]
    pub site_profile: Option<String>,
    #[arg(long)]
    pub email: Option<String>,
    #[arg(long)]
    pub caldav_url: Option<String>,
    #[arg(long)]
    pub carddav_url: Option<String>,
}

#[derive(Subcommand)]
pub enum AuthCommand {
    #[command(about = "隱藏輸入密碼，驗證四種服務並存入系統密碼庫")]
    Login {
        #[arg(long, help = "僅驗證，不保存密碼")]
        no_store: bool,
        #[arg(long, help = "尚無設定時使用的帳號；省略則詢問")]
        username: Option<String>,
        #[arg(long, help = "尚無設定時使用的 email")]
        email: Option<String>,
        #[arg(
            long,
            help = "尚無設定時使用的伺服器；省略時使用 --site-profile 的 server"
        )]
        server: Option<String>,
        #[arg(long, help = "尚無設定時使用的 site profile：內建名稱或 JSON 檔路徑")]
        site_profile: Option<String>,
    },
    #[command(about = "驗證 IMAP、SMTP、CalDAV 與 CardDAV")]
    Check,
    #[command(about = "移除系統密碼庫的密碼，保留離線資料")]
    Logout,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    Show,
    Set {
        #[arg(long)]
        sync_interval_minutes: Option<u64>,
        #[arg(long, help = "郵件保存天數，0 表示永久")]
        mail_retention_days: Option<u32>,
        #[arg(long, help = "過去事件保存天數，0 表示永久")]
        calendar_retention_days: Option<u32>,
        #[arg(long, help = "未再出現的聯絡人保存天數，0 表示永久")]
        contacts_retention_days: Option<u32>,
        #[arg(long)]
        imap_host: Option<String>,
        #[arg(long)]
        imap_port: Option<u16>,
        #[arg(long)]
        smtp_host: Option<String>,
        #[arg(long)]
        smtp_port: Option<u16>,
        #[arg(long)]
        caldav_url: Option<String>,
        #[arg(long)]
        carddav_url: Option<String>,
    },
}

#[derive(Args)]
pub struct ListArgs {
    #[arg(long)]
    pub query: Option<String>,
    #[arg(long)]
    pub collection: Option<String>,
    #[arg(long, default_value_t = 100)]
    pub limit: usize,
}

#[derive(Args)]
pub struct SendArgs {
    #[arg(long, value_delimiter = ',', required = true)]
    pub to: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    pub cc: Vec<String>,
    #[arg(long)]
    pub subject: String,
    #[arg(long, help = "UTF-8 文字檔；- 從標準輸入讀取")]
    pub body_file: PathBuf,
    #[arg(long)]
    pub attach: Vec<PathBuf>,
}

#[derive(Subcommand)]
pub enum MailCommand {
    #[command(about = "常駐監看新郵件，增量保存並推送 webhook；支援 IDLE 與斷線接續")]
    Watch(m2k::mail_webhook::WatchArgs),
    #[command(flatten)]
    Workflow(m2k::mail_workflow::MailWorkflowCommand),
    Folders,
    List(ListArgs),
    Search {
        query: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    #[command(about = "讀取指定郵件的新回覆正文；--full 包含歷史引用")]
    Show {
        id: String,
        #[arg(long, help = "輸出完整 RFC822 原始郵件")]
        raw: bool,
        #[arg(
            long,
            conflicts_with = "raw",
            help = "顯示完整正文，包含歷史回覆與引用"
        )]
        full: bool,
    },
    Send(SendArgs),
    Reply {
        id: String,
        #[arg(long)]
        body_file: PathBuf,
        #[arg(long)]
        attach: Vec<PathBuf>,
    },
    Forward {
        id: String,
        #[arg(long, value_delimiter = ',', required = true)]
        to: Vec<String>,
        #[arg(long)]
        body_file: Option<PathBuf>,
    },
    Flag {
        id: String,
        #[arg(long, value_parser=["seen","flagged","answered"])]
        flag: String,
        #[arg(long)]
        clear: bool,
    },
    Move {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
        #[arg(long)]
        folder: String,
        #[arg(long)]
        dry_run: bool,
    },
    Delete {
        id: String,
    },
    Attachment {
        id: String,
        #[arg(long)]
        index: usize,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Args)]
pub struct EventFieldArgs {
    #[arg(long, help = "事件標題")]
    pub title: Option<String>,
    #[arg(
        long,
        help = "開始：明天 14:00、下週一 下午兩點、2026-10-22 09:30 或 RFC3339（台北時間）"
    )]
    pub start: Option<String>,
    #[arg(long, help = "結束；只寫時刻時沿用開始日期；全天事件為最後一天（含）")]
    pub end: Option<String>,
    #[arg(long, help = "地點；更新時給空字串會移除")]
    pub location: Option<String>,
    #[arg(long, help = "說明；更新時給空字串會移除")]
    pub description: Option<String>,
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "true",
        value_name = "BOOL",
        help = "全天事件；更新時 --all-day=false 改回定時（需 --start）"
    )]
    pub all_day: Option<bool>,
}

impl EventFieldArgs {
    pub fn into_input(self) -> m2k::calendar_event::EventInput {
        m2k::calendar_event::EventInput {
            title: self.title,
            start: self.start,
            end: self.end,
            location: self.location,
            description: self.description,
            all_day: self.all_day,
        }
    }
}

const EVENT_FIELDS: [&str; 6] = [
    "title",
    "start",
    "end",
    "location",
    "description",
    "all_day",
];

#[derive(Subcommand)]
pub enum CalendarCommand {
    #[command(
        about = "建立事件：--file ICS，或 --title --start --end 等欄位",
        long_about = "建立個人行事曆事件。\n\n以欄位建立時必填 --title 與 --start；定時事件需 --end，--all-day 時 --end 可省略（單日）。日期時刻以台北時間的人話解析，例如「明天 14:00」「下週一 下午兩點」。建立使用 If-None-Match: *，不自動重試。"
    )]
    Create {
        #[arg(long, required_unless_present = "title", conflicts_with_all = EVENT_FIELDS)]
        file: Option<PathBuf>,
        #[arg(long)]
        collection: Option<String>,
        #[command(flatten)]
        fields: EventFieldArgs,
    },
    #[command(
        about = "更新事件：--file 整份 ICS，或只改指定欄位",
        long_about = "以欄位更新時只改給定的欄位，保留原 UID 與其他屬性；只改開始時保留原長度。重複事件、別人邀請的事件、有其他與會者或會議室的會議，以及會議室行事曆會拒絕。使用原 ETag 的 If-Match，不自動重試。"
    )]
    #[command(group(
        ArgGroup::new("change")
            .required(true)
            .multiple(true)
            .args(["file", "title", "start", "end", "location", "description", "all_day"])
    ))]
    Update {
        id: String,
        #[arg(long, conflicts_with_all = EVENT_FIELDS)]
        file: Option<PathBuf>,
        #[command(flatten)]
        fields: EventFieldArgs,
    },
    #[command(flatten)]
    Dav(DavCommand),
}

#[derive(Subcommand)]
pub enum ContactsCommand {
    Create {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        collection: Option<String>,
    },
    Update {
        id: String,
        #[arg(long)]
        file: PathBuf,
    },
    #[command(flatten)]
    Dav(DavCommand),
}

#[derive(Subcommand)]
pub enum DavCommand {
    Collections,
    List(ListArgs),
    Search {
        query: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Show {
        id: String,
        #[arg(long)]
        raw: bool,
    },
    Delete {
        id: String,
    },
    Export {
        id: String,
        #[arg(long)]
        output: PathBuf,
    },
}

#[cfg(test)]
mod move_tests {
    use super::*;

    #[test]
    fn move_accepts_single_batch_and_dry_run_and_requires_ids() {
        for args in [
            vec!["m2k", "mail", "move", "one", "--folder", "Archive"],
            vec![
                "m2k",
                "mail",
                "move",
                "--folder",
                "Archive",
                "one",
                "two",
                "--dry-run",
            ],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            let Some(Command::Mail {
                command:
                    MailCommand::Move {
                        ids,
                        folder,
                        dry_run,
                    },
            }) = cli.command
            else {
                panic!("expected mail move");
            };
            assert_eq!(folder, "Archive");
            assert_eq!(dry_run, args.contains(&"--dry-run"));
            assert_eq!(ids.len(), if dry_run { 2 } else { 1 });
        }
        assert!(Cli::try_parse_from(["m2k", "mail", "move", "--folder", "Archive"]).is_err());
    }
}
