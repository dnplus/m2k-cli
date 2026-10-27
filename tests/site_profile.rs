use assert_cmd::Command;
use m2k::{
    config::{self, Config},
    site::SiteProfile,
};
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

// A keychain file that does not exist: m2k must never read or write the
// login keychain from tests.
const NO_KEYCHAIN: &str = "/nonexistent/m2k-test.keychain-db";

/// Rooms are listed from the profile without credentials: the `.demo` marker
/// makes `meeting resources` skip the password, so no request leaves the test.
fn offline_resources(config: &Config, extra: &[&str]) -> Value {
    let dir = TempDir::new().unwrap();
    config::save(dir.path(), config).unwrap();
    std::fs::write(dir.path().join(".demo"), "offline profile test\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args(["meeting", "resources"])
        .args(extra)
        .env_remove("M2K_PASSWORD")
        .env("M2K_KEYCHAIN", NO_KEYCHAIN)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn write_profile(dir: &Path, json: &str) -> String {
    let path = dir.join("profile.json");
    std::fs::write(&path, json).unwrap();
    path.to_string_lossy().into_owned()
}

fn install_user_profile(data_dir: &Path, name: &str) {
    let profiles = data_dir.join("site-profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/site-profile.example.json"
        ),
        profiles.join(format!("{name}.json")),
    )
    .unwrap();
}

fn m2k(data_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_m2k"));
    command
        .args(["--json", "--data-dir"])
        .arg(data_dir)
        .env_remove("M2K_PASSWORD");
    command
}

fn resource_names(data_dir: &Path, extra: &[&str]) -> Vec<String> {
    let output = m2k(data_dir)
        .args(["meeting", "resources"])
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    value["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|room| room["name"].as_str().unwrap().to_owned())
        .collect()
}

// Read config.json directly: user profile names resolve against the data dir
// the binary was started with, which this test process does not set.
fn saved_config(data_dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(data_dir.join("config.json")).unwrap()).unwrap()
}

/// A config.json as versions before `init` recorded the profile wrote it.
fn write_legacy_config(data_dir: &Path, site_profile: Option<&str>) {
    let mut config = serde_json::json!({
        "username": "alice",
        "email": "alice@example.test",
        "imap_host": "mail.example.test",
        "imap_port": 993,
        "smtp_host": "mail.example.test",
        "smtp_port": 587,
        "caldav_url": "https://mail.example.test/cgi-bin/cal/caldav/calendars/alice%40example.test/default/",
        "carddav_url": "",
        "sync_interval_minutes": 360,
        "mail_retention_days": 180,
        "calendar_retention_days": 365,
        "contacts_retention_days": 0
    });
    if let Some(name) = site_profile {
        config["site_profile"] = name.into();
    }
    let path = data_dir.join("config.json");
    std::fs::write(&path, config.to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

// Site-specific profiles (with real room access names) are not compiled in.
// An existing install keeps working once its profile file is copied to
// `<data-dir>/site-profiles/<name>.json`: configs that name the profile and
// configs that relied on server auto-selection both resolve to it unchanged.
#[test]
fn user_profile_in_data_dir_keeps_existing_configs_working() {
    let example = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/site-profile.example.json"
    ))
    .unwrap();
    let example = SiteProfile::parse(&example).unwrap();
    assert!(SiteProfile::builtin("example").is_none());

    let named = TempDir::new().unwrap();
    install_user_profile(named.path(), "example");
    write_legacy_config(named.path(), Some("example"));
    std::fs::write(named.path().join(".demo"), "offline profile test\n").unwrap();
    let all = resource_names(named.path(), &[]);
    assert_eq!(all, ["N-Board_12", "N-Focus_04", "S-Huddle_06"]);
    assert_eq!(
        resource_names(named.path(), &["--site", "South"]),
        ["S-Huddle_06"]
    );
    let shown = m2k(named.path())
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let shown: Value = serde_json::from_slice(&shown).unwrap();
    assert_eq!(
        shown["site_profile"], "example",
        "the stored name is kept, not rewritten to a path"
    );

    // Configs from older versions have no site_profile: auto-selected by
    // server from the user dir.
    let auto = TempDir::new().unwrap();
    install_user_profile(auto.path(), "example");
    write_legacy_config(auto.path(), None);
    std::fs::write(auto.path().join(".demo"), "offline profile test\n").unwrap();
    assert_eq!(resource_names(auto.path(), &[]), all);

    // New inits record what auto-selection picked, so the choice is visible
    // and no longer depends on the server name later.
    let recorded = TempDir::new().unwrap();
    install_user_profile(recorded.path(), "example");
    m2k(recorded.path())
        .args(["init", "--username", "bob", "--server", "mail.example.test"])
        .assert()
        .success();
    assert_eq!(saved_config(recorded.path())["site_profile"], "example");
    let generic = TempDir::new().unwrap();
    m2k(generic.path())
        .args(["init", "--username", "bob", "--server", "mail.example.test"])
        .assert()
        .success();
    assert_eq!(saved_config(generic.path())["site_profile"], "mail2000");

    // `--site-profile NAME` stores the name so the file can be updated in place.
    let by_name = TempDir::new().unwrap();
    install_user_profile(by_name.path(), "example");
    m2k(by_name.path())
        .args(["init", "--username", "carol", "--site-profile", "example"])
        .assert()
        .success();
    let saved = saved_config(by_name.path());
    assert_eq!(saved["site_profile"], "example");
    assert_eq!(saved["imap_host"], example.server.as_deref().unwrap());
}

#[test]
fn missing_user_profile_names_the_expected_location() {
    let dir = TempDir::new().unwrap();
    let output = m2k(dir.path())
        .args(["init", "--username", "alice", "--site-profile", "gone"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("找不到 site profile：gone"), "{text}");
    assert!(text.contains("site-profiles"), "{text}");
    assert!(!dir.path().join("config.json").exists());

    // An upgraded install whose profile file was not copied yet must not
    // silently list zero rooms.
    install_user_profile(dir.path(), "gone");
    m2k(dir.path())
        .args(["init", "--username", "alice", "--site-profile", "gone"])
        .assert()
        .success();
    std::fs::remove_dir_all(dir.path().join("site-profiles")).unwrap();
    std::fs::write(dir.path().join(".demo"), "offline profile test\n").unwrap();
    let output = m2k(dir.path())
        .args(["meeting", "resources"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("找不到 site profile：gone"), "{text}");
}

// Another Mail2000 installation only needs a profile file: every endpoint and
// room URL is derived from it and from the account, never from another site's defaults.
#[test]
fn fictional_site_profile_drives_every_endpoint() {
    let dir = TempDir::new().unwrap();
    let profile = write_profile(
        dir.path(),
        r#"{
          "name": "example",
          "server": "mail.example.test",
          "company_directory_root": "EXAMPLE_ALL",
          "sites": [
            { "name": "North", "prefixes": ["N-"] },
            { "name": "South", "prefixes": ["S-"] }
          ],
          "default_site": "North",
          "room_capacity_from_name_suffix": true,
          "rooms": [
            { "name": "N-Board_12", "access": "nboard" },
            { "name": "S-Huddle_04", "access": "shuddle" },
            { "name": "Lobby", "access": "lobby" }
          ]
        }"#,
    );
    let config = Config::with_site_profile("alice".into(), "", Some(profile.clone())).unwrap();
    assert_eq!(config.email, "alice@example.test");
    assert_eq!(config.imap_host, "mail.example.test");
    assert_eq!(
        config.caldav_url,
        "https://mail.example.test/cgi-bin/cal/caldav/calendars/alice%40example.test/default/"
    );
    assert_eq!(
        config.carddav_url,
        "https://mail.example.test/cgi-bin/carddav/principals/mPA.000@example.test"
    );

    let resources = offline_resources(&config, &[]);
    let rooms = resources["resources"].as_array().unwrap();
    let summary = rooms
        .iter()
        .map(|room| {
            (
                room["name"].as_str().unwrap(),
                room["site"].as_str().unwrap(),
                room["capacity"].as_u64(),
                room["calendar_url"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            (
                "N-Board_12",
                "North",
                Some(12),
                "https://mail.example.test/cgi-bin/cal/caldav/calendars/%40example.test/nboard"
            ),
            (
                "S-Huddle_04",
                "South",
                Some(4),
                "https://mail.example.test/cgi-bin/cal/caldav/calendars/%40example.test/shuddle"
            ),
            (
                "Lobby",
                "North",
                None,
                "https://mail.example.test/cgi-bin/cal/caldav/calendars/%40example.test/lobby"
            ),
        ]
    );
    let south = offline_resources(&config, &["--site", "South"]);
    assert_eq!(south["resources"].as_array().unwrap().len(), 1);

    let profile = SiteProfile::for_config(Some(&config)).unwrap();
    assert_eq!(
        profile.validate_site(Some("北區")).unwrap_err().to_string(),
        "site 必須是North或South"
    );
}

#[test]
fn carddav_principal_follows_the_account_domain() {
    for (username, server, expected) in [
        (
            "bob@example.test",
            "mail.example.test",
            "https://mail.example.test/cgi-bin/carddav/principals/mPA.000@example.test",
        ),
        (
            "carol",
            "https://mail.corp.example:9443",
            "https://mail.corp.example:9443/cgi-bin/carddav/principals/mPA.000@corp.example",
        ),
    ] {
        let config = Config::new(username.into(), server).unwrap();
        assert_eq!(config.carddav_url, expected);
        assert_eq!(
            SiteProfile::for_config(Some(&config)).unwrap().name,
            "mail2000"
        );
    }
}

#[test]
fn init_requires_a_server_unless_the_profile_names_one() {
    let dir = TempDir::new().unwrap();
    Command::new(env!("CARGO_BIN_EXE_m2k"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["init", "--username", "alice"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--server"));
    assert!(!dir.path().join("config.json").exists());

    let profile_dir = TempDir::new().unwrap();
    let profile = write_profile(
        profile_dir.path(),
        r#"{"name":"example","server":"mail.example.test"}"#,
    );
    Command::new(env!("CARGO_BIN_EXE_m2k"))
        .args(["--json", "--data-dir"])
        .arg(dir.path())
        .args(["init", "--username", "alice", "--site-profile", &profile])
        .assert()
        .success();
    let saved = config::load(dir.path()).unwrap();
    assert_eq!(saved.email, "alice@example.test");
    let stored = std::fs::canonicalize(&profile).unwrap();
    assert_eq!(
        saved.site_profile.as_deref(),
        Some(stored.to_string_lossy().as_ref()),
        "relative or symlinked paths are stored absolute so cwd does not matter"
    );
}

#[test]
fn invalid_profile_is_rejected_when_config_is_loaded() {
    let dir = TempDir::new().unwrap();
    let profile = write_profile(
        dir.path(),
        r#"{"name":"bad","sites":[{"name":"A"}],"default_site":"B"}"#,
    );
    let error = Config::with_site_profile(
        "alice@example.test".into(),
        "mail.example.test",
        Some(profile),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("default_site"), "{error:#}");
    let error = Config::with_site_profile(
        "alice@example.test".into(),
        "mail.example.test",
        Some("no-such-profile".into()),
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("找不到 site profile"),
        "{error:#}"
    );
}

/// Doctor's JSON report; other checks (no database yet) may fail, so the exit
/// code is not asserted here.
fn doctor_report(data_dir: &Path) -> Value {
    let output = m2k(data_dir).arg("doctor").output().unwrap();
    serde_json::from_slice(&output.stdout).unwrap()
}

// The upgrade trap: a legacy config with no site_profile and no profile file
// falls back to the generic profile. That must be visible in doctor and in
// room listings instead of an empty, successful answer.
#[test]
fn legacy_config_without_profile_file_warns_with_migration_steps() {
    let dir = TempDir::new().unwrap();
    write_legacy_config(dir.path(), None);
    std::fs::write(dir.path().join(".demo"), "offline profile test\n").unwrap();

    let output = m2k(dir.path())
        .args(["meeting", "resources"])
        .assert()
        .success()
        .get_output()
        .clone();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["resources"].as_array().unwrap().len(), 0);
    let warning = value["warnings"][0].as_str().unwrap();
    assert!(warning.contains("site-profiles"), "{warning}");
    assert!(warning.contains("從內建站台 profile 遷移"), "{warning}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("警告：") && stderr.contains("site-profiles"),
        "{stderr}"
    );

    let report = doctor_report(dir.path());
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "site_profile")
        .unwrap();
    assert_eq!(check["status"], "warn");
    assert!(check["detail"].as_str().unwrap().contains("site-profiles"));

    // Once the profile file is in place both warnings disappear.
    install_user_profile(dir.path(), "example");
    let value: Value = serde_json::from_slice(
        &m2k(dir.path())
            .args(["meeting", "resources"])
            .assert()
            .success()
            .get_output()
            .stdout,
    )
    .unwrap();
    assert!(value.get("warnings").is_none(), "{value}");
    let report = doctor_report(dir.path());
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "site_profile")
        .unwrap();
    assert_eq!(check["status"], "pass");
    assert!(check["detail"].as_str().unwrap().starts_with("example"));
}

// A bare name only ever means a user profile: a same-named file in the
// working directory must not be picked up silently.
#[test]
fn bare_name_never_reads_the_working_directory() {
    let dir = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/site-profile.example.json"
        ),
        cwd.path().join("example"),
    )
    .unwrap();
    let output = m2k(dir.path())
        .current_dir(cwd.path())
        .args(["init", "--username", "alice", "--site-profile", "example"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&output).contains("找不到 site profile：example"));
}

#[test]
fn user_profile_shadowed_by_builtin_warns() {
    let dir = TempDir::new().unwrap();
    install_user_profile(dir.path(), "demo");
    let output = m2k(dir.path())
        .args([
            "init",
            "--username",
            "alice",
            "--server",
            "mail.example.test",
        ])
        .args(["--site-profile", "demo"])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8_lossy(&output);
    assert!(stderr.contains("與內建 site profile 同名"), "{stderr}");
}

// Without set_data_dir (library use, tests) only M2K_SITE_PROFILE_DIR can
// expose user profiles, so tests never read the real data directory.
#[test]
fn library_callers_see_user_profiles_only_through_the_override() {
    if std::env::var_os("M2K_SITE_PROFILE_DIR").is_none() {
        assert_eq!(m2k::site::user_profile_dir(), None);
    }
    let dir = TempDir::new().unwrap();
    install_user_profile(dir.path(), "example");
    let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
        .args(["--json", "--data-dir"])
        .arg(TempDir::new().unwrap().path())
        .args(["init", "--username", "alice", "--site-profile", "example"])
        .env("M2K_SITE_PROFILE_DIR", dir.path().join("site-profiles"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["config"]["imap_host"], "mail.example.test");
}
