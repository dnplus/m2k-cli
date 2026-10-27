use crate::{
    config::{self, Config},
    model::{Item, Kind, Snapshot, SyncResult},
    provider::{self, dav::CalendarChanges},
    store::{self, Store},
};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use std::{
    collections::{BTreeMap, HashSet},
    fs::{File, OpenOptions},
    path::Path,
};

pub fn lock(data_dir: &Path) -> Result<File> {
    std::fs::create_dir_all(data_dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(data_dir.join("sync.lock"))?;
    file.try_lock_exclusive().map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::Error::new(crate::error::Failure {
                code: "account_busy",
                message: "另一個同步或寫入程序正在執行，請稍後重試".into(),
                retryable: true,
            })
        } else {
            error.into()
        }
    })?;
    Ok(file)
}

pub fn run(data_dir: &Path, kinds: &[Kind]) -> Result<Vec<SyncResult>> {
    run_with_policy(data_dir, kinds, CalendarPolicy::Full)
}

pub fn run_periodic(data_dir: &Path, kinds: &[Kind]) -> Result<Vec<SyncResult>> {
    run_with_policy(data_dir, kinds, CalendarPolicy::Periodic)
}

enum CalendarPolicy {
    Full,
    Periodic,
}

fn reject_contacts(kinds: &[Kind]) -> Result<()> {
    if kinds.contains(&Kind::Contacts) {
        bail!("通訊錄僅支援手動同步；請執行 m2k directory sync --scope company");
    }
    Ok(())
}

fn calendar_window(center: DateTime<Utc>) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let start = center
        .checked_sub_signed(Duration::days(30))
        .context("Calendar 查詢起點超過可表示的日期範圍")?;
    let end = center
        .checked_add_signed(Duration::days(30))
        .context("Calendar 查詢終點超過可表示的日期範圍")?;
    Ok((start, end))
}

fn needs_full_calendar_sync(last_success: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match last_success {
        Some(last) if last <= now => now.signed_duration_since(last) >= Duration::hours(24),
        _ => true,
    }
}

fn merge_calendar_window(
    mut snapshot: Snapshot,
    existing: &[Item],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Snapshot> {
    if snapshot.kind != Kind::Calendar || start >= end {
        bail!("Calendar 區間快照或查詢範圍無效");
    }
    let collections = snapshot
        .collections
        .iter()
        .map(|collection| collection.id.as_str())
        .collect::<HashSet<_>>();
    let returned_ids = snapshot
        .items
        .iter()
        .map(|item| item.id.clone())
        .collect::<HashSet<_>>();
    snapshot.items.extend(
        existing
            .iter()
            .filter(|item| {
                item.kind == Kind::Calendar
                    && collections.contains(item.collection.as_str())
                    && !returned_ids.contains(&item.id)
                    && !provider::dav::known_nonrecurring_in_window(item, start, end)
            })
            .cloned(),
    );
    snapshot.items.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(snapshot)
}

struct CalendarOutcome {
    response: Result<Snapshot>,
    full: bool,
    /// The near window was checked with ETags (window or full sync).
    window_checked: bool,
    sync_tokens: Option<BTreeMap<String, String>>,
}

/// A sync-collection delta is only trusted between near-window ETag checks,
/// so a server that silently answers empty deltas delays near changes by at
/// most this long (plus one interval) instead of until the 24h full sync.
fn needs_window_check(last_success: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match last_success {
        Some(last) if last <= now => now.signed_duration_since(last) >= Duration::hours(1),
        _ => true,
    }
}

/// Picks the calendar strategy: a full retention sync (which also refreshes
/// RFC 6578 sync-tokens); a sync-collection delta when every collection has a
/// stored token and the near window was ETag-checked within the last hour;
/// or the near-window ETag refresh otherwise. A token rejected with
/// `valid-sync-token` is cleared and replaced by one full sync in the same
/// run; any other untrustworthy sync-collection answer clears the tokens and
/// falls back to the near window in the same run.
fn sync_calendar(
    store: &mut Store,
    cfg: &Config,
    password: &str,
    existing: &[Item],
    full: bool,
    now: DateTime<Utc>,
) -> Result<CalendarOutcome> {
    if !full {
        let stored = if needs_window_check(store.last_calendar_window_sync()?, now) {
            BTreeMap::new()
        } else {
            store.calendar_sync_tokens()?
        };
        let changes = if stored.is_empty() {
            Ok(CalendarChanges::Unavailable)
        } else {
            provider::dav::sync_calendar_changes(cfg, password, existing, &stored)
        };
        match changes {
            Ok(CalendarChanges::Synced(sync)) => {
                return Ok(CalendarOutcome {
                    response: Ok(sync.snapshot),
                    full: false,
                    window_checked: false,
                    sync_tokens: Some(sync.sync_tokens),
                })
            }
            Ok(CalendarChanges::TokenInvalid) => {
                store.set_calendar_sync_tokens(&BTreeMap::new())?;
            }
            Ok(changes @ (CalendarChanges::Unavailable | CalendarChanges::Unusable(_))) => {
                if matches!(changes, CalendarChanges::Unusable(_)) {
                    store.set_calendar_sync_tokens(&BTreeMap::new())?;
                }
                let (start, end) = calendar_window(now)?;
                return Ok(CalendarOutcome {
                    response: provider::dav::sync_calendar_window(
                        cfg, password, existing, start, end,
                    )
                    .and_then(|snapshot| merge_calendar_window(snapshot, existing, start, end)),
                    full: false,
                    window_checked: true,
                    sync_tokens: None,
                });
            }
            Err(error) => {
                return Ok(CalendarOutcome {
                    response: Err(error),
                    full: false,
                    window_checked: false,
                    sync_tokens: None,
                })
            }
        }
    }
    Ok(
        match provider::dav::sync_calendar_full(cfg, password, existing) {
            Ok(sync) => CalendarOutcome {
                response: Ok(sync.snapshot),
                full: true,
                window_checked: true,
                sync_tokens: Some(sync.sync_tokens),
            },
            Err(error) => CalendarOutcome {
                response: Err(error),
                full: true,
                window_checked: true,
                sync_tokens: None,
            },
        },
    )
}

fn finish(
    store: &mut Store,
    kind: Kind,
    response: Result<Snapshot>,
    password: &str,
    now: DateTime<Utc>,
) -> Result<SyncResult> {
    let result = match response {
        Ok(snapshot) => {
            if snapshot.kind != kind {
                bail!("同步快照種類與請求不一致");
            }
            match kind {
                Kind::Mail => store.apply_snapshot(&snapshot, now)?,
                Kind::Calendar => store.apply_collection_snapshot(&snapshot, now)?,
                Kind::Contacts => bail!("通訊錄不支援排程同步"),
            }
            SyncResult {
                kind,
                count: snapshot.items.len(),
                error: None,
            }
        }
        Err(error) => {
            let error = format!("{error:#}");
            SyncResult {
                kind,
                count: 0,
                error: Some(if password.is_empty() {
                    error
                } else {
                    error.replace(password, "[已隱藏]")
                }),
            }
        }
    };
    store.record_sync(&result, now)?;
    Ok(result)
}

fn run_with_policy(
    data_dir: &Path,
    kinds: &[Kind],
    calendar_policy: CalendarPolicy,
) -> Result<Vec<SyncResult>> {
    reject_contacts(kinds)?;
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    let _lock = lock(data_dir)?;
    let mut progress = crate::sync_progress::Reporter::start(data_dir)?;
    let outcome = (|| {
        if data_dir.join(".demo").exists() {
            bail!("展示資料不連線；請在另一個資料目錄執行 m2k init");
        }
        let cfg = config::load(data_dir)?;
        let password = config::password(data_dir, &cfg)?;
        let mut store = Store::open(data_dir)?;
        store.ensure_account(&cfg)?;
        let mut results = Vec::new();
        let mut seen = HashSet::new();
        let mut prunable = Vec::new();
        let mut full_calendar_succeeded = false;
        for &kind in kinds {
            if !seen.insert(kind) {
                continue;
            }
            progress.update("fetching", Some(kind), 0)?;
            let existing = store.list(kind, None, None, usize::MAX)?;
            let now = Utc::now();
            let mut full_calendar = kind == Kind::Calendar
                && (matches!(calendar_policy, CalendarPolicy::Full)
                    || needs_full_calendar_sync(store.last_full_calendar_sync()?, now));
            let mut sync_tokens = None;
            let mut window_checked = false;
            let response = match kind {
                Kind::Mail => provider::mail::sync(&cfg, &password, &existing)
                    .map(|snapshot| store::without_expired_mail(snapshot, &cfg, Utc::now())),
                Kind::Calendar => {
                    let outcome =
                        sync_calendar(&mut store, &cfg, &password, &existing, full_calendar, now)?;
                    full_calendar = outcome.full;
                    window_checked = outcome.window_checked;
                    sync_tokens = outcome.sync_tokens;
                    outcome.response
                }
                Kind::Contacts => unreachable!("contacts rejected before loading credentials"),
            };
            let result = finish(&mut store, kind, response, &password, Utc::now())?;
            if result.error.is_none() {
                if let Some(tokens) = &sync_tokens {
                    store.set_calendar_sync_tokens(tokens)?;
                }
                if window_checked {
                    store.mark_calendar_window_sync(Utc::now())?;
                }
                if kind == Kind::Mail || full_calendar {
                    prunable.push(kind);
                }
                full_calendar_succeeded |= full_calendar;
            }
            progress.update("saved", Some(kind), result.count)?;
            results.push(result);
        }
        store.prune_kinds(&cfg, Utc::now(), &prunable)?;
        if full_calendar_succeeded {
            store.mark_full_calendar_sync(Utc::now())?;
        }
        Ok(results)
    })();
    progress.finish(outcome.as_ref().is_ok_and(|results: &Vec<SyncResult>| {
        results.iter().all(|result| result.error.is_none())
    }))?;
    outcome
}

pub fn refresh_calendar(data_dir: &Path, center: DateTime<Utc>) -> Result<Vec<SyncResult>> {
    let (start, end) = calendar_window(center)?;
    let _lock = lock(data_dir)?;
    let mut progress = crate::sync_progress::Reporter::start(data_dir)?;
    progress.update("fetching", Some(Kind::Calendar), 0)?;
    let outcome = (|| {
        if data_dir.join(".demo").exists() {
            bail!("展示資料不連線；請在另一個資料目錄執行 m2k init");
        }
        let cfg = config::load(data_dir)?;
        let password = config::password(data_dir, &cfg)?;
        let mut store = Store::open(data_dir)?;
        store.ensure_account(&cfg)?;
        let existing = store.list(Kind::Calendar, None, None, usize::MAX)?;
        let response = provider::dav::sync_calendar_window(&cfg, &password, &existing, start, end)
            .and_then(|snapshot| merge_calendar_window(snapshot, &existing, start, end));
        Ok(vec![finish(
            &mut store,
            Kind::Calendar,
            response,
            &password,
            Utc::now(),
        )?])
    })();
    if let Ok(results) = &outcome {
        progress.update(
            "saved",
            Some(Kind::Calendar),
            results
                .first()
                .map_or(0, |result: &SyncResult| result.count),
        )?;
    }
    progress.finish(outcome.as_ref().is_ok_and(|results: &Vec<SyncResult>| {
        results.iter().all(|result| result.error.is_none())
    }))?;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Collection;
    use crate::provider::dav::known_nonrecurring_in_window;

    fn time(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn event(id: &str, start: &str, end: &str, extra: &str) -> Item {
        let raw = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{id}\r\n\
             DTSTART:{}\r\nDTEND:{}\r\n{extra}END:VEVENT\r\nEND:VCALENDAR\r\n",
            time(start).format("%Y%m%dT%H%M%SZ"),
            time(end).format("%Y%m%dT%H%M%SZ"),
        );
        Item {
            id: id.into(),
            kind: Kind::Calendar,
            collection: "known".into(),
            remote_id: format!("{id}.ics"),
            etag: Some("old".into()),
            uid_validity: None,
            title: id.into(),
            subtitle: String::new(),
            date: Some(start.into()),
            end_date: Some(end.into()),
            text: String::new(),
            raw,
            flags: Vec::new(),
            emails: Vec::new(),
            phones: Vec::new(),
            attachments: Vec::new(),
        }
    }

    fn snapshot(items: Vec<Item>) -> Snapshot {
        Snapshot {
            kind: Kind::Calendar,
            collections: vec![Collection {
                kind: Kind::Calendar,
                id: "known".into(),
                name: "Known calendar".into(),
            }],
            items,
        }
    }

    #[test]
    fn near_merge_keeps_outside_and_unknown_but_deletes_known_overlaps() {
        let start = time("2026-10-01T00:00:00Z");
        let end = time("2026-11-01T00:00:00Z");
        let before = event("before", "2026-09-01T09:00:00Z", "2026-09-01T10:00:00Z", "");
        let after = event("after", "2026-12-01T09:00:00Z", "2026-12-01T10:00:00Z", "");
        let inside = event("inside", "2026-10-07T09:00:00Z", "2026-10-07T10:00:00Z", "");
        let overlap = event(
            "overlap",
            "2026-09-30T23:00:00Z",
            "2026-10-01T01:00:00Z",
            "",
        );
        let mut unknown = inside.clone();
        unknown.id = "unknown".into();
        unknown.end_date = None;
        unknown.raw = unknown
            .raw
            .lines()
            .filter(|line| !line.starts_with("DTEND:"))
            .collect::<Vec<_>>()
            .join("\r\n")
            + "\r\n";
        let mut malformed = inside.clone();
        malformed.id = "malformed".into();
        malformed.raw = "malformed ICS".into();
        let merged = merge_calendar_window(
            snapshot(Vec::new()),
            &[before, after, inside, overlap, unknown, malformed],
            start,
            end,
        )
        .unwrap();
        assert_eq!(
            merged
                .items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["after", "before", "malformed", "unknown"]
        );
    }

    #[test]
    fn near_merge_uses_exclusive_window_boundaries() {
        let start = time("2026-10-01T00:00:00Z");
        let end = time("2026-11-01T00:00:00Z");
        let ending_at_start = event("ending", "2026-09-30T23:00:00Z", "2026-10-01T00:00:00Z", "");
        let starting_at_end = event(
            "starting",
            "2026-11-01T00:00:00Z",
            "2026-11-01T01:00:00Z",
            "",
        );
        let merged = merge_calendar_window(
            snapshot(Vec::new()),
            &[ending_at_start, starting_at_end],
            start,
            end,
        )
        .unwrap();
        assert_eq!(merged.items.len(), 2);
    }

    #[test]
    fn near_merge_preserves_all_recurrence_properties_and_multi_event_resources() {
        let start = time("2026-10-01T00:00:00Z");
        let end = time("2026-11-01T00:00:00Z");
        let properties = [
            "RRULE:FREQ=WEEKLY;\r\n COUNT=4\r\n",
            "RDATE;VALUE=DATE:20261008\r\n",
            "EXDATE:20261008T090000Z\r\n",
            "RECURRENCE-ID:20261007T090000Z\r\n",
        ];
        let mut existing = properties
            .iter()
            .enumerate()
            .map(|(index, property)| {
                event(
                    &format!("recurring-{index}"),
                    "2026-10-07T09:00:00Z",
                    "2026-10-07T10:00:00Z",
                    property,
                )
            })
            .collect::<Vec<_>>();
        let mut multiple = event(
            "multiple",
            "2026-10-07T09:00:00Z",
            "2026-10-07T10:00:00Z",
            "",
        );
        multiple.raw = multiple.raw.replace("END:VCALENDAR", "BEGIN:VEVENT\r\nUID:second\r\nDTSTART:20261201T090000Z\r\nDTEND:20261201T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR");
        existing.push(multiple);
        let merged = merge_calendar_window(snapshot(Vec::new()), &existing, start, end).unwrap();
        assert_eq!(merged.items.len(), existing.len());
    }

    #[test]
    fn returned_id_wins_and_unqueried_collection_is_not_replaced() {
        let center = time("2026-10-07T00:00:00Z");
        let (start, end) = calendar_window(center).unwrap();
        let old = event(
            "same-id",
            "2026-10-07T09:00:00Z",
            "2026-10-07T10:00:00Z",
            "",
        );
        let mut replacement = old.clone();
        replacement.title = "new title".into();
        replacement.etag = Some("new".into());
        let mut unqueried = old.clone();
        unqueried.id = "unqueried".into();
        unqueried.collection = "other".into();
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let mut initial = snapshot(vec![old.clone(), unqueried.clone()]);
        initial.collections.push(Collection {
            kind: Kind::Calendar,
            id: "other".into(),
            name: "Other".into(),
        });
        store.apply_collection_snapshot(&initial, center).unwrap();
        let merged =
            merge_calendar_window(snapshot(vec![replacement]), &[old, unqueried], start, end)
                .unwrap();
        assert_eq!(merged.items.len(), 1);
        store.apply_collection_snapshot(&merged, center).unwrap();
        let items = store.list(Kind::Calendar, None, None, usize::MAX).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items
                .iter()
                .find(|item| item.id == "same-id")
                .unwrap()
                .title,
            "new title"
        );
        assert!(items.iter().any(|item| item.id == "unqueried"));
    }

    #[test]
    fn recurrence_words_in_plain_description_do_not_prevent_authoritative_deletion() {
        let item = event(
            "plain",
            "2026-10-07T09:00:00Z",
            "2026-10-07T10:00:00Z",
            "DESCRIPTION:Example RRULE: plain text\r\n",
        );
        let (start, end) = calendar_window(time("2026-10-07T00:00:00Z")).unwrap();
        assert!(known_nonrecurring_in_window(&item, start, end));
    }

    #[test]
    fn full_sync_due_policy_is_deterministic_across_a_restart() {
        let now = time("2020-01-01T00:00:00Z");
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert!(needs_full_calendar_sync(
            store.last_full_calendar_sync().unwrap(),
            now
        ));
        store.mark_full_calendar_sync(now).unwrap();
        drop(store);
        let reopened = Store::open(dir.path()).unwrap();
        let stamp = reopened.last_full_calendar_sync().unwrap();
        assert!(!needs_full_calendar_sync(stamp, now + Duration::hours(23)));
        assert!(needs_full_calendar_sync(stamp, now + Duration::hours(24)));
        assert!(needs_full_calendar_sync(
            Some(now + Duration::minutes(1)),
            now
        ));
    }

    #[test]
    fn failed_full_response_records_error_without_advancing_persisted_stamp() {
        let previous = time("2020-01-01T00:00:00Z");
        let now = previous + Duration::days(1);
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.mark_full_calendar_sync(previous).unwrap();
        let result = finish(
            &mut store,
            Kind::Calendar,
            Err(anyhow::anyhow!("remote rejected secret")),
            "secret",
            now,
        )
        .unwrap();
        assert!(result.error.unwrap().contains("[已隱藏]"));
        assert_eq!(store.last_full_calendar_sync().unwrap(), Some(previous));
        assert!(store.sync_status().unwrap()["calendar"]["error"]
            .as_str()
            .unwrap()
            .contains("[已隱藏]"));
        drop(store);
        assert!(needs_full_calendar_sync(
            Store::open(dir.path())
                .unwrap()
                .last_full_calendar_sync()
                .unwrap(),
            now
        ));
    }

    #[test]
    fn successful_near_refresh_preserves_distant_cache_and_full_sync_stamp() {
        let previous = time("2020-01-01T00:00:00Z");
        let center = time("2026-10-07T00:00:00Z");
        let distant = event(
            "distant",
            "2026-01-01T09:00:00Z",
            "2026-01-01T10:00:00Z",
            "",
        );
        let deleted_near = event(
            "deleted-near",
            "2026-10-07T09:00:00Z",
            "2026-10-07T10:00:00Z",
            "",
        );
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let initial = snapshot(vec![distant, deleted_near]);
        store.apply_collection_snapshot(&initial, center).unwrap();
        store.mark_full_calendar_sync(previous).unwrap();
        let (start, end) = calendar_window(center).unwrap();
        let merged =
            merge_calendar_window(snapshot(Vec::new()), &initial.items, start, end).unwrap();
        let result = finish(&mut store, Kind::Calendar, Ok(merged), "", center).unwrap();
        assert!(result.error.is_none());
        let items = store.list(Kind::Calendar, None, None, usize::MAX).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "distant");
        assert_eq!(store.last_full_calendar_sync().unwrap(), Some(previous));
    }
}
