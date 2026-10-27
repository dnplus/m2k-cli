use anyhow::{bail, Context, Result};
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags};
use std::fs::{self, File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn open(path: &Path) -> Result<(Connection, File)> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let initialization = options.open(path.with_extension("init.lock"))?;
    let started = Instant::now();
    loop {
        match initialization.try_lock_exclusive() {
            Ok(()) => break,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && started.elapsed() < BUSY_TIMEOUT =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("SQLite 初始化正在由其他程序執行，請稍後重試"),
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        options.open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    options.open(path)?;
    let conn = Connection::open(path)
        .with_context(|| format!("無法開啟 SQLite 資料庫：{}", path.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if !journal_mode(&conn)?.eq_ignore_ascii_case("wal") {
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            bail!("SQLite 資料庫無法啟用 WAL：{}", path.display());
        }
    }
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok((conn, initialization))
}

pub(crate) fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    if !journal_mode(&conn)?.eq_ignore_ascii_case("wal") {
        bail!("SQLite 資料庫需要先初始化 WAL");
    }
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(conn)
}

fn journal_mode(conn: &Connection) -> Result<String> {
    Ok(conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_connection_uses_wal_and_at_least_a_five_second_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("database.sqlite3");
        let (writable, _initialization) = open(&path).unwrap();
        let reader = open_reader(&path).unwrap();
        for conn in [&writable, &reader] {
            assert_eq!(journal_mode(conn).unwrap(), "wal");
            let timeout: i64 = conn
                .pragma_query_value(None, "busy_timeout", |row| row.get(0))
                .unwrap();
            assert!(timeout >= 5_000);
        }
        assert!(reader.is_readonly(rusqlite::DatabaseName::Main).unwrap());
    }
}
