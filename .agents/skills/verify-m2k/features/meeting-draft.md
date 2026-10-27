# Meeting draft

A user prepares a meeting invitation as an ICS file without sending it, then checks what `invite` would send with a dry-run.

## Sub-features

- `meeting-prepare` writes a METHOD:REQUEST ICS with a stable UID.
- `meeting-invite-dry-run` parses the ICS and reports `sent: false`.

## How to get to it (user POV)

- `m2k meeting prepare --title ... --start ... --end ... --attendee ... --output FILE`.
- `m2k meeting invite --file FILE --dry-run`.
- TUI 找會議室 → pick a slot → save draft (not mapped yet).

## Driving it with m2kv

Preconditions: baseline. `ICS=.tmp/verify/$RUN/draft.ics`.

- **Prepare.** `m2kv cli "$RUN" -- --json meeting prepare --title 驗證會議 --start 2026-10-21T10:00:00+08:00 --end 2026-10-21T11:00:00+08:00 --attendee ming@example.test --output "$ICS"`. Exit 0; JSON has `output`, a `uid` ending `@m2k`, and the attendee.
- **Read back the file.** `grep -E 'METHOD:REQUEST|SUMMARY|ATTENDEE' "$ICS"`. All three lines are present.
- **Dry-run.** `m2kv cli "$RUN" -- --json meeting invite --file "$ICS" --dry-run`. Exit 0; same `uid`, `"dry_run": true`, `"sent": false`, `"message_id": null`.
- **Real send refused.** `m2kv cli "$RUN" -- --json meeting invite --file "$ICS" --send`. Exit 1, `operation_failed` (demo data).

## Gotchas

- `invite` reports times in UTC (`02:00:00+00:00`) though input was +08:00; compare instants, not strings.
- Keep the ICS under `.tmp/verify/$RUN/` so it survives cleanup as evidence.
