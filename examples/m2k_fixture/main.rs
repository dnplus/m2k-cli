//! `cargo run --example m2k_fixture -- --data-dir <dir>`
//!
//! Local fake Mail2000 for exercising the real m2k binary online paths.
//! See examples/m2k_fixture/README.md for the end-to-end walkthrough.

#[path = "fixture.rs"]
mod fixture;

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Loopback fake Mail2000 (IMAP/SMTP/CalDAV/CardDAV) for m2k verification")]
struct Args {
    /// Fixture state and side-effect log; must be new, empty or a previous fixture dir.
    #[arg(long)]
    data_dir: PathBuf,
    #[arg(long, default_value = "fixture@example.test")]
    user: String,
    #[arg(long, default_value = "fixture-password")]
    password: String,
    /// 0 picks a free port.
    #[arg(long, default_value_t = 0)]
    imap_port: u16,
    #[arg(long, default_value_t = 0)]
    smtp_port: u16,
    #[arg(long, default_value_t = 0)]
    http_port: u16,
    /// Advertise RFC 6578 sync-token / sync-collection on calendars.
    #[arg(long)]
    sync_collection: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let info = fixture::start(fixture::Options {
        data_dir: args.data_dir,
        user: args.user,
        password: args.password,
        imap_port: args.imap_port,
        smtp_port: args.smtp_port,
        http_port: args.http_port,
        sync_collection: args.sync_collection,
    })?;
    println!("{}", serde_json::to_string(&info)?);
    eprintln!(
        "[fixture] ready; endpoints in {}; Ctrl-C to stop",
        info.data_dir.join("fixture.json").display()
    );
    loop {
        std::thread::park();
    }
}
