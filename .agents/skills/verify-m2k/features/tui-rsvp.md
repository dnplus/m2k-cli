# TUI meeting RSVP

A user opens an invitation mail in the TUI, reads the invite summary above the body (title, organizer, Taipei start–end, location, their current PARTSTAT), presses `i`, picks accept / decline / tentative with an optional comment, and confirms with `y`. The send goes through `m2k::meeting_rsvp::respond`, the same function as `m2k meeting rsvp`, in the background; it never edits a calendar and is never retried.

## Sub-features

- `tui-rsvp-summary` an opened mail whose raw message has a `text/calendar` part shows `── 會議邀請 ──` with `主題：`, `主辦人：`, `時間：YYYY-MM-DD HH:MM – HH:MM（台北）`, `地點：` (`（未提供）` when absent), `我的回覆：<PARTSTAT>` (`未知` when you are not listed) and `按 i 回覆邀請（接受／拒絕／暫定）`. The detail footer lists `i 回覆邀請`.
- `tui-rsvp-prompt` `i` opens 回覆會議邀請: ← / → / Tab cycle `[接受]` `[拒絕]` `[暫定]`, typing fills `附註（COMMENT，可留空）`, Enter shows 確認回覆邀請 with `會議：「…」`, `寄給主辦人：…`, `回覆：<名稱>（PARTSTAT=…）`, `附註：…`. Only `y` sends; Esc says `已取消，沒有變更` and writes nothing.
- `tui-rsvp-guards` `i` on a mail without an invite only sets the status `這封郵件沒有會議邀請（text/calendar），沒有可回覆的內容`. Invites you cannot answer (you are the organizer, not an attendee, METHOD not REQUEST) say `無法回覆邀請：<CLI 的原因>` and open no prompt. Demo data refuses the send after `y` with `回覆邀請失敗：展示資料不提供實際寄信；可加上 --dry-run 預覽回覆`.
- `tui-rsvp-send` success status `已寄送 METHOD:REPLY（<PARTSTAT>）給 <organizer>；未修改本機或伺服器行事曆`; the detail stays open. Ctrl-C while `正在回覆邀請…` waits for the result.

## How to get to it (user POV)

- `m2k tui` → `1` mail tab → select the invite (or `/` search its subject) → Enter (wait for the body) → `i`.

## Driving it with m2kv

**Demo path (m2kv).** Preconditions: baseline. The demo seed has no invite mail, so m2kv proves only the no-invite guard and the help text.

- `m2kv tui "$RUN" start`, `keys Enter` (「本週產品同步會議」), `keys i`, `capture no-invite`: status `這封郵件沒有會議邀請（text/calendar），沒有可回覆的內容`; footer has `i 回覆邀請`.
- `keys ?`, `capture help`: `郵件詳情內 i           回覆會議邀請：接受／拒絕／暫定（確認畫面 y 才寄出）`. `keys Escape`, `m2kv tui "$RUN" stop`.

**Fixture path (loopback fake Mail2000, real SMTP, tmux by hand).** m2kv only drives demo dirs. Start `examples/m2k_fixture` and init/sync a client as in `meeting-rsvp.md`, then `tmux new-session -d -s m2k-rsvp-fx -x 120 -y 32 -e M2K_PASSWORD=fixture-password -e M2K_TEST_ROOT_CERT=<fx>/tls/ca.pem -e M2K_KEYCHAIN=/nonexistent/m2k.keychain-db "$PWD/target/debug/m2k --data-dir <client> tui"`, and save every `tmux capture-pane -p` into `.tmp/verify/$RUN/`.

- `/`, `季度`, Enter, Enter, capture: the summary shows `主題：季度回顧會議`, `主辦人：carol@example.test`, `時間：<後天> 14:00 – 15:00（台北）`, `地點：（未提供）`, `我的回覆：NEEDS-ACTION`.
- `i`, `Right`, type `那天出差`, capture: `[拒絕]`, `附註（COMMENT，可留空）：那天出差_`. Enter, capture: 確認回覆邀請 with `回覆：拒絕（PARTSTAT=DECLINED）`. Escape: `已取消，沒有變更`; `<fx>/events.jsonl` does not exist yet.
- `i`, `Left`, Enter, `y`, capture until the status changes: `已寄送 METHOD:REPLY（TENTATIVE）給 carol@example.test；未修改本機或伺服器行事曆`.
- Read back: `grep smtp.message <fx>/events.jsonl` has one new line, `"to":["carol@example.test"]`, subject `暫定：季度回顧會議`; its `text/calendar; method=REPLY` part decodes to `METHOD:REPLY`, `UID:fixture-invite-1@example.test` and exactly one `ATTENDEE;PARTSTAT=TENTATIVE:mailto:fixture@example.test`.
- `src/tui.rs` test `rsvp_against_fixture_sends_one_reply_with_the_chosen_partstat` runs the same keys automatically, including Ctrl-C during the send.

## Gotchas

- Gap: the demo seed has no invite mail. Adding one would change the seed counts (2 3 2), `mail graph` threads and several recipes, so the summary, prompt, confirmation and demo refusal are covered by `src/tui.rs` tests that upsert an invite into a demo dir (`invite_mail_shows_its_summary_…`, `demo_refuses_the_rsvp_send_…`) and by the fixture path, not by m2kv.
- `i` needs the mail opened with Enter and its body loaded, like `F` and `a`.
- The summary reads only the first VEVENT; a recurring series with overrides still replies to every instance you attend, exactly like the CLI.
- RSVP never changes your own calendar copy (`calendar_updated` is always false in the CLI); the TUI does not either. Use `e` on the calendar tab or `calendar update` if you want the local PARTSTAT changed.
- Each `y` sends once. A second `i … y` sends a second REPLY; the organizer's client keeps the latest.
- On the loopback fixture the send finishes in well under a second, so Ctrl-C by hand usually lands after it; the wait-for-result guard is proven by the unit test.
- Online behavior against the real Mail2000 server is NOT RUN here.
