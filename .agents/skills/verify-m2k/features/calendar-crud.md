# Calendar create / edit / delete

A user adds, edits or deletes a personal event without writing ICS: on the CLI with `calendar create --title --start --end` / `calendar update ID --title …`, or in the TUI calendar tab with `n` / `e` / `x`. Every write shows a confirmation first (TUI) and goes through the same ETag-conditional path as `calendar create|update|delete --file`; nothing is retried by itself.

## Sub-features

- `cal-crud-cli-create` `calendar create --title --start --end [--location --description --all-day]`; times are Taipei human input (`明天 14:00`, `下週一 下午兩點`, `2026-11-03 14:00`), a time-only `--end` uses the start day, all-day `--end` is the last day (inclusive).
- `cal-crud-cli-update` `calendar update ID --field …` changes only the named fields, keeps the UID and every other property; moving only `--start` keeps the length; `--location ''` removes it.
- `cal-crud-tui-form` `n` opens 新增事件 prefilled with the first free whole hour of the selected/visible day; `e` opens 編輯事件 prefilled from the event; `x` opens 確認刪除. Tab/↑↓ fields, Space toggles 全天, Enter previews, `y` writes, `e` back to the form, Esc back/cancel.
- `cal-crud-guards` recurring events (RRULE/RDATE/EXDATE/RECURRENCE-ID), events organised by someone else, self-organised meetings with other ATTENDEEs or a room (`meeting book`), and room/other people's calendars (also as `create --collection`) are refused with the reason on the status line (TUI) or as the error (CLI field update).
- `cal-crud-demo` demo data dirs reach the confirmation but `y` is refused; the CLI refuses with `operation_failed`.
- `cal-crud-conflict` a stale ETag (someone else changed the event) fails with 412; the TUI keeps the input, and after Esc → `r` → `e` brings back only the fields the user changed.
- `cal-crud-ctrl-c` Ctrl-C waits while a write is in flight; with unsaved input it saves `drafts/event-*.txt` and quits.

## How to get to it (user POV)

- CLI: `m2k calendar create --title '專案同步' --start '明天 14:00' --end '15:00'`, `m2k calendar update ID --location 'B 室'`, `m2k calendar delete ID`.
- TUI: `m2k tui` → `2` → `n`, or select an event with `j`/`k` → `e` / `x`.

## Driving it with m2kv

### Demo: form, confirmation, guards (no write)

Preconditions: baseline.

- `m2kv tui "$RUN" start`, `keys 2`, `keys n`, `capture create-form`. Title `新增事件`, 開始/結束 prefilled with a whole hour (`YYYY-MM-DD HH:00`), `> 標題` focused.
- `keys 驗收新增`, `keys Enter`, `capture create-confirm`. Title `確認新增`, the typed title and time, `展示資料不會寫入`.
- `keys y`, `capture create-demo-refuse`. Still `確認新增`, yellow notice `展示資料不會寫入行事曆`. `keys Escape` (back to the form), `keys Escape` again closes it.
- `keys g` selects the first event, `m2k 骨架驗收` on a fresh seed (it starts at launch time + 1 day, before the 10:30 and 14:00 room events unless launched after 10:30); check the detail pane, move with `j`/`k` if needed. `keys e`, `capture edit-form`: 標題 `m2k 骨架驗收`, times from the event. `keys Escape`.
- `keys G` selects the last event, `下午簡報` (北區甲室 room event); `keys e`, `capture room-refused`: status line `無法編輯：這是會議室或其他人的行事曆…`. Same with `x` → `無法刪除：…`.
- Read back: `m2kv cli "$RUN" -- --json calendar show demo-event-1` still has title `m2k 骨架驗收`; `doctor` counts stay `2 3 2`.
- CLI demo guard: `m2kv cli "$RUN" -- --json calendar create --title x --start '明天 14:00' --end '15:00'` → exit 1, `operation_failed`, message `展示資料不連線`.

### Real writes against the loopback fixture

Preconditions: `cargo run --offline --example m2k_fixture -- --data-dir $FX/server --http-port P` running; client dir initialised with `m2k --data-dir $FX/client init --username fixture@example.test --server http://127.0.0.1:P` and `sync --kind calendar` (`M2K_PASSWORD=fixture-password`). Not an m2kv demo dir: start tmux yourself with `M2K_PASSWORD` set, `m2k --data-dir $FX/client tui`.

- CLI: `calendar create --title 欄位建立 --start '2026-11-03 14:00' --end 15:30 --location '北區, 3F'` → JSON item. Proof: `grep dav.put $FX/server/events.jsonl` (`created: true`) and the `dav/*.ics` it points to has `DTSTART:20261103T060000Z`, `DTEND:20261103T073000Z`, `LOCATION:北區\, 3F`, no `METHOD`.
- `calendar update ID --start '2026-11-04 09:00'` → new `dav.put` with `created: false`, the same `uid`, end moved to 10:30.
- TUI: `2`, `n` (prefill uses the selected event's day, so with the fixture seed it is usually tomorrow), type a title with `tmux send-keys -l`, Enter, `y` → status `已新增「…」`, the event is selected; `e`, Tab×3, type a location, Enter, `y` → `已更新「…」`; `x`, `y` → `已刪除活動` and a `dav.delete` line.
- Booked meeting: `calendar create --file` an ICS with `ORGANIZER:mailto:<self>` and `ATTENDEE:mailto:room-a@example.test`; in the TUI select it and press `e` → status `無法編輯：這是有其他與會者或會議室（room-a@example.test）的會議…`, no PUT.
- Kept input: after a 412 on edit, Esc Esc → status `輸入已保留 · 按 r 更新後再按 e 帶回`; `r`, select the event, `e` → notice `已帶回上次未寫入的輸入…`, your title change is back while the other client's location stays; Enter, `y` writes it.
- Conflict: update the event from a second client dir (`calendar update ID --location B`), then in the first client `calendar update ID --title X` → exit 1 with `412`; no new `dav.put`. In the TUI the same gives `寫入失敗：… 412 …ETag 不符…` with the form kept and no `dav.delete`/`dav.put`; Esc, `r` (detail shows the other client's change), then `x`, `y` → `已刪除活動`.

## Gotchas

- Send each tmux key as its own `keys` call with a short pause: two Escapes in one send can reach crossterm as a single Alt-Esc, leaving the form open and turning later keys into typed text.

- Keys `n`/`e`/`x` only act on the calendar tab; on mail `R` is reply and `e`/`x` do nothing. `d`/`w`/`m`/`v` stay view switches; inside the form they are typed text.
- The form appends at the end of each field (no cursor); Backspace deletes from the end.
- The write takes `sync.lock`; if a sync is running the write fails with the busy message and the input stays.
- A `partial` receipt (remote accepted, local cache/ETag not confirmed) locks the TUI form: only Esc/Enter closes it; press `r` to reconcile. Never recreate the event.
- Real Mail2000 writes are NOT RUN here; the fixture is the evidence for the protocol path.
