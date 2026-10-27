//! Loopback fake Mail2000: IMAP (TLS), SMTP (implicit TLS) and
//! CalDAV/CardDAV (HTTP) sharing one account, seeded on every start.
//! Side effects are written to `<data-dir>/events.jsonl` plus the raw
//! SMTP messages (`smtp/`) and PUT bodies (`dav/`).

#[path = "dav.rs"]
mod dav;
#[path = "imap.rs"]
mod imap;
#[path = "net.rs"]
mod net;
#[path = "smtp.rs"]
mod smtp;
#[path = "state.rs"]
pub mod state;
#[path = "tls.rs"]
mod tls;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{
    fs,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

const MARKER: &str = ".m2k-fixture";
const OWNED: [&str; 8] = [
    "tls",
    "smtp",
    "dav",
    "imap",
    "webhook",
    "samples",
    "events.jsonl",
    "http.jsonl",
];

pub struct Options {
    pub data_dir: PathBuf,
    pub user: String,
    pub password: String,
    pub imap_port: u16,
    pub smtp_port: u16,
    pub http_port: u16,
    /// Advertise RFC 6578 sync-collection on calendar collections.
    pub sync_collection: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Info {
    pub data_dir: PathBuf,
    pub user: String,
    pub password: String,
    pub host: String,
    pub imap_port: u16,
    pub smtp_port: u16,
    pub http_port: u16,
    pub server: String,
    pub ca_cert: PathBuf,
    pub room_calendar_url: String,
    pub webhook_url: String,
    pub booking_ics: PathBuf,
    pub mail_body: PathBuf,
    pub events: PathBuf,
    /// One JSON line per DAV HTTP request (method, path, status, responses).
    pub http_log: PathBuf,
}

/// Prepares `data_dir`, seeds state and starts all listeners on 127.0.0.1.
/// Listener threads run until the process exits.
pub fn start(options: Options) -> Result<Info> {
    prepare(&options.data_dir)?;
    let data_dir = fs::canonicalize(&options.data_dir)?;
    let acceptor = Arc::new(tls::generate(&data_dir.join("tls"))?);
    let now = chrono::Utc::now();
    let fixture = Arc::new(state::Fixture::seeded(
        options.user.clone(),
        options.password.clone(),
        data_dir.clone(),
        now,
        options.sync_collection,
    ));
    let host = "127.0.0.1";
    let imap = TcpListener::bind((host, options.imap_port)).context("bind IMAP")?;
    let smtp = TcpListener::bind((host, options.smtp_port)).context("bind SMTP")?;
    let http = TcpListener::bind((host, options.http_port)).context("bind HTTP")?;
    let info = Info {
        user: options.user.clone(),
        password: options.password.clone(),
        host: host.to_owned(),
        imap_port: imap.local_addr()?.port(),
        smtp_port: smtp.local_addr()?.port(),
        http_port: http.local_addr()?.port(),
        server: format!("http://{host}:{}", http.local_addr()?.port()),
        ca_cert: data_dir.join("tls/ca.pem"),
        room_calendar_url: format!(
            "http://{host}:{}{}",
            http.local_addr()?.port(),
            state::calendar_href(state::ROOM_EMAIL)
        ),
        webhook_url: format!("http://{host}:{}/webhook", http.local_addr()?.port()),
        booking_ics: data_dir.join("samples/booking.ics"),
        mail_body: data_dir.join("samples/body.txt"),
        events: data_dir.join("events.jsonl"),
        http_log: data_dir.join("http.jsonl"),
        data_dir: data_dir.clone(),
    };
    write_samples(&info, now)?;
    fs::write(
        data_dir.join("fixture.json"),
        serde_json::to_vec_pretty(&info)?,
    )?;
    spawn_tls("imap", imap, fixture.clone(), acceptor.clone(), imap::serve);
    spawn_tls("smtp", smtp, fixture.clone(), acceptor, smtp::serve);
    let dav_fixture = fixture;
    thread::spawn(move || {
        for stream in http.incoming().flatten() {
            let fixture = dav_fixture.clone();
            thread::spawn(move || {
                if let Err(error) = dav::serve(fixture, stream) {
                    eprintln!("[fixture] http: {error:#}");
                }
            });
        }
    });
    Ok(info)
}

fn spawn_tls(
    label: &'static str,
    listener: TcpListener,
    fixture: Arc<state::Fixture>,
    acceptor: Arc<native_tls::TlsAcceptor>,
    serve: fn(Arc<state::Fixture>, native_tls::TlsStream<TcpStream>) -> Result<()>,
) {
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let fixture = fixture.clone();
            let acceptor = acceptor.clone();
            thread::spawn(move || {
                let result = acceptor
                    .accept(stream)
                    .map_err(|error| anyhow::anyhow!("TLS accept: {error}"))
                    .and_then(|stream| serve(fixture, stream));
                if let Err(error) = result {
                    eprintln!("[fixture] {label}: {error:#}");
                }
            });
        }
    });
}

/// Refuses directories the fixture did not create; otherwise starts empty.
fn prepare(dir: &Path) -> Result<()> {
    if dir.exists() {
        let has_entries = fs::read_dir(dir)?.next().is_some();
        if has_entries && !dir.join(MARKER).exists() {
            bail!(
                "{} is not empty and not a fixture data dir; pick a new directory",
                dir.display()
            );
        }
        for name in OWNED.iter().chain(["fixture.json"].iter()) {
            let path = dir.join(name);
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else if path.exists() {
                fs::remove_file(&path)?;
            }
        }
    }
    fs::create_dir_all(dir.join("samples"))?;
    fs::write(dir.join(MARKER), "m2k loopback fixture data\n")?;
    Ok(())
}

fn write_samples(info: &Info, now: chrono::DateTime<chrono::Utc>) -> Result<()> {
    let day = (now + chrono::Duration::days(1)).date_naive();
    let at = |hour| {
        day.and_hms_opt(hour, 0, 0)
            .expect("hour")
            .and_utc()
            .format("%Y%m%dT%H%M%SZ")
            .to_string()
    };
    let booking = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//m2k//fixture sample//ZH-TW\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:fixture-booking-{}\r\nDTSTAMP:{}\r\nDTSTART:{}\r\nDTEND:{}\r\nSUMMARY:Fixture 預約測試\r\nLOCATION:第一會議室 (A)\r\nORGANIZER:mailto:{}\r\nATTENDEE;CUTYPE=ROOM;CN=第一會議室 (A):mailto:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        now.timestamp(),
        now.format("%Y%m%dT%H%M%SZ"),
        at(8),
        at(9),
        info.user,
        state::ROOM_EMAIL
    );
    fs::write(&info.booking_ics, booking)?;
    fs::write(&info.mail_body, "這是 m2k fixture 寄送測試。\n")?;
    Ok(())
}
