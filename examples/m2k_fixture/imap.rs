//! Stateful IMAP4rev1 subset: exactly what m2k sync, search, show, flag,
//! move, delete and watch (IDLE) send. Not a general-purpose server.

use super::net::{is_timeout, LineConn};
use super::state::{same_mailbox, Fixture, Mailbox, Message};
use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::json;
use std::{collections::BTreeSet, net::TcpStream, sync::Arc, time::Duration};

type Conn = LineConn<native_tls::TlsStream<TcpStream>>;

const CAPABILITIES: &str = "IMAP4rev1 LITERAL+ MOVE UIDPLUS IDLE UNSELECT";

struct Session {
    authenticated: bool,
    selected: Option<Selected>,
}

struct Selected {
    name: String,
    read_only: bool,
    exists: usize,
}

enum Flow {
    Continue,
    Logout,
}

pub fn serve(fixture: Arc<Fixture>, stream: native_tls::TlsStream<TcpStream>) -> Result<()> {
    let mut conn = LineConn::new(stream);
    conn.write(format!(
        "* OK [CAPABILITY {CAPABILITIES}] m2k fixture IMAP ready\r\n"
    ))?;
    let mut session = Session {
        authenticated: false,
        selected: None,
    };
    loop {
        let Some((line, literals)) = read_command(&mut conn)? else {
            return Ok(());
        };
        let Some((tag, rest)) = line.split_once(' ') else {
            conn.write("* BAD missing command\r\n")?;
            continue;
        };
        let args = tokenize(rest);
        let result = handle(&fixture, &mut conn, &mut session, tag, &args, &literals);
        match result {
            Ok(Flow::Continue) => {}
            Ok(Flow::Logout) => return Ok(()),
            Err(error) => conn.write(format!("{tag} NO {error}\r\n"))?,
        }
    }
}

/// Reads one command, inlining literals as `\0<index>\0` placeholders.
fn read_command(conn: &mut Conn) -> Result<Option<(String, Vec<Vec<u8>>)>> {
    let mut line = String::new();
    let mut literals = Vec::new();
    loop {
        let Some(part) = conn.read_line()? else {
            return Ok(None);
        };
        let part = String::from_utf8_lossy(&part).into_owned();
        let literal = part
            .strip_suffix('}')
            .and_then(|head| head.rsplit_once('{'))
            .and_then(|(head, count)| {
                let (count, plus) = match count.strip_suffix('+') {
                    Some(count) => (count, true),
                    None => (count, false),
                };
                count.parse::<usize>().ok().map(|count| (head, count, plus))
            });
        match literal {
            Some((head, count, plus)) => {
                if !plus {
                    conn.write("+ go ahead\r\n")?;
                }
                line.push_str(head);
                line.push_str(&format!("\0{}\0", literals.len()));
                literals.push(conn.read_bytes(count)?);
            }
            None => {
                line.push_str(&part);
                return Ok(Some((line, literals)));
            }
        }
    }
}

/// Splits arguments; quoted strings are unquoted, parenthesized lists and
/// bracketed sections (with spaces) stay raw.
fn tokenize(input: &str) -> Vec<String> {
    let chars = input.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == ' ' {
            index += 1;
            continue;
        }
        if chars[index] == '"' {
            let mut value = String::new();
            index += 1;
            while index < chars.len() && chars[index] != '"' {
                if chars[index] == '\\' && index + 1 < chars.len() {
                    index += 1;
                }
                value.push(chars[index]);
                index += 1;
            }
            index += 1;
            tokens.push(value);
            continue;
        }
        let start = index;
        let mut depth = 0_i32;
        let mut quoted = false;
        while index < chars.len() {
            let c = chars[index];
            if quoted {
                if c == '\\' {
                    index += 1;
                } else if c == '"' {
                    quoted = false;
                }
            } else if c == '"' {
                quoted = true;
            } else if c == '(' || c == '[' {
                depth += 1;
            } else if c == ')' || c == ']' {
                depth -= 1;
            } else if c == ' ' && depth <= 0 {
                break;
            }
            index += 1;
        }
        tokens.push(chars[start..index].iter().collect());
    }
    tokens
}

fn literal_index(token: &str) -> Option<usize> {
    token
        .strip_prefix('\0')
        .and_then(|rest| rest.strip_suffix('\0'))
        .and_then(|index| index.parse().ok())
}

fn text_arg(token: &str, literals: &[Vec<u8>]) -> String {
    match literal_index(token) {
        Some(index) => String::from_utf8_lossy(&literals[index]).into_owned(),
        None => token.to_owned(),
    }
}

fn arg<'a>(args: &'a [String], index: usize, name: &str) -> Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing {name}"))
}

fn handle(
    fixture: &Fixture,
    conn: &mut Conn,
    session: &mut Session,
    tag: &str,
    args: &[String],
    literals: &[Vec<u8>],
) -> Result<Flow> {
    let command = arg(args, 0, "command")?.to_ascii_uppercase();
    match command.as_str() {
        "CAPABILITY" => {
            conn.write(format!("* CAPABILITY {CAPABILITIES}\r\n{tag} OK done\r\n"))?;
            return Ok(Flow::Continue);
        }
        "NOOP" | "CHECK" => {
            notify_exists(fixture, conn, session)?;
            conn.write(format!("{tag} OK done\r\n"))?;
            return Ok(Flow::Continue);
        }
        "LOGOUT" => {
            conn.write(format!("* BYE fixture closing\r\n{tag} OK done\r\n"))?;
            return Ok(Flow::Logout);
        }
        "LOGIN" => {
            let user = text_arg(arg(args, 1, "user")?, literals);
            let password = text_arg(arg(args, 2, "password")?, literals);
            if user != fixture.user || password != fixture.password {
                conn.write(format!(
                    "{tag} NO [AUTHENTICATIONFAILED] invalid credentials\r\n"
                ))?;
            } else {
                session.authenticated = true;
                conn.write(format!(
                    "{tag} OK [CAPABILITY {CAPABILITIES}] logged in\r\n"
                ))?;
            }
            return Ok(Flow::Continue);
        }
        _ => {}
    }
    if !session.authenticated {
        bail!("not authenticated");
    }
    match command.as_str() {
        "LIST" | "LSUB" => list(fixture, conn, tag, &command, args, literals)?,
        "STATUS" => status(fixture, conn, tag, args, literals)?,
        "SELECT" | "EXAMINE" => select(fixture, conn, session, tag, &command, args, literals)?,
        "CREATE" => {
            let name = text_arg(arg(args, 1, "mailbox")?, literals);
            let mut mail = fixture.mail.lock().expect("mail lock");
            if mail.get(&name).is_none() {
                let validity = mail.boxes.iter().map(|b| b.uid_validity).max().unwrap_or(1) + 1;
                mail.boxes.push(Mailbox {
                    name,
                    special: None,
                    uid_validity: validity,
                    uid_next: 1,
                    messages: Vec::new(),
                });
            }
            conn.write(format!("{tag} OK created\r\n"))?;
        }
        "APPEND" => append(fixture, conn, tag, args, literals)?,
        "IDLE" => idle(fixture, conn, session, tag)?,
        "CLOSE" | "UNSELECT" => {
            if command == "CLOSE" {
                if let Some(selected) = session.selected.as_ref().filter(|s| !s.read_only) {
                    expunge(fixture, None, &selected.name, None)?;
                }
            }
            session.selected = None;
            conn.write(format!("{tag} OK closed\r\n"))?;
        }
        "UID" => {
            let sub = arg(args, 1, "UID command")?.to_ascii_uppercase();
            selected_command(
                fixture,
                conn,
                session,
                tag,
                &sub,
                &args[2..],
                literals,
                true,
            )?;
        }
        _ => selected_command(
            fixture,
            conn,
            session,
            tag,
            &command,
            &args[1..],
            literals,
            false,
        )?,
    }
    Ok(Flow::Continue)
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn list(
    fixture: &Fixture,
    conn: &mut Conn,
    tag: &str,
    command: &str,
    args: &[String],
    literals: &[Vec<u8>],
) -> Result<()> {
    let pattern = text_arg(arg(args, 2, "pattern")?, literals);
    let mut out = String::new();
    if pattern.is_empty() {
        out.push_str(&format!("* {command} (\\Noselect) \"/\" \"\"\r\n"));
    } else {
        let mail = fixture.mail.lock().expect("mail lock");
        for mailbox in &mail.boxes {
            let mut attributes = vec!["\\HasNoChildren"];
            attributes.extend(mailbox.special);
            out.push_str(&format!(
                "* {command} ({}) \"/\" {}\r\n",
                attributes.join(" "),
                quote(&mailbox.name)
            ));
        }
    }
    out.push_str(&format!("{tag} OK done\r\n"));
    conn.write(out)?;
    Ok(())
}

fn status(
    fixture: &Fixture,
    conn: &mut Conn,
    tag: &str,
    args: &[String],
    literals: &[Vec<u8>],
) -> Result<()> {
    let name = text_arg(arg(args, 1, "mailbox")?, literals);
    let mail = fixture.mail.lock().expect("mail lock");
    let mailbox = mail.get(&name).ok_or_else(|| anyhow!("no such mailbox"))?;
    let unseen = mailbox
        .messages
        .iter()
        .filter(|m| !m.flags.contains("\\Seen"))
        .count();
    conn.write(format!(
        "* STATUS {} (MESSAGES {} UIDNEXT {} UIDVALIDITY {} UNSEEN {unseen})\r\n{tag} OK done\r\n",
        quote(&mailbox.name),
        mailbox.messages.len(),
        mailbox.uid_next,
        mailbox.uid_validity
    ))?;
    Ok(())
}

fn select(
    fixture: &Fixture,
    conn: &mut Conn,
    session: &mut Session,
    tag: &str,
    command: &str,
    args: &[String],
    literals: &[Vec<u8>],
) -> Result<()> {
    let name = text_arg(arg(args, 1, "mailbox")?, literals);
    let mail = fixture.mail.lock().expect("mail lock");
    let Some(mailbox) = mail.get(&name) else {
        session.selected = None;
        bail!("no such mailbox");
    };
    let read_only = command == "EXAMINE";
    session.selected = Some(Selected {
        name: mailbox.name.clone(),
        read_only,
        exists: mailbox.messages.len(),
    });
    conn.write(format!(
        "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n* {} EXISTS\r\n* 0 RECENT\r\n* OK [UIDVALIDITY {}] ok\r\n* OK [UIDNEXT {}] ok\r\n* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)] ok\r\n{tag} OK [{}] {command} done\r\n",
        mailbox.messages.len(),
        mailbox.uid_validity,
        mailbox.uid_next,
        if read_only { "READ-ONLY" } else { "READ-WRITE" }
    ))?;
    Ok(())
}

fn notify_exists(fixture: &Fixture, conn: &mut Conn, session: &mut Session) -> Result<()> {
    let Some(selected) = session.selected.as_mut() else {
        return Ok(());
    };
    let mail = fixture.mail.lock().expect("mail lock");
    let count = mail.get(&selected.name).map_or(0, |m| m.messages.len());
    drop(mail);
    if count != selected.exists {
        selected.exists = count;
        conn.write(format!("* {count} EXISTS\r\n"))?;
    }
    Ok(())
}

fn idle(fixture: &Fixture, conn: &mut Conn, session: &mut Session, tag: &str) -> Result<()> {
    conn.write("+ idling\r\n")?;
    conn.stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(250)))?;
    let result = loop {
        match conn.read_line() {
            Ok(Some(line)) if line.eq_ignore_ascii_case(b"DONE") => break Ok(()),
            Ok(Some(_)) => {}
            Ok(None) => break Err(anyhow!("client closed during IDLE")),
            Err(error) if is_timeout(&error) => notify_exists(fixture, conn, session)?,
            Err(error) => break Err(error.into()),
        }
    };
    conn.stream.get_ref().set_read_timeout(None)?;
    result?;
    conn.write(format!("{tag} OK IDLE terminated\r\n"))?;
    Ok(())
}

fn append(
    fixture: &Fixture,
    conn: &mut Conn,
    tag: &str,
    args: &[String],
    literals: &[Vec<u8>],
) -> Result<()> {
    let name = text_arg(arg(args, 1, "mailbox")?, literals);
    let mut flags = BTreeSet::new();
    let mut raw = None;
    for token in &args[2..] {
        if let Some(index) = literal_index(token) {
            raw = Some(literals[index].clone());
        } else if let Some(list) = token.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
            flags.extend(list.split_whitespace().map(ToOwned::to_owned));
        }
    }
    let raw = raw.ok_or_else(|| anyhow!("APPEND needs a literal"))?;
    let mut mail = fixture.mail.lock().expect("mail lock");
    let mailbox = mail
        .get_mut(&name)
        .ok_or_else(|| anyhow!("[TRYCREATE] no such mailbox"))?;
    let uid = mailbox.append(raw.clone(), flags, Utc::now());
    let validity = mailbox.uid_validity;
    drop(mail);
    fixture.record(
        "imap.append",
        json!({"folder": name, "uid": uid}),
        Some(("imap", "append.eml", &raw)),
    );
    conn.write(format!(
        "{tag} OK [APPENDUID {validity} {uid}] appended\r\n"
    ))?;
    Ok(())
}

/// Parses `1,3:5,7:*` against the largest value in scope.
fn parse_set(set: &str, max: u32) -> Result<Vec<(u32, u32)>> {
    let value = |part: &str| -> Result<u32> {
        if part == "*" {
            Ok(max)
        } else {
            part.parse().map_err(|_| anyhow!("bad sequence set {set}"))
        }
    };
    set.split(',')
        .map(|part| match part.split_once(':') {
            Some((a, b)) => {
                let (a, b) = (value(a)?, value(b)?);
                Ok((a.min(b), a.max(b)))
            }
            None => {
                let v = value(part)?;
                Ok((v, v))
            }
        })
        .collect()
}

fn in_set(ranges: &[(u32, u32)], value: u32) -> bool {
    ranges.iter().any(|(a, b)| (*a..=*b).contains(&value))
}

fn looks_like_set(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ':' | ',' | '*'))
}

/// Indices (0-based) of messages addressed by a UID or sequence set.
fn addressed(mailbox: &Mailbox, set: &str, uid: bool) -> Result<Vec<usize>> {
    let max = if uid {
        mailbox.messages.last().map_or(0, |m| m.uid)
    } else {
        mailbox.messages.len() as u32
    };
    let ranges = parse_set(set, max)?;
    Ok(mailbox
        .messages
        .iter()
        .enumerate()
        .filter(|(index, message)| {
            let key = if uid { message.uid } else { *index as u32 + 1 };
            in_set(&ranges, key)
        })
        .map(|(index, _)| index)
        .collect())
}

#[allow(clippy::too_many_arguments)]
fn selected_command(
    fixture: &Fixture,
    conn: &mut Conn,
    session: &mut Session,
    tag: &str,
    command: &str,
    args: &[String],
    literals: &[Vec<u8>],
    uid: bool,
) -> Result<()> {
    let selected = session
        .selected
        .as_ref()
        .ok_or_else(|| anyhow!("no mailbox selected"))?;
    let name = selected.name.clone();
    let read_only = selected.read_only;
    match command {
        "SEARCH" => {
            let mail = fixture.mail.lock().expect("mail lock");
            let mailbox = mail.get(&name).ok_or_else(|| anyhow!("mailbox vanished"))?;
            let hits = search(mailbox, args, literals)?;
            let values = hits
                .into_iter()
                .map(|index| {
                    if uid {
                        mailbox.messages[index].uid
                    } else {
                        index as u32 + 1
                    }
                })
                .map(|value| value.to_string())
                .collect::<Vec<_>>();
            drop(mail);
            let line = if values.is_empty() {
                "* SEARCH\r\n".to_owned()
            } else {
                format!("* SEARCH {}\r\n", values.join(" "))
            };
            conn.write(format!("{line}{tag} OK search done\r\n"))?;
        }
        "FETCH" => {
            let set = arg(args, 0, "set")?;
            let items = fetch_items(arg(args, 1, "items")?, uid)?;
            let mut mail = fixture.mail.lock().expect("mail lock");
            let mailbox = mail
                .get_mut(&name)
                .ok_or_else(|| anyhow!("mailbox vanished"))?;
            let mut out = Vec::new();
            for index in addressed(mailbox, set, uid)? {
                let message = &mut mailbox.messages[index];
                if !read_only && items.iter().any(|item| item.marks_seen()) {
                    message.flags.insert("\\Seen".to_owned());
                }
                out.extend(fetch_response(index + 1, message, &items));
            }
            drop(mail);
            out.extend(format!("{tag} OK fetch done\r\n").into_bytes());
            conn.write(out)?;
        }
        "STORE" => {
            if read_only {
                bail!("mailbox is read-only");
            }
            let set = arg(args, 0, "set")?;
            let operation = arg(args, 1, "operation")?.to_ascii_uppercase();
            let flags = args[2..]
                .join(" ")
                .trim_matches(|c| c == '(' || c == ')')
                .split_whitespace()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            let silent = operation.ends_with(".SILENT");
            let mut mail = fixture.mail.lock().expect("mail lock");
            let mailbox = mail
                .get_mut(&name)
                .ok_or_else(|| anyhow!("mailbox vanished"))?;
            let mut out = String::new();
            let mut touched = Vec::new();
            for index in addressed(mailbox, set, uid)? {
                let message = &mut mailbox.messages[index];
                match operation.trim_end_matches(".SILENT") {
                    "+FLAGS" => message.flags.extend(flags.iter().cloned()),
                    "-FLAGS" => message.flags.retain(|flag| !flags.contains(flag)),
                    "FLAGS" => message.flags = flags.iter().cloned().collect(),
                    other => bail!("unsupported STORE {other}"),
                }
                touched.push(message.uid);
                if !silent {
                    out.push_str(&format!(
                        "* {} FETCH (UID {} FLAGS ({}))\r\n",
                        index + 1,
                        message.uid,
                        flag_list(message)
                    ));
                }
            }
            drop(mail);
            fixture.record(
                "imap.store",
                json!({"folder": name, "uids": touched, "operation": operation, "flags": flags}),
                None,
            );
            conn.write(format!("{out}{tag} OK store done\r\n"))?;
        }
        "COPY" | "MOVE" => {
            if read_only && command == "MOVE" {
                bail!("mailbox is read-only");
            }
            let set = arg(args, 0, "set")?;
            let target = text_arg(arg(args, 1, "mailbox")?, literals);
            if same_mailbox(&name, &target) {
                bail!("source and target are the same");
            }
            let mut mail = fixture.mail.lock().expect("mail lock");
            if mail.get(&target).is_none() {
                bail!("[TRYCREATE] no such mailbox");
            }
            let source = mail
                .get_mut(&name)
                .ok_or_else(|| anyhow!("mailbox vanished"))?;
            let indices = addressed(source, set, uid)?;
            let copies = indices
                .iter()
                .map(|&index| {
                    let m = &source.messages[index];
                    (m.uid, m.raw.clone(), m.flags.clone(), m.internal_date)
                })
                .collect::<Vec<_>>();
            let mut expunged = Vec::new();
            if command == "MOVE" {
                for &index in indices.iter().rev() {
                    source.messages.remove(index);
                    expunged.push(index + 1);
                }
            }
            let destination = mail.get_mut(&target).expect("target checked");
            let validity = destination.uid_validity;
            let mut source_uids = Vec::new();
            let mut target_uids = Vec::new();
            for (source_uid, raw, flags, date) in copies {
                source_uids.push(source_uid.to_string());
                target_uids.push(destination.append(raw, flags, date).to_string());
            }
            drop(mail);
            if let Some(selected) = session.selected.as_mut() {
                selected.exists -= expunged.len();
            }
            fixture.record(
                &format!("imap.{}", command.to_ascii_lowercase()),
                json!({"from": name, "to": target, "uids": source_uids, "new_uids": target_uids}),
                None,
            );
            let mut out = String::new();
            if !source_uids.is_empty() {
                out.push_str(&format!(
                    "* OK [COPYUID {validity} {} {}] done\r\n",
                    source_uids.join(","),
                    target_uids.join(",")
                ));
            }
            for seq in expunged {
                out.push_str(&format!("* {seq} EXPUNGE\r\n"));
            }
            conn.write(format!("{out}{tag} OK {command} done\r\n"))?;
        }
        "EXPUNGE" => {
            if read_only {
                bail!("mailbox is read-only");
            }
            let uid_set = if uid {
                Some(arg(args, 0, "set")?)
            } else {
                None
            };
            let removed = expunge(fixture, Some(conn), &name, uid_set)?;
            if let Some(selected) = session.selected.as_mut() {
                selected.exists -= removed;
            }
            conn.write(format!("{tag} OK expunge done\r\n"))?;
        }
        other => bail!("unsupported command {other}"),
    }
    Ok(())
}

fn expunge(
    fixture: &Fixture,
    mut conn: Option<&mut Conn>,
    name: &str,
    uid_set: Option<&str>,
) -> Result<usize> {
    let mut mail = fixture.mail.lock().expect("mail lock");
    let mailbox = mail
        .get_mut(name)
        .ok_or_else(|| anyhow!("mailbox vanished"))?;
    let scope = match uid_set {
        Some(set) => Some(addressed(mailbox, set, true)?),
        None => None,
    };
    let mut removed = Vec::new();
    let mut out = String::new();
    let mut index = 0;
    let mut original = 0;
    while index < mailbox.messages.len() {
        let message = &mailbox.messages[index];
        let in_scope = scope.as_ref().is_none_or(|s| s.contains(&original));
        if in_scope && message.flags.contains("\\Deleted") {
            removed.push(message.uid);
            out.push_str(&format!("* {} EXPUNGE\r\n", index + 1));
            mailbox.messages.remove(index);
        } else {
            index += 1;
        }
        original += 1;
    }
    drop(mail);
    if !removed.is_empty() {
        fixture.record(
            "imap.expunge",
            json!({"folder": name, "uids": removed}),
            None,
        );
    }
    if let Some(conn) = conn.as_mut() {
        conn.write(out)?;
    }
    Ok(removed.len())
}

fn search(mailbox: &Mailbox, args: &[String], literals: &[Vec<u8>]) -> Result<Vec<usize>> {
    let mut hits = (0..mailbox.messages.len()).collect::<Vec<_>>();
    let mut index = 0;
    let next = |index: &mut usize| -> Result<String> {
        *index += 1;
        args.get(*index)
            .map(|token| text_arg(token, literals))
            .ok_or_else(|| anyhow!("search criterion needs a value"))
    };
    while index < args.len() {
        let key = args[index].to_ascii_uppercase();
        let messages = &mailbox.messages;
        match key.as_str() {
            "ALL" => {}
            "CHARSET" => {
                next(&mut index)?;
            }
            "UID" => {
                let set = next(&mut index)?;
                let max = messages.last().map_or(0, |m| m.uid);
                let ranges = parse_set(&set, max)?;
                hits.retain(|&i| in_set(&ranges, messages[i].uid));
            }
            "SINCE" | "BEFORE" | "ON" => {
                let date = NaiveDate::parse_from_str(&next(&mut index)?, "%d-%b-%Y")
                    .map_err(|_| anyhow!("bad search date"))?;
                hits.retain(|&i| {
                    let day = messages[i].internal_date.date_naive();
                    match key.as_str() {
                        "SINCE" => day >= date,
                        "BEFORE" => day < date,
                        _ => day == date,
                    }
                });
            }
            "TEXT" | "BODY" | "SUBJECT" | "FROM" | "TO" => {
                let needle = next(&mut index)?.to_lowercase();
                hits.retain(|&i| {
                    decoded_text(&messages[i].raw)
                        .to_lowercase()
                        .contains(&needle)
                });
            }
            "SEEN" | "UNSEEN" | "FLAGGED" | "UNFLAGGED" | "DELETED" | "UNDELETED" => {
                let (flag, wanted) = match key.as_str() {
                    "SEEN" => ("\\Seen", true),
                    "UNSEEN" => ("\\Seen", false),
                    "FLAGGED" => ("\\Flagged", true),
                    "UNFLAGGED" => ("\\Flagged", false),
                    "DELETED" => ("\\Deleted", true),
                    _ => ("\\Deleted", false),
                };
                hits.retain(|&i| messages[i].flags.contains(flag) == wanted);
            }
            token if looks_like_set(token) => {
                let ranges = parse_set(token, messages.len() as u32)?;
                hits.retain(|&i| in_set(&ranges, i as u32 + 1));
            }
            other => bail!("unsupported SEARCH key {other}"),
        }
        index += 1;
    }
    Ok(hits)
}

/// Raw message plus decoded headers, so TEXT search finds UTF-8 subjects.
fn decoded_text(raw: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(raw).into_owned();
    if let Ok(parsed) = mailparse::parse_mail(raw) {
        for header in &parsed.headers {
            text.push('\n');
            text.push_str(&header.get_value());
        }
    }
    text
}

enum FetchItem {
    Uid,
    Flags,
    InternalDate,
    Size,
    Section {
        label: String,
        section: String,
        peek: bool,
    },
}

impl FetchItem {
    fn marks_seen(&self) -> bool {
        matches!(self, FetchItem::Section { peek: false, .. })
    }
}

fn fetch_items(spec: &str, uid: bool) -> Result<Vec<FetchItem>> {
    let inner = spec
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(spec);
    let mut items = Vec::new();
    if uid {
        items.push(FetchItem::Uid);
    }
    for token in tokenize(inner) {
        let upper = token.to_ascii_uppercase();
        match upper.as_str() {
            "UID" => {
                if !uid {
                    items.push(FetchItem::Uid);
                }
            }
            "FLAGS" => items.push(FetchItem::Flags),
            "INTERNALDATE" => items.push(FetchItem::InternalDate),
            "RFC822.SIZE" => items.push(FetchItem::Size),
            "FAST" | "ALL" | "FULL" => {
                items.extend([FetchItem::Flags, FetchItem::InternalDate, FetchItem::Size])
            }
            "RFC822" | "RFC822.HEADER" | "RFC822.TEXT" => items.push(FetchItem::Section {
                label: upper.clone(),
                section: match upper.as_str() {
                    "RFC822" => String::new(),
                    "RFC822.HEADER" => "HEADER".to_owned(),
                    _ => "TEXT".to_owned(),
                },
                peek: upper == "RFC822.HEADER",
            }),
            _ => {
                let (peek, rest) = if let Some(rest) = upper.strip_prefix("BODY.PEEK[") {
                    (true, rest)
                } else if let Some(rest) = upper.strip_prefix("BODY[") {
                    (false, rest)
                } else {
                    bail!("unsupported FETCH item {token}");
                };
                let section = rest
                    .strip_suffix(']')
                    .ok_or_else(|| anyhow!("partial FETCH is not supported: {token}"))?;
                items.push(FetchItem::Section {
                    label: format!("BODY[{section}]"),
                    section: section.to_owned(),
                    peek,
                });
            }
        }
    }
    Ok(items)
}

fn flag_list(message: &Message) -> String {
    message.flags.iter().cloned().collect::<Vec<_>>().join(" ")
}

fn split_header(raw: &[u8]) -> usize {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(raw.len())
}

fn section_bytes(raw: &[u8], section: &str) -> Vec<u8> {
    let header_end = split_header(raw);
    match section {
        "" => raw.to_vec(),
        "HEADER" => raw[..header_end].to_vec(),
        "TEXT" => raw[header_end..].to_vec(),
        _ => {
            let (not, list) = if let Some(list) = section.strip_prefix("HEADER.FIELDS.NOT") {
                (true, list)
            } else if let Some(list) = section.strip_prefix("HEADER.FIELDS") {
                (false, list)
            } else {
                return Vec::new();
            };
            let names = list
                .trim()
                .trim_matches(|c| c == '(' || c == ')')
                .split_whitespace()
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>();
            let header = String::from_utf8_lossy(&raw[..header_end]);
            let mut out = String::new();
            let mut keep = false;
            for line in header.split("\r\n").filter(|line| !line.is_empty()) {
                if !line.starts_with([' ', '\t']) {
                    let name = line.split(':').next().unwrap_or("").to_ascii_lowercase();
                    keep = names.contains(&name) != not;
                }
                if keep {
                    out.push_str(line);
                    out.push_str("\r\n");
                }
            }
            out.push_str("\r\n");
            out.into_bytes()
        }
    }
}

fn internal_date(date: DateTime<Utc>) -> String {
    date.format("%d-%b-%Y %H:%M:%S +0000").to_string()
}

fn fetch_response(seq: usize, message: &Message, items: &[FetchItem]) -> Vec<u8> {
    let mut out = format!("* {seq} FETCH (").into_bytes();
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(b' ');
        }
        match item {
            FetchItem::Uid => out.extend(format!("UID {}", message.uid).into_bytes()),
            FetchItem::Flags => out.extend(format!("FLAGS ({})", flag_list(message)).into_bytes()),
            FetchItem::InternalDate => out.extend(
                format!("INTERNALDATE \"{}\"", internal_date(message.internal_date)).into_bytes(),
            ),
            FetchItem::Size => {
                out.extend(format!("RFC822.SIZE {}", message.raw.len()).into_bytes())
            }
            FetchItem::Section { label, section, .. } => {
                let bytes = section_bytes(&message.raw, section);
                out.extend(format!("{label} {{{}}}\r\n", bytes.len()).into_bytes());
                out.extend(bytes);
            }
        }
    }
    out.extend(b")\r\n");
    out
}
