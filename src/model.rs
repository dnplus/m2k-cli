use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Mail,
    Calendar,
    Contacts,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mail => "mail",
            Self::Calendar => "calendar",
            Self::Contacts => "contacts",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub kind: Kind,
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub index: usize,
    pub filename: String,
    pub content_type: String,
    pub size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub kind: Kind,
    pub collection: String,
    pub remote_id: String,
    pub etag: Option<String>,
    pub uid_validity: Option<u32>,
    pub title: String,
    pub subtitle: String,
    pub date: Option<String>,
    pub end_date: Option<String>,
    pub text: String,
    pub raw: String,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub emails: Vec<String>,
    #[serde(default)]
    pub phones: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub kind: Kind,
    pub collections: Vec<Collection>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub kind: Kind,
    pub count: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OutgoingMail {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<std::path::PathBuf>,
    pub in_reply_to: Option<String>,
}
