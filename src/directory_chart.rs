use crate::{
    directory::{self, DirectoryContact, DirectoryNode},
    model::Kind,
};
use anyhow::Result;
use clap::{Args, ValueEnum};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Mail2000 的 vCard 只有 FN／N／NICKNAME／EMAIL／TEL／ORG，沒有主管或匯報欄位，因此不畫匯報線也不推估。
pub const REPORTING_NOTE: &str =
    "通訊錄 vCard 沒有主管或匯報欄位；本圖只呈現部門歸屬，不含匯報線，也不推估主管。";

#[derive(Args, Debug)]
pub struct ChartArgs {
    #[arg(
        long,
        visible_alias = "addressbook",
        help = "只輸出指定分支及其子部門；可填 path、名稱或 URL"
    )]
    pub branch: Option<String>,
    #[arg(
        long,
        value_enum,
        default_value_t = ChartFormat::Markdown,
        help = "非 JSON 輸出格式；全域 --json 優先"
    )]
    pub format: ChartFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ChartFormat {
    Markdown,
    Mermaid,
}

#[derive(Debug, Serialize)]
pub struct OrgChart {
    /// 指定分支時為該分支 path。
    pub branch: Option<String>,
    /// 成員 email 最常見的網域；Markdown 只另列不是「帳號@此網域」的 email。
    pub email_domain: Option<String>,
    pub department_count: usize,
    /// 不重複的成員數；同一人可屬多個部門。
    pub member_count: usize,
    pub reporting_note: &'static str,
    pub departments: Vec<Department>,
}

#[derive(Debug, Serialize)]
pub struct Department {
    pub name: String,
    pub path: String,
    /// 直屬此部門的成員，依姓名排序。
    pub members: Vec<Member>,
    pub children: Vec<Department>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Member {
    /// vCard N 的全名（Mail2000 放中文姓名）；沒有時退回 FN。
    pub name: String,
    /// vCard FN，在 Mail2000 等於 email 前綴；與 name 相同時省略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub phones: Vec<String>,
    pub emails: Vec<String>,
    #[serde(skip)]
    id: String,
}

impl Member {
    /// 對應 `DirectoryContact::id`，供 TUI 查回完整聯絡人詳情。
    pub fn id(&self) -> &str {
        &self.id
    }
}

pub fn execute(dir: &Path, args: ChartArgs, json: bool) -> Result<()> {
    let store = directory::local_store(dir)?;
    let nodes = directory::address_book_tree(&store.collections(Kind::Contacts)?, None);
    let contacts = directory::merge_contact_items(
        &store.list(Kind::Contacts, None, None, usize::MAX)?,
        &nodes,
    );
    let chart = build(&nodes, &contacts, args.branch.as_deref())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&chart)?);
    } else {
        match args.format {
            ChartFormat::Markdown => print!("{}", render_markdown(&chart)),
            ChartFormat::Mermaid => print!("{}", render_mermaid(&chart)),
        }
    }
    Ok(())
}

/// 由部門節點與聯絡人建立組織圖；兩者都不需預先排序。
///
/// 每位聯絡人列在其所屬的每個部門；最上層通訊錄是全公司名單，只收沒有更深部門歸屬的人。
pub fn build(
    nodes: &[DirectoryNode],
    contacts: &[DirectoryContact],
    branch: Option<&str>,
) -> Result<OrgChart> {
    let mut nodes = nodes
        .iter()
        .filter(|node| !node.path.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    nodes.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));
    nodes.dedup_by(|a, b| a.path == b.path);
    let depth_by_path = nodes
        .iter()
        .map(|node| (node.path.as_str(), node.depth))
        .collect::<BTreeMap<_, _>>();

    let mut members = BTreeMap::<&str, Vec<Member>>::new();
    for contact in contacts {
        let known = contact
            .memberships
            .iter()
            .filter_map(|membership| depth_by_path.get_key_value(membership.path.as_str()))
            .collect::<BTreeSet<_>>();
        let has_department = known.iter().any(|(_, &depth)| depth > 0);
        for (&path, &depth) in known {
            if has_department && depth == 0 {
                continue;
            }
            let name = contact
                .structured_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(&contact.name)
                .to_owned();
            let differs = |value: &&String| {
                !value.eq_ignore_ascii_case(&name) && !value.eq_ignore_ascii_case(&contact.name)
            };
            members.entry(path).or_default().push(Member {
                username: (!contact.name.eq_ignore_ascii_case(&name)).then(|| contact.name.clone()),
                title: contact.title.clone(),
                // Mail2000 的 NICKNAME 與 FN 相同，重複時不列。
                nickname: contact.nickname.as_ref().filter(differs).cloned(),
                phones: contact.phones.clone(),
                emails: contact.emails.clone(),
                name,
                id: contact.id.clone(),
            });
        }
    }
    for list in members.values_mut() {
        list.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    }

    let mut children = BTreeMap::<&str, Vec<&DirectoryNode>>::new();
    let mut roots = Vec::new();
    for node in &nodes {
        match node.parent_path.as_deref() {
            Some(parent) if depth_by_path.contains_key(parent) => {
                children.entry(parent).or_default().push(node)
            }
            _ => roots.push(node),
        }
    }
    // 部門 path 不含前後斜線，但使用者常照 Mail2000 顯示名稱寫成 `/A/B`。
    let branch = branch
        .map(|selector| {
            let path = selector.trim().trim_matches('/');
            match nodes.iter().find(|node| node.path == path) {
                Some(node) => Ok(node),
                None => directory::select_node(&nodes, selector),
            }
        })
        .transpose()?;
    if let Some(node) = branch {
        roots = vec![node];
    }

    let departments = roots
        .into_iter()
        .map(|node| department(node, &children, &members))
        .collect::<Vec<_>>();
    let mut department_count = 0;
    let mut ids = BTreeSet::new();
    let mut stack = departments.iter().collect::<Vec<_>>();
    while let Some(department) = stack.pop() {
        department_count += 1;
        ids.extend(department.members.iter().map(|member| member.id.as_str()));
        stack.extend(&department.children);
    }
    let member_count = ids.len();
    let mut domains = BTreeMap::<&str, usize>::new();
    for member in members.values().flatten() {
        for email in &member.emails {
            if let Some((_, domain)) = email.split_once('@') {
                *domains.entry(domain).or_default() += 1;
            }
        }
    }
    let email_domain = domains
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(domain, _)| domain.to_owned());
    Ok(OrgChart {
        branch: branch.map(|node| node.path.clone()),
        email_domain,
        department_count,
        member_count,
        reporting_note: REPORTING_NOTE,
        departments,
    })
}

fn department(
    node: &DirectoryNode,
    children: &BTreeMap<&str, Vec<&DirectoryNode>>,
    members: &BTreeMap<&str, Vec<Member>>,
) -> Department {
    Department {
        name: node.name.clone(),
        path: node.path.clone(),
        members: members.get(node.path.as_str()).cloned().unwrap_or_default(),
        children: children
            .get(node.path.as_str())
            .into_iter()
            .flatten()
            .map(|child| department(child, children, members))
            .collect(),
    }
}

/// 給代理程式讀的縮排 Markdown：部門以粗體標示，成員列在所屬部門下。
pub fn render_markdown(chart: &OrgChart) -> String {
    let mut out = String::from("# 部門組織圖\n\n");
    match &chart.branch {
        Some(path) => out.push_str(&format!("- 範圍：{path}\n")),
        None => out.push_str("- 範圍：全部已快取分支\n"),
    }
    out.push_str(&format!(
        "- 部門 {} 個，成員 {} 位（同一人可屬多個部門）\n- 匯報線：{}\n",
        chart.department_count, chart.member_count, chart.reporting_note
    ));
    if let Some(domain) = &chart.email_domain {
        out.push_str(&format!(
            "- Email：括號內帳號 @{domain}（沒有括號時為姓名欄位本身）；例外另列\n"
        ));
    }
    if chart.member_count == 0 {
        out.push_str("- 本機尚無聯絡人快取；先執行 `m2k directory sync --scope company`\n");
    }
    out.push('\n');
    for department in &chart.departments {
        markdown_department(&mut out, department, chart.email_domain.as_deref(), 0);
    }
    out
}

fn markdown_department(
    out: &mut String,
    department: &Department,
    domain: Option<&str>,
    level: usize,
) {
    let indent = "  ".repeat(level);
    out.push_str(&format!(
        "{indent}- **{}** `{}`（{} 人）\n",
        department.name,
        department.path,
        department.members.len()
    ));
    for member in &department.members {
        let mut line = member.name.clone();
        if let Some(username) = &member.username {
            line.push_str(&format!("（{username}）"));
        }
        for value in member.title.iter().chain(&member.nickname) {
            line.push_str(&format!(" · {value}"));
        }
        if !member.phones.is_empty() {
            line.push_str(&format!(" · {}", phone_label(&member.phones)));
        }
        let account = member.username.as_deref().unwrap_or(&member.name);
        let unusual = member
            .emails
            .iter()
            .filter(|email| {
                domain.is_none_or(|domain| {
                    !email.eq_ignore_ascii_case(&format!("{account}@{domain}"))
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if !unusual.is_empty() {
            line.push_str(&format!(" · {}", unusual.join(", ")));
        }
        out.push_str(&format!("{indent}  - {line}\n"));
    }
    for child in &department.children {
        markdown_department(out, child, domain, level + 1);
    }
}

const MAX_EXTENSION_DIGITS: usize = 6;

fn phone_label(phones: &[String]) -> String {
    phones
        .iter()
        .map(|phone| {
            if phone.len() <= MAX_EXTENSION_DIGITS && phone.chars().all(|c| c.is_ascii_digit()) {
                format!("分機 {phone}")
            } else {
                format!("電話 {phone}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Mermaid 流程圖只畫部門與直屬人數；成員名單請用 Markdown 或 JSON。
pub fn render_mermaid(chart: &OrgChart) -> String {
    let mut out = String::from("flowchart TD\n");
    let mut next = 0;
    for department in &chart.departments {
        mermaid_department(&mut out, department, None, &mut next);
    }
    out
}

fn mermaid_department(
    out: &mut String,
    department: &Department,
    parent: Option<usize>,
    next: &mut usize,
) {
    let id = *next;
    *next += 1;
    out.push_str(&format!(
        "  d{id}[\"{}<br/>{} 人\"]\n",
        department.name.replace('"', "#quot;"),
        department.members.len()
    ));
    if let Some(parent) = parent {
        out.push_str(&format!("  d{parent} --> d{id}\n"));
    }
    for child in &department.children {
        mermaid_department(out, child, Some(id), next);
    }
}
