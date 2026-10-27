# TUI workbench

A user opens the terminal workbench, reads a mail, switches to calendar and contacts, searches a contact in Chinese, and quits with the terminal restored.

## Sub-features

- `tui-mail-detail` Enter opens the selected mail, Esc returns.
- `tui-tabs` keys `1` `2` `3` `4` switch tabs.
- `tui-contact-search` `/` + query + Enter filters contacts.
- `tui-quit` `q` exits with status 0.

## How to get to it (user POV)

- `m2k tui` (or `m2k demo` for demo data).

## Driving it with m2kv

Preconditions: baseline; nothing else is attached to tmux session `m2kv-$RUN`.

- **Open.** `m2kv tui "$RUN" start`. Prints `tui ready`. `m2kv tui "$RUN" capture mail` shows `郵件 · 全部 · 2 筆` and both demo subjects.
- **Read mail.** `m2kv tui "$RUN" keys Enter`, then `capture mail-detail`. The frame shows body text 「週三下午兩點討論本機同步進度」. `keys Escape` returns to the list.
- **Calendar.** `keys 2`, `capture calendar`. The frame shows `m2k 骨架驗收`.
- **Contact search.** `keys 3`, `keys /`, `keys 王小明`, `keys Enter`, `capture contacts-search`. The list shows 王小明 and 產品工程部.
- **Quit.** `m2kv tui "$RUN" stop`. Prints `tui exited cleanly`.

## Gotchas

- The first selected mail is the newest (`本週產品同步會議`), not `demo-mail-1`.
- Search applies after a 200 ms pause or on Enter; `keys` already waits 0.5 s.
- Calendar shows a 3-day window around today; the demo event is launch time + 1 day, so it is visible on a fresh run.
- Terminal size is fixed at 120×32. Width < 112 switches to single-pane layout and changes what a capture contains.
