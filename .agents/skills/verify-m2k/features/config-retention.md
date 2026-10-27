# Config and retention

A user changes the sync interval and retention days, and a later process sees the same settings.

## Sub-features

- `config-set` saves interval and retention values.
- `config-show` reads them back in a new process.

## How to get to it (user POV)

- `m2k config set --sync-interval-minutes N --mail-retention-days N --calendar-retention-days N --contacts-retention-days N`.
- `m2k config show`, `m2k status --json`.

## Driving it with m2kv

Preconditions: baseline (interval 360, mail retention 180).

- **Set.** `m2kv cli "$RUN" -- config set --sync-interval-minutes 30 --mail-retention-days 90`. Exit 0.
- **Read back.** `m2kv cli "$RUN" -- --json config show`. Exit 0; `"sync_interval_minutes": 30`, `"mail_retention_days": 90`, `"calendar_retention_days": 365` unchanged.
- **Same view from status.** `m2kv cli "$RUN" -- --json status`. `config` holds the same values.

## Gotchas

- `config set` prints the new config as JSON even without `--json`; assert from `config show`, which is the second process.
- `0` means keep forever, not zero days.
