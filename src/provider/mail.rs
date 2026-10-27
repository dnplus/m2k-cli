use crate::{
    config::Config,
    model::{Attachment, Collection, Item, Kind, OutgoingMail, Snapshot},
};
use anyhow::{anyhow, bail, Context, Result};
use async_imap::{
    imap_proto::{MailboxDatum, Response, Status},
    types::{Flag, NameAttribute},
    Client, Session,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::{DateTime, Duration, Utc};
use futures_util::{pin_mut, TryStreamExt};
use ical::IcalParser;
use lettre::{
    message::{header, Attachment as LettreAttachment, Mailbox, Message, MultiPart, SinglePart},
    transport::smtp::{
        authentication::Credentials,
        client::{Tls, TlsParameters},
    },
    SmtpTransport, Transport,
};
use mailparse::{
    addrparse_header, parse_content_disposition, parse_headers, parse_mail, DispositionType,
    MailAddr, MailHeader, MailHeaderMap, ParsedMail,
};
use native_tls::TlsConnector as NativeTlsConnector;
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    io::BufReader,
    path::Path,
    time::Duration as StdDuration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    runtime::Builder,
    time::timeout,
};
use tokio_native_tls::{TlsConnector, TlsStream};
use uuid::Uuid;

const IMAP_TIMEOUT: StdDuration = StdDuration::from_secs(30);
const SMTP_TIMEOUT: StdDuration = StdDuration::from_secs(30);
const RAW_BASE64_PREFIX: &str = "base64:";
const CACHE_DECODE_CHUNK: usize = 4096;
const CACHE_DECODE_OUTPUT: usize = CACHE_DECODE_CHUNK / 4 * 3;
const SEARCH_MAX_ITEMS: usize = 500;
const SEARCH_WINDOW_SIZE: u32 = 2048;
const SEARCH_MAX_WINDOWS: usize = 32;
const TEST_ROOT_CERT_ENV: &str = "M2K_TEST_ROOT_CERT";

type ImapStream = TlsStream<TcpStream>;
type ImapSession = Session<ImapStream>;

mod move_batch;
pub mod watch;
pub use move_batch::{move_many, MovePath, MovePlan};

trait MailTransport: AsyncRead + AsyncWrite + Unpin + Debug + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Debug + Send> MailTransport for T {}

#[derive(Debug)]
pub struct SearchResults {
    pub items: Vec<Item>,
    pub truncated: bool,
}

struct FetchedMessage {
    uid: u32,
    flags: Vec<String>,
    raw: Option<Vec<u8>>,
    internal_date: Option<String>,
}

fn run_async<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("建立網路執行環境失敗")?
        .block_on(future)
}

fn require(value: &str, name: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("缺少 {name}");
    }
    Ok(value.to_owned())
}

fn require_password(value: &str) -> Result<&str> {
    if value.is_empty() {
        bail!("缺少密碼");
    }
    Ok(value)
}

fn normalize_folders(mut folders: Vec<String>) -> Result<Vec<String>> {
    folders.sort();
    folders.dedup();
    if folders.is_empty() {
        bail!("IMAP 沒有可選取的資料夾");
    }
    Ok(folders)
}

fn valid_base64_cache(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return false;
    }
    let chunk_count = bytes.len().div_ceil(CACHE_DECODE_CHUNK);
    let mut decoded_len = 0;
    let mut output = [0_u8; CACHE_DECODE_OUTPUT];
    for (index, chunk) in bytes.chunks(CACHE_DECODE_CHUNK).enumerate() {
        let is_last = index + 1 == chunk_count;
        if !is_last && chunk.contains(&b'=') {
            return false;
        }
        let Ok(length) = BASE64_STANDARD.decode_slice(chunk, &mut output) else {
            return false;
        };
        if !is_last && length != CACHE_DECODE_OUTPUT {
            return false;
        }
        decoded_len += length;
    }
    decoded_len > 0
}

fn cached_body_present(item: &Item) -> bool {
    if item.raw.is_empty() {
        return false;
    }
    item.raw
        .strip_prefix(RAW_BASE64_PREFIX)
        .map(valid_base64_cache)
        .unwrap_or(true)
}

fn imap_port(config: &Config) -> Result<u16> {
    if config.imap_port == 0 {
        bail!("IMAP port 不可為 0");
    }
    Ok(config.imap_port)
}

fn smtp_port(config: &Config) -> Result<u16> {
    match config.smtp_port {
        465 | 587 => Ok(config.smtp_port),
        // Loopback fixtures cannot bind 465 portably; any other port is implicit TLS.
        port if is_loopback_host(&config.smtp_host) => Ok(port),
        port => bail!("SMTP 僅支援 TLS 465 或 STARTTLS 587，目前是 {port}"),
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

// Extra trust anchor for the local fixture server only. Certificate and
// hostname verification stay on; a non-loopback host refuses to connect.
fn test_root_certificate(host: &str) -> Result<Option<Vec<u8>>> {
    read_test_root_certificate(std::env::var_os(TEST_ROOT_CERT_ENV), host)
}

fn read_test_root_certificate(
    path: Option<std::ffi::OsString>,
    host: &str,
) -> Result<Option<Vec<u8>>> {
    let Some(path) = path.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if !is_loopback_host(host) {
        bail!("{TEST_ROOT_CERT_ENV} 只能用於 loopback 主機，目前是 {host}");
    }
    std::fs::read(&path).map(Some).with_context(|| {
        format!(
            "無法讀取 {TEST_ROOT_CERT_ENV}：{}",
            Path::new(&path).display()
        )
    })
}

async fn connect_imap(config: &Config, password: &str) -> Result<ImapSession> {
    let host = require(&config.imap_host, "IMAP host")?;
    let username = require(&config.username, "帳號")?;
    let password = require_password(password)?.to_owned();
    let port = imap_port(config)?;
    let tcp = timeout(IMAP_TIMEOUT, TcpStream::connect((host.as_str(), port)))
        .await
        .map_err(|_| anyhow!("IMAP TCP 連線逾時"))?
        .with_context(|| format!("IMAP TCP 連線失敗: {host}:{port}"))?;
    let mut native = NativeTlsConnector::builder();
    if let Some(pem) = test_root_certificate(&host)? {
        native.add_root_certificate(
            native_tls::Certificate::from_pem(&pem).context("測試根憑證格式錯誤")?,
        );
    }
    let native = native.build().context("建立 IMAP TLS connector 失敗")?;
    let tls = timeout(IMAP_TIMEOUT, TlsConnector::from(native).connect(&host, tcp))
        .await
        .map_err(|_| anyhow!("IMAP TLS 連線逾時"))?
        .context("IMAP TLS 握手失敗")?;
    let mut client = Client::new(tls);
    timeout(IMAP_TIMEOUT, client.read_response())
        .await
        .map_err(|_| anyhow!("IMAP greeting 逾時"))?
        .context("讀取 IMAP greeting 失敗")?
        .ok_or_else(|| anyhow!("IMAP 伺服器沒有送出 greeting"))?;
    let session = timeout(IMAP_TIMEOUT, client.login(username, password))
        .await
        .map_err(|_| anyhow!("IMAP 登入逾時"))?
        .map_err(|(error, _)| anyhow!("IMAP 登入失敗: {error}"))?;
    Ok(session)
}

async fn logout(mut session: ImapSession) {
    let _ = timeout(IMAP_TIMEOUT, session.logout()).await;
}

async fn list_folders<S: MailTransport>(session: &mut Session<S>) -> Result<Vec<String>> {
    let mut stream = timeout(IMAP_TIMEOUT, session.list(None, Some("*")))
        .await
        .map_err(|_| anyhow!("列出 IMAP 資料夾逾時"))?
        .context("列出 IMAP 資料夾失敗")?;
    let mut folders = Vec::new();
    while let Some(name) = timeout(IMAP_TIMEOUT, stream.try_next())
        .await
        .map_err(|_| anyhow!("讀取 IMAP 資料夾逾時"))?
        .context("讀取 IMAP 資料夾失敗")?
    {
        if name
            .attributes()
            .iter()
            .any(|attribute| matches!(attribute, NameAttribute::NoSelect))
        {
            continue;
        }
        let folder = name.name().trim();
        if !folder.is_empty() {
            folders.push(folder.to_owned());
        }
    }
    normalize_folders(folders)
}

const FETCH_BATCH_SIZE: usize = 100;

fn uid_set(uids: &[u32]) -> String {
    uids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn ensure_fetched(
    expected: &HashSet<u32>,
    fetched: &HashMap<u32, FetchedMessage>,
    operation: &str,
) -> Result<()> {
    let missing = expected
        .iter()
        .filter(|uid| !fetched.contains_key(uid))
        .map(u32::to_string)
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("{operation} 未取得預期 UID: {}", missing.join(","));
    }
    Ok(())
}

fn flag_text(flag: Flag<'_>) -> String {
    match flag {
        Flag::Seen => "\\Seen".to_owned(),
        Flag::Answered => "\\Answered".to_owned(),
        Flag::Flagged => "\\Flagged".to_owned(),
        Flag::Deleted => "\\Deleted".to_owned(),
        Flag::Draft => "\\Draft".to_owned(),
        Flag::Recent => "\\Recent".to_owned(),
        Flag::MayCreate => "\\*".to_owned(),
        Flag::Custom(value) => value.into_owned(),
    }
}

fn flags(fetch: &async_imap::types::Fetch) -> Vec<String> {
    let mut flags = fetch.flags().map(flag_text).collect::<Vec<_>>();
    flags.sort();
    flags.dedup();
    flags
}

async fn fetch_messages<S: MailTransport>(
    session: &mut Session<S>,
    uids: &[u32],
    query: &str,
) -> Result<HashMap<u32, FetchedMessage>> {
    let mut messages = HashMap::new();
    for chunk in uids.chunks(FETCH_BATCH_SIZE) {
        fetch_chunk(session, chunk, query, &mut messages).await?;
    }
    Ok(messages)
}

async fn fetch_chunk<S: MailTransport>(
    session: &mut Session<S>,
    uids: &[u32],
    query: &str,
    messages: &mut HashMap<u32, FetchedMessage>,
) -> Result<()> {
    let mut stream = timeout(IMAP_TIMEOUT, session.uid_fetch(uid_set(uids), query))
        .await
        .map_err(|_| anyhow!("IMAP FETCH 逾時"))?
        .context("IMAP FETCH 失敗")?;
    while let Some(fetch) = timeout(IMAP_TIMEOUT, stream.try_next())
        .await
        .map_err(|_| anyhow!("IMAP FETCH 回應逾時"))?
        .context("讀取 IMAP FETCH 回應失敗")?
    {
        let uid = fetch
            .uid
            .ok_or_else(|| anyhow!("IMAP FETCH 回應缺少 UID"))?;
        let internal_date = fetch.internal_date().map(|value| value.to_rfc3339());
        let raw = if query.contains("BODY.PEEK[HEADER]") {
            fetch.header()
        } else {
            fetch.body()
        }
        .map(ToOwned::to_owned);
        messages.insert(
            uid,
            FetchedMessage {
                uid,
                flags: flags(&fetch),
                raw,
                internal_date,
            },
        );
    }
    Ok(())
}

fn retention_query(config: &Config) -> Result<String> {
    let days = i64::from(config.mail_retention_days);
    if days <= 0 {
        return Ok("ALL".to_owned());
    }
    let date = Utc::now() - Duration::days(days);
    Ok(format!("SINCE {}", date.format("%d-%b-%Y")))
}

fn parse_uid(item: &Item) -> Result<u32> {
    if item.kind != Kind::Mail {
        bail!("此項目不是郵件");
    }
    let remote_id = item.remote_id.trim();
    if remote_id.is_empty() {
        bail!("郵件缺少 remote UID");
    }
    remote_id.parse::<u32>().context("郵件 remote UID 無效")
}

fn stable_id(folder: &str, uid_validity: u32, uid: u32) -> String {
    format!("mail:{folder}:{uid_validity}:{uid}")
}

fn first_header(mail: &ParsedMail<'_>, name: &str) -> String {
    mail.headers
        .get_first_value(name)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn addresses(mail: &ParsedMail<'_>, name: &str) -> Vec<String> {
    addresses_in_headers(&mail.headers, name)
}

fn addresses_in_headers(headers: &[MailHeader<'_>], name: &str) -> Vec<String> {
    let mut result = Vec::new();
    for header in headers.get_all_headers(name) {
        let Ok(parsed) = addrparse_header(header) else {
            continue;
        };
        for address in parsed.iter() {
            match address {
                MailAddr::Single(single) => result.push(single.addr.trim().to_lowercase()),
                MailAddr::Group(group) => result.extend(
                    group
                        .addrs
                        .iter()
                        .map(|single| single.addr.trim().to_lowercase()),
                ),
            }
        }
    }
    result.retain(|value| !value.is_empty());
    result.sort();
    result.dedup();
    result
}

fn part_filename(part: &ParsedMail<'_>) -> Option<String> {
    let disposition = parse_content_disposition(&first_header(part, "Content-Disposition"));
    disposition
        .params
        .get("filename")
        .cloned()
        .or_else(|| part.ctype.params.get("name").cloned())
        .filter(|value| !value.trim().is_empty())
}

fn is_attachment(part: &ParsedMail<'_>) -> bool {
    let disposition = parse_content_disposition(&first_header(part, "Content-Disposition"));
    disposition.disposition == DispositionType::Attachment || part_filename(part).is_some()
}

fn attachment_body(part: &ParsedMail<'_>) -> Result<Vec<u8>> {
    if part.subparts.is_empty() {
        part.get_body_raw().context("解碼郵件附件失敗")
    } else {
        Ok(part.raw_bytes.to_vec())
    }
}

fn collect_attachments(
    part: &ParsedMail<'_>,
    output: &mut Vec<(String, String, Vec<u8>)>,
) -> Result<()> {
    if is_attachment(part) {
        let filename =
            part_filename(part).unwrap_or_else(|| format!("attachment-{}", output.len() + 1));
        let content_type = part.ctype.mimetype.to_owned();
        let body = attachment_body(part)?;
        output.push((filename, content_type, body));
        return Ok(());
    }
    if !part.subparts.is_empty() {
        for subpart in &part.subparts {
            collect_attachments(subpart, output)?;
        }
        return Ok(());
    }
    Ok(())
}

fn find_body(part: &ParsedMail<'_>, mimetype: &str) -> Result<Option<String>> {
    if is_attachment(part) {
        return Ok(None);
    }
    if !part.subparts.is_empty() {
        for subpart in &part.subparts {
            if let Some(body) = find_body(subpart, mimetype)? {
                return Ok(Some(body));
            }
        }
        return Ok(None);
    }
    if !part.ctype.mimetype.eq_ignore_ascii_case(mimetype) {
        return Ok(None);
    }
    let body = part.get_body().context("解碼郵件本文失敗")?;
    let body = if mimetype == "text/html" {
        html_to_text(&body)
    } else {
        body
    };
    Ok((!body.trim().is_empty()).then_some(body))
}

fn body_text(mail: &ParsedMail<'_>) -> Result<String> {
    if let Some(body) = find_body(mail, "text/plain")? {
        return Ok(body);
    }
    Ok(find_body(mail, "text/html")?.unwrap_or_default())
}

fn decode_html_entities(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest
            .as_bytes()
            .iter()
            .take(17)
            .position(|byte| *byte == b';')
        else {
            output.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "ndash" => Some('–'),
            "mdash" => Some('—'),
            "hellip" => Some('…'),
            "bull" => Some('•'),
            "copy" => Some('©'),
            "reg" => Some('®'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|digits| digits.parse().ok())
                })
                .and_then(char::from_u32),
        };
        if let Some(character) = decoded {
            output.push(character);
            rest = &rest[end + 1..];
        } else {
            output.push('&');
            rest = &rest[1..];
        }
    }
    output.push_str(rest);
    output
}

fn html_to_text(html: &str) -> String {
    let mut output = String::with_capacity(html.len());
    let mut rest = html;
    let mut hidden = None::<String>;
    while !rest.is_empty() {
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        if rest.starts_with('<') {
            let mut quote = None;
            let end = rest.char_indices().skip(1).find_map(|(index, character)| {
                match (quote, character) {
                    (Some(current), character) if current == character => quote = None,
                    (None, '\'' | '"') => quote = Some(character),
                    (None, '>') => return Some(index),
                    _ => {}
                }
                None
            });
            if let Some(end) = end {
                let tag = rest[1..end].trim();
                let closing = tag.starts_with('/');
                let name = tag
                    .trim_start_matches('/')
                    .split_ascii_whitespace()
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches('/')
                    .to_ascii_lowercase();
                if name.starts_with(|character: char| character.is_ascii_alphabetic())
                    || name.starts_with(['!', '?'])
                {
                    if hidden.as_deref() == Some(name.as_str()) && closing {
                        hidden = None;
                    } else if hidden.is_none() {
                        if !closing && matches!(name.as_str(), "head" | "script" | "style") {
                            hidden = Some(name);
                        } else if matches!(
                            name.as_str(),
                            "br" | "p"
                                | "div"
                                | "li"
                                | "tr"
                                | "hr"
                                | "h1"
                                | "h2"
                                | "h3"
                                | "h4"
                                | "h5"
                                | "h6"
                                | "blockquote"
                                | "pre"
                        ) {
                            output.push('\n');
                        } else if matches!(name.as_str(), "td" | "th") {
                            output.push(' ');
                        }
                    }
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        let character = rest.chars().next().expect("rest is nonempty");
        if hidden.is_none() {
            output.push(character);
        }
        rest = &rest[character.len_utf8()..];
    }
    decode_html_entities(&output)
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn validated_internal_date(value: Option<&str>) -> Option<String> {
    value.and_then(|value| {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|date| date.with_timezone(&Utc).to_rfc3339())
    })
}

fn parse_date(mail: &ParsedMail<'_>, internal_date: Option<&str>) -> Option<String> {
    parse_header_date(&mail.headers, internal_date)
}

fn parse_header_date(headers: &[MailHeader<'_>], internal_date: Option<&str>) -> Option<String> {
    if let Some(date) = validated_internal_date(internal_date) {
        return Some(date);
    }
    if let Some(value) = headers.get_first_value("Date") {
        if let Ok(timestamp) = mailparse::dateparse(&value) {
            if let Some(date) = DateTime::<Utc>::from_timestamp(timestamp, 0) {
                return Some(date.to_rfc3339());
            }
        }
    }
    None
}

fn parse_item(folder: &str, uid_validity: u32, fetched: &FetchedMessage) -> Result<Item> {
    let raw_bytes = fetched
        .raw
        .as_deref()
        .ok_or_else(|| anyhow!("IMAP FETCH UID {} 缺少正文", fetched.uid))?;
    let mail = parse_mail(raw_bytes).context("解析郵件 MIME 失敗")?;
    let subject = display_header(&first_header(&mail, "Subject"));
    let from = display_header(&first_header(&mail, "From"));
    let text = body_text(&mail)?;
    let mut parsed_attachments = Vec::new();
    collect_attachments(&mail, &mut parsed_attachments)?;
    let attachments = parsed_attachments
        .iter()
        .enumerate()
        .map(|(index, (filename, content_type, body))| Attachment {
            index,
            filename: filename.clone(),
            content_type: content_type.clone(),
            size: body.len(),
        })
        .collect();
    let mut emails = addresses(&mail, "From");
    emails.extend(addresses(&mail, "To"));
    emails.extend(addresses(&mail, "Cc"));
    emails.sort();
    emails.dedup();
    Ok(Item {
        id: stable_id(folder, uid_validity, fetched.uid),
        kind: Kind::Mail,
        collection: folder.to_owned(),
        remote_id: fetched.uid.to_string(),
        etag: Some(format!("{uid_validity}:{uid}", uid = fetched.uid)),
        uid_validity: Some(uid_validity),
        title: if subject.is_empty() {
            "(無主旨)".to_owned()
        } else {
            subject
        },
        subtitle: from,
        date: parse_date(&mail, fetched.internal_date.as_deref()),
        end_date: None,
        text,
        raw: format!("{RAW_BASE64_PREFIX}{}", BASE64_STANDARD.encode(raw_bytes)),
        flags: fetched.flags.clone(),
        emails,
        phones: Vec::new(),
        attachments,
    })
}

async fn sync_async(config: &Config, password: &str, existing: &[Item]) -> Result<Snapshot> {
    let query = retention_query(config)?;
    let mut session = connect_imap(config, password).await?;
    let result = sync_with_session(&mut session, existing, &query).await;
    logout(session).await;
    result
}

async fn sync_with_session<S: MailTransport>(
    session: &mut Session<S>,
    existing: &[Item],
    query: &str,
) -> Result<Snapshot> {
    let folders = list_folders(session).await?;
    let mut collections = Vec::with_capacity(folders.len());
    let mut items = Vec::new();
    let mut cache = HashMap::new();
    for item in existing.iter().filter(|item| item.kind == Kind::Mail) {
        if cached_body_present(item) {
            if let (Some(uid_validity), Ok(uid)) = (item.uid_validity, parse_uid(item)) {
                cache.insert((item.collection.clone(), uid_validity, uid), item);
            }
        }
    }
    for folder in folders {
        let mailbox = timeout(IMAP_TIMEOUT, session.examine(&folder))
            .await
            .map_err(|_| anyhow!("選取 IMAP 資料夾 {folder} 逾時"))?
            .with_context(|| format!("選取 IMAP 資料夾失敗: {folder}"))?;
        let uid_validity = mailbox
            .uid_validity
            .ok_or_else(|| anyhow!("IMAP 資料夾 {folder} 缺少 UIDVALIDITY"))?;
        let selected = timeout(IMAP_TIMEOUT, session.uid_search(query))
            .await
            .map_err(|_| anyhow!("搜尋 IMAP 資料夾 {folder} 逾時"))?
            .with_context(|| format!("搜尋 IMAP 資料夾失敗: {folder}"))?;
        let mut uids = selected.into_iter().collect::<Vec<_>>();
        uids.sort_unstable();
        collections.push(Collection {
            kind: Kind::Mail,
            id: folder.clone(),
            name: folder.clone(),
        });
        if uids.is_empty() {
            continue;
        }
        let expected = uids.iter().copied().collect::<HashSet<_>>();
        let flag_fetches = fetch_messages(session, &uids, "(UID FLAGS INTERNALDATE)").await?;
        ensure_fetched(&expected, &flag_fetches, "FLAGS FETCH")?;
        let missing_body = uids
            .iter()
            .copied()
            .filter(|uid| !cache.contains_key(&(folder.clone(), uid_validity, *uid)))
            .collect::<Vec<_>>();
        let body_fetches = fetch_messages(
            session,
            &missing_body,
            "(UID FLAGS INTERNALDATE BODY.PEEK[])",
        )
        .await?;
        let missing_expected = missing_body.iter().copied().collect::<HashSet<_>>();
        ensure_fetched(&missing_expected, &body_fetches, "BODY.PEEK FETCH")?;
        for uid in uids {
            let flag_fetch = flag_fetches
                .get(&uid)
                .ok_or_else(|| anyhow!("FLAGS FETCH 遺失 UID {uid}"))?;
            let key = (folder.clone(), uid_validity, uid);
            if let Some(cached_item) = cache.get(&key) {
                let mut item = (*cached_item).clone();
                let _ = normalize_cached(&mut item);
                item.flags = flag_fetch.flags.clone();
                if let Some(date) = validated_internal_date(flag_fetch.internal_date.as_deref()) {
                    item.date = Some(date);
                }
                item.uid_validity = Some(uid_validity);
                item.remote_id = uid.to_string();
                item.id = stable_id(&folder, uid_validity, uid);
                items.push(item);
            } else {
                let body_fetch = body_fetches
                    .get(&uid)
                    .ok_or_else(|| anyhow!("BODY.PEEK FETCH 遺失 UID {uid}"))?;
                let mut item = parse_item(&folder, uid_validity, body_fetch)?;
                item.flags = flag_fetch.flags.clone();
                items.push(item);
            }
        }
    }
    items.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(Snapshot {
        kind: Kind::Mail,
        collections,
        items,
    })
}

pub fn verify(config: &Config, password: &str) -> Result<Option<u16>> {
    verify_imap(config, password)?;
    verify_smtp(config, password)
}

pub fn verify_imap(config: &Config, password: &str) -> Result<()> {
    run_async(async {
        let session = connect_imap(config, password).await?;
        logout(session).await;
        Ok(())
    })
}

pub fn sync(config: &Config, password: &str, existing: &[Item]) -> Result<Snapshot> {
    run_async(sync_async(config, password, existing))
}

fn search_text(query: &str) -> Result<String> {
    if query.chars().any(char::is_control) {
        bail!("搜尋文字不可包含控制字元");
    }
    let query = query.trim();
    if query.is_empty() {
        bail!("搜尋文字不可為空白");
    }
    if query.len() > 4096 {
        bail!("搜尋文字不可超過 4096 bytes");
    }
    Ok(query.to_owned())
}

async fn search_window<S: MailTransport>(
    session: &mut Session<S>,
    start: u32,
    end: u32,
    text: &str,
) -> Result<HashSet<u32>> {
    if text.is_ascii() {
        let quoted = text.replace('\\', "\\\\").replace('"', "\\\"");
        return session
            .uid_search(format!("{start}:{end} TEXT \"{quoted}\""))
            .await
            .context("IMAP SEARCH 失敗");
    }
    let request = session
        .run_command(format!(
            "UID SEARCH CHARSET UTF-8 {start}:{end} TEXT {{{}}}",
            text.len()
        ))
        .await?;
    loop {
        let response = session
            .read_response()
            .await?
            .ok_or_else(|| anyhow!("IMAP UTF-8 SEARCH 等待 literal 時斷線"))?;
        match response.parsed() {
            Response::Continue { .. } => break,
            Response::Done { .. }
            | Response::Data {
                status: Status::Bye,
                ..
            } => {
                bail!("IMAP 伺服器拒絕 UTF-8 SEARCH literal");
            }
            _ => {}
        }
    }
    session.run_command_untagged(text).await?;
    let mut uids = HashSet::new();
    loop {
        let response = session
            .read_response()
            .await?
            .ok_or_else(|| anyhow!("IMAP UTF-8 SEARCH 讀取結果時斷線"))?;
        match response.parsed() {
            Response::MailboxData(MailboxDatum::Search(found)) => {
                uids.extend(found.iter().copied());
                if uids.len() > SEARCH_WINDOW_SIZE as usize {
                    bail!("IMAP SEARCH 回應超過搜尋窗口上限");
                }
            }
            Response::Done { tag, status, .. } if tag == &request => {
                if *status != Status::Ok {
                    bail!("IMAP 伺服器 UTF-8 SEARCH 失敗");
                }
                return Ok(uids);
            }
            Response::Done { .. }
            | Response::Data {
                status: Status::Bye,
                ..
            } => {
                bail!("IMAP UTF-8 SEARCH 回應狀態無效");
            }
            _ => {}
        }
    }
}

fn search_collection(collection: Option<&str>) -> Result<Option<String>> {
    collection
        .map(|folder| {
            if folder.chars().any(char::is_control) {
                bail!("IMAP 資料夾不可包含控制字元");
            }
            require(folder, "IMAP 資料夾")
        })
        .transpose()
}

fn parse_summary(folder: &str, uid_validity: u32, fetched: &FetchedMessage) -> Result<Item> {
    let raw = fetched
        .raw
        .as_deref()
        .ok_or_else(|| anyhow!("IMAP HEADER FETCH UID {} 缺少標頭", fetched.uid))?;
    let (headers, _) = parse_headers(raw).context("解析郵件標頭失敗")?;
    let title = display_header(&headers.get_first_value("Subject").unwrap_or_default());
    let mut emails = addresses_in_headers(&headers, "From");
    emails.extend(addresses_in_headers(&headers, "To"));
    emails.extend(addresses_in_headers(&headers, "Cc"));
    emails.sort();
    emails.dedup();
    Ok(Item {
        id: stable_id(folder, uid_validity, fetched.uid),
        kind: Kind::Mail,
        collection: folder.to_owned(),
        remote_id: fetched.uid.to_string(),
        etag: Some(format!("{uid_validity}:{}", fetched.uid)),
        uid_validity: Some(uid_validity),
        title: if title.trim().is_empty() {
            "(無主旨)".to_owned()
        } else {
            title.trim().to_owned()
        },
        subtitle: display_header(headers.get_first_value("From").unwrap_or_default().trim()),
        date: parse_header_date(&headers, fetched.internal_date.as_deref()),
        end_date: None,
        text: String::new(),
        raw: String::new(),
        flags: fetched.flags.clone(),
        emails,
        phones: Vec::new(),
        attachments: Vec::new(),
    })
}

async fn search_with_session<S: MailTransport>(
    session: &mut Session<S>,
    text: &str,
    collection: Option<&str>,
    limit: usize,
) -> Result<SearchResults> {
    let folders = match collection {
        Some(folder) => vec![folder.to_owned()],
        None => list_folders(session).await?,
    };
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut truncated = false;
    let mut windows = 0;
    for (folder_index, folder) in folders.iter().enumerate() {
        if items.len() == limit || windows == SEARCH_MAX_WINDOWS {
            truncated = true;
            break;
        }
        let mailbox = session
            .examine(folder)
            .await
            .with_context(|| format!("選取 IMAP 搜尋資料夾失敗: {folder}"))?;
        let uid_validity = mailbox
            .uid_validity
            .filter(|value| *value > 0)
            .ok_or_else(|| anyhow!("IMAP 資料夾 {folder} 缺少有效 UIDVALIDITY"))?;
        let mut end = mailbox.exists;
        while end > 0 {
            if items.len() == limit || windows == SEARCH_MAX_WINDOWS {
                truncated = true;
                break;
            }
            let start = end.saturating_sub(SEARCH_WINDOW_SIZE - 1).max(1);
            let selected = search_window(session, start, end, text)
                .await
                .with_context(|| format!("搜尋 IMAP 資料夾失敗: {folder}"))?;
            windows += 1;
            if selected.len() > SEARCH_WINDOW_SIZE as usize || selected.contains(&0) {
                bail!("IMAP SEARCH 回應超過搜尋窗口上限或包含無效 UID");
            }
            let mut uids = selected
                .into_iter()
                .filter(|uid| !seen.contains(&(folder.clone(), uid_validity, *uid)))
                .collect::<Vec<_>>();
            uids.sort_unstable_by(|left, right| right.cmp(left));
            let remaining = limit - items.len();
            if uids.len() > remaining {
                uids.truncate(remaining);
                truncated = true;
            }
            if !uids.is_empty() {
                let fetched =
                    fetch_messages(session, &uids, "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER])")
                        .await?;
                let expected = uids.iter().copied().collect::<HashSet<_>>();
                ensure_fetched(&expected, &fetched, "HEADER FETCH")?;
                for uid in uids {
                    items.push(parse_summary(folder, uid_validity, &fetched[&uid])?);
                    seen.insert((folder.clone(), uid_validity, uid));
                }
            }
            end = start - 1;
        }
        if items.len() == limit && folder_index + 1 < folders.len() {
            truncated = true;
        }
    }
    items.sort_by(|left, right| {
        right
            .date
            .cmp(&left.date)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(SearchResults { items, truncated })
}

pub fn search(
    config: &Config,
    password: &str,
    query: &str,
    collection: Option<&str>,
    limit: usize,
) -> Result<SearchResults> {
    let text = search_text(query)?;
    let collection = search_collection(collection)?;
    let limit = limit.min(SEARCH_MAX_ITEMS);
    if limit == 0 {
        return Ok(SearchResults {
            items: Vec::new(),
            truncated: true,
        });
    }
    run_async(async {
        timeout(IMAP_TIMEOUT, async {
            let mut session = connect_imap(config, password).await?;
            let result =
                search_with_session(&mut session, &text, collection.as_deref(), limit).await;
            logout(session).await;
            result
        })
        .await
        .map_err(|_| anyhow!("IMAP 搜尋整體請求逾時 (30 秒)"))?
    })
}

async fn fetch_item_with_session<S: MailTransport>(
    session: &mut Session<S>,
    item: &Item,
) -> Result<Item> {
    let uid = parse_uid(item)?;
    let expected_validity = item
        .uid_validity
        .filter(|value| *value > 0)
        .ok_or_else(|| anyhow!("郵件缺少有效 UIDVALIDITY，請重新搜尋或同步"))?;
    let mailbox = session
        .examine(&item.collection)
        .await
        .context("唯讀選取郵件資料夾失敗")?;
    if mailbox.uid_validity != Some(expected_validity) {
        bail!("郵件 UIDVALIDITY 已變更，請重新搜尋或同步");
    }
    let mut messages =
        fetch_messages(session, &[uid], "(UID FLAGS INTERNALDATE BODY.PEEK[])").await?;
    let message = messages
        .remove(&uid)
        .ok_or_else(|| anyhow!("IMAP BODY.PEEK FETCH 找不到 UID {uid}"))?;
    parse_item(&item.collection, expected_validity, &message)
}

pub fn fetch_item(config: &Config, password: &str, item: &Item) -> Result<Item> {
    if item.kind != Kind::Mail {
        bail!("此項目不是郵件");
    }
    if cached_body_present(item) {
        return Ok(item.clone());
    }
    let uid = parse_uid(item)?;
    if uid == 0 || item.uid_validity.filter(|value| *value > 0).is_none() {
        bail!("郵件缺少有效 UID 或 UIDVALIDITY，請重新搜尋或同步");
    }
    search_collection(Some(&item.collection))?;
    run_async(async {
        timeout(IMAP_TIMEOUT, async {
            let mut session = connect_imap(config, password).await?;
            let result = fetch_item_with_session(&mut session, item).await;
            logout(session).await;
            result
        })
        .await
        .map_err(|_| anyhow!("IMAP 本文載入整體請求逾時 (30 秒)"))?
    })
}

fn normalize_flag(flag: &str) -> Result<String> {
    let trimmed = flag.trim();
    if trimmed.is_empty() || trimmed.contains([' ', '\t', '\r', '\n', '(', ')']) {
        bail!("郵件 flag 無效");
    }
    let lower = trimmed.to_ascii_lowercase();
    let normalized = match lower.as_str() {
        "seen" | "\\seen" => "\\Seen",
        "answered" | "\\answered" => "\\Answered",
        "flagged" | "\\flagged" => "\\Flagged",
        "deleted" | "\\deleted" => "\\Deleted",
        "draft" | "\\draft" => "\\Draft",
        "recent" | "\\recent" | "\\*" => bail!("不可修改此郵件 flag"),
        _ => {
            if !trimmed.bytes().all(|value| {
                value.is_ascii_alphanumeric()
                    || matches!(value, b'-' | b'_' | b'.' | b':' | b'/' | b'@')
            }) {
                bail!("郵件 flag 無效");
            }
            trimmed
        }
    };
    if normalized.starts_with('\\')
        && !matches!(
            normalized,
            "\\Seen" | "\\Answered" | "\\Flagged" | "\\Deleted" | "\\Draft"
        )
    {
        bail!("不可修改此郵件 flag");
    }
    Ok(normalized.to_owned())
}

async fn select_and_check(session: &mut ImapSession, item: &Item) -> Result<u32> {
    let uid = parse_uid(item)?;
    let expected = item
        .uid_validity
        .ok_or_else(|| anyhow!("郵件缺少 UIDVALIDITY，拒絕變更"))?;
    let mailbox = timeout(IMAP_TIMEOUT, session.select(&item.collection))
        .await
        .map_err(|_| anyhow!("選取郵件資料夾逾時"))?
        .with_context(|| format!("選取郵件資料夾失敗: {}", item.collection))?;
    let actual = mailbox
        .uid_validity
        .ok_or_else(|| anyhow!("郵件資料夾缺少 UIDVALIDITY"))?;
    if actual != expected {
        bail!("郵件已過期：UIDVALIDITY 從 {expected} 變成 {actual}");
    }
    let found = timeout(IMAP_TIMEOUT, session.uid_search(format!("UID {uid}")))
        .await
        .map_err(|_| anyhow!("確認郵件 UID 逾時"))?
        .context("確認郵件 UID 失敗")?;
    if !found.contains(&uid) {
        bail!("找不到郵件 UID {uid}");
    }
    Ok(uid)
}

async fn drain_fetches<S>(mut stream: S, operation: &str) -> Result<()>
where
    S: futures_util::Stream<
            Item = std::result::Result<async_imap::types::Fetch, async_imap::error::Error>,
        > + Unpin,
{
    while timeout(IMAP_TIMEOUT, stream.try_next())
        .await
        .map_err(|_| anyhow!("{operation} 回應逾時"))?
        .with_context(|| format!("讀取 {operation} 回應失敗"))?
        .is_some()
    {}
    Ok(())
}

pub fn flag(config: &Config, password: &str, item: &Item, flag: &str, enabled: bool) -> Result<()> {
    let flag = normalize_flag(flag)?;
    run_async(async {
        let mut session = connect_imap(config, password).await?;
        let result = async {
            let uid = select_and_check(&mut session, item).await?;
            let query = if enabled {
                format!("+FLAGS.SILENT ({flag})")
            } else {
                format!("-FLAGS.SILENT ({flag})")
            };
            let updates = timeout(IMAP_TIMEOUT, session.uid_store(uid.to_string(), query))
                .await
                .map_err(|_| anyhow!("修改郵件 flag 逾時"))?
                .context("修改郵件 flag 失敗")?;
            drain_fetches(updates, "修改郵件 flag").await
        }
        .await;
        logout(session).await;
        result
    })
}

async fn server_has(session: &mut ImapSession, capability: &str) -> Result<bool> {
    let capabilities = timeout(IMAP_TIMEOUT, session.capabilities())
        .await
        .map_err(|_| anyhow!("讀取 IMAP capability 逾時"))?
        .context("讀取 IMAP capability 失敗")?;
    Ok(capabilities.has_str(capability))
}

async fn move_with_session(session: &mut ImapSession, uid: u32, target: &str) -> Result<()> {
    let has_move = server_has(session, "MOVE").await?;
    if !has_move && !server_has(session, "UIDPLUS").await? {
        bail!("伺服器不支援 UID MOVE 與 UIDPLUS，拒絕不安全的全量 EXPUNGE fallback");
    }
    move_supported_set(session, &uid.to_string(), target, has_move).await
}

async fn move_supported_set(
    session: &mut ImapSession,
    uid: &str,
    target: &str,
    has_move: bool,
) -> Result<()> {
    if has_move {
        timeout(IMAP_TIMEOUT, session.uid_mv(uid.to_string(), target))
            .await
            .map_err(|_| anyhow!("UID MOVE 逾時"))?
            .context("UID MOVE 失敗")?;
        return Ok(());
    }
    timeout(IMAP_TIMEOUT, session.uid_copy(uid.to_string(), target))
        .await
        .map_err(|_| anyhow!("UID COPY 逾時"))?
        .context("UID COPY 失敗")?;
    let updates = timeout(
        IMAP_TIMEOUT,
        session.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)"),
    )
    .await
    .map_err(|_| anyhow!("標記來源郵件逾時"))?
    .context("標記來源郵件失敗")?;
    drain_fetches(updates, "標記來源郵件").await?;
    let removed = timeout(IMAP_TIMEOUT, session.uid_expunge(uid.to_string()))
        .await
        .map_err(|_| anyhow!("UID EXPUNGE 逾時"))?
        .context("UID EXPUNGE 失敗")?;
    pin_mut!(removed);
    while timeout(IMAP_TIMEOUT, removed.try_next())
        .await
        .map_err(|_| anyhow!("UID EXPUNGE 回應逾時"))?
        .context("讀取 UID EXPUNGE 回應失敗")?
        .is_some()
    {}
    Ok(())
}

pub fn move_to(config: &Config, password: &str, item: &Item, target: &str) -> Result<()> {
    move_many(config, password, std::slice::from_ref(item), target, false)?;
    Ok(())
}

async fn find_trash(session: &mut ImapSession, source: &str) -> Result<Option<String>> {
    let folders = list_folders(session).await?;
    let preferred = [
        "Trash",
        "INBOX.Trash",
        "Deleted Items",
        "Deleted",
        "垃圾桶",
        "回收筒",
    ];
    for candidate in preferred {
        if folders.iter().any(|folder| {
            folder.eq_ignore_ascii_case(candidate) && !folder.eq_ignore_ascii_case(source)
        }) {
            return Ok(folders.into_iter().find(|folder| {
                folder.eq_ignore_ascii_case(candidate) && !folder.eq_ignore_ascii_case(source)
            }));
        }
    }
    Ok(None)
}

pub fn delete(config: &Config, password: &str, item: &Item) -> Result<()> {
    run_async(async {
        let mut session = connect_imap(config, password).await?;
        let result = async {
            let uid = select_and_check(&mut session, item).await?;
            if let Some(trash) = find_trash(&mut session, &item.collection).await? {
                move_with_session(&mut session, uid, &trash).await
            } else {
                if !server_has(&mut session, "UIDPLUS").await? {
                    bail!(
                        "找不到 Trash 且伺服器不支援 UIDPLUS，拒絕可能刪除其他郵件的全量 EXPUNGE"
                    );
                }
                let updates = timeout(
                    IMAP_TIMEOUT,
                    session.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)"),
                )
                .await
                .map_err(|_| anyhow!("標記郵件刪除逾時"))?
                .context("標記郵件刪除失敗")?;
                drain_fetches(updates, "標記郵件刪除").await?;
                let removed = timeout(IMAP_TIMEOUT, session.uid_expunge(uid.to_string()))
                    .await
                    .map_err(|_| anyhow!("UID EXPUNGE 逾時"))?
                    .context("UID EXPUNGE 失敗")?;
                pin_mut!(removed);
                while timeout(IMAP_TIMEOUT, removed.try_next())
                    .await
                    .map_err(|_| anyhow!("UID EXPUNGE 回應逾時"))?
                    .context("讀取 UID EXPUNGE 回應失敗")?
                    .is_some()
                {}
                Ok(())
            }
        }
        .await;
        logout(session).await;
        result
    })
}

// Select before lettre starts TLS/EHLO/AUTH or sends any message. Never retry
// transport.send(): a network timeout there may occur after DATA was accepted.
fn select_smtp_port(port: u16, probe: impl FnOnce() -> std::io::Result<()>) -> Result<u16> {
    if port != 587 {
        return Ok(port);
    }
    match probe() {
        Ok(()) => Ok(port),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(465)
        }
        Err(error) => Err(error).context("SMTP TCP 連線失敗"),
    }
}

fn smtp_tcp_probe(host: &str, port: u16) -> std::io::Result<()> {
    use std::net::{TcpStream, ToSocketAddrs};
    // DNS errors are deliberately outside the fallback decision.
    let addresses = (host, port)
        .to_socket_addrs()
        .map_err(std::io::Error::other)?
        .collect::<Vec<_>>();
    let mut last_error =
        std::io::Error::new(std::io::ErrorKind::NotFound, "SMTP host 沒有 IP 位址");
    for address in addresses {
        match TcpStream::connect_timeout(&address, SMTP_TIMEOUT) {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn smtp_transport(config: &Config, password: &str) -> Result<(SmtpTransport, Option<u16>)> {
    let host = require(&config.smtp_host, "SMTP host")?;
    let username = require(&config.username, "帳號")?;
    let password = require_password(password)?.to_owned();
    let configured_port = smtp_port(config)?;
    let port = select_smtp_port(configured_port, || smtp_tcp_probe(&host, configured_port))?;
    let fallback = (port != configured_port).then_some(port);
    let builder = match test_root_certificate(&host)? {
        Some(pem) => {
            let parameters = TlsParameters::builder(host.clone())
                .add_root_certificate(
                    lettre::transport::smtp::client::Certificate::from_pem(&pem)
                        .context("測試根憑證格式錯誤")?,
                )
                .build()
                .context("建立 SMTP TLS 參數失敗")?;
            let tls = if port == 587 {
                Tls::Required(parameters)
            } else {
                Tls::Wrapper(parameters)
            };
            SmtpTransport::builder_dangerous(&host).tls(tls)
        }
        None if port == 587 => {
            SmtpTransport::starttls_relay(&host).context("建立 SMTP TLS transport 失敗")?
        }
        None => SmtpTransport::relay(&host).context("建立 SMTP TLS transport 失敗")?,
    };
    Ok((
        builder
            .port(port)
            .credentials(Credentials::new(username, password))
            .timeout(Some(SMTP_TIMEOUT))
            .build(),
        fallback,
    ))
}

pub fn verify_smtp(config: &Config, password: &str) -> Result<Option<u16>> {
    let (transport, fallback) = smtp_transport(config, password)?;
    if !transport.test_connection().context("SMTP 驗證失敗")? {
        bail!("SMTP 驗證失敗：伺服器拒絕連線");
    }
    Ok(fallback)
}

fn mailbox(value: &str, label: &str) -> Result<Mailbox> {
    let value = require(value, label)?;
    value.parse().with_context(|| format!("{label} 位址無效"))
}

fn attachment_content_type(path: &Path) -> header::ContentType {
    let value = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("txt") => "text/plain",
        Some("html") | Some("htm") => "text/html",
        Some("pdf") => "application/pdf",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("ics") => "text/calendar",
        Some("vcf") => "text/vcard",
        _ => "application/octet-stream",
    };
    header::ContentType::parse(value).expect("固定 MIME type 必定有效")
}

fn ical_property<'a>(properties: &'a [ical::property::Property], name: &str) -> Option<&'a str> {
    properties
        .iter()
        .find(|property| property.name.eq_ignore_ascii_case(name))
        .and_then(|property| property.value.as_deref())
}

fn unescape_ical_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            output.push(match character {
                'n' | 'N' => '\n',
                '\\' => '\\',
                ',' => ',',
                ';' => ';',
                other => other,
            });
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            output.push(character);
        }
    }
    if escaped {
        output.push('\\');
    }
    output
}

fn invite_details(ics: &str) -> Result<(String, Vec<String>)> {
    if ics.trim().is_empty() {
        bail!("ICS 不可為空白");
    }
    let mut parser = IcalParser::new(BufReader::new(ics.as_bytes()));
    let calendar = parser
        .next()
        .ok_or_else(|| anyhow!("ICS 缺少 VCALENDAR"))?
        .context("ICS 格式錯誤")?;
    let method =
        ical_property(&calendar.properties, "METHOD").ok_or_else(|| anyhow!("ICS 缺少 METHOD"))?;
    if !method.trim().eq_ignore_ascii_case("REQUEST") {
        bail!("ICS METHOD 必須是 REQUEST");
    }
    let event = calendar
        .events
        .first()
        .ok_or_else(|| anyhow!("ICS 缺少 VEVENT"))?;
    let title = ical_property(&event.properties, "SUMMARY")
        .map(unescape_ical_text)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("ICS 缺少事件主旨"))?;
    let mut recipients = Vec::new();
    for property in event
        .properties
        .iter()
        .filter(|property| property.name.eq_ignore_ascii_case("ATTENDEE"))
    {
        let value = property
            .value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("ICS ATTENDEE 缺少位址"))?;
        let email = value
            .get(..7)
            .filter(|prefix| prefix.eq_ignore_ascii_case("mailto:"))
            .map(|_| value[7..].trim())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("ICS ATTENDEE 必須使用 MAILTO"))?;
        if email.contains(['\r', '\n']) {
            bail!("ICS ATTENDEE 位址無效");
        }
        mailbox(email, "會議收件者")?;
        if !recipients
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(email))
        {
            recipients.push(email.to_owned());
        }
    }
    if recipients.is_empty() {
        bail!("ICS 缺少 ATTENDEE 收件者");
    }
    Ok((title, recipients))
}

pub fn send(config: &Config, password: &str, mail: &OutgoingMail) -> Result<String> {
    let from = mailbox(&config.email, "寄件者")?;
    let mut builder = Message::builder().from(from).subject(mail.subject.clone());
    let mut recipient_count = 0usize;
    for address in &mail.to {
        builder = builder.to(mailbox(address, "收件者")?);
        recipient_count += 1;
    }
    for address in &mail.cc {
        builder = builder.cc(mailbox(address, "副本收件者")?);
        recipient_count += 1;
    }
    if recipient_count == 0 {
        bail!("至少需要一位收件者");
    }
    let message_id = format!("<{}@{}>", Uuid::new_v4(), config.smtp_host.trim());
    builder = builder.message_id(Some(message_id.clone()));
    if let Some(reply) = &mail.in_reply_to {
        builder = builder.in_reply_to(reply.clone());
    }
    let message = if mail.attachments.is_empty() {
        builder.singlepart(SinglePart::plain(mail.body.clone()))?
    } else {
        let mut multipart = MultiPart::mixed().singlepart(SinglePart::plain(mail.body.clone()));
        for path in &mail.attachments {
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("附件檔名無效"))?;
            let body =
                std::fs::read(path).with_context(|| format!("讀取附件失敗: {}", path.display()))?;
            multipart = multipart.singlepart(
                LettreAttachment::new(filename.to_owned())
                    .body(body, attachment_content_type(path)),
            );
        }
        builder.multipart(multipart)?
    };
    let (transport, _) = smtp_transport(config, password)?;
    transport.send(&message).context("SMTP 寄信失敗")?;
    Ok(message_id)
}

pub fn forward(
    config: &Config,
    password: &str,
    item: &Item,
    to: &[String],
    introduction: &str,
) -> Result<String> {
    if item.kind != Kind::Mail {
        bail!("此項目不是郵件");
    }
    let raw = raw_bytes(item)?;
    if raw.is_empty() {
        bail!("原郵件內容為空");
    }
    let from = mailbox(&config.email, "寄件者")?;
    let mut builder = Message::builder().from(from);
    for address in to {
        builder = builder.to(mailbox(address, "收件者")?);
    }
    if to.is_empty() {
        bail!("至少需要一位收件者");
    }
    let subject = if item.title.to_ascii_lowercase().starts_with("fwd:") {
        item.title.clone()
    } else {
        format!("Fwd: {}", item.title)
    };
    let mut body = String::new();
    if !introduction.trim().is_empty() {
        body.push_str(introduction.trim_end());
        body.push_str("\n\n");
    }
    body.push_str("---------- 轉寄郵件 ----------\n");
    body.push_str("主旨: ");
    body.push_str(&item.title);
    body.push('\n');
    if !item.subtitle.trim().is_empty() {
        body.push_str("寄件者: ");
        body.push_str(&item.subtitle);
        body.push('\n');
    }
    body.push('\n');
    body.push_str(&item.text);
    let message_id = format!("<{}@{}>", Uuid::new_v4(), config.smtp_host.trim());
    let message = builder
        .subject(subject)
        .message_id(Some(message_id.clone()))
        .multipart(
            MultiPart::mixed()
                .singlepart(SinglePart::plain(body))
                .singlepart(
                    LettreAttachment::new("forwarded.eml".to_owned())
                        .body(raw, header::ContentType::parse("message/rfc822")?),
                ),
        )?;
    let (transport, _) = smtp_transport(config, password)?;
    transport.send(&message).context("SMTP 轉寄失敗")?;
    Ok(message_id)
}

pub fn send_invite(config: &Config, password: &str, ics: &str) -> Result<String> {
    let (subject, recipients) = invite_details(ics)?;
    let from = mailbox(&config.email, "寄件者")?;
    let mut builder = Message::builder().from(from).subject(subject.clone());
    for recipient in recipients {
        builder = builder.to(mailbox(&recipient, "會議收件者")?);
    }
    let message_id = format!("<{}@{}>", Uuid::new_v4(), config.smtp_host.trim());
    let calendar_type = header::ContentType::parse("text/calendar; method=REQUEST; charset=utf-8")?;
    let message = builder.message_id(Some(message_id.clone())).multipart(
        MultiPart::alternative()
            .singlepart(SinglePart::plain(format!("會議邀請：{subject}\n\n{ics}")))
            .singlepart(
                SinglePart::builder()
                    .header(calendar_type)
                    .body(ics.to_owned()),
            ),
    )?;
    let (transport, _) = smtp_transport(config, password)?;
    transport.send(&message).context("SMTP 會議邀請寄送失敗")?;
    Ok(message_id)
}

/// Sends an iTIP METHOD:REPLY to the organizer. Like every other SMTP path it
/// is attempted once: a timeout may happen after DATA was accepted.
pub fn send_calendar_reply(
    config: &Config,
    password: &str,
    reply: &CalendarReply<'_>,
) -> Result<String> {
    let from = mailbox(&config.email, "寄件者")?;
    let mut builder = Message::builder()
        .from(from)
        .to(mailbox(reply.to, "主辦人")?)
        .subject(reply.subject.to_owned());
    let message_id = format!("<{}@{}>", Uuid::new_v4(), config.smtp_host.trim());
    builder = builder.message_id(Some(message_id.clone()));
    if let Some(parent) = reply.in_reply_to {
        builder = builder.in_reply_to(parent.to_owned());
    }
    let calendar_type = header::ContentType::parse("text/calendar; method=REPLY; charset=utf-8")?;
    let message = builder.multipart(
        MultiPart::alternative()
            .singlepart(SinglePart::plain(reply.body.to_owned()))
            .singlepart(
                SinglePart::builder()
                    .header(calendar_type)
                    .body(reply.ics.to_owned()),
            ),
    )?;
    let (transport, _) = smtp_transport(config, password)?;
    transport.send(&message).context("SMTP 會議回覆寄送失敗")?;
    Ok(message_id)
}

pub struct CalendarReply<'a> {
    pub to: &'a str,
    pub subject: &'a str,
    pub body: &'a str,
    pub ics: &'a str,
    pub in_reply_to: Option<&'a str>,
}

fn attachment_parts<'a>(
    mail: &'a ParsedMail<'a>,
    result: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    if is_attachment(mail) {
        let filename =
            part_filename(mail).unwrap_or_else(|| format!("attachment-{}", result.len() + 1));
        result.push((filename, attachment_body(mail)?));
        return Ok(());
    }
    if !mail.subparts.is_empty() {
        for part in &mail.subparts {
            attachment_parts(part, result)?;
        }
        return Ok(());
    }
    Ok(())
}

pub fn attachment(item: &Item, index: usize) -> Result<(String, Vec<u8>)> {
    if item.kind != Kind::Mail {
        bail!("此項目不是郵件");
    }
    let raw = raw_bytes(item)?;
    let parsed = parse_mail(&raw).context("解析郵件 MIME 失敗")?;
    let mut parts = Vec::new();
    attachment_parts(&parsed, &mut parts)?;
    parts
        .into_iter()
        .nth(index)
        .ok_or_else(|| anyhow!("找不到附件索引 {index}"))
}

pub fn raw_bytes(item: &Item) -> Result<Vec<u8>> {
    if let Some(encoded) = item.raw.strip_prefix(RAW_BASE64_PREFIX) {
        return BASE64_STANDARD
            .decode(encoded)
            .context("郵件 raw base64 解碼失敗");
    }
    Ok(item.raw.as_bytes().to_vec())
}

pub fn display_header(value: &str) -> String {
    if !value.contains("=?") {
        return value.to_owned();
    }
    let bytes = value.as_bytes();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    let mut last_was_encoded = false;
    while index < bytes.len() {
        let Some(rel) = find_bytes(&bytes[index..], b"=?") else {
            output.push_str(&value[index..]);
            break;
        };
        let start = index + rel;
        let before = &value[index..start];
        if !before.is_empty() && !(last_was_encoded && is_linear_whitespace(before)) {
            output.push_str(before);
        }
        match decode_encoded_word(&value[start..]) {
            Some((decoded, consumed)) => {
                output.push_str(&decoded);
                index = start + consumed;
                last_was_encoded = true;
            }
            None => {
                output.push_str("=?");
                index = start + 2;
                last_was_encoded = false;
            }
        }
    }
    output
}

fn is_linear_whitespace(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_encoded_word(value: &str) -> Option<(String, usize)> {
    let bytes = value.as_bytes();
    if bytes.len() < 6 || &bytes[..2] != b"=?" {
        return None;
    }
    let charset_end = find_bytes(&bytes[2..], b"?")? + 2;
    let encoding_end = find_bytes(&bytes[charset_end + 1..], b"?")? + charset_end + 1;
    let text_start = encoding_end + 1;
    let text_end = find_bytes(&bytes[text_start..], b"?=")? + text_start;
    let charset = &value[2..charset_end];
    let encoding = &value[charset_end + 1..encoding_end];
    let text = &value[text_start..text_end];
    if encoding.len() != 1 {
        return None;
    }
    let raw = match encoding.as_bytes()[0] {
        b'B' | b'b' => decode_b_encoded(text)?,
        b'Q' | b'q' => decode_q_encoded(text)?,
        _ => return None,
    };
    let decoded = decode_charset_bytes(charset, &raw);
    Some((decoded, text_end + 2))
}

fn decode_b_encoded(text: &str) -> Option<Vec<u8>> {
    let mut padded = text.as_bytes().to_vec();
    while !padded.len().is_multiple_of(4) {
        padded.push(b'=');
    }
    BASE64_STANDARD.decode(padded).ok()
}

fn decode_q_encoded(text: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'_' => {
                output.push(b' ');
                index += 1;
            }
            b'=' if index + 2 < bytes.len() => {
                let hi = from_hex(bytes[index + 1])?;
                let lo = from_hex(bytes[index + 2])?;
                output.push((hi << 4) | lo);
                index += 3;
            }
            byte if byte.is_ascii() && !byte.is_ascii_control() => {
                output.push(byte);
                index += 1;
            }
            _ => return None,
        }
    }
    Some(output)
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn decode_charset_bytes(charset: &str, raw: &[u8]) -> String {
    if charset.eq_ignore_ascii_case("utf-8")
        || charset.eq_ignore_ascii_case("utf8")
        || charset.eq_ignore_ascii_case("us-ascii")
        || charset.eq_ignore_ascii_case("ascii")
    {
        return String::from_utf8_lossy(raw).into_owned();
    }
    if charset.eq_ignore_ascii_case("iso-8859-1")
        || charset.eq_ignore_ascii_case("latin1")
        || charset.eq_ignore_ascii_case("latin-1")
    {
        return raw.iter().map(|&byte| byte as char).collect();
    }
    String::from_utf8_lossy(raw).into_owned()
}

pub fn display_collection(name: &str) -> String {
    let mut output = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        let Some(end) = rest.find('-') else {
            return name.to_owned();
        };
        let encoded = &rest[..end];
        if encoded.is_empty() {
            output.push('&');
        } else {
            if encoded.len() % 4 == 1
                || !encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b','))
            {
                return name.to_owned();
            }
            let mut standard = encoded.replace(',', "/");
            standard.extend(std::iter::repeat_n('=', (4 - encoded.len() % 4) % 4));
            let Ok(bytes) = BASE64_STANDARD.decode(standard) else {
                return name.to_owned();
            };
            if bytes.len() % 2 != 0 {
                return name.to_owned();
            }
            let units = bytes
                .chunks(2)
                .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            let Ok(decoded) = String::from_utf16(&units) else {
                return name.to_owned();
            };
            if decoded
                .chars()
                .any(|character| character.is_control() || (' '..='~').contains(&character))
            {
                return name.to_owned();
            }
            output.push_str(&decoded);
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

pub fn normalize_cached(item: &mut Item) -> Result<bool> {
    if item.kind != Kind::Mail {
        return Ok(false);
    }
    let mut title = display_header(&item.title);
    let mut subtitle = display_header(&item.subtitle);
    let mut text = None;
    if item.text.trim().is_empty() && !item.raw.is_empty() {
        let raw = raw_bytes(item)?;
        let mail = parse_mail(&raw).context("解析快取郵件 MIME 失敗")?;
        if !mail.headers.is_empty() {
            let subject = first_header(&mail, "Subject");
            if !subject.is_empty() {
                title = subject;
            }
            let from = first_header(&mail, "From");
            if !from.is_empty() {
                subtitle = from;
            }
            let body = body_text(&mail)?;
            if !body.trim().is_empty() {
                text = Some(body);
            }
        }
    }
    let changed = title != item.title || subtitle != item.subtitle || text.is_some();
    item.title = title;
    item.subtitle = subtitle;
    if let Some(text) = text {
        item.text = text;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, Read, Write},
        net::{SocketAddr, TcpListener},
        thread::{self, JoinHandle},
    };

    fn mock_imap(steps: Vec<(String, String)>) -> (SocketAddr, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock IMAP");
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept IMAP");
            stream
                .set_read_timeout(Some(StdDuration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            stream.write_all(b"* OK mock IMAP ready\r\n").unwrap();
            let mut requests = Vec::new();
            let mut current_tag = String::new();
            let login = ("LOGIN \"test\" \"password\"".to_owned(), String::new());
            for (expected, response) in std::iter::once(login).chain(steps) {
                if let Some(literal) = expected.strip_prefix("LITERAL:") {
                    let mut bytes = vec![0; literal.len() + 2];
                    reader.read_exact(&mut bytes).expect("read IMAP literal");
                    assert_eq!(&bytes[..literal.len()], literal.as_bytes());
                    assert_eq!(&bytes[literal.len()..], b"\r\n");
                    requests.push(literal.to_owned());
                } else {
                    let mut request = String::new();
                    reader.read_line(&mut request).expect("read IMAP command");
                    let command = if request.trim_end() == "DONE" {
                        "DONE"
                    } else {
                        let (tag, command) = request.trim_end().split_once(' ').unwrap();
                        current_tag = tag.to_owned();
                        command
                    };
                    assert_eq!(command, expected);
                    requests.push(command.to_owned());
                }
                let response = if response.starts_with('+') {
                    response
                } else if response.contains("{tag}") {
                    response.replace("{tag}", &current_tag)
                } else {
                    format!("{response}{current_tag} OK complete\r\n")
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
            requests
        });
        (address, handle)
    }

    async fn mock_login(address: SocketAddr) -> Session<TcpStream> {
        let stream = TcpStream::connect(address).await.unwrap();
        let mut client = Client::new(stream);
        client.read_response().await.unwrap().unwrap();
        client.login("test", "password").await.unwrap()
    }

    fn examine_step(folder: &str, count: u32, validity: u32) -> (String, String) {
        (
            format!("EXAMINE \"{folder}\""),
            format!(
                "* {count} EXISTS\r\n* OK [UIDVALIDITY {validity}] valid\r\n{tag} OK [READ-ONLY] selected\r\n",
                tag = "{tag}"
            ),
        )
    }

    fn literal_fetch(uid: u32, section: &str, body: &str, flags: &str) -> String {
        format!(
            "* 1 FETCH (UID {uid} FLAGS ({flags}) INTERNALDATE \"07-Oct-2026 09:00:00 +0000\" BODY[{section}] {{{length}}}\r\n{body})\r\n",
            length = body.len()
        )
    }

    fn watch_examine(next: u32, validity: u32) -> (String, String) {
        ("EXAMINE \"INBOX\"".into(), format!("* 3 EXISTS\r\n* OK [UIDVALIDITY {validity}] valid\r\n* OK [UIDNEXT {next}] next\r\n{{tag}} OK [READ-ONLY] selected\r\n"))
    }

    #[test]
    fn mail_watch_baselines_existing_uids_and_resets_changed_validity_without_fetch() {
        use watch::Cursor;
        let (address, server) = mock_imap(vec![watch_examine(11, 7), watch_examine(3, 42)]);
        run_async(async {
            let mut session = mock_login(address).await;
            let initial = watch::poll_with_session(&mut session, "INBOX", None).await?;
            assert!(initial.baseline && initial.items.is_empty());
            assert_eq!(
                initial.cursor,
                Cursor {
                    uid_validity: 7,
                    last_uid: 10
                }
            );
            let reset =
                watch::poll_with_session(&mut session, "INBOX", Some(initial.cursor)).await?;
            assert!(reset.baseline && reset.items.is_empty());
            assert_eq!(
                reset.cursor,
                Cursor {
                    uid_validity: 42,
                    last_uid: 2
                }
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn mail_watch_fetches_only_new_uid_bodies_and_filters_reversed_star_range() {
        use watch::Cursor;
        let raw = "Subject: new mail\r\nFrom: sender@example.test\r\n\r\nnew body";
        let (address, server) = mock_imap(vec![
            watch_examine(9, 7),
            ("UID SEARCH UID 6:*".into(), "* SEARCH 5 6 8\r\n".into()),
            (
                "UID FETCH 6,8 (UID FLAGS INTERNALDATE BODY.PEEK[])".into(),
                literal_fetch(6, "", raw, "") + &literal_fetch(8, "", raw, "\\Seen"),
            ),
            watch_examine(9, 7),
            ("UID SEARCH UID 9:*".into(), "* SEARCH 8\r\n".into()),
        ]);
        run_async(async {
            let mut session = mock_login(address).await;
            let delta = watch::poll_with_session(
                &mut session,
                "INBOX",
                Some(Cursor {
                    uid_validity: 7,
                    last_uid: 5,
                }),
            )
            .await?;
            assert_eq!(delta.items.len(), 2);
            assert_eq!(delta.cursor.last_uid, 8);
            assert!(delta.items[0].text.contains("new body"));
            assert_eq!(delta.items[0].id, "mail:INBOX:7:6");
            assert!(!delta.baseline && !delta.has_more);
            let unchanged =
                watch::poll_with_session(&mut session, "INBOX", Some(delta.cursor)).await?;
            assert!(unchanged.items.is_empty());
            assert_eq!(unchanged.cursor, delta.cursor);
            Ok(())
        })
        .unwrap();
        assert_eq!(server.join().unwrap().len(), 6);
    }

    #[test]
    fn mail_watch_batches_backlog_and_does_not_advance_past_fetched_mail() {
        use watch::Cursor;
        let uids = (1..=101).map(|uid| uid.to_string()).collect::<Vec<_>>();
        let body = "Subject: incremental\r\n\r\nbody";
        let response = (1..=100)
            .map(|uid| literal_fetch(uid, "", body, ""))
            .collect::<String>();
        let (address, server) = mock_imap(vec![
            watch_examine(102, 7),
            (
                "UID SEARCH UID 1:*".into(),
                format!("* SEARCH {}\r\n", uids.join(" ")),
            ),
            (
                format!(
                    "UID FETCH {} (UID FLAGS INTERNALDATE BODY.PEEK[])",
                    uids[..100].join(",")
                ),
                response,
            ),
        ]);
        run_async(async {
            let mut session = mock_login(address).await;
            let delta = watch::poll_with_session(
                &mut session,
                "INBOX",
                Some(Cursor {
                    uid_validity: 7,
                    last_uid: 0,
                }),
            )
            .await?;
            assert_eq!(delta.items.len(), 100);
            assert!(delta.has_more);
            assert_eq!(delta.cursor.last_uid, 100);
            Ok(())
        })
        .unwrap();
        assert_eq!(server.join().unwrap().len(), 4);
    }

    #[test]
    fn mail_watch_preserves_malformed_message_and_continues_to_newer_mail() {
        let malformed = "Subject: malformed\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\n%%%";
        let valid = "Subject: valid\r\n\r\nnew valid body";
        let (address, server) = mock_imap(vec![
            watch_examine(8, 7),
            ("UID SEARCH UID 6:*".into(), "* SEARCH 6 7\r\n".into()),
            (
                "UID FETCH 6,7 (UID FLAGS INTERNALDATE BODY.PEEK[])".into(),
                literal_fetch(6, "", malformed, "") + &literal_fetch(7, "", valid, ""),
            ),
        ]);
        let result = run_async(async {
            let mut session = mock_login(address).await;
            watch::poll_with_session(
                &mut session,
                "INBOX",
                Some(watch::Cursor {
                    uid_validity: 7,
                    last_uid: 5,
                }),
            )
            .await
        });
        let delta = result.unwrap_or_else(|error| panic!("{error:#}"));
        assert_eq!(delta.cursor.last_uid, 7);
        assert_eq!(delta.items.len(), 2);
        assert_eq!(delta.items[1].text, "new valid body");
        assert!(delta.items[0].raw.starts_with("base64:"));
        assert_eq!(delta.items[0].title, "malformed");
        assert!(delta.parse_errors[&6].contains("Base64 decode error"));
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::new("test".into(), "mail.example.test").unwrap();
        let sub = crate::mail_webhook::Subscription::new(&cfg, "INBOX", "http://127.0.0.1:1/hook")
            .unwrap();
        let old = watch::Cursor {
            uid_validity: 7,
            last_uid: 5,
        };
        let mut store = crate::store::Store::open(dir.path()).unwrap();
        store.ensure_account(&cfg).unwrap();
        store
            .record_mail_delta(
                &sub,
                None,
                &watch::Delta {
                    cursor: old,
                    items: vec![],
                    baseline: true,
                    has_more: false,
                    parse_errors: Default::default(),
                },
                Utc::now(),
            )
            .unwrap();
        store
            .record_mail_delta(&sub, Some(old), &delta, Utc::now())
            .unwrap();
        drop(store);
        let mut store = crate::store::Store::open(dir.path()).unwrap();
        assert_eq!(store.mail_webhook_cursor(&sub).unwrap(), Some(delta.cursor));
        let mut events = Vec::new();
        for _ in 0..2 {
            let (id, payload, _) = store
                .mail_webhook_pending(&sub, Utc::now())
                .unwrap()
                .unwrap();
            events.push(serde_json::from_str::<serde_json::Value>(&payload).unwrap());
            store.mail_webhook_delivered(&sub, &id).unwrap();
        }
        events.sort_by_key(|event| event["uid"].as_u64().unwrap());
        assert_eq!(events[0]["parse_error"]["code"], "mail_parse_failed");
        assert_eq!(events[0]["mail"]["text"], "");
        assert!(events[1]["parse_error"].is_null());
        assert_eq!(events[1]["mail"]["text"], "new valid body");
        assert_eq!(
            BASE64_STANDARD
                .decode(
                    store
                        .get("mail:INBOX:7:6")
                        .unwrap()
                        .raw
                        .strip_prefix(RAW_BASE64_PREFIX)
                        .unwrap()
                )
                .unwrap(),
            malformed.as_bytes()
        );
        server.join().unwrap();
    }

    #[test]
    fn mail_watch_missing_body_fails_without_producing_a_cursor() {
        use watch::Cursor;
        let (address, server) = mock_imap(vec![
            watch_examine(7, 7),
            ("UID SEARCH UID 6:*".into(), "* SEARCH 6\r\n".into()),
            (
                "UID FETCH 6 (UID FLAGS INTERNALDATE BODY.PEEK[])".into(),
                String::new(),
            ),
        ]);
        let error = run_async(async {
            let mut session = mock_login(address).await;
            watch::poll_with_session(
                &mut session,
                "INBOX",
                Some(Cursor {
                    uid_validity: 7,
                    last_uid: 5,
                }),
            )
            .await
            .map(|_| ())
        })
        .unwrap_err();
        assert!(error.to_string().contains("未取得預期 UID"));
        server.join().unwrap();
    }

    #[test]
    fn mail_watch_idle_returns_on_data_timeout_and_cancellation_with_done() {
        use std::sync::atomic::AtomicBool;
        for (response, cancelled) in [
            ("+ idling\r\n* 2 EXISTS\r\n", false),
            ("+ idling\r\n", false),
            ("+ idling\r\n", true),
        ] {
            let (address, server) = mock_imap(vec![
                ("IDLE".into(), response.into()),
                ("DONE".into(), "{tag} OK idle complete\r\n".into()),
            ]);
            run_async(async {
                let session = mock_login(address).await;
                let _ = watch::wait_idle(
                    session,
                    StdDuration::from_millis(10),
                    &AtomicBool::new(cancelled),
                )
                .await?;
                Ok(())
            })
            .unwrap();
            let requests = server.join().unwrap();
            assert_eq!(requests[1], "IDLE");
            assert_eq!(requests[2], "DONE");
        }
    }

    #[test]
    fn mail_watch_cancels_stalled_network_work_without_waiting_for_timeout() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(StdDuration::from_millis(10));
            signal.store(true, Ordering::Relaxed);
        });
        let error = run_async(watch::cancellable(
            std::future::pending::<Result<()>>(),
            &stop,
        ))
        .unwrap_err();
        sender.join().unwrap();
        assert!(error.to_string().contains("已停止"));
    }

    #[test]
    fn mail_watch_new_message_reaches_webhook_and_restart_does_not_repeat_it() {
        use crate::{
            config::Config,
            mail_webhook::{Delivery, Subscription},
            store::Store,
        };
        use std::sync::atomic::AtomicBool;
        let http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/hook", http.local_addr().unwrap());
        let receiver = std::thread::spawn(move || {
            let (mut stream, _) = http.accept().unwrap();
            stream
                .set_read_timeout(Some(StdDuration::from_secs(5)))
                .unwrap();
            let mut headers = Vec::new();
            let mut byte = [0];
            while !headers.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        });
        let raw = "Subject: webhook flow\r\nFrom: sender@example.test\r\n\r\nnew incremental body";
        let (address, imap) = mock_imap(vec![
            watch_examine(11, 7),
            ("IDLE".into(), "+ idling\r\n* 4 EXISTS\r\n".into()),
            ("DONE".into(), "{tag} OK done\r\n".into()),
            watch_examine(12, 7),
            ("UID SEARCH UID 11:*".into(), "* SEARCH 11\r\n".into()),
            (
                "UID FETCH 11 (UID FLAGS INTERNALDATE BODY.PEEK[])".into(),
                literal_fetch(11, "", raw, ""),
            ),
            watch_examine(12, 7),
            ("UID SEARCH UID 12:*".into(), "* SEARCH 11\r\n".into()),
        ]);
        let runtime = Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        let mut session = runtime.block_on(mock_login(address));
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::new("test".into(), "mail.example.test").unwrap();
        let sub = Subscription::new(&cfg, "INBOX", &endpoint).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        store.ensure_account(&cfg).unwrap();
        let baseline = runtime
            .block_on(watch::poll_with_session(&mut session, "INBOX", None))
            .unwrap();
        store
            .record_mail_delta(&sub, None, &baseline, Utc::now())
            .unwrap();
        session = runtime
            .block_on(watch::wait_idle(
                session,
                StdDuration::from_secs(1),
                &AtomicBool::new(false),
            ))
            .unwrap();
        let delta = runtime
            .block_on(watch::poll_with_session(
                &mut session,
                "INBOX",
                Some(baseline.cursor),
            ))
            .unwrap();
        store
            .record_mail_delta(&sub, Some(baseline.cursor), &delta, Utc::now())
            .unwrap();
        let delivery = Delivery::new(None).unwrap();
        assert_eq!(
            delivery
                .drain(&mut store, &sub, &AtomicBool::new(false))
                .unwrap()["delivered"],
            1
        );
        let event = receiver.join().unwrap();
        assert_eq!(event["account"], "test@example.test");
        assert_eq!(event["mail"]["text"], "new incremental body");
        drop(store);
        let mut restarted = Store::open(dir.path()).unwrap();
        let cursor = restarted.mail_webhook_cursor(&sub).unwrap();
        assert_eq!(cursor, Some(delta.cursor));
        let unchanged = runtime
            .block_on(watch::poll_with_session(&mut session, "INBOX", cursor))
            .unwrap();
        restarted
            .record_mail_delta(&sub, cursor, &unchanged, Utc::now())
            .unwrap();
        assert_eq!(
            delivery
                .drain(&mut restarted, &sub, &AtomicBool::new(false))
                .unwrap()["delivered"],
            0
        );
        assert!(restarted
            .get("mail:INBOX:7:11")
            .unwrap()
            .raw
            .starts_with("base64:"));
        assert_eq!(imap.join().unwrap().len(), 9);
    }

    #[test]
    fn search_protocol_utf8_literal_caps_headers_and_preserves_identity() {
        let query = "中文 \"quote\" \\ path";
        let text = search_text(query).unwrap();
        let header = concat!(
            "Subject: 中文郵件\r\nFrom: Sender <sender@example.test>\r\n",
            "Content-Type: multipart/mixed; boundary=outer\r\n\r\n",
        );
        let (address, server) = mock_imap(vec![
            examine_step("INBOX", 4, 7),
            (
                format!("UID SEARCH CHARSET UTF-8 1:4 TEXT {{{}}}", query.len()),
                "+ continue\r\n".to_owned(),
            ),
            (
                format!("LITERAL:{query}"),
                "* SEARCH 12 17 19 18\r\n".to_owned(),
            ),
            (
                "UID FETCH 19,18 (UID FLAGS INTERNALDATE BODY.PEEK[HEADER])".to_owned(),
                literal_fetch(18, "HEADER", header, "\\Flagged")
                    + &literal_fetch(19, "HEADER", header, "\\Seen"),
            ),
        ]);
        let result = run_async(async {
            let mut session = mock_login(address).await;
            search_with_session(&mut session, &text, Some("INBOX"), 2).await
        })
        .unwrap();
        assert!(result.truncated);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].id, "mail:INBOX:7:18");
        assert_eq!(result.items[0].uid_validity, Some(7));
        assert_eq!(result.items[0].flags, vec!["\\Flagged"]);
        assert_eq!(result.items[0].title, "中文郵件");
        assert_eq!(result.items[0].emails, vec!["sender@example.test"]);
        assert_eq!(
            result.items[0].date.as_deref(),
            Some("2026-10-07T09:00:00+00:00")
        );
        assert!(result.items.iter().all(|item| item.raw.is_empty()
            && item.text.is_empty()
            && item.attachments.is_empty()));
        let requests = server.join().unwrap();
        assert!(requests
            .iter()
            .all(|request| !request.starts_with("SELECT ")
                && !request.contains("UID STORE")
                && !request.contains("BODY.PEEK[]")));
    }

    #[test]
    fn search_protocol_escapes_ascii_quotes_and_backslashes() {
        let query = "a \"quoted\" \\ path";
        let (address, server) = mock_imap(vec![
            examine_step("INBOX", 1, 7),
            (
                "UID SEARCH 1:1 TEXT \"a \\\"quoted\\\" \\\\ path\"".to_owned(),
                "* SEARCH\r\n".to_owned(),
            ),
        ]);
        let result = run_async(async {
            let mut session = mock_login(address).await;
            search_with_session(&mut session, query, Some("INBOX"), 500).await
        })
        .unwrap();
        assert!(!result.truncated);
        assert!(result.items.is_empty());
        server.join().unwrap();
    }

    #[test]
    fn search_protocol_reports_rejected_utf8_literal() {
        let query = "中文";
        let (address, server) = mock_imap(vec![
            examine_step("INBOX", 1, 7),
            (
                format!("UID SEARCH CHARSET UTF-8 1:1 TEXT {{{}}}", query.len()),
                "{tag} NO [BADCHARSET (US-ASCII)] unavailable\r\n".to_owned(),
            ),
        ]);
        let error = run_async(async {
            let mut session = mock_login(address).await;
            search_with_session(&mut session, query, Some("INBOX"), 500).await
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("UTF-8 SEARCH literal"));
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn search_protocol_keeps_same_uid_in_different_folders() {
        let header = "Subject: match\r\n\r\n";
        let (address, server) = mock_imap(vec![
            (
                "LIST \"\" *".to_owned(),
                concat!(
                    "* LIST (\\Noselect) \"/\" \"Root\"\r\n",
                    "* LIST () \"/\" \"INBOX\"\r\n",
                    "* LIST () \"/\" \"Archive\"\r\n",
                )
                .to_owned(),
            ),
            examine_step("Archive", 1, 7),
            (
                "UID SEARCH 1:1 TEXT \"match\"".to_owned(),
                "* SEARCH 12\r\n".to_owned(),
            ),
            (
                "UID FETCH 12 (UID FLAGS INTERNALDATE BODY.PEEK[HEADER])".to_owned(),
                literal_fetch(12, "HEADER", header, ""),
            ),
            examine_step("INBOX", 1, 8),
            (
                "UID SEARCH 1:1 TEXT \"match\"".to_owned(),
                "* SEARCH 12\r\n".to_owned(),
            ),
            (
                "UID FETCH 12 (UID FLAGS INTERNALDATE BODY.PEEK[HEADER])".to_owned(),
                literal_fetch(12, "HEADER", header, ""),
            ),
        ]);
        let result = run_async(async {
            let mut session = mock_login(address).await;
            search_with_session(&mut session, "match", None, 500).await
        })
        .unwrap();
        assert!(!result.truncated);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].id, "mail:Archive:7:12");
        assert_eq!(result.items[1].id, "mail:INBOX:8:12");
        server.join().unwrap();
    }

    #[test]
    fn search_protocol_reports_later_folder_failure() {
        let (address, server) = mock_imap(vec![
            (
                "LIST \"\" *".to_owned(),
                "* LIST () \"/\" \"Archive\"\r\n* LIST () \"/\" \"INBOX\"\r\n".to_owned(),
            ),
            examine_step("Archive", 1, 7),
            (
                "UID SEARCH 1:1 TEXT \"match\"".to_owned(),
                "* SEARCH 12\r\n".to_owned(),
            ),
            (
                "UID FETCH 12 (UID FLAGS INTERNALDATE BODY.PEEK[HEADER])".to_owned(),
                literal_fetch(12, "HEADER", "Subject: match\r\n\r\n", ""),
            ),
            (
                "EXAMINE \"INBOX\"".to_owned(),
                "{tag} NO unavailable\r\n".to_owned(),
            ),
        ]);
        let result = run_async(async {
            let mut session = mock_login(address).await;
            search_with_session(&mut session, "match", None, 500).await
        });
        assert!(result.unwrap_err().to_string().contains("INBOX"));
        server.join().unwrap();
    }

    #[test]
    fn search_protocol_bounds_scan_of_large_mailbox() {
        let count = SEARCH_MAX_WINDOWS as u32 * SEARCH_WINDOW_SIZE + 1;
        let mut steps = vec![examine_step("INBOX", count, 7)];
        let mut end = count;
        for _ in 0..SEARCH_MAX_WINDOWS {
            let start = end.saturating_sub(SEARCH_WINDOW_SIZE - 1).max(1);
            steps.push((
                format!("UID SEARCH {start}:{end} TEXT \"absent\""),
                "* SEARCH\r\n".to_owned(),
            ));
            end = start - 1;
        }
        let (address, server) = mock_imap(steps);
        let result = run_async(async {
            let mut session = mock_login(address).await;
            search_with_session(&mut session, "absent", Some("INBOX"), 500).await
        })
        .unwrap();
        assert!(result.truncated);
        assert!(result.items.is_empty());
        assert_eq!(server.join().unwrap().len(), SEARCH_MAX_WINDOWS + 2);
    }

    #[test]
    fn lazy_body_protocol_examines_and_fetches_without_marking_seen() {
        let raw = "Subject: loaded\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n中文本文\r\n";
        let (address, server) = mock_imap(vec![
            examine_step("INBOX", 1, 1),
            (
                "UID FETCH 2 (UID FLAGS INTERNALDATE BODY.PEEK[])".to_owned(),
                literal_fetch(2, "", raw, "\\Flagged"),
            ),
        ]);
        let item = cache_item("");
        let loaded = run_async(async {
            let mut session = mock_login(address).await;
            fetch_item_with_session(&mut session, &item).await
        })
        .unwrap();
        assert_eq!(loaded.id, item.id);
        assert_eq!(loaded.text.trim(), "中文本文");
        assert_eq!(loaded.flags, vec!["\\Flagged"]);
        assert_eq!(raw_bytes(&loaded).unwrap(), raw.as_bytes());
        assert_eq!(server.join().unwrap().len(), 3);
    }

    #[test]
    fn lazy_body_protocol_rejects_uidvalidity_before_fetch() {
        let (address, server) = mock_imap(vec![examine_step("INBOX", 1, 999)]);
        let item = cache_item("");
        let error = run_async(async {
            let mut session = mock_login(address).await;
            fetch_item_with_session(&mut session, &item).await
        })
        .unwrap_err();
        assert!(error.to_string().contains("UIDVALIDITY"));
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[test]
    fn search_protocol_rejects_missing_fetched_header() {
        let (address, server) = mock_imap(vec![
            examine_step("INBOX", 1, 7),
            (
                "UID SEARCH 1:1 TEXT \"match\"".to_owned(),
                "* SEARCH 12\r\n".to_owned(),
            ),
            (
                "UID FETCH 12 (UID FLAGS INTERNALDATE BODY.PEEK[HEADER])".to_owned(),
                "* 1 FETCH (UID 12 FLAGS ())\r\n".to_owned(),
            ),
        ]);
        let error = run_async(async {
            let mut session = mock_login(address).await;
            search_with_session(&mut session, "match", Some("INBOX"), 500).await
        })
        .expect_err("missing header must fail");
        assert!(error.to_string().contains("缺少標頭"));
        server.join().unwrap();
    }

    #[test]
    fn search_protocol_rejects_unrequested_body_instead_of_header() {
        let (address, server) = mock_imap(vec![
            examine_step("INBOX", 1, 7),
            (
                "UID SEARCH 1:1 TEXT \"match\"".to_owned(),
                "* SEARCH 12\r\n".to_owned(),
            ),
            (
                "UID FETCH 12 (UID FLAGS INTERNALDATE BODY.PEEK[HEADER])".to_owned(),
                literal_fetch(12, "", "Subject: match\r\n\r\nbody", ""),
            ),
        ]);
        let error = run_async(async {
            let mut session = mock_login(address).await;
            search_with_session(&mut session, "match", Some("INBOX"), 500).await
        })
        .unwrap_err();
        assert!(error.to_string().contains("缺少標頭"));
        server.join().unwrap();
    }

    fn cache_item(raw: &str) -> Item {
        Item {
            id: "mail:INBOX:1:2".to_owned(),
            kind: Kind::Mail,
            collection: "INBOX".to_owned(),
            remote_id: "2".to_owned(),
            etag: Some("1:2".to_owned()),
            uid_validity: Some(1),
            title: "subject".to_owned(),
            subtitle: "sender@example.com".to_owned(),
            date: None,
            end_date: None,
            text: "body".to_owned(),
            raw: raw.to_owned(),
            flags: Vec::new(),
            emails: Vec::new(),
            phones: Vec::new(),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn sync_unchanged_uid_updates_flags_repairs_cache_and_drops_deleted_without_body_fetch() {
        let raw = concat!(
            "Subject: synthetic\r\n",
            "Content-Type: multipart/mixed; boundary=outer\r\n\r\n",
            "--outer\r\nContent-Type: multipart/alternative; boundary=inner\r\n\r\n",
            "--inner\r\nContent-Type: text/plain; charset=utf-8\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n\r\n",
            "Hi =E5=90=84=E4=BD=8D\r\n--inner--\r\n--outer--\r\n",
        );
        let mut cached = cache_item(raw);
        cached.text.clear();
        cached.flags = vec!["\\Seen".into()];
        let mut deleted = cached.clone();
        deleted.id = "mail:INBOX:1:3".into();
        deleted.remote_id = "3".into();
        deleted.etag = Some("1:3".into());
        let (address, server) = mock_imap(vec![
            (
                "LIST \"\" *".to_owned(),
                "* LIST () \"/\" \"INBOX\"\r\n".to_owned(),
            ),
            examine_step("INBOX", 1, 1),
            ("UID SEARCH ALL".to_owned(), "* SEARCH 2\r\n".to_owned()),
            (
                "UID FETCH 2 (UID FLAGS INTERNALDATE)".to_owned(),
                "* 1 FETCH (UID 2 FLAGS (\\Flagged) INTERNALDATE \"07-Oct-2026 09:00:00 +0000\")\r\n"
                    .to_owned(),
            ),
        ]);
        let snapshot = run_async(async {
            let mut session = mock_login(address).await;
            sync_with_session(&mut session, &[cached.clone(), deleted], "ALL").await
        })
        .unwrap();
        assert_eq!(
            snapshot.items.len(),
            1,
            "deleted UID must leave the snapshot"
        );
        let updated = &snapshot.items[0];
        assert_eq!(updated.id, cached.id);
        assert_eq!(updated.etag, cached.etag);
        assert_eq!(updated.raw, cached.raw);
        assert_eq!(updated.flags, vec!["\\Flagged"]);
        assert_eq!(updated.date.as_deref(), Some("2026-10-07T09:00:00+00:00"));
        assert!(updated.text.starts_with("Hi 各位"));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests.iter().all(|request| !request.contains("BODY")));
    }

    #[test]
    fn empty_folder_list_is_rejected() {
        assert!(normalize_folders(Vec::new()).is_err());
    }

    #[test]
    fn cache_presence_accepts_valid_encoded_body() {
        assert!(cached_body_present(&cache_item("base64:SGVsbG8=")));
    }

    #[test]
    fn cache_presence_accepts_encoded_body_crossing_decode_buffer() {
        let encoded = BASE64_STANDARD.encode(vec![b'x'; 3073]);
        let raw = format!("{RAW_BASE64_PREFIX}{encoded}");
        assert!(cached_body_present(&cache_item(&raw)));
    }

    #[test]
    fn cache_presence_accepts_legacy_raw_body() {
        assert!(cached_body_present(&cache_item(
            "Subject: test\r\n\r\nbody"
        )));
    }

    #[test]
    fn cache_presence_rejects_empty_or_invalid_encoded_body() {
        assert!(!cached_body_present(&cache_item("base64:")));
        assert!(!cached_body_present(&cache_item("base64:SGVsbG8")));
        assert!(!cached_body_present(&cache_item("base64:SGVsbG8!")));
        assert!(!cached_body_present(&cache_item("base64:SGVsbG8=SGVsbG8=")));
    }

    #[test]
    fn internal_date_wins_over_old_header_date() {
        let raw = concat!(
            "Date: Mon, 01 Jan 2001 00:00:00 +0000\r\n",
            "Content-Type: text/plain\r\n",
            "\r\n",
            "body\r\n",
        );
        let parsed = parse_mail(raw.as_bytes()).expect("message should parse");
        let date = parse_date(&parsed, Some("2026-10-06T12:34:56+00:00"))
            .expect("internal date should parse");
        assert_eq!(date, "2026-10-06T12:34:56+00:00");
    }

    #[test]
    fn nested_multipart_quoted_printable_plain_is_not_lost() {
        let plain = format!(
            "Hi =E5=90=84=E4=BD=8D\r\n{}",
            "Synthetic line for MIME traversal.\r\n".repeat(430)
        );
        let raw = format!(
            "Subject: synthetic\r\nContent-Type: multipart/mixed; boundary=outer\r\n\r\n\
             --outer\r\nContent-Type: multipart/alternative; boundary=inner\r\n\r\n\
             --inner\r\nContent-Type: text/plain; charset=utf-8\r\n\
             Content-Transfer-Encoding: quoted-printable\r\n\r\n{plain}\
             --inner--\r\n--outer--\r\n"
        );
        let fetched = FetchedMessage {
            uid: 4,
            flags: Vec::new(),
            raw: Some(raw.into_bytes()),
            internal_date: None,
        };
        let item = parse_item("INBOX", 9, &fetched).expect("synthetic MIME should parse");
        assert!(
            item.text.starts_with("Hi 各位"),
            "nested plain text was lost"
        );
        assert!(item.text.len() > 14_000);
    }

    #[test]
    fn full_items_and_search_summaries_decode_q_and_b_headers_before_storage() {
        let raw = concat!(
            "Subject: =?UTF-8?Q?=E6=B8=AC?=\r\n =?UTF-8?B?6Kmm?=\r\n",
            "From: =?UTF-8?B?5ris6Kmm?= <sender@example.test>\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n\r\nbody\r\n",
        );
        let fetched = FetchedMessage {
            uid: 4,
            flags: Vec::new(),
            raw: Some(raw.as_bytes().to_vec()),
            internal_date: None,
        };
        for item in [
            parse_item("INBOX", 9, &fetched).unwrap(),
            parse_summary("INBOX", 9, &fetched).unwrap(),
        ] {
            assert_eq!(item.title, "測試");
            assert_eq!(item.subtitle, "測試 <sender@example.test>");
            assert_eq!(item.emails, vec!["sender@example.test"]);
        }
    }

    #[test]
    fn attached_multipart_stays_one_attachment() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=outer\r\n",
            "\r\n",
            "--outer\r\n",
            "Content-Type: multipart/mixed; boundary=inner\r\n",
            "Content-Disposition: attachment; filename=bundle.eml\r\n",
            "\r\n",
            "--inner\r\n",
            "Content-Type: text/plain\r\n",
            "Content-Disposition: attachment; filename=child.txt\r\n",
            "\r\n",
            "child\r\n",
            "--inner--\r\n",
            "--outer--\r\n",
        );
        let fetched = FetchedMessage {
            uid: 4,
            flags: Vec::new(),
            raw: Some(raw.as_bytes().to_vec()),
            internal_date: None,
        };
        let item = parse_item("INBOX", 9, &fetched).expect("MIME should parse");
        assert_eq!(item.attachments.len(), 1);
        assert_eq!(item.attachments[0].filename, "bundle.eml");
        assert!(item.text.is_empty());
        let (filename, bytes) = attachment(&item, 0).expect("attachment should be present");
        assert_eq!(filename, "bundle.eml");
        assert!(bytes
            .windows(b"child.txt".len())
            .any(|window| window == b"child.txt"));
    }
}

#[cfg(test)]
mod smtp_fallback_tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[test]
    fn fallback_only_for_587_tcp_refused_or_timeout() {
        for kind in [ErrorKind::ConnectionRefused, ErrorKind::TimedOut] {
            assert_eq!(
                select_smtp_port(587, || Err(Error::from(kind))).unwrap(),
                465
            );
        }
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidData,
            ErrorKind::NotFound,
            ErrorKind::ConnectionReset,
        ] {
            assert!(select_smtp_port(587, || Err(Error::from(kind))).is_err());
        }
        assert_eq!(select_smtp_port(587, || Ok(())).unwrap(), 587);
        for port in [465, 2525] {
            assert_eq!(
                select_smtp_port(port, || panic!("must not probe")).unwrap(),
                port
            );
        }
    }

    #[test]
    fn custom_smtp_port_is_only_accepted_for_loopback_fixtures() {
        let mut config = Config::new("user@example.com".into(), "mail.example.test").unwrap();
        config.smtp_port = 2525;
        assert!(smtp_port(&config).is_err());
        for host in ["127.0.0.1", "localhost", "::1", "[::1]"] {
            config.smtp_host = host.into();
            assert_eq!(smtp_port(&config).unwrap(), 2525);
        }
        for host in [
            "localhost.evil.example",
            "127.0.0.1.nip.io",
            "::ffff:127.0.0.1",
        ] {
            config.smtp_host = host.into();
            assert!(smtp_port(&config).is_err(), "{host} is not loopback");
        }
    }

    #[test]
    fn test_root_certificate_never_applies_to_a_real_server() {
        let cert = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(cert.path(), b"pem").unwrap();
        let path = Some(cert.path().as_os_str().to_owned());
        let error = read_test_root_certificate(path.clone(), "mail.example.test").unwrap_err();
        assert!(error.to_string().contains("loopback"));
        assert_eq!(
            read_test_root_certificate(path, "127.0.0.1").unwrap(),
            Some(b"pem".to_vec())
        );
        assert_eq!(
            read_test_root_certificate(None, "mail.example.test").unwrap(),
            None
        );
        assert_eq!(
            read_test_root_certificate(Some("".into()), "mail.example.test").unwrap(),
            None
        );
    }

    #[test]
    fn local_tcp_probe_distinguishes_open_and_refused() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(
            select_smtp_port(587, || smtp_tcp_probe("127.0.0.1", port)).unwrap(),
            587
        );
        drop(listener);
        assert_eq!(
            select_smtp_port(587, || smtp_tcp_probe("127.0.0.1", port)).unwrap(),
            465
        );
    }
}
