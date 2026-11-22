//! Output-time domain trust. Cached MIME and attachment access remain unchanged.
use crate::{config, model::Item, provider::mail};
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
    List,
    Add { domain: String },
    Remove { domain: String },
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
    let domains = file.accounts.entry(key).or_default();
    let changed = !matches!(command, Command::List);
    match command {
        Command::List => {}
        Command::Add { domain: value } => {
            domains.insert(domain(&value)?);
        }
        Command::Remove { domain: value } => {
            domains.remove(&domain(&value)?);
        }
    }
    let result = json!({"account": cfg.email, "default_domain": address_domain(&cfg.email)?, "domains": domains});
    if changed {
        crate::local_state::write_atomic(
            &parent.join("domains.json"),
            &serde_json::to_vec_pretty(&file)?,
        )?;
    }
    Ok(result)
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
    if returns.len() > 1 {
        bail!("多個 Return-Path");
    }
    if let Some(value) = returns.first() {
        let addresses = mailparse::addrparse(value)?;
        let [MailAddr::Single(address)] = addresses.as_slice() else {
            bail!("無效 Return-Path");
        };
        let rp = address_domain(&address.addr)?;
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
