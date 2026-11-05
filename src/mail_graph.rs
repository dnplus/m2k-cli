//! 郵件關係圖：從本機快取的近期郵件，確定性地整理人脈與信件串，作為代理程式的參考脈絡。
//! 不呼叫模型、不連線；相同快取與相同時間點必得相同輸出。

use crate::{
    config, mail_read, mail_workflow::normalize_thread_subject, model::Item, provider, store::Store,
};
use anyhow::{bail, Result};
use chrono::{DateTime, Duration, Utc};
use clap::Args;
use mailparse::{MailAddr, MailHeaderMap};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

#[derive(Args, Debug)]
pub struct GraphArgs {
    #[arg(long, default_value_t = 14, help = "整理最近幾天的郵件（1–365）")]
    pub days: u32,
    #[arg(long, help = "只看與此人有關的往來；比對 email 或姓名片段")]
    pub person: Option<String>,
    #[arg(long, default_value_t = 20, help = "最多列出幾位聯絡人")]
    pub people: usize,
    #[arg(long, default_value_t = 30, help = "最多列出幾個信件串")]
    pub threads: usize,
    #[arg(long, default_value_t = 280, help = "每串最新回覆摘錄的字數上限")]
    pub excerpt_chars: usize,
}

#[derive(Debug, Serialize)]
pub struct MailGraph {
    pub me: String,
    pub since: String,
    pub until: String,
    pub days: u32,
    pub person: Option<String>,
    pub message_count: usize,
    pub people: Vec<Person>,
    pub threads: Vec<Thread>,
}

#[derive(Debug, Serialize)]
pub struct Person {
    pub email: String,
    pub name: Option<String>,
    /// 視窗內含此人的郵件數（寄件或收件）。
    pub messages: usize,
    pub from_them: usize,
    pub from_me: usize,
    pub threads: usize,
    pub last_contact: String,
    pub recent_subjects: Vec<String>,
    pub often_with: Vec<String>,
    /// 寄來的信全是自動信（或地址像系統帳號）；排序在真人之後。
    pub automated: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// 最後一封由他人寄出、我在收件人且尚未標記已回覆。
    AwaitingMe,
    /// 最後一封由我寄出。
    AwaitingThem,
    /// 最後一封已標記 \Answered，但回覆不在視窗或快取內。
    Replied,
    /// 我只是副本、自動通知或其他知會。
    Fyi,
}

#[derive(Debug, Serialize)]
pub struct Thread {
    pub subject: String,
    pub status: ThreadStatus,
    pub message_count: usize,
    pub participants: Vec<String>,
    pub first: String,
    pub last: String,
    pub last_from: String,
    /// 由新到舊的本機郵件 ID，可交給 `mail show`。
    pub ids: Vec<String>,
    pub latest_excerpt: String,
}

struct Message {
    id: String,
    message_id: Option<String>,
    parents: Vec<String>,
    subject: String,
    from: String,
    to: Vec<String>,
    cc: Vec<String>,
    date: DateTime<Utc>,
    answered: bool,
    /// 自動產生的信（系統通知、電子報、noreply），不期待回覆。
    automated: bool,
    text: String,
    /// 本封標頭裡的 (email, 顯示名稱)。
    names: Vec<(String, String)>,
}

impl Message {
    fn participants(&self) -> impl Iterator<Item = &String> {
        // 缺少可解析的 From 時 `from` 為空字串，不能算成聯絡人。
        std::iter::once(&self.from)
            .chain(&self.to)
            .chain(&self.cc)
            .filter(|email| !email.is_empty())
    }
}

pub fn execute(dir: &Path, args: GraphArgs, json: bool) -> Result<()> {
    if !(1..=365).contains(&args.days) {
        bail!("--days 必須介於 1 到 365");
    }
    let config = config::load(dir)?;
    let mut store = Store::open(dir)?;
    store.ensure_account(&config)?;
    let now = Utc::now();
    let items = store.mail_since(now - Duration::days(i64::from(args.days)))?;
    let graph = build(items, &config.email, &config.automated_senders, now, &args);
    if json {
        println!("{}", serde_json::to_string_pretty(&graph)?);
    } else {
        print!("{}", render_markdown(&graph));
    }
    Ok(())
}

/// 由郵件項目建立關係圖；`items` 不需預先排序，視窗外與無日期的郵件會被略過。
/// `automated_senders` 是使用者設定的系統寄件者（見 `Config::automated_senders`）。
pub fn build(
    items: Vec<Item>,
    me: &str,
    automated_senders: &[String],
    now: DateTime<Utc>,
    args: &GraphArgs,
) -> MailGraph {
    let me = me.trim().to_lowercase();
    let listed = |email: &str| listed_sender(email, automated_senders);
    let since = now - Duration::days(i64::from(args.days));
    let mut parsed = items
        .into_iter()
        .filter_map(parse_message)
        .map(|mut message| {
            message.automated |= listed(&message.from);
            message
        })
        .filter(|message| message.date >= since && message.date <= now)
        .collect::<Vec<_>>();
    // 先排序，去重後保留哪一份才與輸入順序無關。
    parsed.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| a.id.cmp(&b.id)));
    let mut messages: Vec<Message> = Vec::new();
    let mut seen_message_ids = HashMap::new();
    for message in parsed {
        // 同一封信可能同時在收件匣與寄件備份；以 Message-ID 去重，合併旗標與串信連結。
        if let Some(message_id) = &message.message_id {
            if let Some(&index) = seen_message_ids.get(message_id) {
                let existing: &mut Message = &mut messages[index];
                existing.answered |= message.answered;
                existing.parents.extend(message.parents);
                continue;
            }
            seen_message_ids.insert(message_id.clone(), messages.len());
        }
        messages.push(message);
    }
    // 同一地址的名稱可能不同；依排序後順序取最新一封的寫法，與輸入順序無關。
    let mut names = HashMap::new();
    for (email, name) in messages.iter().flat_map(|message| &message.names) {
        names.entry(email.clone()).or_insert_with(|| name.clone());
    }

    let person = args
        .person
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_lowercase);
    let matches_person = |email: &str| {
        person.as_deref().is_some_and(|needle| {
            email.contains(needle)
                || names
                    .get(email)
                    .is_some_and(|name: &String| name.to_lowercase().contains(needle))
        })
    };

    let mut groups = group_threads(&messages);
    if person.is_some() {
        groups.retain(|group| {
            group.iter().any(|&index| {
                messages[index]
                    .participants()
                    .any(|email| matches_person(email))
            })
        });
    }
    let kept = groups.iter().flatten().copied().collect::<BTreeSet<_>>();
    let mut threads = groups
        .iter()
        .map(|group| thread(&messages, group, &me, &names, args.excerpt_chars))
        .collect::<Vec<_>>();
    threads.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.subject.cmp(&b.subject)));
    threads.truncate(args.threads);

    let mut people = people(&messages, &groups, &kept, &me, &names);
    for person in &mut people {
        person.automated |= listed(&person.email);
    }
    // 名單可能把原本排在前面的人改成自動，重新排序。
    sort_people(&mut people);
    if person.is_some() {
        people.sort_by_key(|entry| !matches_person(&entry.email));
    }
    people.truncate(args.people);

    MailGraph {
        me,
        since: since.to_rfc3339(),
        until: now.to_rfc3339(),
        days: args.days,
        person: args.person.clone(),
        message_count: kept.len(),
        people,
        threads,
    }
}

fn parse_message(mut item: Item) -> Option<Message> {
    let mut names = HashMap::new();
    let date = item
        .date
        .as_deref()
        .and_then(|date| DateTime::parse_from_rfc3339(date).ok())?
        .with_timezone(&Utc);
    let _ = provider::mail::normalize_cached(&mut item);
    let bytes = provider::mail::raw_bytes(&item).unwrap_or_default();
    let headers = mailparse::parse_headers(&bytes)
        .ok()
        .map(|(headers, _)| headers);
    let mut addresses = |value: Option<String>| {
        value
            .map(|value| collect_addresses(&value, &mut names))
            .unwrap_or_default()
    };
    let automated_headers = headers.as_ref().is_some_and(|headers| automated(headers));
    let (message_id, parents, from, mut to, cc) = match &headers {
        Some(headers) if !headers.is_empty() => {
            let mut parents = message_ids(headers.get_first_value("References"));
            parents.extend(message_ids(headers.get_first_value("In-Reply-To")));
            (
                message_ids(headers.get_first_value("Message-ID"))
                    .into_iter()
                    .next(),
                parents,
                addresses(headers.get_first_value("From")),
                addresses(headers.get_first_value("To")),
                addresses(headers.get_first_value("Cc")),
            )
        }
        // 只有摘要的快取沒有原始標頭：寄件者取自 subtitle，其餘地址都視為收件人。
        _ => (
            None,
            Vec::new(),
            addresses(Some(item.subtitle.clone())),
            Vec::new(),
            Vec::new(),
        ),
    };
    let from = from.into_iter().next().unwrap_or_default();
    let automated = automated_headers || automated_address(&from);
    if headers.as_ref().is_none_or(|headers| headers.is_empty()) {
        to = item
            .emails
            .iter()
            .filter(|email| **email != from)
            .cloned()
            .collect();
    }
    Some(Message {
        id: item.id,
        message_id,
        parents,
        subject: item.title,
        from,
        to,
        cc,
        date,
        answered: item
            .flags
            .iter()
            .any(|flag| flag.eq_ignore_ascii_case("\\Answered")),
        automated,
        text: item.text,
        names: names.into_iter().collect(),
    })
}

/// RFC 3834 Auto-Submitted、郵遞清單與大量寄送標頭。
fn automated(headers: &[mailparse::MailHeader<'_>]) -> bool {
    let value = |name: &str| {
        headers
            .get_first_value(name)
            .map(|value| value.trim().to_ascii_lowercase())
    };
    value("Auto-Submitted").is_some_and(|value| !value.is_empty() && value != "no")
        || value("Precedence")
            .is_some_and(|value| matches!(value.as_str(), "bulk" | "list" | "junk"))
        || [
            "List-Id",
            "List-Unsubscribe",
            "X-Auto-Response-Suppress",
            "Feedback-ID",
        ]
        .iter()
        .any(|name| headers.get_first_value(name).is_some())
}

/// 沒有標頭可判斷時，看地址的 local part 是否為常見系統帳號。
fn automated_address(email: &str) -> bool {
    let local = email.split('@').next().unwrap_or_default();
    let compact = local
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>();
    // `kmbot` 算；`j.talbot` 這類有分隔符的人名不算。
    let bot = compact.ends_with("bot") && !local.contains(['.', '_', '-']);
    compact.contains("noreply")
        || compact.contains("donotreply")
        || bot
        || matches!(
            compact.trim_end_matches(|c: char| c.is_ascii_digit()),
            "mailerdaemon"
                | "postmaster"
                | "notification"
                | "notifications"
                | "notify"
                | "alert"
                | "alerts"
                | "newsletter"
                | "news"
                | "edm"
                | "bounce"
                | "bounces"
        )
}

fn listed_sender(email: &str, senders: &[String]) -> bool {
    let (local, domain) = email.split_once('@').unwrap_or((email, ""));
    senders.iter().any(|sender| {
        let sender = sender.trim().to_lowercase();
        match sender.strip_prefix('@') {
            Some(listed_domain) => !listed_domain.is_empty() && listed_domain == domain,
            None if sender.contains('@') => sender == email,
            None => !sender.is_empty() && sender == local,
        }
    })
}

fn collect_addresses(value: &str, names: &mut HashMap<String, String>) -> Vec<String> {
    let Ok(list) = mailparse::addrparse(&provider::mail::display_header(value)) else {
        return Vec::new();
    };
    let mut output = Vec::new();
    let mut push = |single: &mailparse::SingleInfo| {
        let email = single.addr.trim().to_lowercase();
        if email.is_empty() {
            return;
        }
        if let Some(name) = single.display_name.as_deref().map(str::trim) {
            if !name.is_empty() {
                names
                    .entry(email.clone())
                    .or_insert_with(|| name.to_owned());
            }
        }
        output.push(email);
    };
    for address in list.iter() {
        match address {
            MailAddr::Single(single) => push(single),
            MailAddr::Group(group) => group.addrs.iter().for_each(&mut push),
        }
    }
    output
}

fn message_ids(value: Option<String>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|c| c == '<' || c == '>' || c == ',')
                .to_lowercase()
        })
        .filter(|token| token.contains('@'))
        .collect()
}

/// 以 Message-ID／References 與正規化主旨（同 `mail thread`）做聯集，回傳每串的訊息索引（新到舊）。
fn group_threads(messages: &[Message]) -> Vec<Vec<usize>> {
    let mut parent = (0..messages.len()).collect::<Vec<_>>();
    fn find(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            parent[index] = parent[parent[index]];
            index = parent[index];
        }
        index
    }
    let mut owners = HashMap::<String, usize>::new();
    for (index, message) in messages.iter().enumerate() {
        let subject = normalize_thread_subject(&message.subject);
        let keys = message
            .message_id
            .iter()
            .chain(&message.parents)
            .map(|id| format!("id:{id}"))
            .chain(
                (!subject.is_empty() && subject != "(無主旨)")
                    .then(|| format!("subject:{subject}")),
            );
        for key in keys {
            let owner = *owners.entry(key).or_insert(index);
            let (a, b) = (find(&mut parent, owner), find(&mut parent, index));
            if a != b {
                parent[a.max(b)] = a.min(b);
            }
        }
    }
    let mut groups = BTreeMap::<usize, Vec<usize>>::new();
    for index in 0..messages.len() {
        let root = find(&mut parent, index);
        groups.entry(root).or_default().push(index);
    }
    groups.into_values().collect()
}

fn thread(
    messages: &[Message],
    group: &[usize],
    me: &str,
    names: &HashMap<String, String>,
    excerpt_chars: usize,
) -> Thread {
    let latest = &messages[group[0]];
    let earliest = &messages[*group.last().unwrap()];
    let mut participants = BTreeSet::new();
    for &index in group {
        participants.extend(
            messages[index]
                .participants()
                .filter(|email| *email != me)
                .cloned(),
        );
    }
    let status = if latest.from == me {
        ThreadStatus::AwaitingThem
    } else if latest.answered {
        ThreadStatus::Replied
    } else if !latest.automated && latest.to.iter().any(|email| email == me) {
        ThreadStatus::AwaitingMe
    } else {
        ThreadStatus::Fyi
    };
    Thread {
        subject: earliest.subject.clone(),
        status,
        message_count: group.len(),
        participants: participants.into_iter().collect(),
        first: earliest.date.to_rfc3339(),
        last: latest.date.to_rfc3339(),
        last_from: label(&latest.from, names),
        ids: group
            .iter()
            .map(|&index| messages[index].id.clone())
            .collect(),
        latest_excerpt: excerpt(&mail_read::latest_reply(&latest.text), excerpt_chars),
    }
}

fn people(
    messages: &[Message],
    groups: &[Vec<usize>],
    kept: &BTreeSet<usize>,
    me: &str,
    names: &HashMap<String, String>,
) -> Vec<Person> {
    #[derive(Default)]
    struct Tally {
        messages: usize,
        from_them: usize,
        automated_from_them: usize,
        from_me: usize,
        threads: BTreeSet<usize>,
        last: Option<DateTime<Utc>>,
        subjects: Vec<String>,
        with: BTreeMap<String, usize>,
    }
    let thread_of = groups
        .iter()
        .enumerate()
        .flat_map(|(thread, group)| group.iter().map(move |&index| (index, thread)))
        .collect::<HashMap<_, _>>();
    let mut tallies = BTreeMap::<String, Tally>::new();
    // `kept` 由小到大即由新到舊，最近主旨會先被收集。
    for &index in kept {
        let message = &messages[index];
        let others = message
            .participants()
            .filter(|email| *email != me)
            .collect::<BTreeSet<_>>();
        for &email in &others {
            let tally = tallies.entry(email.clone()).or_default();
            tally.messages += 1;
            if message.from == *email {
                tally.from_them += 1;
                tally.automated_from_them += usize::from(message.automated);
            } else if message.from == me {
                tally.from_me += 1;
            }
            tally.threads.insert(thread_of[&index]);
            tally.last = tally.last.max(Some(message.date));
            let subject = normalize_thread_subject(&message.subject);
            if tally.subjects.len() < 3
                && !tally
                    .subjects
                    .iter()
                    .any(|existing| normalize_thread_subject(existing) == subject)
            {
                tally.subjects.push(message.subject.clone());
            }
            for &other in &others {
                if other != email {
                    *tally.with.entry(other.clone()).or_default() += 1;
                }
            }
        }
    }
    let people = tallies
        .into_iter()
        .map(|(email, tally)| {
            let mut with = tally.with.into_iter().collect::<Vec<_>>();
            with.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Person {
                name: names.get(&email).cloned(),
                messages: tally.messages,
                from_them: tally.from_them,
                from_me: tally.from_me,
                threads: tally.threads.len(),
                last_contact: tally.last.map(|date| date.to_rfc3339()).unwrap_or_default(),
                recent_subjects: tally.subjects,
                often_with: with.into_iter().take(3).map(|(email, _)| email).collect(),
                automated: automated_address(&email)
                    || (tally.from_them > 0 && tally.automated_from_them == tally.from_them),
                email,
            }
        })
        .collect::<Vec<_>>();
    people
}

fn sort_people(people: &mut [Person]) {
    people.sort_by(|a, b| {
        a.automated
            .cmp(&b.automated)
            .then_with(|| b.messages.cmp(&a.messages))
            .then_with(|| b.last_contact.cmp(&a.last_contact))
            .then_with(|| a.email.cmp(&b.email))
    });
}

fn label(email: &str, names: &HashMap<String, String>) -> String {
    match names.get(email) {
        Some(name) => format!("{name} <{email}>"),
        None if email.is_empty() => "(未知寄件者)".to_owned(),
        None => email.to_owned(),
    }
}

fn excerpt(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let mut cut = flat.chars().take(limit).collect::<String>();
    cut.push('…');
    cut
}

fn local_time(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|date| {
            date.with_timezone(&chrono_tz::Asia::Taipei)
                .format("%Y/%m/%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_owned())
}

fn cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

/// 給代理程式讀的 Markdown；時間以台北時區顯示。
pub fn render_markdown(graph: &MailGraph) -> String {
    let mut out = format!(
        "# 郵件關係圖\n\n- 帳號：{}\n- 範圍：{} ～ {}（近 {} 天，台北時間）\n- 郵件：{} 封，聯絡人 {} 位，信件串 {} 個\n",
        graph.me,
        local_time(&graph.since),
        local_time(&graph.until),
        graph.days,
        graph.message_count,
        graph.people.len(),
        graph.threads.len(),
    );
    if let Some(person) = &graph.person {
        out.push_str(&format!("- 篩選：{person}\n"));
    }
    out.push_str("\n## 聯絡人\n\n");
    if graph.people.is_empty() {
        out.push_str("（無）\n");
    } else {
        out.push_str("| 聯絡人 | 往來 | 對方寄 | 我寄 | 串 | 最近 | 近期主旨 | 常同框 |\n|---|---|---|---|---|---|---|---|\n");
        for person in &graph.people {
            let mut who = match &person.name {
                Some(name) => format!("{name} <{}>", person.email),
                None => person.email.clone(),
            };
            if person.automated {
                who.push_str("（自動）");
            }
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                cell(&who),
                person.messages,
                person.from_them,
                person.from_me,
                person.threads,
                local_time(&person.last_contact),
                cell(&person.recent_subjects.join("；")),
                cell(&person.often_with.join(", ")),
            ));
        }
    }
    out.push_str("\n## 信件串（新到舊）\n");
    if graph.threads.is_empty() {
        out.push_str("\n（無）\n");
    }
    for (index, thread) in graph.threads.iter().enumerate() {
        let status = match thread.status {
            ThreadStatus::AwaitingMe => "待我回覆",
            ThreadStatus::AwaitingThem => "等對方回覆",
            ThreadStatus::Replied => "已回覆",
            ThreadStatus::Fyi => "知會",
        };
        out.push_str(&format!(
            "\n### {}. {}〔{status}〕\n\n- 期間：{} → {}，共 {} 封；最後寄件：{}\n- 參與者：{}\n- 郵件 ID：{}\n",
            index + 1,
            thread.subject,
            local_time(&thread.first),
            local_time(&thread.last),
            thread.message_count,
            thread.last_from,
            thread.participants.join(", "),
            thread.ids.join(" "),
        ));
        if !thread.latest_excerpt.is_empty() {
            out.push_str(&format!("- 最新回覆：{}\n", thread.latest_excerpt));
        }
    }
    out
}
