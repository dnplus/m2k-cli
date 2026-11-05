# Directory chart

A user asks for the org chart of the cached company directory: each department with its direct members (name, account, title, extension or phone), as indented Markdown, nested JSON, or a Mermaid flowchart of departments only. It is offline and deterministic, and says explicitly that it draws membership, not reporting lines.

## Sub-features

- `chart-markdown` `directory chart` prints `# 部門組織圖`, a header with scope, counts, the reporting-line note and the email rule, then one bullet per department with its members.
- `chart-json` `--json directory chart` returns `branch`, `email_domain`, `department_count`, `member_count`, `reporting_note`, nested `departments[]` with `members[]` and `children[]`.
- `chart-mermaid` `--format mermaid` prints `flowchart TD` with one node per department and its member count.
- `chart-branch` `--branch <path|name|URL>` (alias `--addressbook`) limits output to that branch; an unknown branch is refused.

## How to get to it (user POV)

- `m2k directory chart [--branch /EXAMPLE_ALL/Engineering] [--format markdown|mermaid]`, `m2k --json directory chart`.
- On a real account this needs `m2k directory sync --scope company` first (online), or a shared file through `M2K_DIRECTORY_CACHE` (see directory-export-cache.md).
- No TUI entry.

## Driving it with m2kv

Preconditions: baseline. The demo store has one address book, `demo-addressbook` named 聯絡人, holding 王小明 and 展示使用者.

- **Markdown.** `m2kv cli "$RUN" -- directory chart`. Exit 0; `- 範圍：全部已快取分支`, `- 部門 1 個，成員 2 位（同一人可屬多個部門）`, the line `- 匯報線：通訊錄 vCard 沒有主管或匯報欄位；本圖只呈現部門歸屬，不含匯報線，也不推估主管。`, then `- **聯絡人** \`demo-addressbook\`（2 人）` with `展示使用者 · demo@example.test` and `王小明 · 電話 +886-2-0000-0000 · ming@example.test`.
- **JSON.** `m2kv cli "$RUN" -- --json directory chart`. Exit 0; `"branch": null`, `"email_domain": "example.test"`, `"department_count": 1`, `"member_count": 2`; `departments[0]` has `"path": "demo-addressbook"`, two `members` with `emails`, and `"children": []`.
- **Mermaid.** `m2kv cli "$RUN" -- directory chart --format mermaid`. Exit 0; stdout is exactly `flowchart TD` and `  d0["聯絡人<br/>2 人"]`; no member names.
- **Branch.** `m2kv cli "$RUN" -- directory chart --branch demo-addressbook`. Exit 0; `- 範圍：demo-addressbook`. `m2kv cli "$RUN" -- --json directory chart --branch 不存在` exits 1 with `operation_failed`, message `找不到指定的通訊錄或分支：不存在`.
- **Determinism.** Run the JSON command twice; the two evidence files' stdout sections are identical (`diff` them).

## Gotchas

- The demo seed is a single flat address book with no company directory root, no sub-departments, no person in two departments, no vCard `N` (names come from `FN`), and no `TITLE`. Nesting, `children`, multi-membership counted once, `N`-based full names with the account in parentheses, and title/extension columns cannot be driven here. Report them as NOT RUN in the demo; `tests/directory_chart.rs` covers them.
- Demo emails are `FN`-named, so the account-in-parentheses rule never fires and every member lists its email.
- `--json` wins over `--format mermaid`.
- `directory sync` and `directory tree` are online and refused on demo (`展示資料不連線；請指定正式帳號的 --data-dir`, exit 1).
