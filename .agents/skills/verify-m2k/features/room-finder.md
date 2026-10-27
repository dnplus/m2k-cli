# Room finder

A user asks which 北區 rooms are free at a given time. Demo uses the fictional `demo` site profile (`src/site/demo.json`, sites 北區／中區／南區); the answer comes from the room calendars seeded into the local store: some rooms free, some busy, the rest unknown because nothing is cached for them.

## Sub-features

- `rooms-exact` `meeting rooms --start/--end` lists free rooms for an exact window.
- `rooms-busy` a room with an overlapping event is `busy` and is excluded.
- `rooms-unknown` rooms with no cached calendar are `unknown`, listed separately, never reported free.
- `rooms-minutes` `--minutes N` lists each room's longest free gap, so a partly busy room still appears with its free part.
- `rooms-tui` TUI tab `4 找會議室` runs the same query from a form.

## How to get to it (user POV)

- `m2k meeting rooms --site 北區 --capacity 16 --start <RFC3339+offset> --end <RFC3339+offset> [--minutes N] [--json]`.
- TUI: press `4`, fill the form, Enter.

## Driving it with m2kv

Preconditions: baseline. The seeded room events sit on **Taipei tomorrow**: 北區甲室 has only 14:00–15:00 (free at 10:00–11:00), 北區乙室 is busy 10:30–11:30. Compute the date at run time, never hard-code it:

```sh
D=$(python3 -c 'import datetime as d; print((d.datetime.now(d.timezone(d.timedelta(hours=8)))+d.timedelta(days=1)).date())')
```

- **Free room.** `m2kv cli "$RUN" -- meeting rooms --site 北區 --capacity 16 --start ${D}T10:00:00+08:00 --end ${D}T11:00:00+08:00`. Exit 0; `找到 1 間可用會議室`, row `北區甲室_16`, and a 查詢問題 line for `北區示範會議室甲_20` (`本機沒有此對象的行事曆快取`). 北區乙室 is not listed.
- **Busy and unknown, in JSON.** Same command with `--json` before `meeting`. `availability` has 北區甲室 `state: free`, 北區乙室 `state: busy`, 北區示範會議室甲 `state: unknown`; `calendars[].source` is `demo`.
- **Partial availability.** Add `--minutes 30` to the exact command. Exit 0; 北區甲室 spans 10:00–11:00 and 北區乙室 appears with 10:00–10:30 (the free part before its 10:30 event). Unknown rooms stay excluded.
- **TUI.** `m2kv tui "$RUN" start`, `keys 4`, then `keys C-u`, `keys '明天 10:00'`, `keys Tab C-u '明天 11:00'`, `keys Tab 北區`, `keys Tab C-u 16`, `keys Tab C-u` (minutes empty = exact query), `capture rooms-form`, `keys Enter`, `capture rooms-result`. The result panel titles `可用會議室 1 間`, lists `北區甲室_16`, and the status panel lists the unknown `北區示範會議室甲_20`. Leave with `keys Escape` then `m2kv tui "$RUN" stop`.

## Gotchas

- The demo seed fills the room calendars only for 北區甲室 and 北區乙室. Any other room is `unknown`; that is the correct offline answer, not a failure.
- Demo room calendars are not time-limited, but the real cache has a 10 minute TTL and is not used in demo. Nothing here proves cache expiry or `--refresh`; those are covered by `tests/room_cache.rs`.
- Query windows must carry a timezone offset on the CLI (`2026-…T10:00:00+08:00`); the TUI accepts 人話 like `明天 10:00`.
- `q` typed in the form is text, not quit. Press `Escape` to return to mail before `m2kv tui … stop`, or `stop` reports "did not exit on q" and kills the session.
- The seeded room events also appear in calendar `list` and the TUI calendar tab, hence calendar count 3.
- Real sites get rooms and `--site` values from their own site profile (a user profile in `<data-dir>/site-profiles/`); demo never shows them.
- Booking (`meeting book`) and live free-busy are online and NOT RUN here.
