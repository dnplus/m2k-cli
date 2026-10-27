use chrono::{DateTime, Utc};
use m2k::{model::Kind, sync};

#[test]
fn periodic_contacts_are_rejected_before_config_credentials_network_or_directory_creation() {
    let dir = tempfile::tempdir().unwrap();
    let uninitialized = dir.path().join("not-created");
    let error = sync::run_periodic(
        &uninitialized,
        &[Kind::Mail, Kind::Calendar, Kind::Contacts],
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("m2k directory sync --scope company"));
    assert!(!uninitialized.exists());
}

#[test]
fn explicit_contacts_sync_uses_company_directory_hint() {
    let dir = tempfile::tempdir().unwrap();
    let uninitialized = dir.path().join("not-created");
    let error = sync::run(&uninitialized, &[Kind::Contacts]).unwrap_err();
    assert!(error.to_string().contains("--scope company"));
    assert!(!uninitialized.exists());
}

#[test]
fn empty_periodic_kinds_are_a_noop_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let uninitialized = dir.path().join("not-created");
    assert!(sync::run_periodic(&uninitialized, &[]).unwrap().is_empty());
    assert!(!uninitialized.exists());
}

#[test]
fn calendar_extreme_dates_are_rejected_without_acquiring_a_lock() {
    let dir = tempfile::tempdir().unwrap();
    let uninitialized = dir.path().join("not-created");
    for center in [DateTime::<Utc>::MIN_UTC, DateTime::<Utc>::MAX_UTC] {
        assert!(sync::refresh_calendar(&uninitialized, center).is_err());
        assert!(!uninitialized.exists());
    }
}

#[test]
fn calendar_demo_refresh_never_loads_credentials_or_connects() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".demo"), "").unwrap();
    let error = sync::refresh_calendar(dir.path(), Utc::now()).unwrap_err();
    assert!(error.to_string().contains("展示資料不連線"));
    assert!(!dir.path().join("database.sqlite3").exists());
}
