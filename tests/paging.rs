use chrono::Utc;
use m2k::model::{Attachment, Collection, Item, Kind, Snapshot};
use m2k::store::{ItemPage, Store};
use rusqlite::Connection;
use tempfile::TempDir;

fn mail(id: &str, title: &str, date: Option<&str>) -> Item {
    Item {
        id: id.into(),
        kind: Kind::Mail,
        collection: "INBOX".into(),
        remote_id: id.into(),
        etag: None,
        uid_validity: Some(11),
        title: title.into(),
        subtitle: "sender@example.com".into(),
        date: date.map(str::to_owned),
        end_date: None,
        text: format!("Cached body for {id}"),
        raw: format!("Cached RFC822 for {id}"),
        flags: vec!["\\Seen".into()],
        emails: vec!["sender@example.com".into()],
        phones: Vec::new(),
        attachments: Vec::new(),
    }
}

fn cache(store: &mut Store, items: Vec<Item>) {
    let mut collections: Vec<_> = items.iter().map(|item| item.collection.clone()).collect();
    collections.sort();
    collections.dedup();
    store
        .apply_snapshot(
            &Snapshot {
                kind: Kind::Mail,
                collections: collections
                    .into_iter()
                    .map(|id| Collection {
                        kind: Kind::Mail,
                        name: id.clone(),
                        id,
                    })
                    .collect(),
                items,
            },
            Utc::now(),
        )
        .unwrap();
}

fn ids(page: &ItemPage) -> Vec<&str> {
    page.items.iter().map(|item| item.id.as_str()).collect()
}

#[test]
fn pages_use_date_title_and_id_for_stable_boundaries() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    cache(
        &mut store,
        vec![
            mail("undated", "AAA", None),
            mail("same-title-2", "alpha", Some("2026-10-07T00:00:00Z")),
            mail("old", "AAA", Some("2026-10-06T00:00:00Z")),
            mail("same-title-1", "ALPHA", Some("2026-10-07T00:00:00Z")),
            mail("zulu", "Zulu", Some("2026-10-07T00:00:00Z")),
        ],
    );
    let first = store
        .list_summaries_page(Kind::Mail, None, None, 2, 0)
        .unwrap();
    let second = store
        .list_summaries_page(Kind::Mail, None, None, 2, 2)
        .unwrap();
    let last = store
        .list_summaries_page(Kind::Mail, None, None, 2, 4)
        .unwrap();
    assert_eq!(ids(&first), vec!["same-title-1", "same-title-2"]);
    assert_eq!(ids(&second), vec!["zulu", "old"]);
    assert_eq!(ids(&last), vec!["undated"]);
    assert_eq!((first.total, second.total, last.total), (5, 5, 5));
    let repeated = store
        .list_summaries_page(Kind::Mail, None, None, 2, 2)
        .unwrap();
    assert_eq!(ids(&second), ids(&repeated));
    assert!(first.items.iter().all(|item| item.text.is_empty()
        && item.raw.is_empty()
        && item.emails.is_empty()
        && item.attachments.is_empty()));
    assert_eq!(store.get("old").unwrap().text, "Cached body for old");
}

#[test]
fn page_counts_apply_the_same_unicode_and_collection_filters() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut first = mail("one", "Normal subject", None);
    first.collection = "Équipe".into();
    first.text = "ÉQUIPE searchable body".into();
    let mut second = mail("two", "Normal subject", None);
    second.collection = "Équipe".into();
    second.attachments = vec![Attachment {
        index: 0,
        filename: "Résumé-附件.pdf".into(),
        content_type: "application/pdf".into(),
        size: 42,
    }];
    let mut outside = mail("outside", "ÉQUIPE", None);
    outside.collection = "Other".into();
    cache(&mut store, vec![first, second, outside]);

    let page = store
        .list_summaries_page(Kind::Mail, Some("  éQUIPE  "), Some(" éQUIPE "), 1, 1)
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.items.len(), 1);
    assert!(page.items.iter().all(|item| item.collection == "Équipe"));
    let attachment = store
        .list_summaries_page(Kind::Mail, Some("résUMÉ-附件"), Some("équipe"), 10, 0)
        .unwrap();
    assert_eq!(ids(&attachment), vec!["two"]);
    assert_eq!(attachment.total, 1);
    let full = store
        .list(Kind::Mail, Some("résUMÉ-附件"), Some("équipe"), 0)
        .unwrap();
    assert_eq!(attachment.total, full.len());
}

#[test]
fn zero_and_out_of_range_pages_keep_the_total() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    cache(&mut store, vec![mail("one", "One", None)]);
    for (limit, offset) in [(0, 0), (10, 1), (10, 100_000)] {
        let page = store
            .list_summaries_page(Kind::Mail, None, None, limit, offset)
            .unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.total, 1);
    }
    let no_match = store
        .list_summaries_page(Kind::Mail, Some("not found"), None, 10, 0)
        .unwrap();
    assert!(no_match.items.is_empty());
    assert_eq!(no_match.total, 0);
}

#[test]
fn thousands_of_bodies_do_not_get_decoded_for_a_page() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let items = (0..2_050)
        .map(|index| {
            let mut item = mail(
                &format!("mail-{index:04}"),
                &format!("Subject {index:04}"),
                Some("2026-10-07T00:00:00Z"),
            );
            item.text = "Large body data. ".repeat(256);
            item.raw = "Large raw body. ".repeat(256);
            item
        })
        .collect();
    cache(&mut store, items);
    let connection = Connection::open(store.database_path()).unwrap();
    connection
        .execute(
            "UPDATE items SET flags_json = 'not-json' WHERE id = 'mail-0500'",
            [],
        )
        .unwrap();
    drop(connection);

    let page = store
        .list_summaries_page(Kind::Mail, None, None, 17, 20)
        .unwrap();
    assert_eq!(page.total, 2_050);
    assert_eq!(page.items.len(), 17);
    assert_eq!(page.items[0].id, "mail-0020");
    assert_eq!(page.items[16].id, "mail-0036");
    assert!(page
        .items
        .iter()
        .all(|item| item.text.is_empty() && item.raw.is_empty()));
    let last = store
        .list_summaries_page(Kind::Mail, None, None, 17, 2_040)
        .unwrap();
    assert_eq!(last.items.len(), 10);
    assert_eq!(last.total, 2_050);
}

#[test]
fn hybrid_search_deduplicates_and_preserves_cached_bodies() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let local = mail("local", "Needle local", Some("2026-10-06T00:00:00Z"));
    let cached = mail("both", "Cached subject", Some("2026-10-07T00:00:00Z"));
    cache(&mut store, vec![local, cached.clone()]);
    let mut duplicate = cached.clone();
    duplicate.title = "Remote replacement".into();
    duplicate.text.clear();
    duplicate.raw.clear();
    duplicate.flags = vec!["\\Flagged".into()];
    let remote_only = mail(
        "remote-only",
        "Remote subject without the local needle",
        Some("2026-10-08T00:00:00Z"),
    );
    let remote = vec![duplicate.clone(), remote_only.clone(), duplicate];

    let first = store
        .hybrid_summaries_page(Kind::Mail, Some("Needle"), None, &remote, 2, 0)
        .unwrap();
    let last = store
        .hybrid_summaries_page(Kind::Mail, Some("Needle"), None, &remote, 2, 2)
        .unwrap();
    assert_eq!(ids(&first), vec!["remote-only", "both"]);
    assert_eq!(ids(&last), vec!["local"]);
    assert_eq!((first.total, last.total), (3, 3));
    assert_eq!(first.items[1].title, cached.title);
    assert_eq!(first.items[1].flags, cached.flags);
    assert!(first
        .items
        .iter()
        .all(|item| item.text.is_empty() && item.raw.is_empty()));
    let preserved = store.get("both").unwrap();
    assert_eq!(preserved.text, cached.text);
    assert_eq!(preserved.raw, cached.raw);
    assert_eq!(preserved.title, cached.title);
    assert!(store.get("remote-only").is_err());
    assert_eq!(store.counts().unwrap()["mail"], 2);
    let offline = store
        .hybrid_summaries_page(Kind::Mail, Some("Needle"), None, &[], 10, 0)
        .unwrap();
    assert_eq!(ids(&offline), vec!["local"]);
    assert_eq!(offline.total, 1);
}

#[test]
fn hybrid_search_filters_server_hits_and_does_not_leak_across_searches() {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut cached = mail("cached", "Unmatched title", None);
    cached.collection = "Équipe".into();
    cache(&mut store, vec![cached.clone()]);
    let mut included = mail("included", "Server Unicode hit", None);
    included.collection = "Équipe".into();
    let outside = mail("outside", "Server hit in other collection", None);
    let mut contact = mail("wrong-kind", "Server hit in other kind", None);
    contact.kind = Kind::Contacts;
    contact.collection = "Équipe".into();
    let remote = vec![cached, included, outside, contact];
    let matched = store
        .hybrid_summaries_page(
            Kind::Mail,
            Some("僅伺服器能搜到"),
            Some(" éQUIPE "),
            &remote,
            10,
            0,
        )
        .unwrap();
    assert_eq!(ids(&matched), vec!["included", "cached"]);
    assert_eq!(matched.total, 2);
    let next_query = store
        .hybrid_summaries_page(
            Kind::Mail,
            Some("另一個查詢"),
            Some("équipe"),
            &[mail("elsewhere", "Elsewhere", None)],
            10,
            0,
        )
        .unwrap();
    assert!(next_query.items.is_empty());
    assert_eq!(next_query.total, 0);
}
