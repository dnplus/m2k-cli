//! Real m2k binary against the loopback fixture with and without RFC 6578
//! sync-collection: a full `sync` stores the token, the next `watch` tick
//! fetches only the delta, a rejected token falls back to one full sync,
//! and a server without sync-collection keeps the near-window ETag refresh.
//! Request counts come from the fixture's `http.jsonl`.

#[path = "../examples/m2k_fixture/fixture.rs"]
mod fixture;

use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    process::{Command, Stdio},
};

const PASSWORD: &str = "fixture-test-password";
const USER: &str = "fixture@example.test";
/// Unchanged near events: the ETag window REPORT lists them on every tick.
const FILLER: usize = 20;

struct Run {
    _root: tempfile::TempDir,
    info: fixture::Info,
    client: std::path::PathBuf,
}

impl Run {
    fn start(sync_collection: bool) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let info = fixture::start(fixture::Options {
            data_dir: root.path().join("fixture"),
            user: USER.into(),
            password: PASSWORD.into(),
            imap_port: 0,
            smtp_port: 0,
            http_port: 0,
            sync_collection,
        })
        .expect("start fixture");
        let client = root.path().join("client");
        let run = Self {
            _root: root,
            info,
            client,
        };
        for index in 0..FILLER {
            run.put(&format!("filler-{index}"), "填充", 3 + index as i64);
        }
        run.m2k(&["init", "--username", USER, "--server", &run.info.server]);
        let synced = run.m2k(&["sync", "--kind", "calendar"]);
        assert_eq!(synced[0]["count"], 2 + FILLER, "{synced}");
        run
    }

    fn m2k(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
            .arg("--json")
            .arg("--data-dir")
            .arg(&self.client)
            .env("M2K_PASSWORD", PASSWORD)
            .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db")
            .env_remove("M2K_TEST_ROOT_CERT")
            .args(args)
            .output()
            .expect("run m2k");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "m2k {args:?} failed\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_str(stdout.trim()).expect("m2k JSON")
    }

    /// One `watch` cycle: the periodic (non-full) policy, since `sync` just
    /// recorded a full calendar sync.
    fn watch_once(&self) -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_m2k"))
            .arg("--json")
            .arg("--data-dir")
            .arg(&self.client)
            .args(["watch", "--kind", "calendar"])
            .env("M2K_PASSWORD", PASSWORD)
            .env("M2K_KEYCHAIN", "/nonexistent/m2k-test.keychain-db")
            .env_remove("M2K_TEST_ROOT_CERT")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn watch");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .expect("watch output");
        child.kill().ok();
        child.wait().ok();
        let cycle: Value = serde_json::from_str(line.trim()).expect("watch JSON");
        assert_eq!(cycle["state"], "complete", "{cycle}");
        cycle
    }

    fn collection(&self) -> String {
        format!(
            "{}/cgi-bin/cal/caldav/calendars/fixture%40example.test/default/",
            self.info.server
        )
    }

    /// Another client edits the server calendar behind m2k's back.
    fn put(&self, uid: &str, title: &str, days: i64) {
        let start = chrono::Utc::now().date_naive() + chrono::Duration::days(days);
        let at = |hour| format!("{}T{hour:02}0000Z", start.format("%Y%m%d"));
        let body = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:{}\r\nDTEND:{}\r\nSUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            at(1),
            at(2)
        );
        let status = reqwest::blocking::Client::new()
            .put(format!("{}{uid}.ics", self.collection()))
            .basic_auth(USER, Some(PASSWORD))
            .body(body)
            .send()
            .expect("fixture PUT")
            .status();
        assert!(status.is_success(), "PUT {uid}: {status}");
    }

    fn delete(&self, uid: &str) {
        let status = reqwest::blocking::Client::new()
            .delete(format!("{}{uid}.ics", self.collection()))
            .basic_auth(USER, Some(PASSWORD))
            .send()
            .expect("fixture DELETE")
            .status();
        assert!(status.is_success(), "DELETE {uid}: {status}");
    }

    fn http_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.info.http_log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("http.jsonl"))
            .collect()
    }

    fn titles(&self) -> Vec<String> {
        let mut titles = self
            .m2k(&["calendar", "list"])
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["title"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        titles.sort();
        titles
    }

    fn metadata(&self, key_prefix: &str) -> Vec<(String, String)> {
        let db = rusqlite::Connection::open(self.client.join("database.sqlite3")).unwrap();
        let mut statement = db
            .prepare("SELECT key, value FROM metadata WHERE substr(key, 1, ?1) = ?2 ORDER BY key")
            .unwrap();
        statement
            .query_map(
                rusqlite::params![key_prefix.len() as i64, key_prefix],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn set_metadata(&self, key: &str, value: &str) {
        let db = rusqlite::Connection::open(self.client.join("database.sqlite3")).unwrap();
        db.execute(
            "UPDATE metadata SET value = ?2 WHERE key = ?1",
            [key, value],
        )
        .unwrap();
    }

    fn tokens(&self) -> Vec<(String, String)> {
        self.metadata("sync.calendar.token.")
    }

    /// Edit, delete, add, and move an event far outside the ±30 day window,
    /// then run one watch tick and return the DAV requests it made.
    fn change_then_watch(&self) -> Vec<Value> {
        self.put("filler-0", "已修改", 3);
        self.delete("filler-1");
        self.put("brand-new", "新事件", 5);
        self.put("fixture-sync", "改到遠處", 200);
        let before = self.http_lines().len();
        self.watch_once();
        self.http_lines().split_off(before)
    }
}

fn report(requests: &[Value], name: &str) -> Vec<Value> {
    requests
        .iter()
        .filter(|request| request["method"] == "REPORT" && request["report"] == name)
        .cloned()
        .collect()
}

fn methods(requests: &[Value]) -> Vec<String> {
    requests
        .iter()
        .map(|request| request["method"].as_str().unwrap().to_owned())
        .collect()
}

fn responses(requests: &[Value]) -> u64 {
    requests
        .iter()
        .map(|request| request["responses"].as_u64().unwrap())
        .sum()
}

#[test]
fn sync_token_watch_fetches_only_the_delta_and_costs_less_than_etag_window() {
    let token_run = Run::start(true);
    let stored = token_run.tokens();
    assert_eq!(stored.len(), 1, "full sync stores the probed token");
    let token_requests = token_run.change_then_watch();

    let etag_run = Run::start(false);
    assert!(
        etag_run.tokens().is_empty(),
        "no token without sync-collection"
    );
    let etag_requests = etag_run.change_then_watch();

    // Token path: discovery, one delta REPORT, and a GET per changed event.
    assert_eq!(
        methods(&token_requests),
        ["PROPFIND", "REPORT", "GET", "GET", "GET"],
        "{token_requests:#?}"
    );
    let delta = report(&token_requests, "sync-collection");
    assert_eq!(delta.len(), 1);
    assert_eq!(
        delta[0]["responses"], 4,
        "3 changed + 1 deleted, nothing else"
    );
    assert!(report(&token_requests, "calendar-query").is_empty());
    assert_ne!(token_run.tokens(), stored, "token advances after the delta");

    // ETag fallback: near-window listing of every near event, plus GETs to
    // confirm the moved and deleted events that left the window.
    assert!(report(&etag_requests, "sync-collection").is_empty());
    let window = report(&etag_requests, "calendar-query");
    assert_eq!(window.len(), 1, "{etag_requests:#?}");
    assert!(window[0]["responses"].as_u64().unwrap() >= FILLER as u64);
    assert!(
        token_requests.len() < etag_requests.len(),
        "token {} vs etag {} requests",
        token_requests.len(),
        etag_requests.len()
    );
    // Discovery (Depth-1 PROPFIND) is identical for both; the REPORT payload
    // is where the delta saves: changed members only vs. every near event.
    let report_responses = |requests: &[Value]| {
        let reports = requests
            .iter()
            .filter(|request| request["method"] == "REPORT")
            .cloned()
            .collect::<Vec<_>>();
        responses(&reports)
    };
    assert!(report_responses(&token_requests) * 4 < report_responses(&etag_requests));

    // Both strategies converge on the same cache.
    let titles = token_run.titles();
    assert_eq!(titles, etag_run.titles());
    assert!(titles.contains(&"已修改".to_owned()));
    assert!(titles.contains(&"新事件".to_owned()));
    assert!(titles.contains(&"改到遠處".to_owned()));
    assert!(!titles.contains(&"專案同步".to_owned()));
    assert_eq!(titles.len(), 2 + FILLER);
}

#[test]
fn rejected_token_is_cleared_and_replaced_by_one_full_sync() {
    let run = Run::start(true);
    let full_before = run.metadata("sync.calendar.full_success_at");
    let (key, _) = run.tokens().remove(0);
    let db = rusqlite::Connection::open(run.client.join("database.sqlite3")).unwrap();
    // A token from another fixture run, as after a server-side reset.
    db.execute(
        "UPDATE metadata SET value = 'http://m2k.fixture/sync/deadbeef/1' WHERE key = ?1",
        [&key],
    )
    .unwrap();
    drop(db);
    run.put("filler-0", "重設後修改", 3);
    let before = run.http_lines().len();
    run.watch_once();
    let requests = run.http_lines().split_off(before);
    let delta = report(&requests, "sync-collection");
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0]["status"], "403 Forbidden");
    let full = report(&requests, "calendar-query");
    assert_eq!(full.len(), 1, "one full ETag listing: {requests:#?}");
    assert_eq!(
        full[0]["responses"].as_u64().unwrap(),
        2 + FILLER as u64,
        "full retention range, not the near window"
    );
    let tokens = run.tokens();
    assert_eq!(tokens.len(), 1);
    assert!(!tokens[0].1.contains("deadbeef"), "fresh token stored");
    assert_ne!(
        run.metadata("sync.calendar.full_success_at"),
        full_before,
        "fallback counts as the daily full sync"
    );
    assert!(run.titles().contains(&"重設後修改".to_owned()));
}

#[test]
fn untrustworthy_sync_collection_falls_back_to_the_near_window_in_the_same_tick() {
    // Fixture test hooks: 501, and a 207 multistatus without a sync-token.
    for (broken, status) in [
        ("http://m2k.fixture/broken/501", "501 Not Implemented"),
        ("http://m2k.fixture/broken/no-token", "207 Multi-Status"),
    ] {
        let run = Run::start(true);
        let (key, _) = run.tokens().remove(0);
        run.set_metadata(&key, broken);
        run.put("filler-0", "近區間仍更新", 3);
        let before = run.http_lines().len();
        let cycle = run.watch_once();
        assert!(cycle["results"][0]["error"].is_null(), "{cycle}");
        let requests = run.http_lines().split_off(before);
        let delta = report(&requests, "sync-collection");
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0]["status"], status);
        let window = report(&requests, "calendar-query");
        assert_eq!(window.len(), 1, "near-window ETag refresh: {requests:#?}");
        assert!(run.titles().contains(&"近區間仍更新".to_owned()));
        assert!(run.tokens().is_empty(), "untrusted token is dropped");
    }
}

#[test]
fn delta_is_not_trusted_once_the_near_window_check_is_an_hour_old() {
    let run = Run::start(true);
    let stored = run.tokens();
    let stale = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
    run.set_metadata("sync.calendar.window_success_at", &stale);
    let before = run.http_lines().len();
    run.watch_once();
    let requests = run.http_lines().split_off(before);
    assert!(report(&requests, "sync-collection").is_empty());
    assert_eq!(report(&requests, "calendar-query").len(), 1);
    assert_eq!(run.tokens(), stored, "window check keeps the token");
    assert_ne!(
        run.metadata("sync.calendar.window_success_at"),
        [("sync.calendar.window_success_at".to_owned(), stale)]
    );
    // The next tick, within the hour, trusts the delta again.
    let before = run.http_lines().len();
    run.watch_once();
    let requests = run.http_lines().split_off(before);
    assert_eq!(report(&requests, "sync-collection").len(), 1);
    assert!(report(&requests, "calendar-query").is_empty());
}
