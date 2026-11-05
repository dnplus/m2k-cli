# Directory export and shared cache

One account syncs the company directory and exports it to a file; every other data dir sets `M2K_DIRECTORY_CACHE` to that file and gets the same people locally without a slow sync. Lookups (`directory search`, `resolve`, `self`, `chart`) import the file once per `generated_at`, treating each branch in it as an authoritative snapshot, so people missing from a newer file disappear. A missing or bad file only warns.

## Sub-features

- `dir-export` `directory export --output FILE [--addressbook SEL]` writes `m2k-directory/1` JSON atomically and prints `output`, `generated_at`, `collections`, `items`.
- `dir-export-default-root` without `--addressbook` it exports the site profile's `company_directory_root`; a profile without one is refused.
- `dir-cache-import` with `M2K_DIRECTORY_CACHE` set, the first lookup imports the file (stderr `已匯入共用通訊錄快取：N 筆聯絡人`) and replaces the branch's contacts.
- `dir-cache-once` a later lookup with the same file imports nothing and prints no notice.
- `dir-cache-bad-file` a missing file warns on stderr and the lookup still succeeds from the local cache.

## How to get to it (user POV)

- Exporting account: `m2k directory sync --scope company`, then `m2k directory export --output /shared/m2k-dir.json`.
- Every other data dir: `M2K_DIRECTORY_CACHE=/shared/m2k-dir.json m2k directory search 王小明` (or `resolve`, `self`, `chart`).

## Driving it with m2kv

Preconditions: baseline. `E=.tmp/verify/$RUN` (absolute path for the env var: `$PWD/$E`).

- **Default root refused.** `m2kv cli "$RUN" -- --json directory export --output "$E/dir.json"`. Exit 1; `operation_failed`, message `站台設定沒有公司通訊錄根；請以 --addressbook 指定分支`; no `$E/dir.json`.
- **Export a branch.** `m2kv cli "$RUN" -- --json directory export --addressbook demo-addressbook --output "$E/dir.json"`. Exit 0; `"collections": 1`, `"items": 2`. Read the file back: `"format":"m2k-directory/1"`, the same `generated_at`, items `demo-contact-1` (王小明) and `demo-self`, collection `demo-addressbook`, no mail or calendar items.
- **Simulate a newer export.** Stand-in for a later export from the syncing account (the demo cannot change contacts offline): copy `$E/dir.json` to `$E/dir-v2.json` with `python3`, setting `generated_at` to `2099-01-01T00:00:00Z`, removing the `demo-contact-1` item, and adding a copy of `demo-self` with `id`/`remote_id` `demo-contact-3`, `title` `李小華`, `emails` `["hua@example.test"]`, and a matching `raw` vCard (`UID:demo-contact-3`, `FN:李小華`).
- **Import.** `M2K_DIRECTORY_CACHE=$PWD/$E/dir-v2.json m2kv cli "$RUN" -- directory chart`. Exit 0; stderr `已匯入共用通訊錄快取：2 筆聯絡人`; the department lists `李小華 · hua@example.test` and `展示使用者`, not 王小明. The evidence file starts with the `env: M2K_DIRECTORY_CACHE=…` line.
- **Once per file.** Repeat the same command. Exit 0, same chart, no import line on stderr.
- **Removal persisted.** Without the variable: `m2kv cli "$RUN" -- --json directory search 王小明` prints `[]`; `m2kv cli "$RUN" -- --json contacts list` holds `demo-contact-3` and `demo-self`. `doctor` contacts count stays 2.
- **Bad file.** `M2K_DIRECTORY_CACHE=/nonexistent/x.json m2kv cli "$RUN" -- --json directory search 李小華`. Exit 0; stderr `警告：無法匯入共用通訊錄快取 /nonexistent/x.json：…；改用本機快取`; stdout still has `uid:demo-contact-3`.

## Gotchas

- The demo profile has no `company_directory_root`, so the default export path is only provable as a refusal; exporting the real company root is NOT RUN here (`tests/directory.rs` covers it).
- The import is a hand-edited file because demo contacts cannot change offline. It proves import, once-per-`generated_at` and authoritative removal; it does not prove a real `directory sync --scope company` (online, NOT RUN).
- Import compares `generated_at` with the last imported one; reusing an older or equal timestamp imports nothing. Use a later timestamp for each new stand-in file.
- Import mutates the run's contacts. Run this recipe last, or launch a fresh run for other contact or directory recipes.
- Give `M2K_DIRECTORY_CACHE` an absolute path; the notice and warning go to stderr, which `m2kv cli` records under `--- stderr`.
