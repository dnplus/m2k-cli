# Meeting RSVP

A user answers a meeting invitation that arrived by mail: `meeting rsvp` finds the `text/calendar` METHOD:REQUEST part, builds an RFC 5546 METHOD:REPLY (same UID, SEQUENCE and ORGANIZER; only the user as ATTENDEE with PARTSTAT) and mails it to the organizer. It never edits a calendar.

## Sub-features

- `meeting-rsvp-dry-run` prints the REPLY ICS and recipient, sends nothing.
- `meeting-rsvp-send` sends one SMTP message, `text/calendar; method=REPLY`, to the ORGANIZER.
- `meeting-rsvp-guards` refuse demo sending, mails without `text/calendar`, non-REQUEST methods, and invites that do not list the user.

## How to get to it (user POV)

- `m2k mail list` / `m2k mail search 會議` to find the invite ID.
- `m2k meeting rsvp ID --response accept|decline|tentative [--comment TEXT] [--dry-run]`.
- No TUI entry yet.

## Driving it with m2kv

**Demo path (m2kv, offline).** Preconditions: baseline. The demo seed has no invite, so this path only proves the guards.

- **Demo send refused.** `m2kv cli "$RUN" -- --json meeting rsvp demo-mail-2 --response accept`. Exit 1, `operation_failed`, message contains 「展示資料不提供實際寄信」.
- **Not an invite.** `m2kv cli "$RUN" -- --json meeting rsvp demo-mail-2 --response accept --dry-run`. Exit 1, message contains 「沒有 text/calendar」.
- **Bad response value.** `m2kv cli "$RUN" -- --json meeting rsvp demo-mail-2 --response maybe`. Exit 2, `invalid_arguments`.

**Fixture path (loopback fake Mail2000, real SMTP).** m2kv only drives demo dirs, so run the binary directly and `tee` output into `.tmp/verify/$RUN/` as evidence. Setup is the one in `examples/m2k_fixture/README.md`: start the fixture with `--data-dir /tmp/m2k-fx`, then `export M2K_PASSWORD=fixture-password M2K_TEST_ROOT_CERT=/tmp/m2k-fx/tls/ca.pem` and `M='./target/debug/m2k --data-dir /tmp/m2k-fx-client'`; `$M init …`, `$M config set …` as in that README.

- **Sync.** `$M --json sync`. Exit 0, mail count 6. The seeded invite is 「季度回顧會議」 from `carol@example.test`.
- **Find it.** `ID=$($M --json mail search 季度回顧會議 | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])')`.
- **Dry-run.** `$M --json meeting rsvp "$ID" --response accept --dry-run`. Exit 0; `"to": "carol@example.test"`, `"uid": "fixture-invite-1@example.test"`, `"sequence": 1`, `"partstat": "ACCEPTED"`, `"sent": false`, `"calendar_updated": false`; `ics` contains `METHOD:REPLY` and exactly one `ATTENDEE` line. `grep -c smtp.message /tmp/m2k-fx/events.jsonl` is unchanged (0 on a fresh fixture).
- **Send.** `$M --json meeting rsvp "$ID" --response decline --comment 那天出差`. Exit 0, `"sent": true`, a `message_id`.
- **Read back what the server got.** `grep smtp.message /tmp/m2k-fx/events.jsonl | tail -1` shows `"to":["carol@example.test"]`; open the `file` it names under `/tmp/m2k-fx/`. The `text/calendar; method=REPLY` part (base64) decodes to `METHOD:REPLY`, `UID:fixture-invite-1@example.test`, `SEQUENCE:1`, `ORGANIZER;CN=Carol:mailto:carol@example.test`, `ATTENDEE;PARTSTAT=DECLINED:mailto:fixture@example.test`, `COMMENT:那天出差`, and no `bob@example.test`.
- `cargo test --offline --test fixture_e2e` runs the same sync → dry-run → send → inspect flow automatically.

## Gotchas

- `--dry-run` works offline when the mail body is cached; a header-only mail needs one IMAP fetch first (password required even for dry-run).
- RSVP does not change PARTSTAT in your own calendar; `calendar_updated` is always `false`. Use `calendar update` if you want the local copy changed.
- Each send is attempted once. A timeout may still have delivered the reply; check the organizer side before re-running.
- Online behavior against the real Mail2000 server is NOT RUN here; the fixture path is a loopback stand-in.
