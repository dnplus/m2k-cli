# TUI org chart

A user on the contacts tab presses `v` to swap the flat contact list for the department org chart built from the same local cache as `m2k directory chart`, walks it with ↑/↓ or j/k, expands and collapses departments with Enter, and reads a member's details in the right-hand panel. It never goes online; when the cache has no company root or is empty, the status line says why and points to `Shift-R`.

## Sub-features

- `tui-chart-toggle` `v` on tab 3 switches list ↔ org chart; status shows `組織圖 · 部門 N 個 · 成員 M 位 · Enter 展開／收合 · v 回列表`, or a warning when the cache is incomplete.
- `tui-chart-tree` departments render as `[v]` expanded / `[>]` collapsed / `[-]` empty with the direct member count; roots start expanded, sub-departments collapsed.
- `tui-chart-select` ↑/↓, j/k move the selection; `g`/`G` jump to first/last; `[`/`]` move 10 rows.
- `tui-chart-expand` Enter on a department toggles it; Enter on a member opens a full-screen detail only in the narrow (< 80 columns) layout.
- `tui-chart-detail` a selected member shows the same fields as the contact list detail (`format_contact_detail`); a selected department shows its path, member and sub-department counts and the reporting-line note.
- `tui-chart-warning` no address book: `組織圖：本機沒有通訊錄快取 · 按 Shift-R 同步公司目錄`; no members: `組織圖：通訊錄快取沒有成員 …`; only flat roots: `組織圖：快取沒有公司根與子部門，只列出扁平通訊錄 · 按 Shift-R 同步公司目錄`. None of these start a sync.

## How to get to it (user POV)

- `m2k tui`, `3`, `v`. `v` again (or `/`, or switching tabs) returns to the list.
- On a real account the tree needs `m2k directory sync --scope company`, `Shift-R` in the TUI, or a shared file through `M2K_DIRECTORY_CACHE` (see directory-export-cache.md).

## Driving it with m2kv

Preconditions: baseline. `E=.tmp/verify/$RUN`.

- **Flat demo chart.** `m2kv tui "$RUN" start`, `keys 3`, `capture contacts-list`, `keys v`, `capture orgchart-flat`. The left panel is titled `聯絡人 · 組織圖 · 1 個部門` and lists `[v] 聯絡人（2 人）` with 展示使用者 and 王小明; the status line reads `組織圖：快取沒有公司根與子部門，只列出扁平通訊錄 · 按 Shift-R 同步公司目錄`; the right panel shows the department path `demo-addressbook` and the reporting-line note.
- **Member detail matches the list.** `keys j`, `capture orgchart-flat-member`. The right panel for 展示使用者 is line-for-line the `詳情預覽` text in `contacts-list` (`部門／組織：產品工程部`, `Email：demo@example.test`, `資料夾／群組：`, `· demo-addressbook`, the note line).
- **Back to list.** `keys v`, `capture orgchart-back-to-list`. The `資料夾` and `聯絡人 · 全部 · 2 筆` panels are back; status `聯絡人列表 · v 切換組織圖`. `stop`.
- **Nested stand-in.** Export and hand-edit a cache as in directory-export-cache.md: `m2kv cli "$RUN" -- --json directory export --addressbook demo-addressbook --output "$PWD/$E/dir.json"`, then with `python3` write `$E/dir-org.json` with `generated_at` `2099-01-01T00:00:00Z`, an extra collection `{"kind":"contacts","id":"demo-addressbook/eng","name":"產品工程部(/demo-addressbook/eng)"}`, and a copy of the `demo-contact-1` item with `id`/`remote_id` `demo-contact-1-eng` and `collection` `demo-addressbook/eng`. Import it: `M2K_DIRECTORY_CACHE=$PWD/$E/dir-org.json m2kv cli "$RUN" -- directory chart`. Exit 0; stderr `已匯入共用通訊錄快取：3 筆聯絡人`; stdout nests `**產品工程部** \`demo-addressbook/eng\`（1 人）` with 王小明 under `**聯絡人**`（1 人）.
- **Nested tree.** `tui start`, `keys 3`, `keys v`, `capture orgchart-nested`: `[v] 聯絡人（1 人）`, 展示使用者, `[>] 產品工程部（1 人）`, status `組織圖 · 部門 2 個 · 成員 2 位 · Enter 展開／收合 · v 回列表`, no warning.
- **Select and expand.** `keys j j`, `capture orgchart-dept` (detail `路徑：demo-addressbook/eng`); `keys Enter`, `capture orgchart-expanded` (`[v] 產品工程部` with 王小明 indented below); `keys j`, `capture orgchart-member` (detail `王小明`, `部門／組織：產品工程部`, `Email：ming@example.test`, `電話：+886-2-0000-0000`).
- **Collapse.** `keys k Enter`, `capture orgchart-collapsed`: `[>] 產品工程部（1 人）`, 王小明 hidden.
- **Help.** `keys ?`, `capture orgchart-help`: the line `聯絡人 v              列表／組織圖；Enter 展開收合部門`. `keys Escape`, `stop`.

## Gotchas

- The demo seed is one flat address book, so a plain demo run only reaches the "no company root" warning. Nesting, Enter expand/collapse and sub-department detail need the hand-edited `M2K_DIRECTORY_CACHE` stand-in above; it mutates the run's contacts, so run it after the flat steps or on a fresh run.
- The empty-cache warning (`本機沒有通訊錄快取`) cannot be reached here: every m2kv run is a seeded demo dir. `tests/tui.rs` (`org_chart_on_empty_cache_explains_why_and_points_to_shift_r_without_syncing`) covers it; report it as NOT RUN in the harness.
- On demo, `Shift-R` only answers `展示模式 · 聯絡人使用本機資料`; a real company sync from the chart is online and NOT RUN.
- The tree and detail sit side by side at ≥ 80 columns (always true at 120×32). Narrower terminals show the tree alone and Enter on a member opens the detail full-screen; Esc returns.
- Selection is by row index; `r` or a finished `Shift-R` reloads the tree and resets expansion to roots-only.
