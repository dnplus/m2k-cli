---
name: verify-m2k
description: Drive the real m2k binary (Mail2000 Rust CLI + TUI) the way a user does and capture evidence — CLI commands with exit codes and the tmux-driven TUI — against an isolated demo data dir. Use when proving a change to m2k's mail, calendar, contacts, meeting, config, JSON-error or TUI behavior works in the built app, not just in `cargo test`.
---

# Verify m2k

m2k is a short-lived CLI plus a full-screen TUI over a local SQLite data dir. Verification builds the debug binary once, seeds a fresh **demo** data dir per run, then drives each command or a tmux-hosted TUI against that dir. Demo data is fictional and every online path (IMAP/SMTP/CalDAV/CardDAV) refuses to run against it, so nothing here can touch a real mailbox.

All steps go through one helper, run from the repo root:

```sh
.agents/skills/verify-m2k/scripts/m2kv <subcommand> ...
```

## Launch

```sh
RUN=$(.agents/skills/verify-m2k/scripts/m2kv launch)
```

Builds `target/debug/m2k` (`cargo build --offline --locked`), creates `$TMPDIR/m2k-verify/$RUN/data`, runs `m2k demo --no-tui` into it, and prints the run id. Ready means `doctor` passes. There is no server to keep alive: each `cli` call is its own process; the TUI runs in a tmux session named `m2kv-$RUN`.

Each run has its own data dir, so runs can go side by side. Never point the helper at the user's real data dir (`~/Library/Application Support/m2k`) — it only accepts dirs it created.

## Doctor

```sh
.agents/skills/verify-m2k/scripts/m2kv doctor "$RUN"
```

Read-only. Checks the binary's build commit contains `git rev-parse --short=12 HEAD` (a `-dirty` suffix is expected with uncommitted edits), the data dir is a demo dir, and seed counts. A fresh demo seed is mail 2 / calendar 3 (the personal event plus two seeded room events) / contacts 2; anything else prints `WARN` (an earlier mutation in this run). Run it first, and again whenever output looks wrong.

## Drive

CLI — every argument after `--` goes to `m2k --data-dir <run>`:

```sh
.agents/skills/verify-m2k/scripts/m2kv cli "$RUN" -- --json mail list
```

Prefer `--json` for assertions; text lists are fixed-width and truncate with `…`. Errors under `--json` are a single object `{"state":"error","error":{"code":...}}` with a non-zero exit.

TUI — tmux, 120×32:

```sh
.agents/skills/verify-m2k/scripts/m2kv tui "$RUN" start          # waits for the tab bar "1 郵件"
.agents/skills/verify-m2k/scripts/m2kv tui "$RUN" keys 2         # tmux send-keys syntax: Enter, Escape, C-c, /
.agents/skills/verify-m2k/scripts/m2kv tui "$RUN" capture calendar
.agents/skills/verify-m2k/scripts/m2kv tui "$RUN" stop           # sends q, fails if the TUI does not exit
```

Stable handles are the tab labels `1 郵件` `2 行事曆` `3 聯絡人` `4 找會議室`, panel titles (`郵件 · 全部 · 2 筆`, `詳情預覽`), and demo record titles. Key bindings are in README.md's `## TUI` table. `m2k tui --snapshot` prints one frame without a TTY, but it does not exercise key handling; use tmux for user journeys.

The feature recipes live in [features/README.md](features/README.md). Read the index, then the feature file.

## Evidence

Everything lands in `.tmp/verify/$RUN/` (gitignored), numbered in run order:

- `000-launch.json` — the seeded status.
- `NNN-cli.txt` — the exact command, exit code, stdout, stderr.
- `NNN-tui-<name>.txt` — a captured TUI frame.

Proof standards:

- Drive the user's command or keys, not SQLite or test-only code.
- Capture the action and the resulting state. For a mutation (config set, prepare), read it back with a second command (`config show`, `cat` the file, `invite --dry-run`).
- A dry-run claims no side effect; prove it from its output (`"sent": false`) and from the absence of anything else changing.
- Online commands (`sync`, `mail send|flag|move|delete`, `meeting book`, `mail watch`, `auth`) cannot be proven here: against demo data they must fail with `operation_failed`, and that refusal is the only thing this harness verifies about them. Report their real behavior as **NOT RUN**, never as verified. Their protocol logic is covered by loopback fixtures in `tests/` (`cargo test --offline --locked`).

## Cleanup

```sh
.agents/skills/verify-m2k/scripts/m2kv cleanup "$RUN"
```

Kills only the `m2kv-$RUN` tmux session and deletes only `$TMPDIR/m2k-verify/$RUN`. Evidence in `.tmp/verify/$RUN/` survives. Run cleanup after failed attempts too.

## CI smoke

`scripts/smoke` is a non-interactive run of launch → doctor → `mail list` / `mail show missing-id` / `mail send` assertions → TUI `2` key → stop → cleanup; any failed step exits non-zero. It deliberately avoids seed-count assertions. `.github/workflows/ci.yml` runs it after `cargo test` on ubuntu (macos too on manual dispatch and tag pushes), runs `cargo fetch --locked` first (because `launch` builds with `--offline`; the helper is unchanged), and uploads `.tmp/verify/` as the `verify-evidence-<os>` artifact even on failure. CI needs tmux (installed if missing) and python3 (preinstalled on runners).

## Helpers

The skill lives in `.agents/skills/verify-m2k/` (generic agent path); `.claude/skills/verify-m2k` is a symlink to it for Claude Code. Edit only the `.agents` copy.

`scripts/m2kv` is the only helper; `m2kv` with no args prints usage. It needs `cargo`, `tmux`, `python3`, and `git` on PATH.
