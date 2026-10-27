use super::*;

/// Trigram needs three characters to produce a token; shorter queries scan.
const MIN_INDEXED_CHARS: usize = 3;

/// Columns whose change must rewrite the index row. `last_seen`, `etag` and
/// `flags_json` change on every sync and are not searched.
const SEARCHED_COLUMNS: [&str; 13] = [
    "kind",
    "id",
    "collection",
    "remote_id",
    "title",
    "subtitle",
    "date",
    "end_date",
    "text",
    "raw",
    "emails_json",
    "phones_json",
    "attachments_json",
];

/// Creates the trigram index and its triggers. With `rebuild`, the index is
/// recreated from `items` inside the caller's migration transaction.
///
/// The triggers are plain SQL so connections without m2k's custom functions
/// (sqlite3 CLI, benchmark scripts) can still write `items`.
pub(super) fn initialize(conn: &Connection, rebuild: bool) -> Result<()> {
    if rebuild {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS items_search_insert;
             DROP TRIGGER IF EXISTS items_search_delete;
             DROP TRIGGER IF EXISTS items_search_update;
             DROP TABLE IF EXISTS items_search;",
        )?;
    }
    let changed = SEARCHED_COLUMNS
        .iter()
        .map(|column| format!("old.{column} IS NOT new.{column}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS items_search USING fts5(
             document, content='', contentless_delete=1, tokenize='trigram'
         );
         CREATE TRIGGER IF NOT EXISTS items_search_insert AFTER INSERT ON items BEGIN
             INSERT INTO items_search(rowid, document) VALUES (new.rowid, {new});
         END;
         CREATE TRIGGER IF NOT EXISTS items_search_delete AFTER DELETE ON items BEGIN
             DELETE FROM items_search WHERE rowid = old.rowid;
         END;
         CREATE TRIGGER IF NOT EXISTS items_search_update AFTER UPDATE ON items
         WHEN {changed} BEGIN
             DELETE FROM items_search WHERE rowid = old.rowid;
             INSERT INTO items_search(rowid, document) VALUES (new.rowid, {new});
         END;",
        new = document("new."),
    ))?;
    if rebuild {
        conn.execute(
            &format!(
                "INSERT INTO items_search(rowid, document) SELECT rowid, {} FROM items",
                document("")
            ),
            [],
        )?;
    }
    Ok(())
}

/// One newline-joined document per item. It is a superset of what the exact
/// `m2k_contains` predicate checks, so candidates are always re-verified.
fn document(row: &str) -> String {
    let json_text = |column: &str, key: Option<&str>| {
        let key = key.map_or(String::new(), |key| format!(" AND key = '{key}'"));
        format!(
            "CASE WHEN json_valid({row}{column}) THEN COALESCE((
                 SELECT group_concat(value, char(10)) FROM json_tree({row}{column})
                 WHERE type = 'text'{key}
             ), '') ELSE '' END"
        )
    };
    let mut parts: Vec<String> = SEARCHED_COLUMNS[1..8]
        .iter()
        .map(|column| format!("COALESCE({row}{column}, '')"))
        .collect();
    parts.push(format!("{row}text"));
    parts.push(format!(
        "CASE WHEN {row}kind <> 'mail' THEN {row}raw ELSE '' END"
    ));
    parts.push(json_text("emails_json", None));
    parts.push(json_text("phones_json", None));
    parts.push(json_text("attachments_json", Some("filename")));
    parts.join(" || char(10) || ")
}

/// True when opening an existing pre-index database must tokenize cached
/// items, which takes tens of seconds on a large mailbox.
pub(super) fn backfill_is_slow(conn: &Connection, version: i64) -> Result<bool> {
    if version == 0 || version >= 4 {
        return Ok(false);
    }
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM items)", [], |row| row.get(0))?)
}

/// Wraps the exact predicate with an index lookup when the query is long
/// enough for trigram. `fts_index` is the bind slot for the phrase.
pub(super) fn indexed_predicate(query: &str, exact: &str, fts_index: usize) -> Option<String> {
    (query.chars().count() >= MIN_INDEXED_CHARS).then(|| {
        format!(
            "(items.rowid IN (
                 SELECT rowid FROM items_search WHERE items_search MATCH ?{fts_index}
             ) AND {exact})"
        )
    })
}

/// A single FTS5 string token: every syntax character is literal inside
/// double quotes, and embedded quotes are doubled.
pub(super) fn phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}
