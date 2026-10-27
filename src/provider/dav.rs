use crate::{
    config::Config,
    model::{Collection, Item, Kind, Snapshot},
};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use ical::{IcalParser, VcardParser};
use reqwest::{
    blocking::{Client, RequestBuilder, Response},
    header::{HeaderMap, ACCEPT, CONTENT_TYPE, ETAG, IF_MATCH, IF_NONE_MATCH, LOCATION},
    redirect::Policy,
    Method, StatusCode, Url,
};
use roxmltree::{Document, Node};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error as StdError,
    fmt,
    io::BufReader,
    net::IpAddr,
    time::Duration,
};

const DAV_TIMEOUT: Duration = Duration::from_secs(20);
const USER_AGENT: &str = "m2k/0.1 DAV client";
const SYNC_COLLECTION_MAX_PAGES: usize = 64;

#[derive(Debug, Clone)]
struct DavResource {
    href: Url,
    display_name: Option<String>,
    etag: Option<String>,
    data: Option<String>,
    is_collection: bool,
    resource_kind: Option<Kind>,
    home_sets: Vec<Url>,
}

#[derive(Debug, Default)]
struct DavListing {
    resources: Vec<DavResource>,
    denied: Vec<(Url, u16)>,
    missing: Vec<Url>,
}

impl DavListing {
    fn ensure_readable(&self) -> Result<()> {
        self.ensure_permissions()?;
        if let Some(href) = self.missing.first() {
            bail!("DAV 資源缺少 200 OK propstat：404 {href}");
        }
        Ok(())
    }

    fn ensure_permissions(&self) -> Result<()> {
        if !self.denied.is_empty() {
            let details = self
                .denied
                .iter()
                .map(|(href, status)| format!("{status} {href}"))
                .collect::<Vec<_>>()
                .join(", ");
            bail!("DAV 權限不足，無法讀取資源：{details}");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ParsedResource {
    item: Item,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DavOperation {
    Propfind,
    Report,
    Put,
    Delete,
}

#[derive(Clone, Copy)]
enum CalendarSyncScope {
    All,
    Since(DateTime<Utc>),
    Window(DateTime<Utc>, DateTime<Utc>),
}

/// Calendar snapshot plus the RFC 6578 sync-token of every collection in it.
/// `sync_tokens` is empty unless every collection advertised `sync-collection`.
#[derive(Debug)]
pub struct CalendarSync {
    pub snapshot: Snapshot,
    pub sync_tokens: BTreeMap<String, String>,
}

#[derive(Debug)]
pub enum CalendarChanges {
    Synced(CalendarSync),
    /// A discovered collection has no stored token; keep the ETag strategy.
    Unavailable,
    /// The server rejected a stored token with `DAV:valid-sync-token`.
    TokenInvalid,
    /// The server answered sync-collection in a way m2k cannot trust (non-207,
    /// non-standard multistatus, missing token, ...); drop the token and use
    /// the ETag strategy. Authentication and connection errors stay errors.
    Unusable(String),
}

struct SyncCollectionPage {
    changed: Vec<(Url, Option<String>)>,
    removed: Vec<Url>,
    token: String,
    truncated: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct RemoteWritePendingReconcile {
    pub kind: Kind,
    pub collection: String,
    pub resource_url: String,
    pub uid: Option<String>,
    pub operation: String,
    pub status: String,
    pub remote_accepted: bool,
    pub reconcile_required: bool,
    pub reason: String,
}

impl fmt::Display for RemoteWritePendingReconcile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "DAV 遠端寫入已接受，但本機 reconciliation 未完成：{}；原因：{}；請重新同步",
            self.resource_url, self.reason
        )
    }
}

impl StdError for RemoteWritePendingReconcile {}

#[derive(Clone)]
pub struct DavSession<'a> {
    client: Client,
    config: &'a Config,
    password: &'a str,
    known: HashMap<(Kind, String), Collection>,
}

#[derive(Debug, Clone)]
pub struct TreeDiscovery {
    pub collections: Vec<Collection>,
    pub complete: bool,
}

impl<'a> DavSession<'a> {
    pub fn new(config: &'a Config, password: &'a str) -> Result<Self> {
        Ok(Self {
            client: build_client()?,
            config,
            password,
            known: HashMap::new(),
        })
    }

    pub fn discover(&mut self, kind: Kind, url: &str) -> Result<Vec<Collection>> {
        if kind == Kind::Mail {
            bail!("Mail 不使用 DAV 協定");
        }
        let configured = configured_url(self.config, kind)?;
        let probe = resolve_endpoint(&configured, url)?;
        let collections = discover_with_client(
            &self.client,
            self.config,
            self.password,
            &configured,
            &probe,
            kind,
        )?;
        self.remember(&collections)?;
        Ok(collections)
    }

    pub fn discover_full_tree(&mut self, kind: Kind, url: &str) -> Result<TreeDiscovery> {
        if kind == Kind::Mail {
            bail!("Mail 不使用 DAV 協定");
        }
        if kind != Kind::Contacts {
            return Ok(TreeDiscovery {
                collections: self.discover(kind, url)?,
                complete: false,
            });
        }
        let configured = configured_url(self.config, kind)?;
        let probe = resolve_endpoint(&configured, url)?;
        let (collections, complete) = discover_full_tree_with_client(
            &self.client,
            self.config,
            self.password,
            &configured,
            &probe,
            kind,
        )?;
        if complete {
            self.remember(&collections)?;
        }
        Ok(TreeDiscovery {
            collections,
            complete,
        })
    }

    pub fn sync_collection(&self, collection: &Collection, existing: &[Item]) -> Result<Snapshot> {
        if collection.kind == Kind::Mail {
            bail!("Mail 不使用 DAV 協定");
        }
        let verified = self
            .known_collection(collection.kind, &collection.id)?
            .ok_or_else(|| anyhow!("DAV collection 尚未由本 session 驗證：{}", collection.id))?;
        let configured = configured_url(self.config, verified.kind)?;
        if verified.kind == Kind::Calendar {
            return sync_calendar_collections(
                &self.client,
                self.config,
                self.password,
                &configured,
                vec![verified],
                existing,
                CalendarSyncScope::All,
            );
        }
        sync_collections(verified.kind, vec![verified], existing, |collection_url| {
            report(
                &self.client,
                self.config,
                self.password,
                &configured,
                collection_url,
                collection.kind,
            )
        })
    }

    pub fn query_contacts(&self, collection: &Collection, email: &str) -> Result<Snapshot> {
        if collection.kind != Kind::Contacts {
            bail!("指定的 DAV collection 不是聯絡人集合");
        }
        if email.trim().is_empty() {
            bail!("聯絡人查詢電子郵件不可為空白");
        }
        let verified = self
            .known_collection(collection.kind, &collection.id)?
            .ok_or_else(|| anyhow!("DAV collection 尚未由本 session 驗證：{}", collection.id))?;
        let configured = configured_url(self.config, Kind::Contacts)?;
        let collection_url = Url::parse(&verified.id)
            .with_context(|| format!("DAV collection URL 格式錯誤：{}", verified.id))?;
        let resources = report_contacts_query(
            &self.client,
            self.config,
            self.password,
            &configured,
            &collection_url,
            email.trim(),
        )?;
        let collection_id = canonical_collection_id(&collection_url);
        let items = resources
            .into_iter()
            .map(|resource| {
                let remote_url = resource_url(&collection_url, resource.href.as_str())?;
                parse_resource(
                    Kind::Contacts,
                    collection_id.clone(),
                    remote_url.to_string(),
                    resource.etag,
                    resource
                        .data
                        .ok_or_else(|| anyhow!("DAV 資源缺少 address-data"))?,
                    None,
                )
                .map(|parsed| parsed.item)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Snapshot {
            kind: Kind::Contacts,
            collections: vec![Collection {
                kind: Kind::Contacts,
                id: collection_id,
                name: verified.name,
            }],
            items,
        })
    }

    pub fn fetch_range(
        &mut self,
        url: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        expand: bool,
    ) -> Result<Vec<Item>> {
        if start >= end {
            bail!("行事曆時間範圍必須由早到晚");
        }
        let collection = match self.known_collection(Kind::Calendar, url)? {
            Some(collection) => collection,
            None => {
                self.discover(Kind::Calendar, url)?;
                self.known_collection(Kind::Calendar, url)?
                    .ok_or_else(|| anyhow!("指定的 DAV collection 未被 discovery 驗證：{url}"))?
            }
        };
        self.fetch_range_on_collection(&collection, start, end, expand)
    }

    pub fn put(
        &mut self,
        kind: Kind,
        collection: &str,
        raw: &str,
        existing: Option<&Item>,
    ) -> Result<Item> {
        if kind == Kind::Mail {
            bail!("Mail 不使用 DAV 協定");
        }
        if raw.trim().is_empty() {
            bail!("DAV 資源內容不可為空白");
        }
        validate_calendar_write(kind, raw)?;
        let verified = match self.known_collection(kind, collection)? {
            Some(collection) => collection,
            None => {
                self.discover(kind, collection)?;
                self.known_collection(kind, collection)?.ok_or_else(|| {
                    anyhow!("指定的 DAV collection 未被 discovery 驗證：{collection}")
                })?
            }
        };
        let collection_url = Url::parse(&verified.id)
            .with_context(|| format!("DAV collection URL 格式錯誤：{}", verified.id))?;
        put_with_session(self, kind, collection_url, raw, existing)
    }

    fn fetch_range_on_collection(
        &self,
        collection: &Collection,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        expand: bool,
    ) -> Result<Vec<Item>> {
        let configured = configured_url(self.config, Kind::Calendar)?;
        let collection_url = Url::parse(&collection.id)
            .with_context(|| format!("DAV collection URL 格式錯誤：{}", collection.id))?;
        let resources = report_range(
            &self.client,
            self.config,
            self.password,
            &configured,
            &collection_url,
            (start, end),
            expand,
        )?;
        resources
            .into_iter()
            .map(|resource| {
                let remote_url = resource_url(&collection_url, resource.href.as_str())?;
                parse_resource(
                    Kind::Calendar,
                    canonical_collection_id(&collection_url),
                    remote_url.to_string(),
                    resource.etag,
                    resource
                        .data
                        .ok_or_else(|| anyhow!("DAV 資源缺少 calendar-data"))?,
                    None,
                )
                .map(|parsed| parsed.item)
            })
            .collect()
    }

    fn known_collection(&self, kind: Kind, url: &str) -> Result<Option<Collection>> {
        let configured = configured_url(self.config, kind)?;
        let resolved = resolve_endpoint(&configured, url)?;
        let key = (kind, canonical_collection_id(&resolved));
        Ok(self.known.get(&key).cloned())
    }

    fn remember(&mut self, collections: &[Collection]) -> Result<()> {
        for collection in collections {
            let configured = configured_url(self.config, collection.kind)?;
            let resolved = resolve_endpoint(&configured, &collection.id)?;
            let id = canonical_collection_id(&resolved);
            self.known
                .entry((collection.kind, id.clone()))
                .or_insert_with(|| Collection {
                    kind: collection.kind,
                    id,
                    name: collection.name.clone(),
                });
        }
        Ok(())
    }
}

pub fn verify(config: &Config, password: &str, kind: Kind) -> Result<()> {
    if kind == Kind::Mail {
        bail!("Mail 不使用 DAV 協定");
    }
    let url = configured_url(config, kind)?;
    let collections = discover(config, password, kind, url.as_str())?;
    if collections.is_empty() {
        bail!("找不到可用的 {} DAV collection", kind);
    }
    Ok(())
}

pub fn sync(config: &Config, password: &str, kind: Kind, existing: &[Item]) -> Result<Snapshot> {
    if kind == Kind::Mail {
        bail!("Mail 不使用 DAV 協定");
    }
    let url = configured_url(config, kind)?;
    if kind == Kind::Calendar {
        return sync_calendar_retention(config, password, &url, existing);
    }
    sync_collection(config, password, kind, url.as_str(), existing)
}

pub fn discover(config: &Config, password: &str, kind: Kind, url: &str) -> Result<Vec<Collection>> {
    let mut session = DavSession::new(config, password)?;
    session.discover(kind, url)
}

pub fn sync_collection(
    config: &Config,
    password: &str,
    kind: Kind,
    url: &str,
    existing: &[Item],
) -> Result<Snapshot> {
    let mut session = DavSession::new(config, password)?;
    let collections = session.discover(kind, url)?;
    if collections.is_empty() {
        bail!("找不到可用的 {} DAV collection", kind);
    }
    let snapshots = collections
        .iter()
        .map(|collection| session.sync_collection(collection, existing))
        .collect::<Result<Vec<_>>>()?;
    Ok(merge_snapshots(kind, snapshots))
}

fn sync_calendar_retention(
    config: &Config,
    password: &str,
    configured: &Url,
    existing: &[Item],
) -> Result<Snapshot> {
    sync_calendar_incremental(
        config,
        password,
        configured,
        existing,
        retention_scope(config)?,
    )
}

fn retention_scope(config: &Config) -> Result<CalendarSyncScope> {
    Ok(if config.calendar_retention_days == 0 {
        CalendarSyncScope::All
    } else {
        CalendarSyncScope::Since(
            Utc::now()
                .checked_sub_signed(ChronoDuration::days(i64::from(
                    config.calendar_retention_days,
                )))
                .ok_or_else(|| anyhow!("行事曆保留天數超過可表示的日期範圍"))?,
        )
    })
}

/// Full retention sync (same requests as [`sync`]) that also probes each
/// collection for an RFC 6578 sync-token before listing it, so changes made
/// during the listing are replayed by the next [`sync_calendar_changes`].
pub fn sync_calendar_full(
    config: &Config,
    password: &str,
    existing: &[Item],
) -> Result<CalendarSync> {
    let configured = configured_url(config, Kind::Calendar)?;
    let client = build_client()?;
    let collections = discover_with_client(
        &client,
        config,
        password,
        &configured,
        &configured,
        Kind::Calendar,
    )?;
    if collections.is_empty() {
        bail!("找不到可用的 calendar DAV collection");
    }
    let mut sync_tokens = BTreeMap::new();
    for collection in &collections {
        let url = Url::parse(&collection.id)
            .with_context(|| format!("DAV collection URL 格式錯誤：{}", collection.id))?;
        match probe_sync_token(&client, config, password, &configured, &url) {
            Some(token) => {
                sync_tokens.insert(collection.id.clone(), token);
            }
            None => {
                sync_tokens.clear();
                break;
            }
        }
    }
    let snapshot = sync_calendar_collections(
        &client,
        config,
        password,
        &configured,
        collections,
        existing,
        retention_scope(config)?,
    )?;
    Ok(CalendarSync {
        snapshot,
        sync_tokens,
    })
}

/// Probes whether every configured calendar collection supports RFC 6578
/// `sync-collection`; returns (supported, total). Read-only, for `doctor`.
pub fn probe_calendar_sync_tokens(config: &Config, password: &str) -> Result<(usize, usize)> {
    let configured = configured_url(config, Kind::Calendar)?;
    let client = build_client()?;
    let collections = discover_with_client(
        &client,
        config,
        password,
        &configured,
        &configured,
        Kind::Calendar,
    )?;
    let mut supported = 0;
    for collection in &collections {
        let url = Url::parse(&collection.id)
            .with_context(|| format!("DAV collection URL 格式錯誤：{}", collection.id))?;
        if probe_sync_token(&client, config, password, &configured, &url).is_some() {
            supported += 1;
        }
    }
    Ok((supported, collections.len()))
}

/// Incremental calendar sync driven by stored RFC 6578 sync-tokens. The
/// returned snapshot covers each whole collection (cached items minus
/// reported deletions plus re-fetched changes), not just a time window.
pub fn sync_calendar_changes(
    config: &Config,
    password: &str,
    existing: &[Item],
    tokens: &BTreeMap<String, String>,
) -> Result<CalendarChanges> {
    let configured = configured_url(config, Kind::Calendar)?;
    let client = build_client()?;
    let collections = discover_with_client(
        &client,
        config,
        password,
        &configured,
        &configured,
        Kind::Calendar,
    )?;
    if collections.is_empty() {
        bail!("找不到可用的 calendar DAV collection");
    }
    if collections
        .iter()
        .any(|collection| !tokens.contains_key(&collection.id))
    {
        return Ok(CalendarChanges::Unavailable);
    }
    let mut items = Vec::new();
    let mut sync_tokens = BTreeMap::new();
    for collection in &collections {
        let url = Url::parse(&collection.id)
            .with_context(|| format!("DAV collection URL 格式錯誤：{}", collection.id))?;
        let changes = calendar_collection_changes(
            &client,
            config,
            password,
            &configured,
            &url,
            &tokens[&collection.id],
            existing,
        );
        let (collection_items, token) = match changes {
            Ok(Some(changes)) => changes,
            Ok(None) => return Ok(CalendarChanges::TokenInvalid),
            Err(error) if is_auth_or_network(&error) => return Err(error),
            Err(error) => return Ok(CalendarChanges::Unusable(format!("{error:#}"))),
        };
        items.extend(collection_items);
        sync_tokens.insert(collection.id.clone(), token);
    }
    Ok(CalendarChanges::Synced(CalendarSync {
        snapshot: Snapshot {
            kind: Kind::Calendar,
            collections,
            items,
        },
        sync_tokens,
    }))
}

fn calendar_collection_changes(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection_url: &Url,
    token: &str,
    existing: &[Item],
) -> Result<Option<(Vec<Item>, String)>> {
    let mut current = existing
        .iter()
        .filter(|item| {
            item.kind == Kind::Calendar && collection_matches(&item.collection, collection_url)
        })
        .map(|item| {
            // Unresolvable hrefs are kept as-is: only a reported 404/410 deletes.
            let key = resource_url(collection_url, &item.remote_id).map_or_else(
                |_| format!("unresolved:{}", item.remote_id),
                |url| resource_key(&url),
            );
            (key, item.clone())
        })
        .collect::<BTreeMap<_, _>>();
    let mut token = token.to_owned();
    for _ in 0..SYNC_COLLECTION_MAX_PAGES {
        let Some(page) =
            sync_collection_report(client, config, password, configured, collection_url, &token)?
        else {
            return Ok(None);
        };
        for href in page.removed {
            current.remove(&resource_key(&resource_url(collection_url, href.as_str())?));
        }
        for (href, etag) in page.changed {
            let remote_url = resource_url(collection_url, href.as_str())?;
            let remote_id = remote_url.to_string();
            let key = resource_key(&remote_url);
            let cached = current.get(&key).cloned();
            if cached.as_ref().is_some_and(|item| {
                !item.raw.trim().is_empty()
                    && nonempty_etag(item.etag.as_deref()).is_some()
                    && nonempty_etag(item.etag.as_deref()) == nonempty_etag(etag.as_deref())
            }) {
                continue;
            }
            let listed = DavResource {
                href: remote_url.clone(),
                display_name: None,
                etag,
                data: None,
                is_collection: false,
                resource_kind: None,
                home_sets: Vec::new(),
            };
            match calendar_get_optional(
                client,
                config,
                password,
                configured,
                collection_url,
                &remote_url,
                &listed,
            )? {
                Some(fetched) => {
                    let parsed = parse_resource(
                        Kind::Calendar,
                        canonical_collection_id(collection_url),
                        remote_id.clone(),
                        fetched
                            .etag
                            .filter(|etag| nonempty_etag(Some(etag.as_str())).is_some()),
                        fetched.data.context("DAV 行事曆資源缺少內容")?,
                        cached.as_ref(),
                    )?;
                    current.insert(key, parsed.item);
                }
                None => {
                    current.remove(&key);
                }
            }
        }
        token = page.token;
        if !page.truncated {
            let collection = canonical_collection_id(collection_url);
            let items = current
                .into_values()
                .map(|mut item| {
                    item.collection = collection.clone();
                    item
                })
                .collect();
            return Ok(Some((items, token)));
        }
    }
    bail!("DAV sync-collection 分頁超過 {SYNC_COLLECTION_MAX_PAGES} 次上限：{collection_url}")
}

/// Same-origin resource identity that treats `%40` and `@` hrefs alike.
fn resource_key(url: &Url) -> String {
    crate::dav_url::normalized_path(url.path())
}

fn is_auth_or_network(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.is::<reqwest::Error>()
            || cause
                .to_string()
                .contains(&StatusCode::UNAUTHORIZED.to_string())
    })
}

/// Depth-0 PROPFIND for `DAV:sync-token` and `DAV:supported-report-set`.
/// Any failure means "unsupported": probing must never fail a sync.
fn probe_sync_token(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
) -> Option<String> {
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::from_bytes(b"PROPFIND").expect("PROPFIND method"),
        collection,
        DavOperation::Propfind,
    )
    .ok()?
    .header("Depth", "0")
    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
    .header(ACCEPT, "application/xml, text/xml")
    .body("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:propfind xmlns:D=\"DAV:\"><D:prop><D:sync-token/><D:supported-report-set/></D:prop></D:propfind>");
    let response = send(request).ok()?;
    let (_, body) = require_status(response, &[StatusCode::MULTI_STATUS], "DAV PROPFIND").ok()?;
    parse_sync_token_probe(&body, collection)
}

fn parse_sync_token_probe(body: &str, collection: &Url) -> Option<String> {
    let document = Document::parse(body).ok()?;
    let root = document.root_element();
    if !is_element_named(root, "multistatus") {
        return None;
    }
    let response = root.children().find(|response| {
        is_element_named(*response, "response")
            && direct_child(*response, "href")
                .and_then(|href| resolve_href(collection, text_content(href).trim()).ok())
                .is_some_and(|href| same_url(&href, collection))
    })?;
    let properties = response
        .children()
        .filter(|node| is_element_named(*node, "propstat"))
        .filter(|propstat| {
            direct_child(*propstat, "status").and_then(|status| status_code(&text_content(status)))
                == Some(200)
        })
        .filter_map(|propstat| direct_child(propstat, "prop"))
        .collect::<Vec<_>>();
    let supports_report = properties.iter().any(|prop| {
        prop.children()
            .filter(|node| is_element_named(*node, "supported-report-set"))
            .any(|set| {
                set.descendants()
                    .any(|node| is_element_named(node, "sync-collection"))
            })
    });
    let token = property_text(&properties, "sync-token")?;
    (supports_report && !token.trim().is_empty()).then(|| token.trim().to_owned())
}

/// One RFC 6578 `sync-collection` REPORT page. `Ok(None)` means the server
/// rejected the token and the caller must fall back to a full sync.
fn sync_collection_report(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
    token: &str,
) -> Result<Option<SyncCollectionPage>> {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:sync-collection xmlns:D=\"DAV:\"><D:sync-token>{}</D:sync-token><D:sync-level>1</D:sync-level><D:prop><D:getetag/></D:prop></D:sync-collection>",
        xml_escape(token)
    );
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::from_bytes(b"REPORT").expect("REPORT method"),
        collection,
        DavOperation::Report,
    )?
    .header("Depth", "0")
    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
    .header(ACCEPT, "application/xml, text/xml")
    .body(body);
    let response = send(request)?;
    let status = response.status();
    if status != StatusCode::MULTI_STATUS {
        let body = response.text().unwrap_or_default();
        if status.is_client_error() && body.contains("valid-sync-token") {
            return Ok(None);
        }
        bail!("DAV sync-collection REPORT 回應狀態錯誤：{status}");
    }
    let body = response
        .text()
        .context("DAV sync-collection REPORT 回應讀取失敗")?;
    parse_sync_collection(&body, configured, collection).map(Some)
}

fn parse_sync_collection(
    body: &str,
    configured: &Url,
    collection: &Url,
) -> Result<SyncCollectionPage> {
    let document = Document::parse(body).map_err(|error| anyhow!("DAV XML 格式錯誤：{error}"))?;
    let root = document.root_element();
    if !is_element_named(root, "multistatus") {
        bail!("DAV 回應缺少 multistatus 根節點");
    }
    let token = direct_child(root, "sync-token")
        .map(text_content)
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow!("DAV sync-collection 回應缺少 sync-token"))?;
    let mut page = SyncCollectionPage {
        changed: Vec::new(),
        removed: Vec::new(),
        token,
        truncated: false,
    };
    let mut denied = Vec::new();
    for response in root
        .children()
        .filter(|node| is_element_named(*node, "response"))
    {
        let href_node =
            direct_child(response, "href").ok_or_else(|| anyhow!("DAV response 缺少 href"))?;
        let href = resolve_href(collection, text_content(href_node).trim())?;
        ensure_same_origin(configured, &href)?;
        let is_collection = same_url(&href, collection);
        if let Some(status) = direct_child(response, "status") {
            let code = status_code(&text_content(status))
                .ok_or_else(|| anyhow!("DAV response status 格式錯誤：{href}"))?;
            match code {
                507 if is_collection => page.truncated = true,
                404 | 410 if !is_collection => page.removed.push(href),
                401 | 403 => denied.push(format!("{code} {href}")),
                _ => bail!("DAV sync-collection 回應狀態不受支援：{code} {href}"),
            }
            continue;
        }
        if is_collection || href.path().ends_with('/') {
            continue;
        }
        let mut etag = None;
        let mut readable = false;
        let mut forbidden = None;
        for propstat in response
            .children()
            .filter(|node| is_element_named(*node, "propstat"))
        {
            let code = direct_child(propstat, "status")
                .and_then(|status| status_code(&text_content(status)))
                .ok_or_else(|| anyhow!("DAV propstat status 格式錯誤：{href}"))?;
            match code {
                200 => {
                    readable = true;
                    etag = direct_child(propstat, "prop")
                        .and_then(|prop| property_text(&[prop], "getetag"));
                }
                401 | 403 => forbidden = Some(code),
                _ => {}
            }
        }
        match forbidden {
            Some(code) if !readable => denied.push(format!("{code} {href}")),
            _ => page.changed.push((href, etag)),
        }
    }
    if !denied.is_empty() {
        bail!("DAV 權限不足，無法讀取資源：{}", denied.join(", "));
    }
    Ok(page)
}

pub fn sync_calendar_window(
    config: &Config,
    password: &str,
    existing: &[Item],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Snapshot> {
    if start >= end {
        bail!("行事曆同步時間範圍必須由早到晚");
    }
    let configured = configured_url(config, Kind::Calendar)?;
    sync_calendar_incremental(
        config,
        password,
        &configured,
        existing,
        CalendarSyncScope::Window(start, end),
    )
}

fn sync_calendar_incremental(
    config: &Config,
    password: &str,
    configured: &Url,
    existing: &[Item],
    scope: CalendarSyncScope,
) -> Result<Snapshot> {
    let client = build_client()?;
    let collections = discover_with_client(
        &client,
        config,
        password,
        configured,
        configured,
        Kind::Calendar,
    )?;
    if collections.is_empty() {
        bail!("找不到可用的 calendar DAV collection");
    }
    sync_calendar_collections(
        &client,
        config,
        password,
        configured,
        collections,
        existing,
        scope,
    )
}

pub(crate) fn known_nonrecurring_in_window(
    item: &Item,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> bool {
    let mut calendars = ical::IcalParser::new(BufReader::new(item.raw.as_bytes()));
    let Some(Ok(calendar)) = calendars.next() else {
        return false;
    };
    if calendars.next().is_some() || calendar.events.len() != 1 {
        return false;
    }
    let properties = &calendar.events[0].properties;
    if properties.iter().any(|property| {
        ["RRULE", "RDATE", "EXDATE", "RECURRENCE-ID"]
            .iter()
            .any(|name| property.name.eq_ignore_ascii_case(name))
    }) {
        return false;
    }
    let Some((event_start, event_end)) = crate::meeting::single_calendar_interval(&item.raw) else {
        return false;
    };
    event_start < end && start < event_end
}

fn sync_calendar_collections(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collections: Vec<Collection>,
    existing: &[Item],
    scope: CalendarSyncScope,
) -> Result<Snapshot> {
    let mut items = Vec::new();
    for collection in &collections {
        let collection_url = Url::parse(&collection.id)
            .with_context(|| format!("DAV collection URL 格式錯誤：{}", collection.id))?;
        let previous = existing
            .iter()
            .filter(|item| {
                item.kind == Kind::Calendar && collection_matches(&item.collection, &collection_url)
            })
            .filter_map(|item| {
                resource_url(&collection_url, &item.remote_id)
                    .ok()
                    .map(|url| (url.to_string(), item))
            })
            .collect::<HashMap<_, _>>();
        let resources =
            calendar_metadata_report(client, config, password, configured, &collection_url, scope)?;
        let mut seen = HashSet::new();
        for resource in resources {
            if resource.is_collection || resource.href.path().ends_with('/') {
                continue;
            }
            let remote_url = resource_url(&collection_url, resource.href.as_str())?;
            let remote_id = remote_url.to_string();
            if !seen.insert(remote_id.clone()) {
                bail!("DAV 行事曆回應包含重複資源：{remote_id}");
            }
            let cached = previous.get(&remote_id).copied();
            if let Some(cached) = cached.filter(|item| {
                !item.raw.trim().is_empty()
                    && nonempty_etag(item.etag.as_deref()).is_some()
                    && nonempty_etag(item.etag.as_deref())
                        == nonempty_etag(resource.etag.as_deref())
            }) {
                let mut item = cached.clone();
                item.collection = canonical_collection_id(&collection_url);
                item.remote_id = remote_id;
                items.push(item);
                continue;
            }
            let fetched = if resource
                .data
                .as_deref()
                .is_some_and(|data| !data.trim().is_empty())
            {
                resource
            } else {
                calendar_get(
                    client,
                    config,
                    password,
                    configured,
                    &collection_url,
                    &remote_url,
                    &resource,
                )?
            };
            let parsed = parse_resource(
                Kind::Calendar,
                canonical_collection_id(&collection_url),
                remote_id,
                fetched
                    .etag
                    .filter(|etag| nonempty_etag(Some(etag.as_str())).is_some()),
                fetched
                    .data
                    .ok_or_else(|| anyhow!("DAV 行事曆資源缺少內容"))?,
                cached,
            )?;
            items.push(parsed.item);
        }
        if let CalendarSyncScope::Window(start, end) = scope {
            let mut missing = previous
                .iter()
                .filter(|(url, item)| {
                    !seen.contains(*url) && known_nonrecurring_in_window(item, start, end)
                })
                .collect::<Vec<_>>();
            missing.sort_by(|left, right| left.0.cmp(right.0));
            for (url, cached) in missing {
                let href = resource_url(&collection_url, url)?;
                let listed = DavResource {
                    href: href.clone(),
                    display_name: None,
                    etag: None,
                    data: None,
                    is_collection: false,
                    resource_kind: None,
                    home_sets: Vec::new(),
                };
                if let Some(fetched) = calendar_get_optional(
                    client,
                    config,
                    password,
                    configured,
                    &collection_url,
                    &href,
                    &listed,
                )? {
                    items.push(
                        parse_resource(
                            Kind::Calendar,
                            canonical_collection_id(&collection_url),
                            href.to_string(),
                            fetched.etag,
                            fetched.data.context("DAV 行事曆資源缺少內容")?,
                            Some(cached),
                        )?
                        .item,
                    );
                }
            }
        }
    }
    Ok(Snapshot {
        kind: Kind::Calendar,
        collections,
        items,
    })
}

fn sync_collections<F>(
    kind: Kind,
    collections: Vec<Collection>,
    existing: &[Item],
    mut fetch: F,
) -> Result<Snapshot>
where
    F: FnMut(&Url) -> Result<Vec<DavResource>>,
{
    let mut items = Vec::new();
    for collection in &collections {
        let collection_url = Url::parse(&collection.id)
            .with_context(|| format!("DAV collection URL 格式錯誤：{}", collection.id))?;
        let resources = fetch(&collection_url)?;
        for resource in resources {
            let remote_url = resource_url(&collection_url, resource.href.as_str())?;
            let remote_id = remote_url.to_string();
            let previous = existing.iter().find(|item| {
                item.kind == kind
                    && collection_matches(&item.collection, &collection_url)
                    && remote_matches(&item.remote_id, &remote_url)
            });
            let parsed = parse_resource(
                kind,
                canonical_collection_id(&collection_url),
                remote_id,
                resource.etag,
                resource
                    .data
                    .ok_or_else(|| anyhow!("DAV 回應缺少資源內容"))?,
                previous,
            )?;
            items.push(parsed.item);
        }
    }
    Ok(Snapshot {
        kind,
        collections,
        items,
    })
}

fn merge_snapshots(kind: Kind, snapshots: Vec<Snapshot>) -> Snapshot {
    let mut collections = Vec::new();
    let mut items = Vec::new();
    for snapshot in snapshots {
        collections.extend(snapshot.collections);
        items.extend(snapshot.items);
    }
    Snapshot {
        kind,
        collections,
        items,
    }
}

pub fn fetch_range(
    config: &Config,
    password: &str,
    url: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    expand: bool,
) -> Result<Vec<Item>> {
    let mut session = DavSession::new(config, password)?;
    session.fetch_range(url, start, end, expand)
}

pub fn query_contacts(config: &Config, password: &str, url: &str, email: &str) -> Result<Snapshot> {
    if email.trim().is_empty() {
        bail!("聯絡人查詢電子郵件不可為空白");
    }
    let mut session = DavSession::new(config, password)?;
    let collections = session.discover(Kind::Contacts, url)?;
    let collection_url = matching_collection(url, &collections)?;
    let collection_id = canonical_collection_id(&collection_url);
    let collection = collections
        .into_iter()
        .find(|collection| collection.id == collection_id)
        .ok_or_else(|| anyhow!("指定的 DAV collection 不在已發現的集合中：{url}"))?;
    session.query_contacts(&collection, email)
}

pub fn put(
    config: &Config,
    password: &str,
    kind: Kind,
    collection: &str,
    raw: &str,
    existing: Option<&Item>,
) -> Result<Item> {
    let mut session = DavSession::new(config, password)?;
    session.put(kind, collection, raw, existing)
}

fn put_with_session(
    session: &DavSession<'_>,
    kind: Kind,
    collection_url: Url,
    raw: &str,
    existing: Option<&Item>,
) -> Result<Item> {
    if kind == Kind::Mail {
        bail!("Mail 不使用 DAV 協定");
    }
    if raw.trim().is_empty() {
        bail!("DAV 資源內容不可為空白");
    }
    validate_calendar_write(kind, raw)?;
    let raw_uid = extract_uid(kind, raw)?;
    let configured = configured_url(session.config, kind)?;
    let client = &session.client;
    let previous = existing.filter(|item| item.kind == kind);
    let mut receipt_uid = raw_uid.filter(|value| !value.trim().is_empty());
    if receipt_uid.is_none() {
        receipt_uid = previous.and_then(|item| {
            extract_uid(kind, &item.raw)
                .ok()
                .flatten()
                .filter(|value| !value.trim().is_empty())
        });
    }
    let target = if let Some(item) = previous {
        if !collection_matches(&item.collection, &collection_url) {
            bail!("既有 DAV 資源不屬於指定 collection");
        }
        let etag = item
            .etag
            .as_deref()
            .filter(|value| !value.trim().is_empty() && *value != "*")
            .ok_or_else(|| anyhow!("更新 DAV 資源需要現有 ETag"))?;
        let target = resource_url(&collection_url, &item.remote_id)?;
        (target, Some(etag.to_owned()))
    } else {
        let uid = receipt_uid
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        receipt_uid = Some(uid.clone());
        let filename = format!("{}.{}", encode_segment(&uid), extension(kind));
        let target = collection_url
            .join(&filename)
            .map_err(|_| anyhow!("無法建立 DAV 資源 URL"))?;
        ensure_resource_under_collection(&collection_url, &target)?;
        (target, None)
    };
    let content_type = match kind {
        Kind::Calendar => "text/calendar; charset=utf-8",
        Kind::Contacts => "text/vcard; charset=utf-8",
        Kind::Mail => unreachable!(),
    };
    let mut request = checked_request(
        client,
        session.config,
        session.password,
        &configured,
        Method::PUT,
        &target.0,
        DavOperation::Put,
    )?
    .header(CONTENT_TYPE, content_type)
    .body(raw.to_owned());
    if let Some(etag) = target.1.as_deref() {
        request = request.header(IF_MATCH, etag);
    } else {
        request = request.header(IF_NONE_MATCH, "*");
    }
    let response = send(request)?;
    let status = response.status();
    let headers = response.headers().clone();
    if ![StatusCode::OK, StatusCode::CREATED, StatusCode::NO_CONTENT].contains(&status) {
        bail!("DAV PUT 回應狀態錯誤：{status}");
    }
    drop(response);
    let attempted_url = target.0.clone();
    let location = match headers.get(LOCATION) {
        None => Ok(None),
        Some(value) => {
            let value = value
                .to_str()
                .map_err(|error| anyhow!("Location header 格式錯誤：{error}"))?;
            resource_url(&collection_url, value).map(Some)
        }
    };
    let location = match location {
        Ok(location) => location,
        Err(error) => {
            return Err(remote_write_pending_reconcile(
                kind,
                &collection_url,
                &attempted_url,
                receipt_uid,
                status,
                format!("Location 回應無效：{error:#}"),
            ));
        }
    };
    let remote_url = location.unwrap_or_else(|| attempted_url.clone());
    if let Err(error) = ensure_resource_under_collection(&collection_url, &remote_url) {
        return Err(remote_write_pending_reconcile(
            kind,
            &collection_url,
            &attempted_url,
            receipt_uid,
            status,
            format!("Location 不在指定 collection：{error:#}"),
        ));
    }
    let etag = match header_etag(&headers) {
        Some(etag) => Some(etag),
        None => match refresh_etag(
            client,
            session.config,
            session.password,
            &configured,
            &remote_url,
            kind,
        ) {
            Ok(etag) => etag,
            Err(error) => {
                return Err(remote_write_pending_reconcile(
                    kind,
                    &collection_url,
                    &remote_url,
                    receipt_uid.clone(),
                    status,
                    format!("重新取得 ETag 失敗：{error:#}"),
                ));
            }
        },
    };
    if etag.is_none() {
        return Err(remote_write_pending_reconcile(
            kind,
            &collection_url,
            &remote_url,
            receipt_uid.clone(),
            status,
            "PUT 與後續 GET 都未提供 ETag".to_owned(),
        ));
    }
    let parsed = match parse_resource(
        kind,
        canonical_collection_id(&collection_url),
        remote_url.to_string(),
        etag,
        raw.to_owned(),
        previous,
    ) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Err(remote_write_pending_reconcile(
                kind,
                &collection_url,
                &remote_url,
                receipt_uid,
                status,
                format!("寫入後解析本機資源失敗：{error:#}"),
            ));
        }
    };
    Ok(parsed.item)
}

fn remote_write_pending_reconcile(
    kind: Kind,
    collection: &Url,
    resource: &Url,
    uid: Option<String>,
    status: StatusCode,
    reason: String,
) -> anyhow::Error {
    RemoteWritePendingReconcile {
        kind,
        collection: canonical_collection_id(collection),
        resource_url: resource.to_string(),
        uid,
        operation: "PUT".to_owned(),
        status: status.as_str().to_owned(),
        remote_accepted: true,
        reconcile_required: true,
        reason,
    }
    .into()
}

fn refresh_etag(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    resource: &Url,
    kind: Kind,
) -> Result<Option<String>> {
    let accept = match kind {
        Kind::Calendar => "text/calendar, text/plain",
        Kind::Contacts => "text/vcard, text/x-vcard, text/plain",
        Kind::Mail => unreachable!(),
    };
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::GET,
        resource,
        DavOperation::Put,
    )?
    .header(ACCEPT, accept);
    let response = send(request)?;
    let (headers, _) = require_status(response, &[StatusCode::OK], "DAV ETag 重新取得")?;
    Ok(header_etag(&headers))
}

pub fn delete(config: &Config, password: &str, item: &Item) -> Result<()> {
    if item.kind == Kind::Mail {
        bail!("Mail 不使用 DAV 協定");
    }
    let configured = configured_url(config, item.kind)?;
    let client = build_client()?;
    let collections = discover_with_client(
        &client,
        config,
        password,
        &configured,
        &configured,
        item.kind,
    )?;
    let collection_url = matching_collection(&item.collection, &collections)?;
    let remote_url = resource_url(&collection_url, &item.remote_id)?;
    let etag = item
        .etag
        .as_deref()
        .filter(|value| !value.trim().is_empty() && *value != "*")
        .ok_or_else(|| anyhow!("刪除 DAV 資源需要現有 ETag"))?;
    let request = checked_request(
        &client,
        config,
        password,
        &configured,
        Method::DELETE,
        &remote_url,
        DavOperation::Delete,
    )?
    .header(IF_MATCH, etag);
    let response = send(request)?;
    let _ = require_status(
        response,
        &[StatusCode::OK, StatusCode::NO_CONTENT],
        "DAV DELETE",
    )?;
    Ok(())
}

fn build_client() -> Result<Client> {
    Client::builder()
        .use_rustls_tls()
        .redirect(Policy::none())
        .timeout(DAV_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .context("無法建立 DAV HTTP client")
}

fn configured_url(config: &Config, kind: Kind) -> Result<Url> {
    let value = match kind {
        Kind::Calendar => config.caldav_url.as_str(),
        Kind::Contacts => config.carddav_url.as_str(),
        Kind::Mail => bail!("Mail 不使用 DAV 協定"),
    };
    parse_endpoint(value)
}

fn parse_endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value.trim()).map_err(|_| anyhow!("DAV URL 格式錯誤"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("DAV URL 必須使用 HTTP 或 HTTPS");
    }
    if url.host_str().is_none() {
        bail!("DAV URL 缺少主機");
    }
    if url.username() != "" || url.password().is_some() {
        bail!("DAV URL 不可包含帳號或密碼");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("DAV URL 不可包含 query 或 fragment");
    }
    if url.scheme() == "http" && !is_localhost(&url) {
        bail!("非 localhost 的 DAV HTTP 連線已拒絕");
    }
    Ok(url)
}

fn resolve_endpoint(configured: &Url, value: &str) -> Result<Url> {
    let url = if let Ok(parsed) = Url::parse(value.trim()) {
        parsed
    } else {
        configured
            .join(value.trim())
            .map_err(|_| anyhow!("DAV collection URL 格式錯誤"))?
    };
    ensure_same_origin(configured, &url)?;
    if url.query().is_some() || url.fragment().is_some() {
        bail!("DAV collection URL 不可包含 query 或 fragment");
    }
    Ok(url)
}

fn discover_with_client(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    probe: &Url,
    kind: Kind,
) -> Result<Vec<Collection>> {
    let resources = discover_resources(client, config, password, configured, probe, kind)?;
    collections_from_resources(kind, resources, Some(probe))
}

fn collections_from_resources(
    kind: Kind,
    listing: DavListing,
    probe: Option<&Url>,
) -> Result<Vec<Collection>> {
    let resources = &listing.resources;
    let mut collections = Vec::new();
    let mut seen = HashSet::new();
    for resource in resources.iter().filter(|resource| {
        resource
            .resource_kind
            .map(|resource_kind| resource_kind == kind)
            .unwrap_or(false)
    }) {
        let id = canonical_collection_id(&resource.href);
        if !seen.insert(id.clone()) {
            continue;
        }
        collections.push(Collection {
            kind,
            id,
            name: resource
                .display_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| collection_name(&resource.href)),
        });
    }
    if collections.is_empty() {
        if let Some(resource) = resources.iter().find(|resource| {
            resource.is_collection
                && probe
                    .map(|probe| same_url(&resource.href, probe))
                    .unwrap_or(false)
        }) {
            let id = canonical_collection_id(&resource.href);
            collections.push(Collection {
                kind,
                id,
                name: resource
                    .display_name
                    .clone()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| collection_name(&resource.href)),
            });
        }
    }
    if collections.is_empty() {
        listing.ensure_permissions()?;
    }
    Ok(collections)
}

fn discover_full_tree_with_client(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    probe: &Url,
    kind: Kind,
) -> Result<(Vec<Collection>, bool)> {
    let principal_probe = is_principal_url(probe);
    if !principal_probe {
        return match propfind_with_depth_if_supported(
            client, config, password, configured, probe, kind, "infinity",
        )? {
            Some(resources) => Ok((
                collections_from_resources(kind, resources, Some(probe))?,
                true,
            )),
            None => Ok((
                discover_with_client(client, config, password, configured, probe, kind)?,
                false,
            )),
        };
    }
    let initial = propfind_with_depth(client, config, password, configured, probe, kind, "0")?;
    let mut pending = initial
        .resources
        .iter()
        .flat_map(|resource| resource.home_sets.iter().cloned())
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok((
            discover_with_client(client, config, password, configured, probe, kind)?,
            false,
        ));
    }
    let mut resources = initial;
    let mut seen = resources
        .resources
        .iter()
        .map(|resource| resource.href.to_string())
        .collect::<HashSet<_>>();
    let mut visited = HashSet::from([probe.to_string()]);
    let mut saw_infinity = false;
    while let Some(home_set) = pending.pop() {
        ensure_same_origin(configured, &home_set)?;
        if !visited.insert(home_set.to_string()) {
            continue;
        }
        let depth = if is_principal_url(&home_set) {
            "0"
        } else {
            saw_infinity = true;
            "infinity"
        };
        let Some(home_resources) = propfind_with_depth_if_supported(
            client, config, password, configured, &home_set, kind, depth,
        )?
        else {
            return Ok((
                discover_with_client(client, config, password, configured, probe, kind)?,
                false,
            ));
        };
        resources.denied.extend(home_resources.denied);
        for resource in home_resources.resources {
            pending.extend(resource.home_sets.iter().cloned());
            if seen.insert(resource.href.to_string()) {
                resources.resources.push(resource);
            }
        }
    }
    if !saw_infinity {
        return Ok((
            discover_with_client(client, config, password, configured, probe, kind)?,
            false,
        ));
    }
    Ok((
        collections_from_resources(kind, resources, Some(probe))?,
        true,
    ))
}

fn discover_resources(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    probe: &Url,
    kind: Kind,
) -> Result<DavListing> {
    let principal_probe = kind == Kind::Contacts && is_principal_url(probe);
    let initial_depth = if principal_probe { "0" } else { "1" };
    let initial = propfind_with_depth(
        client,
        config,
        password,
        configured,
        probe,
        kind,
        initial_depth,
    )?;
    let home_sets = initial
        .resources
        .iter()
        .flat_map(|resource| resource.home_sets.iter().cloned())
        .collect::<Vec<_>>();
    if !principal_probe {
        return Ok(initial);
    }
    if home_sets.is_empty() {
        let mut listed = propfind(client, config, password, configured, probe, kind)?;
        listed.denied.extend(initial.denied);
        return Ok(listed);
    }
    let mut resources = initial;
    let mut seen = resources
        .resources
        .iter()
        .map(|resource| resource.href.to_string())
        .collect::<Vec<_>>();
    let mut pending = home_sets;
    let mut visited_links = Vec::new();
    while let Some(home_set) = pending.pop() {
        ensure_same_origin(configured, &home_set)?;
        if visited_links.iter().any(|href| href == home_set.as_str()) {
            continue;
        }
        visited_links.push(home_set.to_string());
        let depth = if is_principal_url(&home_set) {
            "0"
        } else {
            "1"
        };
        let listed =
            propfind_with_depth(client, config, password, configured, &home_set, kind, depth)?;
        resources.denied.extend(listed.denied);
        for resource in listed.resources {
            pending.extend(resource.home_sets.iter().cloned());
            if !seen.iter().any(|href| href == resource.href.as_str()) {
                seen.push(resource.href.to_string());
                resources.resources.push(resource);
            }
        }
    }
    if !resources
        .resources
        .iter()
        .any(|resource| resource.resource_kind == Some(kind))
    {
        let listed = propfind_with_depth(client, config, password, configured, probe, kind, "1")?;
        resources.denied.extend(listed.denied);
        for resource in listed.resources {
            if !seen.iter().any(|href| href == resource.href.as_str()) {
                seen.push(resource.href.to_string());
                resources.resources.push(resource);
            }
        }
    }
    Ok(resources)
}

fn propfind(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    url: &Url,
    kind: Kind,
) -> Result<DavListing> {
    propfind_with_depth(client, config, password, configured, url, kind, "1")
}

fn propfind_with_depth(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    url: &Url,
    kind: Kind,
    depth: &str,
) -> Result<DavListing> {
    let body = propfind_body();
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::from_bytes(b"PROPFIND").expect("PROPFIND method"),
        url,
        DavOperation::Propfind,
    )?
    .header("Depth", depth)
    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
    .header(ACCEPT, "application/xml, text/xml")
    .body(body);
    let response = send(request)?;
    let (_, body) = require_status(response, &[StatusCode::MULTI_STATUS], "DAV PROPFIND")?;
    parse_listing(&body, configured, url, kind, false)
}

fn propfind_with_depth_if_supported(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    url: &Url,
    kind: Kind,
    depth: &str,
) -> Result<Option<DavListing>> {
    let body = propfind_body();
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::from_bytes(b"PROPFIND").expect("PROPFIND method"),
        url,
        DavOperation::Propfind,
    )?
    .header("Depth", depth)
    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
    .header(ACCEPT, "application/xml, text/xml")
    .body(body);
    let response = send(request)?;
    let status = response.status();
    if matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::FORBIDDEN
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::NOT_IMPLEMENTED
    ) {
        drop(response);
        return Ok(None);
    }
    let (_, body) = require_status(response, &[StatusCode::MULTI_STATUS], "DAV PROPFIND")?;
    Ok(Some(parse_listing(&body, configured, url, kind, false)?))
}

fn report(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
    kind: Kind,
) -> Result<Vec<DavResource>> {
    if kind == Kind::Contacts {
        return report_contacts_with_fallback(client, config, password, configured, collection);
    }
    report_query(client, config, password, configured, collection, kind)
}

fn report_query(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
    kind: Kind,
) -> Result<Vec<DavResource>> {
    let body = report_body(kind);
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::from_bytes(b"REPORT").expect("REPORT method"),
        collection,
        DavOperation::Report,
    )?
    .header("Depth", "1")
    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
    .header(ACCEPT, "application/xml, text/xml")
    .body(body);
    let response = send(request)?;
    let (_, body) = require_status(response, &[StatusCode::MULTI_STATUS], "DAV REPORT")?;
    parse_multistatus(&body, configured, collection, kind, true)
}

fn report_contacts_with_fallback(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
) -> Result<Vec<DavResource>> {
    let body = report_body(Kind::Contacts);
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::from_bytes(b"REPORT").expect("REPORT method"),
        collection,
        DavOperation::Report,
    )?
    .header("Depth", "1")
    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
    .header(ACCEPT, "application/xml, text/xml")
    .body(body);
    let response = send(request)?;
    if response.status() == StatusCode::MULTI_STATUS {
        let (_, body) = require_status(response, &[StatusCode::MULTI_STATUS], "DAV REPORT")?;
        return parse_multistatus(&body, configured, collection, Kind::Contacts, true);
    }
    let status = response.status();
    let _ = response.text();
    if !status.is_client_error() && !status.is_server_error() {
        bail!("DAV REPORT 回應狀態錯誤：{status}");
    }
    carddav_list_and_fetch(client, config, password, configured, collection)
}

fn carddav_list_and_fetch(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
) -> Result<Vec<DavResource>> {
    let listed = propfind(
        client,
        config,
        password,
        configured,
        collection,
        Kind::Contacts,
    )?;
    listed.ensure_readable()?;
    let hrefs = listed
        .resources
        .iter()
        .filter(|resource| !resource.is_collection && !resource.href.path().ends_with('/'))
        .map(|resource| resource.href.clone())
        .collect::<Vec<_>>();
    if hrefs.is_empty() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for chunk in hrefs.chunks(50) {
        match carddav_multiget(client, config, password, configured, collection, chunk) {
            Ok(mut resources) => result.append(&mut resources),
            Err(error) if error.to_string().contains("回應狀態錯誤") => {
                for href in chunk {
                    result.push(carddav_get(
                        client,
                        config,
                        password,
                        configured,
                        collection,
                        href,
                        listed
                            .resources
                            .iter()
                            .find(|resource| same_url(&resource.href, href)),
                    )?);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(result)
}

fn carddav_multiget(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
    hrefs: &[Url],
) -> Result<Vec<DavResource>> {
    let hrefs = hrefs
        .iter()
        .map(|href| format!("<D:href>{}</D:href>", xml_escape(href.as_str())))
        .collect::<String>();
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><CR:addressbook-multiget xmlns:D=\"DAV:\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\"><D:prop><D:getetag/><CR:address-data/></D:prop>{hrefs}</CR:addressbook-multiget>"
    );
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::from_bytes(b"REPORT").expect("REPORT method"),
        collection,
        DavOperation::Report,
    )?
    .header("Depth", "1")
    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
    .header(ACCEPT, "application/xml, text/xml")
    .body(body);
    let response = send(request)?;
    let (_, body) = require_status(response, &[StatusCode::MULTI_STATUS], "DAV REPORT")?;
    parse_multistatus(&body, configured, collection, Kind::Contacts, true)
}

fn carddav_get(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
    href: &Url,
    listed: Option<&DavResource>,
) -> Result<DavResource> {
    ensure_same_origin(configured, href)?;
    ensure_resource_under_collection(collection, href)?;
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::GET,
        href,
        DavOperation::Report,
    )?
    .header(ACCEPT, "text/vcard, text/x-vcard, text/plain")
    .header("Depth", "0");
    let response = send(request)?;
    let (headers, body) = require_status(response, &[StatusCode::OK], "DAV GET vCard")?;
    Ok(DavResource {
        href: href.clone(),
        display_name: listed.and_then(|resource| resource.display_name.clone()),
        etag: header_etag(&headers).or_else(|| listed.and_then(|resource| resource.etag.clone())),
        data: Some(body),
        is_collection: false,
        resource_kind: None,
        home_sets: Vec::new(),
    })
}

fn report_range(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
    range: (DateTime<Utc>, DateTime<Utc>),
    expand: bool,
) -> Result<Vec<DavResource>> {
    let (start, end) = range;
    let start = start.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ");
    let end = end.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ");
    let expansion = if expand {
        format!("<C:expand start=\"{start}\" end=\"{end}\"/>")
    } else {
        String::new()
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><C:calendar-query xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><D:getetag/><C:calendar-data>{expansion}</C:calendar-data></D:prop><C:filter><C:comp-filter name=\"VCALENDAR\"><C:comp-filter name=\"VEVENT\"><C:time-range start=\"{start}\" end=\"{end}\"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"
    );
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::from_bytes(b"REPORT").expect("REPORT method"),
        collection,
        DavOperation::Report,
    )?
    .header("Depth", "1")
    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
    .header(ACCEPT, "application/xml, text/xml")
    .body(body);
    let response = send(request)?;
    let (_, body) = require_status(response, &[StatusCode::MULTI_STATUS], "DAV REPORT")?;
    parse_multistatus(&body, configured, collection, Kind::Calendar, true)
}

fn calendar_metadata_report(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
    scope: CalendarSyncScope,
) -> Result<Vec<DavResource>> {
    let time_filter = match scope {
        CalendarSyncScope::All => String::new(),
        CalendarSyncScope::Since(start) => format!(
            "<C:time-range start=\"{}\"/>",
            start.format("%Y%m%dT%H%M%SZ")
        ),
        CalendarSyncScope::Window(start, end) => format!(
            "<C:time-range start=\"{}\" end=\"{}\"/>",
            start.format("%Y%m%dT%H%M%SZ"),
            end.format("%Y%m%dT%H%M%SZ")
        ),
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><C:calendar-query xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><D:getetag/></D:prop><C:filter><C:comp-filter name=\"VCALENDAR\"><C:comp-filter name=\"VEVENT\">{time_filter}</C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"
    );
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::from_bytes(b"REPORT").expect("REPORT method"),
        collection,
        DavOperation::Report,
    )?
    .header("Depth", "1")
    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
    .header(ACCEPT, "application/xml, text/xml")
    .body(body);
    let response = send(request)?;
    let (_, body) = require_status(response, &[StatusCode::MULTI_STATUS], "DAV REPORT")?;
    parse_multistatus(&body, configured, collection, Kind::Calendar, false)
}

fn nonempty_etag(etag: Option<&str>) -> Option<&str> {
    etag.map(str::trim)
        .filter(|value| !value.is_empty() && *value != "*")
}

fn calendar_get(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
    href: &Url,
    listed: &DavResource,
) -> Result<DavResource> {
    calendar_get_optional(
        client, config, password, configured, collection, href, listed,
    )?
    .ok_or_else(|| anyhow!("DAV GET iCalendar 資源已不存在：{href}"))
}

fn calendar_get_optional(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
    href: &Url,
    listed: &DavResource,
) -> Result<Option<DavResource>> {
    ensure_same_origin(configured, href)?;
    ensure_resource_under_collection(collection, href)?;
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::GET,
        href,
        DavOperation::Report,
    )?
    .header(ACCEPT, "text/calendar, text/plain")
    .header("Depth", "0");
    let response = send(request)?;
    if matches!(response.status(), StatusCode::NOT_FOUND | StatusCode::GONE) {
        return Ok(None);
    }
    let (headers, body) = require_status(response, &[StatusCode::OK], "DAV GET iCalendar")?;
    Ok(Some(DavResource {
        href: href.clone(),
        display_name: listed.display_name.clone(),
        etag: header_etag(&headers).or_else(|| listed.etag.clone()),
        data: Some(body),
        is_collection: false,
        resource_kind: None,
        home_sets: Vec::new(),
    }))
}

fn report_contacts_query(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    collection: &Url,
    email: &str,
) -> Result<Vec<DavResource>> {
    let email = xml_escape(email);
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><CR:addressbook-query xmlns:D=\"DAV:\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\"><D:prop><D:getetag/><CR:address-data/></D:prop><CR:filter><CR:prop-filter name=\"EMAIL\"><CR:text-match collation=\"i;unicode-casemap\" match-type=\"equals\">{email}</CR:text-match></CR:prop-filter></CR:filter></CR:addressbook-query>"
    );
    let request = checked_request(
        client,
        config,
        password,
        configured,
        Method::from_bytes(b"REPORT").expect("REPORT method"),
        collection,
        DavOperation::Report,
    )?
    .header("Depth", "1")
    .header(CONTENT_TYPE, "application/xml; charset=utf-8")
    .header(ACCEPT, "application/xml, text/xml")
    .body(body);
    let response = send(request)?;
    let (_, body) = require_status(response, &[StatusCode::MULTI_STATUS], "DAV REPORT")?;
    parse_multistatus(&body, configured, collection, Kind::Contacts, true)
}

fn checked_request(
    client: &Client,
    config: &Config,
    password: &str,
    configured: &Url,
    method: Method,
    url: &Url,
    _operation: DavOperation,
) -> Result<RequestBuilder> {
    ensure_same_origin(configured, url)?;
    let login = Some(config.email.trim())
        .filter(|email| !email.is_empty())
        .unwrap_or(&config.username);
    Ok(client
        .request(method, url.clone())
        .basic_auth(login, Some(password)))
}

fn send(request: RequestBuilder) -> Result<Response> {
    let response = request.send().context("DAV HTTP 請求失敗")?;
    if response.status().is_redirection() {
        bail!("DAV 重新導向已拒絕，避免憑證外洩");
    }
    Ok(response)
}

fn require_status(
    response: Response,
    accepted: &[StatusCode],
    operation: &str,
) -> Result<(HeaderMap, String)> {
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .text()
        .with_context(|| format!("{operation} 回應讀取失敗"))?;
    if !accepted.contains(&status) {
        bail!("{operation} 回應狀態錯誤：{status}");
    }
    Ok((headers, body))
}

fn header_etag(headers: &HeaderMap) -> Option<String> {
    headers
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .filter(|value| !value.trim().is_empty() && value != "*")
}

fn parse_multistatus(
    body: &str,
    configured: &Url,
    base: &Url,
    kind: Kind,
    require_data: bool,
) -> Result<Vec<DavResource>> {
    let listing = parse_listing(body, configured, base, kind, require_data)?;
    listing.ensure_readable()?;
    Ok(listing.resources)
}

fn parse_listing(
    body: &str,
    configured: &Url,
    base: &Url,
    kind: Kind,
    require_data: bool,
) -> Result<DavListing> {
    let document = Document::parse(body).map_err(|error| anyhow!("DAV XML 格式錯誤：{error}"))?;
    let root = document.root_element();
    if root.tag_name().name() != "multistatus" {
        bail!("DAV 回應缺少 multistatus 根節點");
    }
    let mut result = DavListing::default();
    for response in root
        .children()
        .filter(|node| is_element_named(*node, "response"))
    {
        let href_node =
            direct_child(response, "href").ok_or_else(|| anyhow!("DAV response 缺少 href"))?;
        let href_text = text_content(href_node);
        if href_text.trim().is_empty() {
            bail!("DAV response href 不可為空白");
        }
        let href = resolve_href(base, href_text.trim())?;
        ensure_same_origin(configured, &href)?;
        let propstats: Vec<Node<'_, '_>> = response
            .children()
            .filter(|node| is_element_named(*node, "propstat"))
            .collect();
        if let Some(status) = direct_child(response, "status") {
            let code = status_code(&text_content(status))
                .ok_or_else(|| anyhow!("DAV response status 格式錯誤：{href}"))?;
            match code {
                401 | 403 => {
                    result.denied.push((href, code));
                    continue;
                }
                404 => {
                    result.missing.push(href);
                    continue;
                }
                _ => bail!("DAV response 回應狀態不受支援：{code} {href}"),
            }
        }
        if propstats.is_empty() {
            bail!("DAV response 缺少 propstat：{href}");
        }
        let mut denied = None;
        let mut properties = Vec::new();
        let mut has_successful_properties = false;
        for propstat in propstats {
            let status = direct_child(propstat, "status")
                .ok_or_else(|| anyhow!("DAV propstat 缺少 status"))?;
            let status_code = status_code(&text_content(status))
                .ok_or_else(|| anyhow!("DAV propstat status 格式錯誤"))?;
            if matches!(status_code, 401 | 403) {
                denied = Some(status_code);
                continue;
            }
            if status_code == 404 {
                continue;
            }
            if status_code != 200 {
                bail!("DAV propstat 回應狀態不受支援：{status_code}");
            }
            has_successful_properties = true;
            let prop =
                direct_child(propstat, "prop").ok_or_else(|| anyhow!("DAV propstat 缺少 prop"))?;
            properties.push(prop);
        }
        if !has_successful_properties {
            if let Some(code) = denied {
                result.denied.push((href, code));
            } else {
                result.missing.push(href);
            }
            continue;
        }
        let display_name = property_text(&properties, "displayname");
        let etag = property_text(&properties, "getetag");
        let data_name = match kind {
            Kind::Calendar => "calendar-data",
            Kind::Contacts => "address-data",
            Kind::Mail => unreachable!(),
        };
        let data = property_text(&properties, data_name);
        if require_data && data.is_none() {
            if let Some(code) = denied {
                bail!("DAV 權限不足：{code} {href}，無法讀取 {data_name}");
            }
            bail!("DAV 資源缺少 {data_name}：{href}");
        }
        let resource_kind = resource_kind(&properties);
        let home_sets = property_hrefs(&properties, "addressbook-home-set")
            .into_iter()
            .chain(property_hrefs(&properties, "calendar-home-set"))
            .chain(property_hrefs(&properties, "current-user-principal"))
            .map(|href| resolve_href(base, &href))
            .collect::<Result<Vec<_>>>()?;
        let is_collection = properties.iter().any(|prop| {
            prop.descendants().any(|node| {
                is_element_named(node, "collection")
                    || is_element_named(node, "calendar")
                    || is_element_named(node, "addressbook")
            })
        });
        result.resources.push(DavResource {
            href,
            display_name,
            etag,
            data,
            is_collection,
            resource_kind,
            home_sets,
        });
    }
    Ok(result)
}

fn resource_kind(properties: &[Node<'_, '_>]) -> Option<Kind> {
    let mut kind = None;
    for prop in properties {
        for node in prop.descendants() {
            if is_element_named(node, "calendar") {
                kind = Some(Kind::Calendar);
            }
            if is_element_named(node, "addressbook") {
                kind = Some(Kind::Contacts);
            }
        }
    }
    kind
}

fn parse_resource(
    kind: Kind,
    collection: String,
    remote_id: String,
    etag: Option<String>,
    raw: String,
    previous: Option<&Item>,
) -> Result<ParsedResource> {
    match kind {
        Kind::Calendar => parse_calendar(collection, remote_id, etag, raw, previous),
        Kind::Contacts => parse_contact(collection, remote_id, etag, raw, previous),
        Kind::Mail => bail!("Mail 不使用 DAV 資源解析"),
    }
}

fn parse_calendar(
    collection: String,
    remote_id: String,
    etag: Option<String>,
    raw: String,
    previous: Option<&Item>,
) -> Result<ParsedResource> {
    let calendars = parse_ical(&raw)?;
    validate_event_uids(&calendars)?;
    let calendar = calendars
        .first()
        .ok_or_else(|| anyhow!("iCalendar 內容缺少 VCALENDAR"))?;
    let event = calendar
        .events
        .first()
        .ok_or_else(|| anyhow!("iCalendar 內容缺少 VEVENT"))?;
    let title = property_value(&event.properties, "SUMMARY").unwrap_or_default();
    let subtitle = property_value(&event.properties, "LOCATION").unwrap_or_default();
    let text = property_value(&event.properties, "DESCRIPTION").unwrap_or_default();
    let date = event_property_date(&event.properties, "DTSTART")?;
    let end_date = event_property_date(&event.properties, "DTEND")?;
    let id = previous
        .map(|item| item.id.clone())
        .unwrap_or_else(|| stable_id(&collection, &remote_id));
    Ok(ParsedResource {
        item: Item {
            id,
            kind: Kind::Calendar,
            collection,
            remote_id,
            etag,
            uid_validity: None,
            title,
            subtitle,
            date,
            end_date,
            text,
            raw,
            flags: Vec::new(),
            emails: Vec::new(),
            phones: Vec::new(),
            attachments: Vec::new(),
        },
    })
}

fn parse_contact(
    collection: String,
    remote_id: String,
    etag: Option<String>,
    raw: String,
    previous: Option<&Item>,
) -> Result<ParsedResource> {
    let parser = VcardParser::new(BufReader::new(raw.as_bytes()));
    let contacts = parser
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| anyhow!("vCard 格式錯誤：{error}"))?;
    let contact = contacts
        .first()
        .ok_or_else(|| anyhow!("vCard 內容缺少 VCARD"))?;
    let title = property_value(&contact.properties, "FN")
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            property_value(&contact.properties, "N").and_then(|value| format_vcard_n(&value))
        })
        .unwrap_or_default();
    let organization = property_value(&contact.properties, "ORG")
        .and_then(|value| meaningful_vcard_component(&value));
    let job_title = property_value(&contact.properties, "TITLE")
        .and_then(|value| meaningful_vcard_component(&value));
    let structured_name =
        property_value(&contact.properties, "N").and_then(|value| format_vcard_n(&value));
    let emails = property_values(&contact.properties, "EMAIL");
    let phones = property_values(&contact.properties, "TEL");
    let subtitle = job_title
        .clone()
        .or_else(|| organization.clone())
        .or_else(|| {
            structured_name
                .clone()
                .filter(|value| value.trim() != title.trim())
        })
        .or_else(|| emails.first().cloned())
        .or_else(|| phones.first().cloned())
        .unwrap_or_default();
    let text = property_value(&contact.properties, "NOTE")
        .and_then(|value| meaningful_vcard_component(&value))
        .unwrap_or_default();
    let date = property_value(&contact.properties, "BDAY");
    let end_date = property_value(&contact.properties, "ANNIVERSARY");
    let id = previous
        .map(|item| item.id.clone())
        .unwrap_or_else(|| stable_id(&collection, &remote_id));
    Ok(ParsedResource {
        item: Item {
            id,
            kind: Kind::Contacts,
            collection,
            remote_id,
            etag,
            uid_validity: None,
            title,
            subtitle,
            date,
            end_date,
            text,
            raw,
            flags: Vec::new(),
            emails,
            phones,
            attachments: Vec::new(),
        },
    })
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

fn extract_uid(kind: Kind, raw: &str) -> Result<Option<String>> {
    match kind {
        Kind::Calendar => {
            let calendars = parse_ical(raw)?;
            validate_event_uids(&calendars)?;
            let calendar = calendars
                .first()
                .ok_or_else(|| anyhow!("iCalendar 內容缺少 VCALENDAR"))?;
            Ok(calendar
                .events
                .first()
                .and_then(|event| property_value(&event.properties, "UID")))
        }
        Kind::Contacts => {
            let parser = VcardParser::new(BufReader::new(raw.as_bytes()));
            let contacts = parser
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| anyhow!("vCard 格式錯誤：{error}"))?;
            let contact = contacts
                .first()
                .ok_or_else(|| anyhow!("vCard 內容缺少 VCARD"))?;
            Ok(property_value(&contact.properties, "UID"))
        }
        Kind::Mail => bail!("Mail 不使用 DAV 資源解析"),
    }
}

fn parse_ical(raw: &str) -> Result<Vec<ical::parser::ical::component::IcalCalendar>> {
    IcalParser::new(BufReader::new(raw.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| anyhow!("iCalendar 格式錯誤：{error}"))
}

fn validate_event_uids(calendars: &[ical::parser::ical::component::IcalCalendar]) -> Result<()> {
    let mut event_count = 0;
    let mut common_uid = None;
    let mut missing_uid = false;
    for calendar in calendars {
        for event in &calendar.events {
            event_count += 1;
            let uid =
                property_value(&event.properties, "UID").filter(|value| !value.trim().is_empty());
            if let Some(uid) = uid {
                if let Some(existing) = common_uid.as_deref() {
                    if existing != uid {
                        bail!("iCalendar 資源包含不同 UID 的多個 VEVENT");
                    }
                } else {
                    common_uid = Some(uid);
                }
            } else {
                missing_uid = true;
            }
        }
    }
    if event_count > 1 && (missing_uid || common_uid.is_none()) {
        bail!("iCalendar 多個 VEVENT 缺少可確認的共同 UID");
    }
    Ok(())
}

fn validate_calendar_write(kind: Kind, raw: &str) -> Result<()> {
    if kind != Kind::Calendar {
        return Ok(());
    }
    let calendars = parse_ical(raw)?;
    if calendars.iter().any(|calendar| {
        calendar
            .properties
            .iter()
            .any(|property| property.name.eq_ignore_ascii_case("METHOD"))
    }) {
        bail!("calendar PUT 不接受含 METHOD 的 iCalendar；請使用會議邀請流程");
    }
    validate_event_uids(&calendars)
}

fn event_property_date(
    properties: &[ical::property::Property],
    name: &str,
) -> Result<Option<String>> {
    let property = properties
        .iter()
        .find(|property| property.name.eq_ignore_ascii_case(name));
    let Some(property) = property else {
        return Ok(None);
    };
    let Some(value) = property.value.as_deref() else {
        return Ok(None);
    };
    let value_type = property_parameter(property, "VALUE");
    if value_type
        .as_deref()
        .map(|value| value.eq_ignore_ascii_case("DATE"))
        .unwrap_or(false)
        || (value.len() == 8 && value.chars().all(|character| character.is_ascii_digit()))
    {
        let date = NaiveDate::parse_from_str(value, "%Y%m%d")
            .map_err(|error| anyhow!("iCalendar 日期格式錯誤：{error}"))?;
        return Ok(Some(date.format("%Y-%m-%d").to_string()));
    }
    Ok(Some(parse_datetime(
        value,
        property_parameter(property, "TZID"),
    )?))
}

fn parse_datetime(value: &str, tzid: Option<String>) -> Result<String> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.to_rfc3339());
    }
    let compact = value.trim_end_matches('Z');
    let naive = NaiveDateTime::parse_from_str(compact, "%Y%m%dT%H%M%S")
        .map_err(|error| anyhow!("iCalendar 日期時間格式錯誤：{error}"))?;
    if value.ends_with('Z') {
        return Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc).to_rfc3339());
    }
    let zone = match tzid {
        Some(zone_name) => match zone_name.parse::<Tz>() {
            Ok(zone) => zone,
            Err(_) => return Ok(value.to_owned()),
        },
        None => chrono_tz::Asia::Taipei,
    };
    match zone.from_local_datetime(&naive).single() {
        Some(local) => Ok(local.to_rfc3339()),
        None => Ok(value.to_owned()),
    }
}

fn property_parameter(property: &ical::property::Property, name: &str) -> Option<String> {
    property.params.as_ref()?.iter().find_map(|(key, values)| {
        if key.eq_ignore_ascii_case(name) {
            values
                .first()
                .map(|value| value.trim_matches('"').to_owned())
        } else {
            None
        }
    })
}

fn property_value(properties: &[ical::property::Property], name: &str) -> Option<String> {
    properties
        .iter()
        .find(|property| property.name.eq_ignore_ascii_case(name))
        .and_then(|property| property.value.clone())
}

fn property_values(properties: &[ical::property::Property], name: &str) -> Vec<String> {
    properties
        .iter()
        .filter(|property| property.name.eq_ignore_ascii_case(name))
        .filter_map(|property| property.value.clone())
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn propfind_body() -> String {
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:propfind xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\"><D:prop><D:displayname/><D:resourcetype/><D:getetag/><D:getcontenttype/><D:current-user-principal/><C:supported-calendar-component-set/><C:calendar-home-set/><CR:supported-address-data/><CR:addressbook-home-set/></D:prop></D:propfind>".to_owned()
}

fn report_body(kind: Kind) -> String {
    match kind {
        Kind::Calendar => "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<C:calendar-query xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:caldav\"><D:prop><D:getetag/><C:calendar-data/></D:prop><C:filter><C:comp-filter name=\"VCALENDAR\"/></C:filter></C:calendar-query>".to_owned(),
        Kind::Contacts => "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<CR:addressbook-query xmlns:D=\"DAV:\" xmlns:CR=\"urn:ietf:params:xml:ns:carddav\"><D:prop><D:getetag/><CR:address-data/></D:prop><CR:filter><CR:prop-filter name=\"FN\"/></CR:filter></CR:addressbook-query>".to_owned(),
        Kind::Mail => String::new(),
    }
}

fn matching_collection(value: &str, collections: &[Collection]) -> Result<Url> {
    let value = value.trim();
    for collection in collections {
        let parsed = Url::parse(&collection.id)
            .with_context(|| format!("DAV collection URL 格式錯誤：{}", collection.id))?;
        if value == collection.id
            || value == parsed.path()
            || value.trim_end_matches('/') == collection.id.trim_end_matches('/')
            || value.trim_end_matches('/') == parsed.path().trim_end_matches('/')
        {
            return Ok(parsed);
        }
    }
    bail!("指定的 DAV collection 不在已發現的集合中：{value}")
}

fn resource_url(collection: &Url, value: &str) -> Result<Url> {
    let remote = if let Ok(parsed) = Url::parse(value.trim()) {
        parsed
    } else {
        collection
            .join(value.trim())
            .map_err(|_| anyhow!("DAV 資源 URL 格式錯誤"))?
    };
    ensure_same_origin(collection, &remote)?;
    if remote.query().is_some() || remote.fragment().is_some() {
        bail!("DAV 資源 URL 不可包含 query 或 fragment");
    }
    ensure_resource_under_collection(collection, &remote)?;
    Ok(remote)
}

fn ensure_resource_under_collection(collection: &Url, resource: &Url) -> Result<()> {
    ensure_same_origin(collection, resource)?;
    let collection_path = crate::dav_url::normalized_path(collection.path());
    let collection_path = collection_path.trim_end_matches('/');
    let resource_path = crate::dav_url::normalized_path(resource.path());
    let prefix = if collection_path.is_empty() {
        "/".to_owned()
    } else {
        format!("{collection_path}/")
    };
    if !resource_path.starts_with(&prefix) {
        bail!("DAV 資源不屬於指定 collection");
    }
    Ok(())
}

fn resolve_href(base: &Url, href: &str) -> Result<Url> {
    let url = if let Ok(url) = Url::parse(href) {
        url
    } else {
        base.join(href)
            .map_err(|_| anyhow!("DAV href URL 格式錯誤：{href}"))?
    };
    if url.query().is_some() || url.fragment().is_some() {
        bail!("DAV href URL 不可包含 query 或 fragment");
    }
    Ok(url)
}

fn ensure_same_origin(left: &Url, right: &Url) -> Result<()> {
    let same_scheme = left.scheme().eq_ignore_ascii_case(right.scheme());
    let same_host = left
        .host_str()
        .zip(right.host_str())
        .map(|(left_host, right_host)| left_host.eq_ignore_ascii_case(right_host))
        .unwrap_or(false);
    let same_port = effective_port(left) == effective_port(right);
    if !same_scheme || !same_host || !same_port {
        bail!("DAV href 跨 origin，已拒絕傳送憑證");
    }
    Ok(())
}

fn effective_port(url: &Url) -> u16 {
    url.port_or_known_default().unwrap_or(0)
}

fn is_localhost(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false)
}

fn is_principal_url(url: &Url) -> bool {
    url.path_segments()
        .map(|mut segments| {
            segments.any(|segment| {
                segment.eq_ignore_ascii_case("principal")
                    || segment.eq_ignore_ascii_case("principals")
            })
        })
        .unwrap_or(false)
}

fn same_url(left: &Url, right: &Url) -> bool {
    left.scheme().eq_ignore_ascii_case(right.scheme())
        && left
            .host_str()
            .zip(right.host_str())
            .map(|(left_host, right_host)| left_host.eq_ignore_ascii_case(right_host))
            .unwrap_or(false)
        && effective_port(left) == effective_port(right)
        && crate::dav_url::normalized_path(left.path()).trim_end_matches('/')
            == crate::dav_url::normalized_path(right.path()).trim_end_matches('/')
}

fn collection_matches(value: &str, collection: &Url) -> bool {
    crate::dav_url::identity(value) == canonical_collection_id(collection)
        || crate::dav_url::normalized_path(value).trim_end_matches('/')
            == crate::dav_url::normalized_path(collection.path()).trim_end_matches('/')
}

fn remote_matches(value: &str, resource: &Url) -> bool {
    if value == resource.as_str() || value == resource.path() {
        return true;
    }
    if let Ok(parsed) = Url::parse(value) {
        return same_url(&parsed, resource);
    }
    false
}

fn canonical_collection_id(url: &Url) -> String {
    crate::dav_url::collection_id(url)
}

fn collection_name(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
        .map(percent_decode)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| url.host_str().unwrap_or("DAV").to_owned())
}

fn percent_decode(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    let mut chars = value.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let high = chars.next();
            let low = chars.next();
            if let (Some(high), Some(low)) = (high, low) {
                if let (Some(high), Some(low)) = (hex_value(high), hex_value(low)) {
                    bytes.push(high * 16 + low);
                    continue;
                }
            }
        }
        bytes.push(byte);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn encode_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(*byte as char);
        } else {
            encoded.push('%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
    }
    encoded
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn extension(kind: Kind) -> &'static str {
    match kind {
        Kind::Calendar => "ics",
        Kind::Contacts => "vcf",
        Kind::Mail => "bin",
    }
}

fn stable_id(collection: &str, remote_id: &str) -> String {
    format!("{collection}::{remote_id}")
}

fn is_element_named(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name().eq_ignore_ascii_case(name)
}

fn direct_child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children().find(|child| is_element_named(*child, name))
}

fn text_content(node: Node<'_, '_>) -> String {
    node.descendants()
        .filter(|descendant| descendant.is_text())
        .filter_map(|descendant| descendant.text())
        .collect::<Vec<_>>()
        .join("")
}

fn property_text(properties: &[Node<'_, '_>], name: &str) -> Option<String> {
    properties.iter().find_map(|property| {
        property.descendants().find_map(|node| {
            if is_element_named(node, name) {
                Some(text_content(node))
            } else {
                None
            }
        })
    })
}

fn property_hrefs(properties: &[Node<'_, '_>], name: &str) -> Vec<String> {
    properties
        .iter()
        .filter_map(|property| {
            property.descendants().find_map(|node| {
                if is_element_named(node, name) {
                    Some(
                        node.descendants()
                            .filter(|descendant| is_element_named(*descendant, "href"))
                            .map(text_content)
                            .collect::<Vec<_>>(),
                    )
                } else {
                    None
                }
            })
        })
        .flatten()
        .filter(|href| !href.trim().is_empty())
        .collect()
}

fn status_code(status: &str) -> Option<u16> {
    status.split_whitespace().find_map(|part| part.parse().ok())
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + value - 10) as char,
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    fn response(href: &str, properties: &str) -> String {
        format!("<d:response><d:href>{href}</d:href>{properties}</d:response>")
    }

    fn propstat(status: u16, properties: &str) -> String {
        format!("<d:propstat><d:prop>{properties}</d:prop><d:status>HTTP/1.1 {status} Status</d:status></d:propstat>")
    }

    fn xml(responses: &str) -> String {
        format!("<d:multistatus xmlns:d='DAV:' xmlns:c='urn:ietf:params:xml:ns:caldav'>{responses}</d:multistatus>")
    }

    fn collections(responses: &str) -> Result<Vec<Collection>> {
        let base = Url::parse("https://example.com/cal/").unwrap();
        let listing = parse_listing(&xml(responses), &base, &base, Kind::Calendar, false)?;
        collections_from_resources(Kind::Calendar, listing, Some(&base))
    }

    fn readable() -> String {
        propstat(
            200,
            "<d:resourcetype><d:collection/><c:calendar/></d:resourcetype>",
        )
    }

    #[test]
    fn mixed_success_and_denied_properties_keep_collection() {
        for status in [401, 403, 404] {
            let rows = response(
                "/cal/ok/",
                &(readable() + &propstat(status, "<d:displayname/>")),
            );
            assert_eq!(collections(&rows).unwrap().len(), 1);
        }
    }

    #[test]
    fn denied_response_does_not_hide_other_collection() {
        for status in [401, 403, 404] {
            let rows = response("/cal/denied/", &propstat(status, "<d:resourcetype/>"))
                + &response("/cal/ok/", &readable());
            let found = collections(&rows).unwrap();
            assert_eq!(found.len(), 1);
            assert!(found[0].id.ends_with("/ok/"));
        }
    }

    #[test]
    fn only_denied_collection_reports_permission_and_href() {
        let error = collections(&response(
            "/cal/denied/",
            &propstat(403, "<d:resourcetype/>"),
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("權限"));
        assert!(error.contains("403"));
        assert!(error.contains("https://example.com/cal/denied/"));
    }

    #[test]
    fn denied_with_readable_noncollection_still_reports_permission() {
        let rows = response("/cal/denied/", &propstat(403, ""))
            + &response(
                "/cal/item.ics",
                &propstat(200, "<d:getetag>etag</d:getetag>"),
            );
        assert!(collections(&rows).unwrap_err().to_string().contains("403"));
    }

    #[test]
    fn reports_never_silently_drop_denied_resources() {
        let base = Url::parse("https://example.com/cal/").unwrap();
        for denied in [
            propstat(403, "<c:calendar-data/>"),
            "<d:status>HTTP/1.1 403 Forbidden</d:status>".into(),
            propstat(401, "<c:calendar-data/>") + &propstat(200, "<d:getetag>etag</d:getetag>"),
        ] {
            let body = xml(&response("/cal/private.ics", &denied));
            let error = parse_multistatus(&body, &base, &base, Kind::Calendar, true)
                .unwrap_err()
                .to_string();
            assert!(error.contains("權限"), "{error}");
            assert!(error.contains("/cal/private.ics"));
        }
    }

    #[test]
    fn report_accepts_readable_data_with_denied_optional_property() {
        let base = Url::parse("https://example.com/cal/").unwrap();
        let body = xml(&response(
            "/cal/item.ics",
            &(propstat(200, "<c:calendar-data>data</c:calendar-data>")
                + &propstat(403, "<d:displayname/>")),
        ));
        assert_eq!(
            parse_multistatus(&body, &base, &base, Kind::Calendar, true).unwrap()[0]
                .data
                .as_deref(),
            Some("data")
        );
    }

    #[test]
    fn unexpected_status_remains_an_error() {
        for status in [302, 409, 500, 503] {
            assert!(
                collections(&response("/cal/", &(readable() + &propstat(status, "")))).is_err()
            );
        }
    }
}

#[cfg(test)]
mod report_missing_tests {
    use super::*;

    #[test]
    fn metadata_report_cannot_silently_drop_missing_resource() {
        let base = Url::parse("https://example.com/cal/").unwrap();
        let body = "<multistatus><response><href>/cal/item.ics</href><propstat><prop/><status>HTTP/1.1 404 Not Found</status></propstat></response></multistatus>";
        let error = parse_multistatus(body, &base, &base, Kind::Calendar, false).unwrap_err();
        assert!(error.to_string().contains("404"));
    }
}

#[cfg(test)]
mod collection_identity_tests {
    use super::*;

    #[test]
    fn discovered_collection_accepts_encoded_config_without_slash() {
        let config = Config::new("fixture@example.test".into(), "example.test").unwrap();
        let mut session = DavSession::new(&config, "unused-fixture").unwrap();
        let discovered = config.caldav_url.replace("%40", "@");
        session
            .remember(&[Collection {
                kind: Kind::Calendar,
                id: discovered.clone(),
                name: "fixture".into(),
            }])
            .unwrap();
        assert!(session
            .known_collection(Kind::Calendar, config.caldav_url.trim_end_matches('/'))
            .unwrap()
            .is_some());
        assert!(session
            .known_collection(Kind::Calendar, &discovered)
            .unwrap()
            .is_some());
        assert!(session
            .known_collection(Kind::Calendar, &format!("{discovered}other/"))
            .unwrap()
            .is_none());
        assert!(collection_matches(
            config.caldav_url.trim_end_matches('/'),
            &Url::parse(&discovered).unwrap()
        ));
        assert!(same_url(
            &Url::parse(&config.caldav_url).unwrap(),
            &Url::parse(&discovered).unwrap()
        ));
    }
}
