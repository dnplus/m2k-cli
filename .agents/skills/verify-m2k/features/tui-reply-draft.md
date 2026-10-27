# TUI reply draft

A user opens a mail in the TUI, presses `R`, writes a reply in `$EDITOR` (or the built-in input when `$EDITOR` is unset), confirms, and either sends it through the same path as `m2k mail reply` or only keeps the draft under `<data-dir>/drafts/`.

## Sub-features

- `tui-reply-open` `R` in an opened mail builds `To:` (Reply-To, else From), `Subject: Re: …` and the quoted original.
- `tui-reply-editor` `$EDITOR` runs on the draft file with the alternate screen left; the TUI is restored afterwards. Without `$EDITOR`, a built-in append-only input (Ctrl-D done, Esc saves and leaves).
- `tui-reply-confirm` `y` sends, `d` only saves, `e` edits again, Esc saves and leaves. Nothing is sent without `y`.
- `tui-reply-demo` demo data dirs can only save drafts; `y` is refused.
- `tui-reply-failure` a failed send shows the reason, keeps the draft file, and does not retry by itself.
- `tui-reply-ctrl-c` Ctrl-C saves the draft and quits with the terminal restored.

## How to get to it (user POV)

- `m2k tui` → `1` mail tab → Enter on a mail (wait for the body) → `R`.

## Driving it with m2kv

### Demo: save a draft (no send)

Preconditions: baseline; `EDITOR` unset in the tmux server (`tmux set-environment -g -r EDITOR`).

- `m2kv tui "$RUN" start`, `keys Enter`, `keys R`, `capture reply-open`. Title `回覆草稿 · 內建輸入`, `To: ming@example.test`, `Subject: Re: 本週產品同步會議`, lines starting `> `.
- `keys 週三見`, `keys C-d`, `capture reply-confirm`. Title `確認回覆`, the typed line, `（另附 N 行引用原文）`, `展示資料只能存草稿`.
- `keys y`, `capture reply-demo-refuse`. Still `確認回覆`; the notice says demo data only saves drafts.
- `keys d`. Status line `草稿已保存：…/drafts/reply-*.txt`. Read it back: `ls -l "$TMPDIR/m2k-verify/$RUN/data/drafts"` (mode `-rw-------`) and `cat` it.

### `$EDITOR`

Set a non-interactive editor before `start` (the tmux server hands its global env to new sessions):

```sh
tmux set-environment -g EDITOR /path/to/fake-editor.sh   # prints a marker, sleeps 2s, writes a reply into the file
```

`keys R`, then `capture editor-running` within 2 s: the editor's marker is on the normal screen (the TUI left the alternate screen). After it exits, `capture confirm-after-editor` shows `確認回覆` and the edited line. Reset with `tmux set-environment -g -r EDITOR`.

### Real send against the loopback fixture

Preconditions: `cargo run --offline --example m2k_fixture -- --data-dir $FX/server --imap-port P1 --smtp-port P2 --http-port P3` running; client dir initialised as in `examples/m2k_fixture/README.md` (`init`, `config set --imap-port --smtp-port`, `sync`). Not an m2kv demo dir: start tmux yourself with `M2K_PASSWORD=fixture-password M2K_TEST_ROOT_CERT=$FX/server/tls/ca.pem` set and the same `tui` command.

- Enter a mail, `R`, finish in the editor, `y`. Status `已寄出回覆 <id>`; the draft file is removed.
- Proof: `grep smtp.message $FX/server/events.jsonl` shows `Re: …` to the original sender; `smtp/0001-message.eml` has `In-Reply-To` equal to the original `Message-ID` (`m2k mail show ID --raw`).
- Failure: stop the fixture, repeat. `寄送失敗：…；草稿保留於 …`, the draft file stays, `events.jsonl` has no new line, and nothing is retried until `y` is pressed again.

### Ctrl-C

Run `m2k tui` from a wrapper script that prints `$?` and `stty -a` afterwards; press `R`, `x`, `C-d`, `C-c`. Expect exit 0, `icanon isig echo` set, and one more file in `drafts/`.

## Gotchas

- `R` needs the mail opened with Enter and its body loaded; otherwise the status line asks for that. On the contacts tab `R` is still the directory sync.
- The built-in input only appends at the end (below the quote); change recipients or subject through `$EDITOR`.
- The send path takes `sync.lock`; if a sync is running the send fails with the busy message and the draft stays.
- Ctrl-C while the editor runs is ignored by m2k and hits the editor; if it dies the TUI reports `編輯器異常結束` and keeps the draft.
- Demo data can never prove a real send; use the fixture run and report real Mail2000 server delivery as NOT RUN.
