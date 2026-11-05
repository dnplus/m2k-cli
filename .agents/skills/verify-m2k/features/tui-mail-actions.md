# TUI mail actions

On the mail tab a user toggles seen/flagged (`s`/`f`), moves (`m`) or deletes (`x`) the selected mail behind a confirmation screen, writes a new mail (`c`) or forwards the opened one (`F`) in the reply composer, and saves an attachment from the opened mail (`a`). Each write goes through the same function as `m2k mail flag|move|delete|send|forward|attachment` (`src/mail_action.rs`).

## Sub-features

- `tui-mail-flag` `s` toggles `\Seen`, `f` toggles `\Flagged` on the selected row in the background; on success the row marker (`o` unread, `*` flagged) and the status line say what changed.
- `tui-mail-move` `m` opens 移動郵件 (type a folder, Tab cycles known local folders), Enter shows 確認移動 with the subject, source and target folder; only `y` moves, Esc cancels with `已取消，沒有變更`.
- `tui-mail-delete` `x` shows 確認刪除 with the subject and folder; only `y` deletes (server Trash, else permanent), Esc cancels.
- `tui-mail-compose` `c` (new mail, `To:`/`Cc:`/`Subject:`) and `F` in an opened mail (forward, `To:` and an optional introduction) reuse the reply composer and confirmation: `y` send, `d` save draft, `e` edit, Esc saves, Ctrl-C saves and quits; drafts are `drafts/mail-*.txt` and `drafts/forward-*.txt`. A send is never retried.
- `tui-mail-attachment` `a` in an opened mail lists attachments (↑/↓), the target directory defaults to `~/Downloads` and is editable; Enter saves `<dir>/<file name>`, an existing file is refused and left untouched, success reports the saved path.
- `tui-mail-demo` demo data refuses every online write: flag/move/delete end with `<動作>失敗：展示資料不連線；請指定正式帳號的 --data-dir`, compose `y` says `展示資料只能存草稿，不會寄出`. Attachment saving is local and works on demo data.

## How to get to it (user POV)

- `m2k tui` → `1` mail tab → ↑/↓ to a mail → `s` / `f` / `m` / `x` / `c`.
- Enter on a mail (wait for the body) → `F` / `a`.

## Driving it with m2kv

Preconditions: baseline; `tmux set-environment -g -r EDITOR` before `start` so the built-in input is used.

### Move and delete: confirmation, cancel, demo refusal

- `m2kv tui "$RUN" start` (the selected row is 「本週產品同步會議」).
- `keys m`, `keys Archive`, `capture move-input`: 移動郵件, `目標資料夾：Archive_`, `（本機沒有其他資料夾…）` (demo has only INBOX).
- `keys Enter`, `capture move-confirm`: 確認移動, `把「本週產品同步會議」`, `從 INBOX 移到 Archive？`, `y 確認移動 · Esc 取消`.
- `keys Escape`, `capture move-cancelled`: status `已取消，沒有變更`.
- `keys x`, `capture delete-confirm`: 確認刪除, `刪除「本週產品同步會議」（INBOX）？`.
- `keys y`, `capture delete-refused` (re-capture until the status changes from `正在刪除…`): status `刪除失敗：展示資料不連線；請指定正式帳號的 --data-dir`; the row is still listed.
- `keys s`, `capture seen-refused`: `標為未讀失敗：展示資料不連線…` (demo-mail-2 is seen). Read back: `m2kv cli "$RUN" -- --json mail list` still has both mails.

### Attachment

- `keys j`, `keys Enter` (「歡迎使用 m2k 原生工作台」), `keys a`, `capture attach-open`: 儲存附件, `> welcome.txt`, `存到目錄：~/Downloads_`.
- Never save into the real `~/Downloads` here: `mkdir -p .tmp/verify/$RUN/attach`, send `BSpace` 11 times, then `keys "$PWD/.tmp/verify/$RUN/attach"`, `keys Enter`, `capture attach-saved`: status `附件已存到 …/attach/welcome.txt（23 bytes）`. `cat` it: `Offline attachment demo`.
- Repeat `a`, the same directory and Enter, `capture attach-no-overwrite`: the prompt stays with `無法建立檔案 …/welcome.txt；不覆寫既有檔案`.

### Forward and new mail (demo save-only)

- In the opened mail `keys Escape` (close the prompt), `keys F`, `keys bob@example.test`, `keys C-d`, `capture forward-confirm`: 確認轉寄, `To: bob@example.test`, `原信以 forwarded.eml 附件附上`, `展示資料只能存草稿`.
- `keys y`, `capture forward-refused`: notice `展示資料只能存草稿，不會寄出`. `keys d`: status `草稿已保存：…/drafts/forward-*.txt`.
- `keys c`, `keys bob@example.test Enter "Subject: 測試" Enter Enter 內文 C-d`, `capture new-confirm`: 確認新郵件, `Subject: 測試`, `內文`. `keys Escape`: status `草稿已保存：…/drafts/mail-*.txt`. `ls "$TMPDIR/m2k-verify/$RUN/data/drafts"`.
- `keys ?`, `capture help`: 操作說明 lists `F / 郵件 c`, `s / f`, `m / x`, `a`. `keys Escape`, `m2kv tui "$RUN" stop`.

### Real writes against the loopback fixture

Not through m2kv: `src/tui.rs` test `mail_actions_against_fixture_toggle_rows_send_move_and_delete` drives the keys against `examples/m2k_fixture` and checks `imap.store`, `smtp.message` and `imap.move` events. For a manual run, set up the client as in `examples/m2k_fixture/README.md` and start tmux yourself with `M2K_PASSWORD` and `M2K_TEST_ROOT_CERT` set.

## Gotchas

- Demo data proves only the confirmation screens, the composer, attachment saving and the refusal message. The real IMAP/SMTP effect of flag/move/delete/send/forward is NOT RUN here; it is covered by the fixture test, and the real Mail2000 server is NOT RUN at all.
- Refusals arrive from a background job; while the status still reads `正在…`, capture again.
- `s`/`f`/`m`/`x`/`c`/`a` act only on the mail tab; on the calendar tab `m`/`x` stay month view / delete event. `F` and `a` need the mail opened with Enter and its body loaded.
- The built-in composer only appends; for new mail type the `Subject:` header yourself after the recipient line, or use `$EDITOR`, which gets a `To:`/`Cc:`/`Subject:` template.
- Every action takes `sync.lock`; while a sync runs it fails with the busy message. Only one mail action runs at a time, and Ctrl-C waits for it.
- After a move or delete the row leaves the local list at once; press `r` to sync and see it in the target folder.
- Long paths are cut at the status line's width; read the saved file back with `ls`/`cat`.
