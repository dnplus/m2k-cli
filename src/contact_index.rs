use crate::directory::{self, DirectoryContact};
use crate::model::{Collection, Kind};
use crate::store::decode_item;
use anyhow::Result;
use rusqlite::{params, Connection};

pub(crate) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS contact_people (
             id TEXT PRIMARY KEY NOT NULL,
             name_key TEXT NOT NULL,
             payload TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS contact_people_order_idx ON contact_people(name_key,id);
         CREATE TABLE IF NOT EXISTS contact_members (
             item_id TEXT PRIMARY KEY NOT NULL REFERENCES items(id) ON DELETE CASCADE,
             person_id TEXT NOT NULL REFERENCES contact_people(id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS contact_members_person_idx ON contact_members(person_id);",
    )?;
    rebuild(conn)
}

pub(crate) fn rebuild(conn: &Connection) -> Result<()> {
    let collections = conn
        .prepare("SELECT id,name FROM collections WHERE kind='contacts' ORDER BY id")?
        .query_map([], |row| {
            Ok(Collection {
                kind: Kind::Contacts,
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let items = conn
        .prepare(
            "SELECT id,kind,collection,remote_id,etag,uid_validity,title,subtitle,date,end_date,
         text,raw,flags_json,emails_json,phones_json,attachments_json,last_seen
         FROM items WHERE kind='contacts' ORDER BY COALESCE(end_date,date,'') DESC,id ASC",
        )?
        .query_map([], decode_item)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let nodes = directory::address_book_tree(&collections, None);
    let people = directory::merge_contact_items(&items, &nodes);
    conn.execute("DELETE FROM contact_people", [])?;
    for person in &people {
        conn.prepare_cached("INSERT INTO contact_people(id,name_key,payload) VALUES (?1,?2,?3)")?
            .execute(params![
                person.id,
                person.name.to_lowercase(),
                serde_json::to_string(person)?
            ])?;
    }
    for item in &items {
        conn.prepare_cached("INSERT INTO contact_members(item_id,person_id) VALUES (?1,?2)")?
            .execute(params![item.id, directory::contact_identity(item)])?;
    }
    Ok(())
}

pub(crate) fn decode(payload: &str) -> Result<crate::model::Item> {
    let contact: DirectoryContact = serde_json::from_str(payload)?;
    Ok(directory::contact_to_item(&contact))
}
