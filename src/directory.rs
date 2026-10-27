use crate::{
    config::{self, Config},
    model::{Collection, Item, Kind, Snapshot},
    provider,
    site::SiteProfile,
    store::Store,
    sync,
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use clap::{Args, Subcommand, ValueEnum};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs::File,
    path::Path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DirectoryScope {
    #[value(name = "self")]
    #[serde(rename = "self")]
    SelfContact,
    #[value(alias = "bu", alias = "branch")]
    Department,
    #[value(alias = "global")]
    Company,
}

#[derive(Debug, Subcommand)]
pub enum DirectoryCommand {
    #[command(about = "探索 CardDAV 通訊錄分支與部門階層")]
    Tree(TreeArgs),
    #[command(about = "按範圍同步聯絡人到本機快取")]
    Sync(SyncArgs),
    #[command(name = "self", about = "顯示目前登入帳號的實際 self contact")]
    SelfContact(SelfArgs),
    #[command(about = "搜尋本機聯絡人，可選擇指定分支即時 hydration")]
    Search(SearchArgs),
    #[command(about = "以完整姓名或 email 唯一解析聯絡人")]
    Resolve(ResolveArgs),
}

#[derive(Debug, Args)]
pub struct TreeArgs {
    #[arg(long, help = "只探索指定通訊錄或分支；可填 path、名稱或 URL")]
    pub addressbook: Option<String>,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    #[arg(long, value_enum, default_value = "self")]
    pub scope: DirectoryScope,
    #[arg(
        long,
        visible_alias = "path",
        help = "限制在指定通訊錄或分支；可填 path、名稱或 URL"
    )]
    pub addressbook: Option<String>,
}

#[derive(Debug, Args)]
pub struct SelfArgs {
    #[arg(long, help = "本機無 self contact 時，對指定分支做即時 hydration")]
    pub online: bool,
    #[arg(
        long,
        visible_alias = "path",
        help = "--online 時指定要 hydration 的通訊錄或分支"
    )]
    pub addressbook: Option<String>,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    pub query: String,
    #[arg(long, help = "本機無結果時，對指定或目前部門分支做即時 hydration")]
    pub online: bool,
    #[arg(
        long,
        visible_alias = "path",
        help = "--online 時指定要 hydration 的通訊錄或分支"
    )]
    pub addressbook: Option<String>,
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct ResolveArgs {
    pub query: String,
    #[arg(long, help = "本機無唯一結果時，對指定或目前部門分支做即時 hydration")]
    pub online: bool,
    #[arg(
        long,
        visible_alias = "path",
        help = "--online 時指定要 hydration 的通訊錄或分支"
    )]
    pub addressbook: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryNode {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub path: String,
    pub parent_path: Option<String>,
    pub depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryMembership {
    pub collection: String,
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryContact {
    pub id: String,
    pub name: String,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub organization: Option<String>,
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub memberships: Vec<DirectoryMembership>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectoryTreeResult {
    pub addressbook: Option<String>,
    pub nodes: Vec<DirectoryNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DirectorySyncResult {
    pub scope: DirectoryScope,
    pub scope_paths: Vec<String>,
    pub collections: Vec<String>,
    pub count: usize,
}

pub fn sync_default(config: &Config, password: &str, _existing: &[Item]) -> Result<Snapshot> {
    let (nodes, session) = discover_self_targets_with_session(config, password)?;
    let mut collections = Vec::new();
    let mut items = Vec::new();
    for node in nodes {
        let snapshot = session
            .query_contacts(
                &Collection {
                    kind: Kind::Contacts,
                    id: node.id.clone(),
                    name: node.display_name.clone(),
                },
                &config.email,
            )
            .map_err(|error| sanitize_error(error, password))?;
        let matched = snapshot
            .items
            .into_iter()
            .filter(|item| is_self_contact(item, config))
            .collect::<Vec<_>>();
        if matched.is_empty() {
            continue;
        }
        collections.push(Collection {
            kind: Kind::Contacts,
            id: node.id,
            name: node.display_name,
        });
        items.extend(matched);
    }
    Ok(Snapshot {
        kind: Kind::Contacts,
        collections,
        items,
    })
}

pub fn execute(data_dir: &Path, command: DirectoryCommand, json: bool) -> Result<()> {
    match command {
        DirectoryCommand::Tree(args) => execute_tree(data_dir, args, json),
        DirectoryCommand::Sync(args) => execute_sync(data_dir, args, json),
        DirectoryCommand::SelfContact(args) => execute_self(data_dir, args, json),
        DirectoryCommand::Search(args) => execute_search(
            data_dir,
            args.query,
            args.online,
            args.addressbook,
            args.limit,
            json,
        ),
        DirectoryCommand::Resolve(args) => {
            execute_resolve(data_dir, args.query, args.online, args.addressbook, json)
        }
    }
}

pub fn address_book_tree(collections: &[Collection], base_url: Option<&str>) -> Vec<DirectoryNode> {
    let base = base_url.and_then(|value| Url::parse(value).ok());
    let raw = collections
        .iter()
        .filter(|collection| collection.kind == Kind::Contacts)
        .map(|collection| {
            let (name, display_path) = parse_display_name(&collection.name);
            let path = display_path
                .or_else(|| relative_collection_path(&base, &collection.id))
                .unwrap_or_else(|| collection.id.clone());
            (collection, name, path)
        })
        .collect::<Vec<_>>();
    let mut nodes = raw
        .into_iter()
        .map(|(collection, name, path)| {
            let path = normalize_path(&path);
            let segments = path_segments(&path);
            let parent_path =
                (segments.len() > 1).then(|| segments[..segments.len() - 1].join("/"));
            DirectoryNode {
                id: collection.id.clone(),
                name: if name.trim().is_empty() {
                    segments
                        .last()
                        .cloned()
                        .unwrap_or_else(|| collection.name.clone())
                } else {
                    name
                },
                display_name: collection.name.clone(),
                path,
                parent_path,
                depth: segments.len().saturating_sub(1),
            }
        })
        .collect::<Vec<_>>();
    nodes.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.id.cmp(&right.id))
    });
    nodes
}

pub fn merge_contact_items(items: &[Item], nodes: &[DirectoryNode]) -> Vec<DirectoryContact> {
    let node_by_id = nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let mut merged = BTreeMap::<String, DirectoryContact>::new();
    for item in items.iter().filter(|item| item.kind == Kind::Contacts) {
        let key = contact_identity(item);
        let entry = merged
            .entry(key.clone())
            .or_insert_with(|| DirectoryContact {
                id: key,
                name: item.title.trim().to_owned(),
                emails: Vec::new(),
                phones: Vec::new(),
                organization: None,
                title: None,
                structured_name: None,
                nickname: None,
                note: None,
                memberships: Vec::new(),
            });
        if entry.name.is_empty() && !item.title.trim().is_empty() {
            entry.name = item.title.trim().to_owned();
        }
        extend_unique(
            &mut entry.emails,
            item.emails.iter().map(|value| value.trim().to_owned()),
        );
        extend_unique(
            &mut entry.phones,
            item.phones.iter().map(|value| value.trim().to_owned()),
        );
        if entry.organization.is_none() {
            entry.organization = vcard_property(&item.raw, "ORG")
                .and_then(|value| meaningful_vcard_component(&value))
                .or_else(|| {
                    meaningful_vcard_component(item.subtitle.trim())
                        .filter(|_| vcard_property(&item.raw, "TITLE").is_none())
                });
        }
        if entry.title.is_none() {
            entry.title = vcard_property(&item.raw, "TITLE")
                .and_then(|value| meaningful_vcard_component(&value));
        }
        if entry.structured_name.is_none() {
            entry.structured_name =
                vcard_property(&item.raw, "N").and_then(|value| format_vcard_n(&value));
        }
        if entry.nickname.is_none() {
            entry.nickname = vcard_property(&item.raw, "NICKNAME")
                .and_then(|value| meaningful_vcard_component(&value));
        }
        if entry.note.is_none() {
            entry.note = vcard_property(&item.raw, "NOTE")
                .and_then(|value| meaningful_vcard_component(&value))
                .or_else(|| meaningful_vcard_component(item.text.trim()));
        }
        let membership = node_by_id
            .get(item.collection.as_str())
            .map(|node| DirectoryMembership {
                collection: node.id.clone(),
                path: node.path.clone(),
                name: node.name.clone(),
            })
            .unwrap_or_else(|| DirectoryMembership {
                collection: item.collection.clone(),
                path: item.collection.clone(),
                name: item.collection.clone(),
            });
        if !entry
            .memberships
            .iter()
            .any(|current| current.collection == membership.collection)
        {
            entry.memberships.push(membership);
            entry
                .memberships
                .sort_by(|left, right| left.path.cmp(&right.path));
        }
    }
    merged
        .into_values()
        .map(|mut contact| {
            apply_department_fallback(&mut contact);
            contact
        })
        .collect()
}

pub fn resolve_unique_contact(
    contacts: &[DirectoryContact],
    query: &str,
) -> Result<DirectoryContact> {
    let normalized = normalize_identifier(query);
    if normalized.is_empty() {
        bail!("解析條件不可為空白");
    }
    let mut matches = contacts
        .iter()
        .filter(|contact| {
            if normalized.contains('@') {
                contact
                    .emails
                    .iter()
                    .any(|email| normalize_identifier(email) == normalized)
            } else {
                normalize_identifier(&contact.name) == normalized
            }
        })
        .cloned()
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.id.cmp(&right.id));
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(anyhow!("找不到唯一聯絡人：{query}")),
        count => Err(anyhow!("聯絡人解析不唯一：{query}（符合 {count} 筆）")),
    }
}

fn execute_tree(data_dir: &Path, args: TreeArgs, json: bool) -> Result<()> {
    let _lock = sync::lock(data_dir)?;
    let (config, password) = online(data_dir)?;
    let nodes = discover_tree(&config, &password, args.addressbook.as_deref())?;
    let result = DirectoryTreeResult {
        addressbook: args.addressbook,
        nodes,
    };
    emit_tree(&result, json)
}

fn execute_sync(data_dir: &Path, args: SyncArgs, json: bool) -> Result<()> {
    emit_value(&sync_contacts(data_dir, args)?, json)
}

pub fn sync_contacts(data_dir: &Path, args: SyncArgs) -> Result<DirectorySyncResult> {
    let lock = sync::lock(data_dir)?;
    sync_contacts_locked(data_dir, args, lock)
}

pub(crate) fn sync_contacts_locked(
    data_dir: &Path,
    args: SyncArgs,
    _lock: File,
) -> Result<DirectorySyncResult> {
    let mut progress = crate::sync_progress::Reporter::start(data_dir)?;
    progress.update("discovering", Some(Kind::Contacts), 0)?;
    let outcome = (|| {
        let (config, password) = online(data_dir)?;
        let mut store = Store::open(data_dir)?;
        store.ensure_account(&config)?;
        let (targets, session) = select_sync_targets_with_session(
            &config,
            &password,
            &store,
            args.scope,
            args.addressbook.as_deref(),
        )?;
        if args.scope == DirectoryScope::Company && args.addressbook.is_none() && targets.is_empty()
        {
            bail!("company 同步找不到通訊錄；請先使用 directory tree 檢查 CardDAV");
        }
        let mut collections = Vec::new();
        let mut count = 0usize;
        let existing_all = store.list(Kind::Contacts, None, None, usize::MAX)?;
        let fetch_targets = targets.clone();
        let mut synced_collections = HashSet::new();
        let session = session;
        for target in &fetch_targets {
            progress.update("fetching", Some(Kind::Contacts), count)?;
            let existing = if args.scope == DirectoryScope::SelfContact {
                existing_all
                    .iter()
                    .filter(|item| item.collection == target.id)
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                existing_all.clone()
            };
            let mut snapshot = if args.scope == DirectoryScope::SelfContact {
                session.query_contacts(
                    &Collection {
                        kind: Kind::Contacts,
                        id: target.id.clone(),
                        name: target.display_name.clone(),
                    },
                    &config.email,
                )
            } else {
                session.sync_collection(
                    &Collection {
                        kind: Kind::Contacts,
                        id: target.id.clone(),
                        name: target.display_name.clone(),
                    },
                    &existing,
                )
            }
            .map_err(|error| sanitize_error(error, &password))?;
            if args.scope == DirectoryScope::SelfContact {
                snapshot.items.retain(|item| is_self_contact(item, &config));
                validate_snapshot_scope(&snapshot, std::slice::from_ref(target))?;
                for collection in &mut snapshot.collections {
                    if collection.id == target.id {
                        collection.name = target.display_name.clone();
                    }
                }
                store
                    .upsert_snapshot(&snapshot, Utc::now())
                    .context("無法寫入 self contact 快取")?;
            } else {
                validate_snapshot_scope(&snapshot, &targets)?;
                for collection in snapshot.collections.clone() {
                    if !synced_collections.insert(collection.id.clone()) {
                        continue;
                    }
                    let items = snapshot
                        .items
                        .iter()
                        .filter(|item| item.collection == collection.id)
                        .cloned()
                        .collect::<Vec<_>>();
                    let collection_id = collection.id.clone();
                    let item_count = items.len();
                    let scoped = Snapshot {
                        kind: Kind::Contacts,
                        collections: vec![collection],
                        items,
                    };
                    store
                        .apply_collection_snapshot(&scoped, Utc::now())
                        .context("無法寫入通訊錄分支快取")?;
                    count += item_count;
                    collections.push(collection_id);
                }
            }
            if args.scope == DirectoryScope::SelfContact {
                count += snapshot.items.len();
                collections.extend(
                    snapshot
                        .collections
                        .into_iter()
                        .map(|collection| collection.id),
                );
            }
        }
        collections.sort();
        collections.dedup();
        let scope_paths = targets
            .iter()
            .map(|target| target.path.clone())
            .collect::<Vec<_>>();
        progress.update("saved", Some(Kind::Contacts), count)?;
        Ok(DirectorySyncResult {
            scope: args.scope,
            scope_paths,
            collections,
            count,
        })
    })();
    progress.finish(outcome.is_ok())?;
    outcome
}

fn execute_search(
    data_dir: &Path,
    query: String,
    online_requested: bool,
    addressbook: Option<String>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let store = local_store(data_dir)?;
    let mut nodes = address_book_tree(&store.collections(Kind::Contacts)?, None);
    let mut contacts = search_contacts(&store, &nodes, &query, limit)?;
    if contacts.is_empty() && online_requested {
        let _lock = sync::lock(data_dir)?;
        let (config, password) = online(data_dir)?;
        let mut store = Store::open(data_dir)?;
        store.ensure_account(&config)?;
        hydrate_search_branch(&config, &password, &mut store, addressbook.as_deref())?;
        nodes = address_book_tree(
            &store.collections(Kind::Contacts)?,
            Some(&config.carddav_url),
        );
        contacts = search_contacts(&store, &nodes, &query, limit)?;
    }
    contacts.truncate(limit.max(1));
    emit_value(&contacts, json)
}

fn execute_self(data_dir: &Path, args: SelfArgs, json: bool) -> Result<()> {
    let config = config::load(data_dir)?;
    let store = local_store(data_dir)?;
    let mut nodes = address_book_tree(&store.collections(Kind::Contacts)?, None);
    let mut contacts =
        merge_contact_items(&store.list(Kind::Contacts, None, None, usize::MAX)?, &nodes);
    let resolve = resolve_unique_contact(&contacts, &config.email);
    let resolved = if resolve.is_err() && args.online {
        let _lock = sync::lock(data_dir)?;
        let (config, password) = online(data_dir)?;
        let mut store = Store::open(data_dir)?;
        store.ensure_account(&config)?;
        if let Some(addressbook) = args.addressbook.as_deref() {
            hydrate_search_branch(&config, &password, &mut store, Some(addressbook))?;
        } else {
            let snapshot = sync_default(&config, &password, &[])?;
            store
                .upsert_snapshot(&snapshot, Utc::now())
                .context("無法寫入 self contact 快取")?;
        }
        nodes = address_book_tree(
            &store.collections(Kind::Contacts)?,
            Some(&config.carddav_url),
        );
        contacts =
            merge_contact_items(&store.list(Kind::Contacts, None, None, usize::MAX)?, &nodes);
        resolve_unique_contact(&contacts, &config.email)
    } else {
        resolve
    }?;
    emit_value(&resolved, json)
}

fn execute_resolve(
    data_dir: &Path,
    query: String,
    online_requested: bool,
    addressbook: Option<String>,
    json: bool,
) -> Result<()> {
    let store = local_store(data_dir)?;
    let mut nodes = address_book_tree(&store.collections(Kind::Contacts)?, None);
    let mut contacts =
        merge_contact_items(&store.list(Kind::Contacts, None, None, usize::MAX)?, &nodes);
    let resolved = resolve_unique_contact(&contacts, &query);
    let resolved = if resolved.is_err() && online_requested {
        let _lock = sync::lock(data_dir)?;
        let (config, password) = online(data_dir)?;
        let mut store = Store::open(data_dir)?;
        store.ensure_account(&config)?;
        hydrate_search_branch(&config, &password, &mut store, addressbook.as_deref())?;
        nodes = address_book_tree(
            &store.collections(Kind::Contacts)?,
            Some(&config.carddav_url),
        );
        contacts =
            merge_contact_items(&store.list(Kind::Contacts, None, None, usize::MAX)?, &nodes);
        resolve_unique_contact(&contacts, &query)
    } else {
        resolved
    }?;
    emit_value(&resolved, json)
}

fn online(data_dir: &Path) -> Result<(Config, String)> {
    if data_dir.join(".demo").exists() {
        bail!("展示資料不連線；請指定正式帳號的 --data-dir");
    }
    let config = config::load(data_dir)?;
    let mut store = Store::open(data_dir)?;
    store.ensure_account(&config)?;
    let password =
        config::password(data_dir, &config).map_err(|error| sanitize_error(error, ""))?;
    Ok((config, password))
}

fn local_store(data_dir: &Path) -> Result<Store> {
    let _lock = sync::lock(data_dir)?;
    let mut store = Store::open(data_dir)?;
    if data_dir.join("config.json").exists() {
        let config = config::load(data_dir)?;
        store.ensure_account(&config)?;
    }
    Ok(store)
}

fn discover_tree(
    config: &Config,
    password: &str,
    selector: Option<&str>,
) -> Result<Vec<DirectoryNode>> {
    let (nodes, _) = discover_tree_with_session(config, password, selector)?;
    Ok(nodes)
}

fn discover_tree_with_session<'a>(
    config: &'a Config,
    password: &'a str,
    selector: Option<&str>,
) -> Result<(Vec<DirectoryNode>, provider::dav::DavSession<'a>)> {
    let mut session = provider::dav::DavSession::new(config, password)
        .map_err(|error| sanitize_error(error, password))?;
    let discovery = session
        .discover_full_tree(Kind::Contacts, &config.carddav_url)
        .map_err(|error| sanitize_error(error, password))?;
    let initial = discovery.collections;
    let initial_nodes = address_book_tree(&initial, Some(&config.carddav_url));
    let roots = if let Some(selector) = selector {
        if let Ok(selected) = select_node(&initial_nodes, selector) {
            vec![selected.id.clone()]
        } else if let Ok(url) = Url::parse(selector) {
            let configured = Url::parse(&config.carddav_url)?;
            if url.scheme() != configured.scheme()
                || url.host_str().map(str::to_lowercase)
                    != configured.host_str().map(str::to_lowercase)
                || url.port_or_known_default() != configured.port_or_known_default()
            {
                bail!("通訊錄 URL 必須與設定端點同 origin");
            }
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                bail!("通訊錄 URL 不可包含帳號、密碼、query 或 fragment");
            }
            vec![url.to_string()]
        } else {
            let roots = initial_nodes
                .iter()
                .map(|node| node.id.clone())
                .collect::<Vec<_>>();
            if roots.is_empty() {
                bail!("找不到指定的通訊錄或分支：{selector}");
            }
            roots
        }
    } else {
        initial_nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>()
    };
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    let mut collections = Vec::new();
    let mut known = HashSet::new();
    if selector.is_none() || discovery.complete {
        for collection in initial.iter().cloned() {
            known.insert(collection.id.clone());
            collections.push(collection);
        }
    }
    if discovery.complete && selector.is_none() {
        for collection in &collections {
            queue.push_back(collection.id.clone());
        }
    } else {
        for root in roots {
            queue.push_back(root);
        }
    }
    while let Some(url) = queue.pop_front() {
        if !visited.insert(url.clone()) {
            continue;
        }
        let discovered = session
            .discover(Kind::Contacts, &url)
            .map_err(|error| sanitize_error(error, password))?;
        for collection in discovered {
            if known.insert(collection.id.clone()) {
                queue.push_back(collection.id.clone());
                collections.push(collection);
            }
        }
    }
    if collections.is_empty() {
        collections = initial;
    }
    let nodes = address_book_tree(&collections, Some(&config.carddav_url));
    let Some(selector) = selector else {
        return Ok((nodes, session));
    };
    if let Ok(selected) = select_node(&nodes, selector) {
        let selected_path = selected.path.clone();
        let selected_id = selected.id.clone();
        return Ok((
            nodes
                .into_iter()
                .filter(|node| {
                    same_or_descendant_path(&node.path, &selected_path)
                        || same_or_descendant_url(&node.id, &selected_id)
                })
                .collect(),
            session,
        ));
    }
    if let Ok(url) = Url::parse(selector) {
        let normalized = normalize_url_prefix(url.as_str());
        return Ok((
            nodes
                .into_iter()
                .filter(|node| normalize_url_prefix(&node.id).starts_with(&normalized))
                .collect(),
            session,
        ));
    }
    bail!("找不到指定的通訊錄或分支：{selector}")
}

fn discover_self_targets_with_session<'a>(
    config: &'a Config,
    password: &'a str,
) -> Result<(Vec<DirectoryNode>, provider::dav::DavSession<'a>)> {
    let mut session = provider::dav::DavSession::new(config, password)
        .map_err(|error| sanitize_error(error, password))?;
    let collections = session
        .discover(Kind::Contacts, &config.carddav_url)
        .map_err(|error| sanitize_error(error, password))?;
    let nodes = address_book_tree(&collections, Some(&config.carddav_url));
    let profile = SiteProfile::for_config(Some(config))?;
    let root = profile.company_directory_root.clone();
    let known = nodes
        .iter()
        .filter(|node| {
            root.as_deref().is_some_and(|root| {
                node.path.eq_ignore_ascii_case(root) || node.name.eq_ignore_ascii_case(root)
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    if known.len() == 1 {
        return Ok((known, session));
    }
    let top_level = nodes
        .iter()
        .filter(|node| node.parent_path.is_none())
        .cloned()
        .collect::<Vec<_>>();
    if known.is_empty() && top_level.len() == 1 {
        return Ok((top_level, session));
    }
    if nodes.len() == 1 {
        return Ok((nodes, session));
    }
    if profile.is_generic() {
        bail!(
            "無法安全判定 self 的根通訊錄；請使用 --addressbook 指定分支。{}",
            crate::site::migration_hint()
        )
    }
    bail!("無法安全判定 self 的根通訊錄；請使用 --addressbook 指定分支")
}

pub fn validate_snapshot_scope(snapshot: &Snapshot, targets: &[DirectoryNode]) -> Result<()> {
    let allowed = targets
        .iter()
        .map(|target| normalize_url_prefix(&target.id))
        .collect::<Vec<_>>();
    for collection in &snapshot.collections {
        let in_scope = allowed.iter().any(|target| {
            let collection_id = normalize_url_prefix(&collection.id);
            collection_id == *target || collection_id.starts_with(target)
        });
        if !in_scope {
            bail!(
                "DAV 回應包含指定範圍外的通訊錄 collection：{}",
                collection.id
            );
        }
    }
    let collection_ids = snapshot
        .collections
        .iter()
        .map(|collection| collection.id.as_str())
        .collect::<HashSet<_>>();
    if snapshot
        .items
        .iter()
        .any(|item| !collection_ids.contains(item.collection.as_str()))
    {
        bail!("DAV 回應項目不屬於本次指定的通訊錄 collection");
    }
    Ok(())
}

fn select_sync_targets_with_session<'a>(
    config: &'a Config,
    password: &'a str,
    store: &Store,
    scope: DirectoryScope,
    selector: Option<&str>,
) -> Result<(Vec<DirectoryNode>, provider::dav::DavSession<'a>)> {
    match scope {
        DirectoryScope::Company => discover_tree_with_session(config, password, selector),
        DirectoryScope::SelfContact => {
            if let Some(selector) = selector {
                return discover_tree_with_session(config, password, Some(selector));
            }
            discover_self_targets_with_session(config, password)
        }
        DirectoryScope::Department => {
            if let Some(selector) = selector {
                return discover_tree_with_session(config, password, Some(selector));
            }
            let cached = store.list(Kind::Contacts, None, None, usize::MAX)?;
            let self_item = cached
                .iter()
                .find(|item| is_self_contact(item, config))
                .ok_or_else(|| {
                    anyhow!("找不到本機 self contact，請先執行 directory sync --scope self")
                })?;
            let department_url =
                ancestor_collection_url(&self_item.collection, 2).map_err(|_| {
                    anyhow!("找不到 self contact 的部門 membership，請使用 --addressbook 指定分支")
                })?;
            discover_tree_with_session(config, password, Some(&department_url))
        }
    }
}

fn hydrate_search_branch(
    config: &Config,
    password: &str,
    store: &mut Store,
    selector: Option<&str>,
) -> Result<()> {
    let (targets, session) = if let Some(selector) = selector {
        discover_tree_with_session(config, password, Some(selector))?
    } else {
        let cached = store.list(Kind::Contacts, None, None, usize::MAX)?;
        let self_item = cached
            .iter()
            .find(|item| is_self_contact(item, config))
            .ok_or_else(|| anyhow!("--online 需要 self contact 或 --addressbook"))?;
        let department_url = ancestor_collection_url(&self_item.collection, 2).map_err(|_| {
            anyhow!("找不到 self contact 的部門 membership，請使用 --addressbook 指定分支")
        })?;
        discover_tree_with_session(config, password, Some(&department_url))?
    };
    let existing_all = store.list(Kind::Contacts, None, None, usize::MAX)?;
    for target in targets {
        let existing = existing_all
            .iter()
            .filter(|item| item.collection == target.id)
            .cloned()
            .collect::<Vec<_>>();
        let snapshot = session
            .sync_collection(
                &Collection {
                    kind: Kind::Contacts,
                    id: target.id.clone(),
                    name: target.display_name.clone(),
                },
                &existing,
            )
            .map_err(|error| sanitize_error(error, password))?;
        validate_snapshot_scope(&snapshot, std::slice::from_ref(&target))?;
        for collection in snapshot.collections.clone() {
            let items = snapshot
                .items
                .iter()
                .filter(|item| item.collection == collection.id)
                .cloned()
                .collect::<Vec<_>>();
            store
                .apply_collection_snapshot(
                    &Snapshot {
                        kind: Kind::Contacts,
                        collections: vec![collection],
                        items,
                    },
                    Utc::now(),
                )
                .context("無法寫入通訊錄分支快取")?;
        }
    }
    Ok(())
}

fn search_contacts(
    store: &Store,
    nodes: &[DirectoryNode],
    query: &str,
    limit: usize,
) -> Result<Vec<DirectoryContact>> {
    let items = store.list(Kind::Contacts, None, None, usize::MAX)?;
    let normalized = normalize_identifier(query);
    let contacts = merge_contact_items(&items, nodes)
        .into_iter()
        .filter(|contact| {
            let values =
                std::iter::once(contact.name.as_str())
                    .chain(contact.emails.iter().map(String::as_str))
                    .chain(contact.phones.iter().map(String::as_str))
                    .chain(contact.organization.as_deref())
                    .chain(contact.title.as_deref())
                    .chain(contact.structured_name.as_deref())
                    .chain(contact.nickname.as_deref())
                    .chain(contact.note.as_deref())
                    .chain(contact.memberships.iter().flat_map(|membership| {
                        [membership.path.as_str(), membership.name.as_str()]
                    }));
            values
                .into_iter()
                .any(|value| normalize_identifier(value).contains(&normalized))
        })
        .take(limit.max(1))
        .collect::<Vec<_>>();
    Ok(contacts)
}

fn select_node<'a>(nodes: &'a [DirectoryNode], selector: &str) -> Result<&'a DirectoryNode> {
    let normalized = normalize_identifier(selector);
    let matches = nodes
        .iter()
        .filter(|node| {
            [
                node.id.as_str(),
                node.path.as_str(),
                node.name.as_str(),
                node.display_name.as_str(),
            ]
            .iter()
            .any(|value| normalize_identifier(value) == normalized)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [node] => Ok(node),
        [] => Err(anyhow!("找不到指定的通訊錄或分支：{selector}")),
        _ => Err(anyhow!("指定的通訊錄不唯一：{selector}")),
    }
}

fn is_self_contact(item: &Item, config: &Config) -> bool {
    let email = normalize_identifier(&config.email);
    let username = normalize_identifier(&config.username);
    item.emails.iter().any(|value| {
        let normalized = normalize_identifier(value);
        !normalized.is_empty() && (normalized == email || normalized == username)
    })
}

fn ancestor_collection_url(collection: &str, depth: usize) -> Result<String> {
    if depth == 0 {
        bail!("通訊錄分支深度必須大於 0");
    }
    let mut url =
        Url::parse(collection).with_context(|| format!("通訊錄 URL 格式錯誤：{collection}"))?;
    let segments = url
        .path_segments()
        .ok_or_else(|| anyhow!("通訊錄 URL 缺少 path：{collection}"))?
        .filter(|segment| !segment.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let branch_start = if let Some(index) = segments
        .iter()
        .rposition(|segment| segment.eq_ignore_ascii_case("addressbooks"))
    {
        if segments.get(index + 1).is_some_and(|segment| {
            segment.contains('@') || segment.to_lowercase().starts_with("mpa.")
        }) {
            index + 2
        } else {
            index + 1
        }
    } else if let Some(index) = segments
        .iter()
        .rposition(|segment| segment.eq_ignore_ascii_case("carddav"))
    {
        if segments
            .get(index + 1)
            .is_some_and(|segment| segment.eq_ignore_ascii_case("home"))
        {
            index + 2
        } else {
            index + 1
        }
    } else {
        0
    };
    if branch_start >= segments.len() || segments.len() - branch_start < depth {
        bail!("無法從通訊錄推導部門分支：{collection}");
    }
    let mut selected = segments[..branch_start].to_vec();
    selected.extend_from_slice(&segments[branch_start..branch_start + depth]);
    let path = format!("/{}/", selected.join("/"));
    url.set_path(&path);
    Ok(url.to_string())
}

fn parse_display_name(value: &str) -> (String, Option<String>) {
    let value = value.trim();
    if let Some(open) = value.rfind("(") {
        if value.ends_with(')') {
            let name = value[..open].trim().to_owned();
            let path = value[open + 1..value.len() - 1].trim();
            if path.starts_with('/') && !path.contains("..") {
                return (name, Some(normalize_path(path)));
            }
        }
    }
    (value.to_owned(), None)
}

fn relative_collection_path(base: &Option<Url>, id: &str) -> Option<String> {
    let url = Url::parse(id).ok()?;
    let id_segments = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if let Some(base) = base {
        let base_segments = base
            .path_segments()
            .into_iter()
            .flatten()
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if id_segments.starts_with(&base_segments) && id_segments.len() > base_segments.len() {
            return Some(id_segments[base_segments.len()..].join("/"));
        }
    }
    if let Some(index) = id_segments
        .iter()
        .rposition(|segment| segment.eq_ignore_ascii_case("addressbooks"))
    {
        if id_segments.len() > index + 2 {
            return Some(id_segments[index + 2..].join("/"));
        }
    }
    id_segments.last().map(|segment| (*segment).to_owned())
}

fn normalize_path(value: &str) -> String {
    value
        .trim()
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn same_or_descendant_path(value: &str, parent: &str) -> bool {
    value == parent || value.starts_with(&format!("{parent}/"))
}

fn normalize_url_prefix(value: &str) -> String {
    value.trim_end_matches('/').to_owned() + "/"
}

fn same_or_descendant_url(value: &str, parent: &str) -> bool {
    let value = normalize_url_prefix(value);
    let parent = normalize_url_prefix(parent);
    value.starts_with(&parent)
}

fn path_segments(value: &str) -> Vec<String> {
    value
        .split('/')
        .filter(|part| !part.trim().is_empty())
        .map(|part| part.trim().to_owned())
        .collect()
}

pub fn contact_summaries_page(
    store: &Store,
    query: Option<&str>,
    collection: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<crate::store::ItemPage> {
    store.contact_page(query, collection, limit, offset)
}

pub fn contact_to_item(contact: &DirectoryContact) -> Item {
    let collection = match contact.memberships.as_slice() {
        [] => String::new(),
        [one] => one.path.clone(),
        many => format!("{} 個資料夾", many.len()),
    };
    Item {
        id: contact.id.clone(),
        kind: Kind::Contacts,
        collection,
        remote_id: contact.id.clone(),
        etag: None,
        uid_validity: None,
        title: contact.name.clone(),
        subtitle: contact_subtitle(contact),
        date: None,
        end_date: None,
        text: format_contact_detail(contact),
        raw: String::new(),
        flags: Vec::new(),
        emails: contact.emails.clone(),
        phones: contact.phones.clone(),
        attachments: Vec::new(),
    }
}

pub fn format_contact_detail(contact: &DirectoryContact) -> String {
    let mut lines = Vec::new();
    lines.push(contact.name.clone());
    if let Some(name) = contact
        .structured_name
        .as_deref()
        .map(str::trim)
        .filter(|value| {
            !value.is_empty() && normalize_identifier(value) != normalize_identifier(&contact.name)
        })
    {
        lines.push(format!("顯示名：{name}"));
    }
    if let Some(nickname) = contact
        .nickname
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if normalize_identifier(nickname) != normalize_identifier(&contact.name) {
            lines.push(format!("暱稱：{nickname}"));
        }
    }
    if let Some(title) = contact
        .title
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        lines.push(format!("職稱：{title}"));
    }
    if let Some(org) = contact
        .organization
        .as_deref()
        .and_then(meaningful_vcard_component)
    {
        lines.push(format!("部門／組織：{org}"));
    }
    if !contact.emails.is_empty() {
        lines.push(format!("Email：{}", contact.emails.join(", ")));
    }
    if !contact.phones.is_empty() {
        lines.push(format!("電話：{}", contact.phones.join(", ")));
    }
    if !contact.memberships.is_empty() {
        lines.push("資料夾／群組：".to_owned());
        for membership in &contact.memberships {
            let label = if membership.path.is_empty() {
                membership.name.as_str()
            } else {
                membership.path.as_str()
            };
            lines.push(format!("  · {label}"));
        }
    }
    if let Some(note) = contact
        .note
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        lines.push(String::new());
        lines.push(note.to_owned());
    }
    lines.join("\n")
}

fn contact_subtitle(contact: &DirectoryContact) -> String {
    contact
        .structured_name
        .as_deref()
        .and_then(meaningful_vcard_component)
        .filter(|value| normalize_identifier(value) != normalize_identifier(&contact.name))
        .or_else(|| contact.emails.first().cloned())
        .or_else(|| contact.phones.first().cloned())
        .or_else(|| contact.title.clone())
        .or_else(|| {
            contact
                .organization
                .as_deref()
                .and_then(meaningful_vcard_component)
        })
        .unwrap_or_default()
}

fn apply_department_fallback(contact: &mut DirectoryContact) {
    if contact
        .organization
        .as_deref()
        .and_then(meaningful_vcard_component)
        .is_some()
    {
        return;
    }
    if let Some(department) = department_from_memberships(&contact.memberships) {
        contact.organization = Some(department);
    }
}

pub fn department_from_memberships(memberships: &[DirectoryMembership]) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for membership in memberships {
        let path = normalize_path(&membership.path);
        if path.is_empty() {
            continue;
        }
        let segments = path_segments(&path);
        if segments.is_empty() {
            continue;
        }
        // The top-level address book is the directory root (the company), not a department.
        if segments.len() == 1 {
            continue;
        }
        let display = segments[1..].join("/");
        if display.is_empty() {
            continue;
        }
        let depth = segments.len();
        match &best {
            Some((best_depth, _)) if *best_depth >= depth => {}
            _ => best = Some((depth, display)),
        }
    }
    best.map(|(_, path)| path)
}

fn meaningful_vcard_component(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let meaningful = trimmed
        .split(';')
        .map(str::trim)
        .any(|part| !part.is_empty());
    meaningful.then(|| trimmed.to_owned())
}

fn format_vcard_n(value: &str) -> Option<String> {
    let parts = value
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

pub(crate) fn contact_identity(item: &Item) -> String {
    if let Some(uid) = vcard_property(&item.raw, "UID") {
        if !uid.trim().is_empty() {
            return format!("uid:{}", normalize_identifier(&uid));
        }
    }
    if let Some(email) = item.emails.iter().find(|email| !email.trim().is_empty()) {
        return format!("email:{}", normalize_identifier(email));
    }
    format!("item:{}", item.id)
}

fn vcard_property(raw: &str, name: &str) -> Option<String> {
    let lines = raw.replace("\r\n ", "").replace("\r\n\t", "");
    lines.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        let field = key.split(';').next()?.trim();
        field
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

fn normalize_identifier(value: &str) -> String {
    value.trim().to_lowercase()
}

fn extend_unique<I>(target: &mut Vec<String>, values: I)
where
    I: IntoIterator<Item = String>,
{
    for value in values {
        if !value.is_empty()
            && !target
                .iter()
                .any(|current| normalize_identifier(current) == normalize_identifier(&value))
        {
            target.push(value);
        }
    }
    target.sort_by_key(|value| normalize_identifier(value));
}

fn sanitize_error(error: anyhow::Error, password: &str) -> anyhow::Error {
    let message = format!("{error:#}");
    if password.is_empty() {
        anyhow!(message)
    } else {
        anyhow!(message.replace(password, "[已隱藏]"))
    }
}

fn emit_tree(result: &DirectoryTreeResult, json: bool) -> Result<()> {
    if json {
        emit_value(result, true)
    } else {
        for node in &result.nodes {
            println!("{}{} [{}]", "  ".repeat(node.depth), node.name, node.path);
        }
        Ok(())
    }
}

fn emit_value<T: Serialize>(value: &T, _json: bool) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
