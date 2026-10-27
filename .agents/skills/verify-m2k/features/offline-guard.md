# Offline guard and JSON errors

Demo data never reaches the network: every online command fails fast with one JSON error and exit 1, and argument errors follow the same contract.

## Sub-features

- `guard-online-write` send/flag/move/book refuse demo data.
- `guard-sync` sync refuses demo data.
- `json-error-contract` one JSON object, non-zero exit, `retryable` flag.

## How to get to it (user POV)

- Any online command with `--json` against a demo `--data-dir`.

## Driving it with m2kv

Preconditions: baseline.

- **Send refused.** `m2kv cli "$RUN" -- --json mail send --to a@example.test --subject x --body-file /dev/null`. Exit 1; `"code":"operation_failed"`, `"retryable":false`.
- **Flag refused.** `m2kv cli "$RUN" -- --json mail flag demo-mail-1 --flag seen`. Exit 1, `operation_failed`.
- **Sync refused.** `m2kv cli "$RUN" -- --json sync`. Exit non-zero with a single JSON error object.
- **Unchanged state.** `m2kv doctor "$RUN"` still reports counts `2 3 2`.

## Gotchas

- This proves the guard, not the online feature. Record the online feature itself as NOT RUN.
- Without `--json`, errors go to stderr as text; assert the exit code there.
