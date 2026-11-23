//! Output-time domain trust. Cached MIME and attachment access remain unchanged.
use crate::{
    config,
    model::{Item, Kind},
    provider::mail,
};
use anyhow::{bail, Context, Result};
use mailparse::{MailAddr, MailHeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(clap::Subcommand)]
pub enum Command {
    /// 列出已信任網域及來源
    List,
    /// 手動信任一個或多個網域
    Add {
        #[arg(required = true)]
        domains: Vec<String>,
    },
    /// 撤銷信任並阻止自動重新加入
    Remove { domain: String },
    /// 根據已驗證的公司寄件紀錄自動信任外部收件網域
    Auto {
        #[arg(long)]
        dry_run: bool,
    },
    /// 列出重複出現的未信任外部寄件網域
    Candidates {
        #[arg(long, default_value_t = 3)]
        min_count: usize,
    },
}

pub fn domain(value: &str) -> Result<String> {
    let value = value.trim();
    let value = value.strip_suffix('.').unwrap_or(value);
    let ascii = idna::domain_to_ascii_strict(value)
        .map_err(|_| anyhow::anyhow!("無效網域"))?
        .to_lowercase();
    if ascii.is_empty()
        || ascii.len() > 253
        || ascii.split('.').any(|s| {
            s.is_empty()
                || s.len() > 63
                || s.starts_with('-')
                || s.ends_with('-')
                || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        bail!("無效網域");
    }
    Ok(ascii)
}
fn address_domain(address: &str) -> Result<String> {
    domain(address.rsplit_once('@').context("缺少寄件網域")?.1)
}
fn within(child: &str, parent: &str) -> bool {
    child == parent || child.strip_suffix(parent).is_some_and(|s| s.ends_with('.'))
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    accounts: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    records: BTreeMap<String, BTreeMap<String, Record>>,
    #[serde(default)]
    revoked: BTreeMap<String, BTreeMap<String, String>>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    source: String,
    added_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
}
fn read(dir: &Path) -> Result<File> {
    let path = dir.join("trust/domains.json");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let file: File = serde_json::from_slice(&bytes).context("信任檔損毀：拒絕讀取正文")?;
            for domains in file.accounts.values() {
                for value in domains {
                    domain(value).context("信任檔包含無效網域")?;
                }
            }
            Ok(file)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
        Err(e) => Err(e).context("無法讀取信任檔：拒絕讀取正文"),
    }
}
fn blocked(file: &File, key: &str, value: &str) -> bool {
    file.revoked
        .get(key)
        .is_some_and(|domains| domains.keys().any(|d| within(value, d)))
}
fn trusted(file: &File, key: &str, value: &str) -> bool {
    file.accounts.get(key).is_some_and(|domains| {
        domains
            .iter()
            .any(|d| domain(d).is_ok_and(|d| within(value, &d)))
    })
}
fn items(dir: &Path, cfg: &config::Config) -> Result<Vec<Item>> {
    let store = crate::store::Store::open_reader(dir)?;
    store.verify_account(cfg)?;
    store.list(Kind::Mail, None, None, usize::MAX)
}
fn from_address(item: &Item) -> Result<mailparse::SingleInfo> {
    let bytes = mail::raw_bytes(item)?;
    let (headers, _) = mailparse::parse_headers(&bytes)?;
    let values = headers.get_all_values("From");
    if values.len() != 1 {
        bail!("From 必須只有一個");
    }
    let addresses = mailparse::addrparse(&values[0])?;
    let [MailAddr::Single(address)] = addresses.as_slice() else {
        bail!("From 必須只有一個地址");
    };
    Ok(address.clone())
}
fn recipients(item: &Item) -> Result<BTreeSet<String>> {
    let bytes = mail::raw_bytes(item)?;
    let (headers, _) = mailparse::parse_headers(&bytes)?;
    let mut result = BTreeSet::new();
    for value in headers
        .get_all_values("To")
        .into_iter()
        .chain(headers.get_all_values("Cc"))
    {
        for address in mailparse::addrparse(&value)?.iter() {
            match address {
                MailAddr::Single(a) => {
                    if let Ok(d) = address_domain(&a.addr) {
                        result.insert(d);
                    }
                }
                MailAddr::Group(g) => {
                    for a in &g.addrs {
                        if let Ok(d) = address_domain(&a.addr) {
                            result.insert(d);
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}
fn auto_apply(
    file: &mut File,
    key: &str,
    cfg: &config::Config,
    items: &[Item],
) -> Result<Vec<Value>> {
    let own = address_domain(&cfg.email)?;
    let mut added = Vec::new();
    for item in items {
        let Ok(from) = sender(item) else { continue };
        if !within(&from, &own) {
            continue;
        }
        let Ok(address) = from_address(item) else {
            continue;
        };
        let source = if address.addr.eq_ignore_ascii_case(&cfg.email) {
            "auto:sent"
        } else {
            "auto:internal"
        };
        let Ok(recipients) = recipients(item) else {
            continue;
        };
        for d in recipients {
            if within(&d, &own) || trusted(file, key, &d) || blocked(file, key, &d) {
                continue;
            }
            let record = Record {
                source: source.into(),
                added_at: Some(chrono::Utc::now().to_rfc3339()),
                message_id: Some(item.id.clone()),
            };
            file.accounts
                .entry(key.into())
                .or_default()
                .insert(d.clone());
            file.records
                .entry(key.into())
                .or_default()
                .insert(d.clone(), record.clone());
            added.push(json!({"domain": d, "source": record.source, "added_at": record.added_at, "message_id": record.message_id}));
        }
    }
    Ok(added)
}
fn candidates(
    file: &File,
    key: &str,
    cfg: &config::Config,
    items: &[Item],
    min_count: usize,
) -> Result<Vec<Value>> {
    let own = address_domain(&cfg.email)?;
    let mut counts: BTreeMap<String, (usize, Option<String>, String)> = BTreeMap::new();
    for item in items {
        let Ok(from) = sender(item) else { continue };
        if within(&from, &own) || trusted(file, key, &from) || blocked(file, key, &from) {
            continue;
        }
        let Ok(address) = from_address(item) else {
            continue;
        };
        let entry = counts.entry(from).or_insert((0, None, String::new()));
        entry.0 += 1;
        if entry.2.is_empty() || item.date > entry.1 {
            entry.1.clone_from(&item.date);
            entry.2 = address.display_name.unwrap_or(address.addr);
        }
    }
    Ok(counts.into_iter().filter(|(_, (count, _, _))| *count >= min_count).map(|(domain, (count, latest, example))| json!({"domain":domain,"count":count,"latest":latest,"example_sender":example})).collect())
}
pub fn execute(dir: &Path, command: Command) -> Result<Value> {
    let cfg = config::load(dir)?;
    let key = config::credential_key(&cfg)?;
    let parent = dir.join("trust");
    crate::local_state::private_directory(&parent)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join(".lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut file = read(dir)?;
    let mut changed = false;
    let mut extra = json!({});
    match command {
        Command::List => {}
        Command::Add { domains } => {
            let domains = domains
                .iter()
                .map(|d| domain(d))
                .collect::<Result<Vec<_>>>()?;
            for d in domains {
                file.accounts
                    .entry(key.clone())
                    .or_default()
                    .insert(d.clone());
                file.revoked.entry(key.clone()).or_default().remove(&d);
                file.records.entry(key.clone()).or_default().insert(
                    d,
                    Record {
                        source: "manual".into(),
                        added_at: Some(chrono::Utc::now().to_rfc3339()),
                        message_id: None,
                    },
                );
            }
            changed = true;
        }
        Command::Remove { domain: value } => {
            let d = domain(&value)?;
            file.accounts.entry(key.clone()).or_default().remove(&d);
            file.records.entry(key.clone()).or_default().remove(&d);
            file.revoked
                .entry(key.clone())
                .or_default()
                .insert(d, chrono::Utc::now().to_rfc3339());
            changed = true;
        }
        Command::Auto { dry_run } => {
            let added = auto_apply(&mut file, &key, &cfg, &items(dir, &cfg)?)?;
            changed = !dry_run && !added.is_empty();
            extra["added"] = json!(added);
            extra["dry_run"] = json!(dry_run);
        }
        Command::Candidates { min_count } => {
            extra["candidates"] = json!(candidates(
                &file,
                &key,
                &cfg,
                &items(dir, &cfg)?,
                min_count
            )?);
        }
    }
    let domains = file.accounts.get(&key).cloned().unwrap_or_default();
    let records: BTreeMap<_, _> = domains
        .iter()
        .map(|d| {
            (
                d.clone(),
                file.records
                    .get(&key)
                    .and_then(|r| r.get(d))
                    .cloned()
                    .unwrap_or(Record {
                        source: "manual".into(),
                        added_at: None,
                        message_id: None,
                    }),
            )
        })
        .collect();
    let mut result = json!({"account": cfg.email, "default_domain": address_domain(&cfg.email)?, "domains": domains, "records": records, "revoked": file.revoked.get(&key).cloned().unwrap_or_default()});
    result
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    if changed {
        crate::local_state::write_atomic(
            &parent.join("domains.json"),
            &serde_json::to_vec_pretty(&file)?,
        )?;
    }
    Ok(result)
}
/// Apply automatic trust to the current account's cached mail after a successful mail sync.
pub fn auto_after_sync(dir: &Path) -> Result<()> {
    execute(dir, Command::Auto { dry_run: false })?;
    Ok(())
}
#[derive(Clone, Debug)]
pub struct Decision {
    pub domain: Option<String>,
    pub reason: Option<&'static str>,
}
impl Decision {
    pub fn message(&self) -> String {
        format!(
            "正文已隱藏：{} {}",
            self.reason.unwrap_or_default(),
            self.domain.as_deref().unwrap_or_default()
        )
    }
    pub fn prepare(&self, item: &mut Item) -> Result<()> {
        if self.reason.is_none() {
            mail::normalize_cached(item)?;
        } else {
            // Repair metadata from MIME headers without decoding a withheld body.
            if let Ok(bytes) = mail::raw_bytes(item) {
                if let Ok((headers, _)) = mailparse::parse_headers(&bytes) {
                    if let Some(subject) = headers.get_first_value("Subject") {
                        item.title = subject;
                    }
                    if let Some(from) = headers.get_first_value("From") {
                        item.subtitle = from;
                    }
                }
            }
            item.title = mail::display_header(&item.title);
            item.subtitle = mail::display_header(&item.subtitle);
            for attachment in &mut item.attachments {
                attachment.filename = mail::display_header(&attachment.filename);
            }
            item.text.clear();
            item.raw.clear();
        }
        Ok(())
    }
    pub fn filter(&self, value: &mut Value) {
        if let Some(reason) = self.reason {
            if let Some(object) = value.as_object_mut() {
                for field in ["text", "raw", "latest_excerpt"] {
                    object.remove(field);
                }
                object.insert("body_withheld".into(), reason.into());
            }
        }
    }
}
fn sender(item: &Item) -> Result<String> {
    let bytes = mail::raw_bytes(item)?;
    let (headers, _) = mailparse::parse_headers(&bytes)?;
    let from = headers.get_all_values("From");
    if from.len() != 1 {
        bail!("From 必須只有一個");
    }
    let addresses = mailparse::addrparse(&from[0])?;
    let [MailAddr::Single(address)] = addresses.as_slice() else {
        bail!("From 必須只有一個地址");
    };
    let from = address_domain(&address.addr)?;
    let returns = headers.get_all_values("Return-Path");
    let mut previous: Option<(String, String)> = None;
    for value in &returns {
        let addresses = mailparse::addrparse(value)?;
        let [MailAddr::Single(address)] = addresses.as_slice() else {
            bail!("無效 Return-Path");
        };
        let (local, _) = address.addr.rsplit_once('@').context("缺少寄件網域")?;
        let rp = address_domain(&address.addr)?;
        if let Some((previous_local, previous_domain)) = &previous {
            if local != previous_local || &rp != previous_domain {
                bail!("多個 Return-Path");
            }
        } else {
            previous = Some((local.to_owned(), rp));
        }
    }
    if let Some((_, rp)) = previous {
        if !within(&from, &rp) && !within(&rp, &from) {
            bail!("From 與 Return-Path 不一致");
        }
    }
    Ok(from)
}
/// Reload on every decision, including TUI redraw and webhook retry.
pub fn check(dir: &Path, item: &Item) -> Result<Decision> {
    let cfg = config::load(dir)?;
    let file = read(dir)?;
    let Ok(from) = sender(item) else {
        return Ok(Decision {
            domain: None,
            reason: Some("identity_unverified"),
        });
    };
    let trusted = within(&from, &address_domain(&cfg.email)?)
        || file
            .accounts
            .get(&config::credential_key(&cfg)?)
            .is_some_and(|domains| {
                domains
                    .iter()
                    .any(|d| domain(d).is_ok_and(|d| within(&from, &d)))
            });
    Ok(Decision {
        domain: Some(from),
        reason: (!trusted).then_some("domain_untrusted"),
    })
}
#[derive(Default)]
pub struct Report {
    domains: BTreeSet<String>,
    ids: BTreeSet<String>,
}
impl Report {
    pub fn record(&mut self, id: &str, decision: &Decision) {
        match decision.reason {
            Some("domain_untrusted") => {
                if let Some(domain) = &decision.domain {
                    self.domains.insert(domain.clone());
                }
            }
            Some("identity_unverified") => {
                self.ids.insert(id.into());
            }
            _ => {}
        }
    }
    pub fn apply(&self, value: &mut Value) {
        value["needs_trust_domains"] = json!(self.domains);
        value["identity_review_ids"] = json!(self.ids);
    }
    pub fn text(&self) -> String {
        if self.domains.is_empty() && self.ids.is_empty() {
            return String::new();
        }
        format!(
            "needs_trust_domains: {}\nidentity_review_ids: {}\n",
            json!(self.domains),
            json!(self.ids)
        )
    }
}
