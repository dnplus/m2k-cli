//! macOS keychain access through Apple-signed `/usr/bin/security`.
//!
//! Items written here trust `/usr/bin/security` instead of the m2k binary, so
//! rebuilt or unsigned m2k binaries never trigger the keychain prompt. The
//! trade-off matches gh: any process of the same user can run `security` and
//! read the item.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SECURITY: &str = "/usr/bin/security";
// `security` exits with the low byte of the OSStatus; errSecItemNotFound is -25300.
const EXIT_ITEM_NOT_FOUND: i32 = 44;

pub struct Output {
    pub status: Option<i32>,
    pub stderr: Vec<u8>,
}

pub trait Runner {
    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<Output, Error>;

    /// Keychain file appended to every command; `None` is the default keychain.
    fn keychain(&self) -> Option<&str> {
        None
    }
}

pub struct SecurityCommand {
    pub program: PathBuf,
    pub keychain: Option<String>,
    pub timeout: Duration,
}

impl SecurityCommand {
    /// `M2K_KEYCHAIN` points every command at a keychain file instead of the
    /// login keychain, so tests and fixtures never touch real credentials.
    pub fn from_env(timeout: Duration) -> Self {
        Self {
            program: PathBuf::from(SECURITY),
            keychain: std::env::var("M2K_KEYCHAIN")
                .ok()
                .filter(|value| !value.is_empty()),
            timeout,
        }
    }
}

impl Runner for SecurityCommand {
    // The child is killed at the deadline: a pending keychain prompt must not
    // leave an orphaned `security` process or a blocked thread behind.
    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<Output, Error> {
        let program = self.program.display();
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| Error::Failed(format!("無法執行 {program}：{error}")))?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            if let Err(error) = pipe.write_all(input) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Failed(format!(
                    "無法寫入 {program} 標準輸入：{error}"
                )));
            }
        }
        let mut pipe = child.stderr.take();
        let reader = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            if let Some(pipe) = pipe.as_mut() {
                let _ = pipe.read_to_end(&mut buffer);
            }
            buffer
        });
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(match result {
                        Err(error) => Error::Failed(format!("無法等待 {program} 結束：{error}")),
                        _ => Error::Timeout(self.timeout),
                    });
                }
            }
        };
        Ok(Output {
            status: status.code(),
            stderr: reader.join().unwrap_or_default(),
        })
    }

    fn keychain(&self) -> Option<&str> {
        self.keychain.as_deref()
    }
}

#[derive(Debug)]
pub enum Error {
    NoEntry,
    Timeout(Duration),
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoEntry => f.write_str("No matching entry found in secure storage"),
            Error::Timeout(timeout) => write!(f, "security 逾時（{} 秒）", timeout.as_secs()),
            Error::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

pub fn get(runner: &dyn Runner, service: &str, account: &str) -> Result<String, Error> {
    // `-g` instead of `-w`: `-w` prints non-printable data as bare hex, which
    // is indistinguishable from a literal hex password. `-g` marks hex with 0x.
    let mut args = vec!["find-generic-password", "-s", service, "-a", account, "-g"];
    args.extend(runner.keychain());
    let output = runner.run(&args, None)?;
    match output.status {
        Some(0) => parse_password(&output.stderr),
        Some(EXIT_ITEM_NOT_FOUND) => Err(Error::NoEntry),
        status => Err(failure("find-generic-password", status, &output.stderr)),
    }
}

/// Replaces the item so its ACL trusts `/usr/bin/security`. `add -U` alone
/// keeps the ACL of an existing item, so the old item is deleted first.
pub fn set(runner: &dyn Runner, service: &str, account: &str, value: &str) -> Result<(), Error> {
    delete(runner, service, account)?;
    add(runner, service, account, value)
}

pub fn add(runner: &dyn Runner, service: &str, account: &str, value: &str) -> Result<(), Error> {
    let keychain = runner.keychain().unwrap_or_default();
    for field in [service, account, keychain] {
        if field.contains(['"', '\\', '\n']) {
            return Err(Error::Failed("密碼庫項目名稱含有不支援的字元".to_owned()));
        }
    }
    // Interactive mode reads the command from stdin, so the password never
    // appears in any process argument list.
    let hex: String = value.bytes().map(|byte| format!("{byte:02x}")).collect();
    let mut script =
        format!("add-generic-password -U -s \"{service}\" -a \"{account}\" -X \"{hex}\"");
    if !keychain.is_empty() {
        script.push_str(&format!(" \"{keychain}\""));
    }
    script.push('\n');
    let output = runner.run(&["-i"], Some(script.as_bytes()))?;
    match output.status {
        // stderr is not echoed: it could contain the command line.
        Some(0) => Ok(()),
        status => Err(failure("add-generic-password", status, b"")),
    }
}

pub fn delete(runner: &dyn Runner, service: &str, account: &str) -> Result<(), Error> {
    let mut args = vec!["delete-generic-password", "-s", service, "-a", account];
    args.extend(runner.keychain());
    let output = runner.run(&args, None)?;
    match output.status {
        Some(0) | Some(EXIT_ITEM_NOT_FOUND) => Ok(()),
        status => Err(failure("delete-generic-password", status, &output.stderr)),
    }
}

fn failure(command: &str, status: Option<i32>, stderr: &[u8]) -> Error {
    let status = status.map_or_else(|| "signal".to_owned(), |code| code.to_string());
    let detail = String::from_utf8_lossy(stderr).trim().to_owned();
    if detail.is_empty() {
        Error::Failed(format!("security {command} 失敗（狀態 {status}）"))
    } else {
        Error::Failed(format!(
            "security {command} 失敗（狀態 {status}）：{detail}"
        ))
    }
}

// `-g` writes `password: "literal"` (printable ASCII only),
// `password: 0xHEX  "escaped"`, or a bare `password: ` for empty data.
fn parse_password(stderr: &[u8]) -> Result<String, Error> {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("password:"))
        .ok_or_else(|| Error::Failed("security 沒有回傳密碼".to_owned()))?;
    let line = line.strip_prefix(' ').unwrap_or(line);
    if line.trim().is_empty() {
        return Err(Error::Failed(
            "系統密碼庫中的密碼是空白；請重新執行 m2k auth login".to_owned(),
        ));
    }
    if let Some(hex) = line.strip_prefix("0x") {
        let hex = hex.split_whitespace().next().unwrap_or_default();
        let bytes = decode_hex(hex)
            .ok_or_else(|| Error::Failed("security 回傳的密碼格式無法辨識".to_owned()))?;
        return String::from_utf8(bytes)
            .map_err(|_| Error::Failed("系統密碼庫中的密碼不是有效的 UTF-8".to_owned()));
    }
    line.strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
        .ok_or_else(|| Error::Failed("security 回傳的密碼格式無法辨識".to_owned()))
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(hex.get(index..index + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records calls and replays scripted outputs; never touches a keychain.
    pub struct FakeRunner {
        pub calls: RefCell<Vec<(Vec<String>, Option<String>)>>,
        pub outputs: RefCell<Vec<Output>>,
    }

    impl FakeRunner {
        pub fn new(outputs: Vec<(i32, &str)>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                outputs: RefCell::new(
                    outputs
                        .into_iter()
                        .map(|(status, stderr)| Output {
                            status: Some(status),
                            stderr: stderr.as_bytes().to_vec(),
                        })
                        .collect(),
                ),
            }
        }

        pub fn commands(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|(args, _)| args[0].clone())
                .collect()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<Output, Error> {
            self.calls.borrow_mut().push((
                args.iter().map(|arg| (*arg).to_owned()).collect(),
                stdin.map(|input| String::from_utf8(input.to_vec()).unwrap()),
            ));
            let mut outputs = self.outputs.borrow_mut();
            if outputs.is_empty() {
                return Err(Error::Failed(format!("unexpected security call: {args:?}")));
            }
            Ok(outputs.remove(0))
        }
    }

    #[test]
    fn reads_literal_and_hex_passwords_unambiguously() {
        // A literal hex-looking password must stay literal.
        let runner = FakeRunner::new(vec![(0, "password: \"e4b8ad\"\n")]);
        assert_eq!(get(&runner, "m2k", "acct").unwrap(), "e4b8ad");
        let runner = FakeRunner::new(vec![(0, "password: 0xE4B8ADE69687 \n")]);
        assert_eq!(get(&runner, "m2k", "acct").unwrap(), "中文");
        let runner = FakeRunner::new(vec![(
            0,
            "password: 0x70407373202277307264225C78  \"p@ss \"w0rd\"\\134x\"\n",
        )]);
        assert_eq!(get(&runner, "m2k", "acct").unwrap(), "p@ss \"w0rd\"\\x");
        assert_eq!(
            runner.calls.borrow()[0].0,
            ["find-generic-password", "-s", "m2k", "-a", "acct", "-g"]
        );
    }

    // An empty item would otherwise surface as an unrecognised format.
    #[test]
    fn empty_password_is_reported_as_empty() {
        for stderr in ["password: \n", "password: ", "password:\n"] {
            let runner = FakeRunner::new(vec![(0, stderr)]);
            let error = get(&runner, "m2k", "acct").unwrap_err().to_string();
            assert!(error.contains("空白"), "{stderr:?}: {error}");
        }
    }

    #[test]
    fn missing_item_maps_to_no_entry() {
        let runner = FakeRunner::new(vec![(44, "security: SecKeychainSearchCopyNext: ...")]);
        assert!(matches!(get(&runner, "m2k", "acct"), Err(Error::NoEntry)));
        let runner = FakeRunner::new(vec![(44, "")]);
        assert!(delete(&runner, "m2k", "acct").is_ok());
    }

    #[test]
    fn other_failures_keep_status_and_detail() {
        let runner = FakeRunner::new(vec![(128, "User canceled the operation.")]);
        let error = get(&runner, "m2k", "acct").unwrap_err().to_string();
        assert!(error.contains("128") && error.contains("canceled"));
        let runner = FakeRunner::new(vec![(0, "keychain: \"login\"\n")]);
        assert!(get(&runner, "m2k", "acct").is_err());
    }

    #[test]
    fn set_replaces_item_and_keeps_password_off_argv() {
        let runner = FakeRunner::new(vec![(44, ""), (0, "")]);
        set(&runner, "m2k", "acct", "s3cret 中").unwrap();
        let calls = runner.calls.borrow();
        assert_eq!(
            calls[0].0,
            ["delete-generic-password", "-s", "m2k", "-a", "acct"]
        );
        assert_eq!(calls[1].0, ["-i"]);
        for (args, _) in calls.iter() {
            assert!(args.iter().all(|arg| !arg.contains("s3cret")));
        }
        assert_eq!(
            calls[1].1.as_deref().unwrap(),
            "add-generic-password -U -s \"m2k\" -a \"acct\" -X \"73336372657420e4b8ad\"\n"
        );
    }

    #[test]
    fn set_failure_does_not_echo_stderr() {
        let runner = FakeRunner::new(vec![(0, ""), (2, "add-generic-password -X \"7333\"")]);
        let error = set(&runner, "m2k", "acct", "s3").unwrap_err().to_string();
        assert!(error.contains("狀態 2"));
        assert!(!error.contains("7333"));
    }

    #[test]
    fn set_rejects_names_that_would_break_the_script() {
        let runner = FakeRunner::new(vec![(0, "")]);
        assert!(set(&runner, "m2k", "a\" -X \"00", "pw").is_err());
        assert_eq!(runner.commands(), ["delete-generic-password"]);
    }

    // A prompt nobody answers must not leave the child running after the
    // caller has given up.
    #[cfg(unix)]
    #[test]
    fn timed_out_child_is_killed() {
        let marker = format!("{}.{}", 30 + std::process::id() % 1000, 12_345);
        let runner = SecurityCommand {
            program: PathBuf::from("/bin/sleep"),
            keychain: None,
            timeout: Duration::from_millis(100),
        };
        let started = Instant::now();
        assert!(matches!(
            runner.run(&[&marker], None),
            Err(Error::Timeout(_))
        ));
        assert!(started.elapsed() < Duration::from_secs(5));
        let alive = Command::new("pgrep")
            .args(["-f", &format!("sleep {marker}")])
            .status()
            .unwrap();
        assert!(!alive.success(), "sleep {marker} is still running");
    }

    /// Real `security` against a throwaway keychain file; the login keychain
    /// is never read or written.
    #[cfg(target_os = "macos")]
    struct TempKeychain {
        _dir: tempfile::TempDir,
        runner: SecurityCommand,
    }

    #[cfg(target_os = "macos")]
    impl TempKeychain {
        fn create() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("m2k-test.keychain-db");
            let path = path.to_str().unwrap().to_owned();
            let status = Command::new(SECURITY)
                .args(["create-keychain", "-p", "m2k-test", &path])
                .status()
                .unwrap();
            assert!(status.success());
            Self {
                _dir: dir,
                runner: SecurityCommand {
                    program: PathBuf::from(SECURITY),
                    keychain: Some(path),
                    timeout: Duration::from_secs(15),
                },
            }
        }

        fn acl(&self) -> String {
            let output = Command::new(SECURITY)
                .args(["dump-keychain", "-a", self.runner.keychain().unwrap()])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for TempKeychain {
        fn drop(&mut self) {
            if let Some(path) = self.runner.keychain() {
                let _ = Command::new(SECURITY)
                    .args(["delete-keychain", path])
                    .status();
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn real_security_round_trip_in_temporary_keychain() {
        let keychain = TempKeychain::create();
        let runner = &keychain.runner;
        assert!(matches!(
            get(runner, "m2k-test", "acct"),
            Err(Error::NoEntry)
        ));
        for value in ["e4b8ad", "p@ss \"w0rd\"\\ 中文", "a'b\tc"] {
            set(runner, "m2k-test", "acct", value).unwrap();
            assert_eq!(get(runner, "m2k-test", "acct").unwrap(), value);
        }
        delete(runner, "m2k-test", "acct").unwrap();
        delete(runner, "m2k-test", "acct").unwrap();
        assert!(matches!(
            get(runner, "m2k-test", "acct"),
            Err(Error::NoEntry)
        ));
    }

    // `add -U` alone keeps the creator's ACL; `set` must recreate the item so
    // it trusts /usr/bin/security instead of the app that created it.
    #[cfg(target_os = "macos")]
    #[test]
    fn real_security_set_replaces_acl_of_item_created_by_another_app() {
        let keychain = TempKeychain::create();
        let script = format!(
            "add-generic-password -s \"m2k-test\" -a \"acct\" -X \"6f6c64\" -T \"/usr/bin/true\" \"{}\"\n",
            keychain.runner.keychain().unwrap()
        );
        let created = keychain
            .runner
            .run(&["-i"], Some(script.as_bytes()))
            .unwrap();
        assert_eq!(created.status, Some(0));
        assert!(keychain.acl().contains("/usr/bin/true"));
        assert!(!keychain.acl().contains("/usr/bin/security"));
        set(&keychain.runner, "m2k-test", "acct", "new").unwrap();
        assert!(keychain.acl().contains("/usr/bin/security"));
        assert!(!keychain.acl().contains("/usr/bin/true"));
        assert_eq!(get(&keychain.runner, "m2k-test", "acct").unwrap(), "new");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn nonexistent_keychain_file_reads_as_missing_without_creating_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.keychain-db");
        let runner = SecurityCommand {
            program: PathBuf::from(SECURITY),
            keychain: Some(path.to_str().unwrap().to_owned()),
            timeout: Duration::from_secs(15),
        };
        assert!(matches!(get(&runner, "m2k", "acct"), Err(Error::NoEntry)));
        assert!(!path.exists());
    }
}
