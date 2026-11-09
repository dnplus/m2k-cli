# File server (SMB)

`m2k files` lists, searches and downloads from an SMB file server named in the site profile's `file_server`, logging in with the mail account and password. The demo data dir has no file server and refuses online access, so this harness proves only the offline and refusal paths; listing, search and download are proven by `tests/files.rs`.

## Sub-features

- `files-shares` offline: prints `file_server` from the profile, or a clear error naming `file_server` when the profile has none.
- `files-demo-refusal` `ls`/`find`/`get` refuse a demo data dir before any connection.
- `files-ls`, `files-find`, `files-get` online; NOT RUN here.

## How to get to it (user POV)

- `m2k files shares`, `m2k files ls SHARE/dir`, `m2k files find SHARE --name TEXT`, `m2k files get PATH --output FILE`, each with `--json` for assertions.

## Driving it with m2kv

Preconditions: baseline.

- **No file server in the demo profile.** `m2kv cli "$RUN" -- --json files shares`. Exit 1, `"state":"error"`, message contains `file_server`.
- **Demo refusal.** `m2kv cli "$RUN" -- --json files ls smb://files.example.test/Shared`. Exit 1, message is the demo offline reason (`展示資料不連線`), same as `mail send`.
- **Help.** `m2kv cli "$RUN" -- files --help` lists `shares`, `ls`, `find`, `get`.

## Gotchas

- The demo cannot reach SMB: never point a demo run at a real server, and record `ls`/`find`/`get` as NOT RUN in this harness.
- `tests/files.rs` runs an in-process SMB2 server (`smb-server` dev-dependency) on a loopback port with NTLM user `alice`, serving a temp directory; it covers nested `ls`, `find` depth/limit/truncation and a skipped unreadable folder, `get` without overwrite, `auth_failed` after a single logon (the server locks the name after two failures), `not_found`, the missing `file_server` error and the demo refusal. The `time_limit` cutoff and the three PATH forms are unit tests in `src/files.rs`.
- The test profile's `file_server.server` is `127.0.0.1:PORT`; a `host:port` server is how a non-445 port is reached.
- `auth_failed` from SMB means the directory password was rejected; agents must stop, not retry.
