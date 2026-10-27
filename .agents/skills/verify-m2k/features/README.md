# m2k verification map

This directory is the maintained source for verifying m2k's user-facing behavior. Read this index, then use the matching feature file as the recipe.

## Baseline preconditions

- `RUN=$(.agents/skills/verify-m2k/scripts/m2kv launch)` from the repo root, giving a fresh demo data dir.
- `m2kv doctor "$RUN"` prints `doctor: OK` and counts `2 3 2`.
- Demo seed: mail `demo-mail-1` 「歡迎使用 m2k 原生工作台」 and `demo-mail-2` 「本週產品同步會議」 from 王小明; event `demo-event-1` 「m2k 骨架驗收」; contacts 王小明 (`demo-contact-1`) and self 展示使用者. `demo-mail-2` carries RFC822 source plus quoted history; `demo-mail-1` carries the attachment `welcome.txt`. Two room events (`demo-room-a` 北區甲室 free, `demo-room-b` 北區乙室 busy 10:30–11:30, rooms from the fictional `demo` site profile) sit on **Taipei tomorrow**, which is the only reason calendar count is 3. Seed timestamps are launch time, so assert titles and IDs, not dates.
- Never drive a data dir the helper did not create.

## Driving conventions

- `m2kv` below means `.agents/skills/verify-m2k/scripts/m2kv`.
- CLI: `m2kv cli "$RUN" -- <m2k args>`. Use `--json` for assertions.
- TUI: `m2kv tui "$RUN" start|keys|capture|stop`. Wait on captured text, not fixed sleeps.
- Start each recipe from the baseline. A recipe that mutates (config, files) says how to read the result back.

## Proof and skip reporting

- CLI proof: the `NNN-cli.txt` file (command, exit code, stdout, stderr).
- TUI proof: `NNN-tui-*.txt` frames before and after the key that changed state.
- A mutation is proven by a second read-only command.
- Online behavior against the real Mail2000 server is NOT RUN in this harness. Report it as such; do not substitute the demo refusal for it.
- Report an unreachable path with the attempted command and the unmet precondition.

## Feature entry contract

Each file has an H1, one paragraph, and four H2s in order: `Sub-features`, `How to get to it (user POV)`, `Driving it with m2kv`, `Gotchas`.

## Features

- [Read mail](./mail-read.md) — list, search, show (new reply vs `--full`, `--raw`), attachment export, thread, not-found error.
- [Room finder](./room-finder.md) — `meeting rooms` free / busy / unknown from the offline room cache, CLI and TUI tab 4.
- [Meeting draft](./meeting-draft.md) — `meeting prepare` writes an ICS; `invite --dry-run` parses it without sending.
- [Meeting RSVP](./meeting-rsvp.md) — `meeting rsvp` METHOD:REPLY: demo guards via m2kv, dry-run and real SMTP send against the loopback fixture.
- [TUI workbench](./tui-workbench.md) — tabs, mail detail, contact search, clean exit.
- [TUI reply draft](./tui-reply-draft.md) — `R` in mail detail, `$EDITOR`/built-in input, confirm, demo save-only, fixture send.
- [Calendar create / edit / delete](./calendar-crud.md) — CLI `calendar create/update --title …`, TUI `n`/`e`/`x` with confirmation, guards, demo refusal, fixture writes and ETag conflict.
- [Config and retention](./config-retention.md) — `config set` persists across processes.
- [Offline guard and JSON errors](./offline-guard.md) — demo refuses online writes; JSON error contract.

## Not yet mapped

Need a real account or a loopback server the harness does not ship: `init`/`auth`, `sync`/`watch`, `mail send|reply|forward|flag|move|delete`, `mail cleanup-*`, `mail watch` webhook, `directory sync|tree`, `meeting free-busy|slots|review|book`, contacts `create|update|delete` (calendar writes are in calendar-crud.md via the fixture). Offline-capable but unmapped: `date-range`, `calendar|contacts export`, `meeting resources`, `skills install`, `prune`, TUI calendar views, and the TUI 找會議室 minutes-mode slot picker (the exact start/end query is mapped in room-finder.md).
