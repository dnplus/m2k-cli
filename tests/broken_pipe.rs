//! `m2k … | head` is how bots and scripts read m2k output. When the reader
//! goes away first, m2k must exit like `cat` does (141) rather than panic.
#![cfg(unix)]

use std::process::{Command, Stdio};
use tempfile::TempDir;

fn m2k(dir: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_m2k"));
    command
        .arg("--data-dir")
        .arg(dir.path())
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db");
    command
}

/// Runs m2k with stdout on a pipe whose read end is already closed, so the
/// first write hits EPIPE regardless of timing.
fn run_into_closed_pipe(dir: &TempDir, args: &[&str]) -> (Option<i32>, String) {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let output = m2k(dir)
        .args(args)
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn closed_stdout_exits_141_without_panic() {
    let dir = TempDir::new().unwrap();
    let demo = m2k(&dir).args(["demo", "--no-tui"]).output().unwrap();
    assert!(demo.status.success(), "demo seed failed: {demo:?}");

    for args in [&["mail", "list"][..], &["--json", "mail", "list"][..]] {
        let (code, stderr) = run_into_closed_pipe(&dir, args);
        assert!(!stderr.contains("panicked"), "{args:?} panicked: {stderr}");
        assert_eq!(code, Some(141), "{args:?} exit code, stderr: {stderr}");
    }
}
