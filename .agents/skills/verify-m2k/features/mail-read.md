# Read mail

A user lists and searches cached mail, reads one message with quoted history stripped by default, reads the whole thread, and gets a clear error for an unknown ID.

## Sub-features

- `mail-list` lists cached mail, text and JSON.
- `mail-search` matches subject/body, including Chinese.
- `mail-show` prints only the new reply text; `--full` shows quoted history; `--raw` prints RFC822 source.
- `mail-attachment` writes an attachment to a file.
- `mail-thread` reads the latest message of a subject thread.
- `mail-not-found` returns `not_found` with exit 1.

## How to get to it (user POV)

- `m2k mail list`, `m2k mail search <q>`, `m2k mail show <ID> [--full|--raw]`, `m2k mail attachment <ID> --index N --output FILE`, `m2k mail thread --id <ID>`.
- TUI tab `1 郵件` (see tui-workbench.md).

## Driving it with m2kv

Preconditions: baseline.

- **List.** `m2kv cli "$RUN" -- --json mail list`. Exit 0; a JSON array with ids `demo-mail-1` and `demo-mail-2`.
- **Chinese search.** `m2kv cli "$RUN" -- mail search 產品`. Exit 0; one row, `demo-mail-2`, 本週產品同步會議. `mail search 不存在的字` prints only the header row.
- **Show.** `m2kv cli "$RUN" -- mail show demo-mail-2`. Exit 0; subject, sender 王小明, and body 「週三下午兩點討論本機同步進度。」
- **Quote stripping.** `m2kv cli "$RUN" -- mail show demo-mail-2` prints the new text and stops before `On Mon, 5 Jan 2026 … wrote:`; stdout must NOT contain 「被引用的歷史內容」. `m2kv cli "$RUN" -- mail show demo-mail-2 --full` exits 0 and DOES contain the `On … wrote:` line, the `>` lines and 「被引用的歷史內容」.
- **Raw.** `m2kv cli "$RUN" -- mail show demo-mail-2 --raw`. Exit 0; stdout starts with `From:` headers (`Message-ID: <demo-2@example.test>`) and ends with the quoted `>` lines. Subject and sender headers are MIME-encoded in raw.
- **Attachment.** `m2kv cli "$RUN" -- --json mail attachment demo-mail-1 --index 0 --output ".tmp/verify/$RUN/welcome.out.txt"`. Exit 0; JSON has `"original_filename": "welcome.txt"`, `"bytes": 23`. Read it back with `cat` on that path: `Offline attachment demo`.
- **Thread.** `m2kv cli "$RUN" -- --json mail thread --id demo-mail-2`. Exit 0; `"count": 1`.
- **Not found.** `m2kv cli "$RUN" -- --json mail show missing-id`. Exit 1; stdout `{"error":{"code":"not_found",...},"state":"error"}`.

## Gotchas

- `mail attachment` is offline: it extracts from the cached RFC822, so it works in demo. Only `demo-mail-1` has an attachment; `demo-mail-2` has none.
- Quote stripping looks at the cached text; the regression set is `tests/mail_read.rs`.
- Text list columns truncate with `…`. Use `--json` to assert full IDs or dates.
- `mail search` text output has a header row even with zero hits; check row count, not exit code.
