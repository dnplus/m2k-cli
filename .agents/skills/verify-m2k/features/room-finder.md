# Room finder

A user asks which 北區 rooms are free at a given time, then books one from the TUI. Demo uses the fictional `demo` site profile (`src/site/demo.json`, sites 北區／中區／南區); the answer comes from the room calendars seeded into the local store: some rooms free, some busy, the rest unknown because nothing is cached for them.

## Sub-features

- `rooms-exact` `meeting rooms --start/--end` lists free rooms for an exact window.
- `rooms-busy` a room with an overlapping event is `busy` and is excluded.
- `rooms-unknown` rooms with no cached calendar are `unknown`, listed separately, never reported free.
- `rooms-minutes` `--minutes N` lists each room's longest free gap, so a partly busy room still appears with its free part.
- `rooms-tui` TUI tab `4 找會議室` runs the same query from a form.
- `rooms-tui-book` after the draft is saved, `b` runs the `meeting book --dry-run` live check in the background; only a pass opens the confirmation (room, Taipei start/end, subject, own and room calendars), `y` books through `meeting book`, Esc writes nothing. After a booking, `i` lists recipients and `y` sends through `meeting invite --send`. Demo refuses with the CLI reason.

## How to get to it (user POV)

- `m2k meeting rooms --site 北區 --capacity 16 --start <RFC3339+offset> --end <RFC3339+offset> [--minutes N] [--json]`.
- TUI: press `4`, fill the form, Enter. With minutes set, Enter on a gap row, fill subject and attendees, Enter saves the draft; then `b`, `y`, `i`, `y`.

## Driving it with m2kv

Preconditions: baseline. The seeded room events sit on **Taipei tomorrow**: 北區甲室 has only 14:00–15:00 (free at 10:00–11:00), 北區乙室 is busy 10:30–11:30. Compute the date at run time, never hard-code it:

```sh
D=$(python3 -c 'import datetime as d; print((d.datetime.now(d.timezone(d.timedelta(hours=8)))+d.timedelta(days=1)).date())')
```

- **Free room.** `m2kv cli "$RUN" -- meeting rooms --site 北區 --capacity 16 --start ${D}T10:00:00+08:00 --end ${D}T11:00:00+08:00`. Exit 0; `找到 1 間可用會議室`, row `北區甲室_16`, and a 查詢問題 line for `北區示範會議室甲_20` (`本機沒有此對象的行事曆快取`). 北區乙室 is not listed.
- **Busy and unknown, in JSON.** Same command with `--json` before `meeting`. `availability` has 北區甲室 `state: free`, 北區乙室 `state: busy`, 北區示範會議室甲 `state: unknown`; `calendars[].source` is `demo`.
- **Partial availability.** Add `--minutes 30` to the exact command. Exit 0; 北區甲室 spans 10:00–11:00 and 北區乙室 appears with 10:00–10:30 (the free part before its 10:30 event). Unknown rooms stay excluded.
- **TUI.** `m2kv tui "$RUN" start`, `keys 4`, then `keys C-u`, `keys '明天 10:00'`, `keys Tab C-u '明天 11:00'`, `keys Tab 北區`, `keys Tab C-u 16`, `keys Tab C-u` (minutes empty = exact query), `capture rooms-form`, `keys Enter`, `capture rooms-result`. The result panel titles `可用會議室 1 間`, lists `北區甲室_16`, and the status panel lists the unknown `北區示範會議室甲_20`. Leave with `keys Escape` then `m2kv tui "$RUN" stop`.

- **TUI booking, demo refusal.** From the TUI step above with minutes `30` and end `明天 12:00`: `keys Enter` (the result), `capture booking-form` shows `段內訂房 · 北區甲室_16`; `keys Tab Tab C-u 展示預約`, `keys Enter`, `capture draft-saved`: title `草稿已保存 · b 檢查並預約`, footer `b 檢查並預約`. `keys y` does nothing. `keys b`, `capture demo-refused`: title `無法預約 · 未寫入`, body `展示資料不提供會議室預約`, status `live 檢查未通過，未寫入；草稿保留`. `m2kv doctor` still counts `2 3 2`; the CLI `meeting book --file <draft>/invite.ics --url <room> --dry-run` exits 1 with the same reason.

**Fixture path (loopback fake Mail2000, tmux by hand).** m2kv only drives demo dirs. Start `examples/m2k_fixture`, init and sync a client as in `examples/m2k_fixture/README.md`, then give the client a user site profile so the room finder lists the fixture room: `<client>/site-profiles/fixture.json` = `{"name":"fixture","room_provider":"room-a@example.test","sites":[{"name":"總部","prefixes":["第一"]}],"rooms":[{"name":"第一會議室_8","access":"default"}],"room_capacity_from_name_suffix":true}` and `"site_profile": "fixture"` in `<client>/config.json`. Run the TUI with `tmux new-session -d -s m2k-rooms-fx -x 120 -y 32 -e M2K_PASSWORD=fixture-password -e M2K_TEST_ROOT_CERT=<fx>/tls/ca.pem -e M2K_KEYCHAIN=/nonexistent/m2k.keychain-db "$PWD/target/debug/m2k --data-dir <client> tui"` and save every `tmux capture-pane -p` into `.tmp/verify/$RUN/`.

- `4`, start `明天 13:00`, end `明天 17:00`, clear site and capacity, minutes `60`, Enter: two gaps for `第一會議室_8`, 13:00–14:00 and 15:00–17:00 (the seeded room booking is 14:00–15:00 Taipei).
- `Down`, Enter, `Tab Tab C-u 'TUI 預約驗收' Tab bob@example.test`, Enter: draft saved. `b`, wait: `確認預約 · y 寫入 · Esc 取消` with `live 檢查通過：會議室在這段時間仍空`, `會議室：第一會議室_8`, `開始：<明天> 15:00（Asia/Taipei）`, `結束：<明天> 16:00（Asia/Taipei）`, `主旨：TUI 預約驗收`, `個人行事曆（fixture@example.test）`, `會議室行事曆（http://127.0.0.1:<port>/…/room-a@example.test/default/）`. `<fx>/events.jsonl` does not exist yet. Escape: `已取消預約，未寫入任何行事曆；草稿保留`, still no events file.
- `b`, wait, `y`, wait: `已預約 · i 寄邀請`. `grep dav.put <fx>/events.jsonl` has exactly two lines, `…/fixture@example.test/default/<uid>.ics` and `…/room-a@example.test/default/<uid>.ics`; no `smtp.message`.
- `i`: `確認寄送邀請` listing `bob@example.test`. Escape: `已取消寄送邀請，未寄出`, no `smtp.message`. `i`, `y`, wait: `已寄送會議邀請給 bob@example.test` with a Message-ID; one `smtp.message` to `["bob@example.test"]` whose `text/calendar; method=REQUEST` part has `METHOD:REQUEST` and the draft UID.
- Stale snapshot: Escape back to the results, which still show 15:00–17:00 free. Enter on it, subject `撞期會議`, Enter, `b`: `無法預約` with `預約前 live recheck 發現會議室衝突，未寫入任何行事曆`; still two `dav.put`.
- `src/tui.rs` test `room_booking_against_fixture_checks_confirms_books_and_invites_once` runs the same journey automatically, including Ctrl-C during the booking.

## Gotchas

- The demo seed fills the room calendars only for 北區甲室 and 北區乙室. Any other room is `unknown`; that is the correct offline answer, not a failure.
- Demo room calendars are not time-limited, but the real cache has a 10 minute TTL and is not used in demo. Nothing here proves cache expiry or `--refresh`; those are covered by `tests/room_cache.rs`.
- Query windows must carry a timezone offset on the CLI (`2026-…T10:00:00+08:00`); the TUI accepts 人話 like `明天 10:00`.
- `q` typed in the form is text, not quit. Press `Escape` to return to mail before `m2kv tui … stop`, or `stop` reports "did not exit on q" and kills the session.
- The seeded room events also appear in calendar `list` and the TUI calendar tab, hence calendar count 3.
- Real sites get rooms and `--site` values from their own site profile (a user profile in `<data-dir>/site-profiles/`); demo never shows them.
- Booking and inviting are online: m2kv proves only the demo refusal; the real writes are proven on the loopback fixture. Online behavior against the real Mail2000 server is NOT RUN here.
- The fixture's room directory discovery answers 404 under the default `mail2000` profile, so without the user site profile above the room finder lists no rooms there.
- A partial booking (own calendar written, room write rejected) cannot be produced by the fixture; the message is covered by the `book.rs` unit test `partial_booking_names_what_was_written_what_failed_and_the_next_step`. It lists 已寫入／未完成 and the CLI remediation, and offers no rebook key.
- The booking on the fixture finishes in well under a second, so Ctrl-C by hand usually lands after it; the wait-for-result guard is proven by the unit test.
