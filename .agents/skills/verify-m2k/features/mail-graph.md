# Mail graph

A user (usually an agent) asks "who have I been mailing and which threads wait on me?" and gets a deterministic summary of the last N days of cached mail: people ranked by message count and threads tagged `awaiting_me` / `awaiting_them` / `replied` / `fyi`, as Markdown or JSON. It is offline and calls no model, so the same cache gives the same output.

## Sub-features

- `graph-markdown` `mail graph` prints `# 郵件關係圖` with an account/range header, a `## 聯絡人` table and `## 信件串（新到舊）`.
- `graph-json` `--json mail graph` returns `me`, `since`, `until`, `days`, `person`, `message_count`, `people[]`, `threads[]`.
- `graph-person` `--person <email or name fragment>` keeps only threads involving that person; an unmatched value gives empty sections, not an error.
- `graph-automated` `config set --automated-senders <list>` marks matching senders `automated` (`（自動）` in Markdown) and turns their threads into `fyi`.
- `graph-days-range` `--days` outside 1–365 is refused.

## How to get to it (user POV)

- `m2k mail graph [--days 14] [--person ming] [--people 20] [--threads 30] [--excerpt-chars 280]`.
- `m2k --json mail graph …` for structured output.
- `m2k config set --automated-senders 'team,@vendor.example'` (replaces the whole list; `''` clears it).
- No TUI entry.

## Driving it with m2kv

Preconditions: baseline (the two demo mails are dated launch time, so the default 14-day window holds both).

- **Markdown.** `m2kv cli "$RUN" -- mail graph`. Exit 0; `- 郵件：2 封，聯絡人 2 位，信件串 2 個`; the table has `王小明 <ming@example.test>` and `team@example.test`, each `1 | 1 | 0 | 1`; threads `### 1. 本週產品同步會議〔待我回覆〕` (`郵件 ID：demo-mail-2`) and `### 2. 歡迎使用 m2k 原生工作台〔待我回覆〕` (`郵件 ID：demo-mail-1`).
- **JSON.** `m2kv cli "$RUN" -- --json mail graph`. Exit 0; `"message_count": 2`, `people[].email` = `ming@example.test` (`"name": "王小明"`) and `team@example.test` (`"name": null`), both `"automated": false`; both `threads[].status` are `awaiting_me`; `threads[].ids` are `["demo-mail-2"]` and `["demo-mail-1"]`, and each ID opens with `mail show`.
- **Person filter.** `m2kv cli "$RUN" -- mail graph --person ming`. Exit 0; `- 篩選：ming`, `郵件：1 封，聯絡人 1 位，信件串 1 個`, only 本週產品同步會議. `--person nobody` exits 0 with `郵件：0 封` and `（無）` under both sections.
- **Automated sender.** `m2kv cli "$RUN" -- config set --automated-senders team`, read back with `m2kv cli "$RUN" -- --json config show` (`"automated_senders": ["team"]`). Then `m2kv cli "$RUN" -- --json mail graph`: `team@example.test` has `"automated": true` and is listed after 王小明; the `demo-mail-1` thread is `"status": "fyi"`; `demo-mail-2` stays `awaiting_me`. Markdown shows `team@example.test（自動）`. Undo with `config set --automated-senders ''`.
- **Range guard.** `m2kv cli "$RUN" -- --json mail graph --days 0`. Exit 1; `operation_failed`, message `--days 必須介於 1 到 365`.

## Gotchas

- The demo seed has no sent mail, no `\Answered` flag, no multi-message thread, no auto-reply headers and no co-recipients, so `awaiting_them`, `replied`, header-based `fyi`, `from_me > 0`, thread merging by `In-Reply-To`/`References` and `often_with` cannot be driven here. Report those as NOT RUN in the demo; `tests/mail_graph.rs` covers them.
- `demo-mail-2` is `\Seen` but not answered, so it is `awaiting_me`; `seen` does not mean `replied`.
- Ranges print in Taipei time and move with launch time; assert counts, IDs and statuses, not the `範圍：` dates.
- `config set` prints the whole config as JSON even without `--json`; prove the setting with `config show`.
- `mail graph` reads only the local cache; it never syncs. On a real account, an empty graph can mean an unsynced cache, not a quiet inbox.
