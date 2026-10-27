//! 回覆郵件：CLI `mail reply` 與 TUI 回覆草稿共用的收件人／主旨判定、寄送路徑與草稿檔。

use crate::{
    config, local_state,
    model::{Item, Kind, OutgoingMail},
    provider,
    store::Store,
    sync,
};
use anyhow::{bail, Context, Result};
use mailparse::MailHeaderMap;
use std::path::{Path, PathBuf};

/// 使用者在 TUI 編輯後的收件人與主旨；CLI 不覆寫，沿用原信判定。
#[derive(Debug)]
pub struct ReplyHeaders {
    pub to: Vec<String>,
    pub subject: String,
}

/// 依原信判定回覆信（正文與附件留空由呼叫端填入）。CLI 與 TUI 共用。
pub fn reply_mail(item: &Item) -> Result<OutgoingMail> {
    let bytes = provider::mail::raw_bytes(item)?;
    let parsed = mailparse::parse_mail(&bytes)?;
    let to = parsed
        .headers
        .get_first_value("Reply-To")
        .or_else(|| parsed.headers.get_first_value("From"))
        .context("郵件缺少回覆地址")?;
    Ok(OutgoingMail {
        to: addresses(&to)?,
        cc: vec![],
        subject: reply_subject(&item.title),
        body: String::new(),
        attachments: vec![],
        in_reply_to: parsed.headers.get_first_value("Message-ID"),
    })
}

pub fn reply_subject(title: &str) -> String {
    if title.to_lowercase().starts_with("re:") {
        title.to_owned()
    } else {
        format!("Re: {title}")
    }
}

fn addresses(value: &str) -> Result<Vec<String>> {
    let mut output = Vec::new();
    for addr in mailparse::addrparse(value)?.iter() {
        match addr {
            mailparse::MailAddr::Single(a) => output.push(a.addr.clone()),
            mailparse::MailAddr::Group(g) => output.extend(g.addrs.iter().map(|a| a.addr.clone())),
        }
    }
    Ok(output)
}

/// 寄出回覆：`m2k mail reply` 與 TUI 共用的唯一寄送路徑，不重試。
pub fn send_reply(
    dir: &Path,
    id: &str,
    read_body: impl FnOnce() -> Result<String>,
    attachments: Vec<PathBuf>,
    headers: Option<ReplyHeaders>,
) -> Result<String> {
    let _lock = sync::lock(dir)?;
    let item = {
        let mut store = Store::open(dir)?;
        store.ensure_account(&config::load(dir)?)?;
        store.get(id)?
    };
    if item.kind != Kind::Mail {
        bail!("資料類型不符：{}", item.kind);
    }
    if dir.join(".demo").exists() {
        bail!("展示資料不連線；請指定正式帳號的 --data-dir");
    }
    let cfg = config::load(dir)?;
    let pass = config::password(dir, &cfg)?;
    let mut mail = reply_mail(&item)?;
    // 與舊 CLI 流程相同：解析完原信才讀正文。
    mail.body = read_body()?;
    mail.attachments = attachments;
    if let Some(headers) = headers {
        mail.to = headers.to;
        mail.subject = headers.subject;
    }
    // CLI 路徑原樣回傳錯誤（與舊行為一致）；密碼遮罩只在 TUI 顯示時做，見 `masked_error`。
    provider::mail::send(&cfg, &pass, &mail)
}

/// TUI 顯示寄送錯誤用：把訊息中的密碼遮掉。太短的密碼（少於 4 字元）會誤傷訊息，不遮。
pub fn mask_secret(message: &str, secret: &str) -> String {
    if secret.chars().count() < 4 {
        message.to_owned()
    } else {
        message.replace(secret, "[已隱藏]")
    }
}

/// 讀取目前帳號密碼並遮罩錯誤訊息；讀不到密碼就原樣顯示。
pub fn masked_error(dir: &Path, error: anyhow::Error) -> anyhow::Error {
    let message = format!("{error:#}");
    match config::load(dir).and_then(|cfg| config::password(dir, &cfg)) {
        Ok(pass) => anyhow::anyhow!(mask_secret(&message, &pass)),
        Err(_) => anyhow::anyhow!(message),
    }
}

/// 草稿：編輯器看到的純文字，標頭（To、Subject）、空行、正文與引用。
pub fn compose_draft(item: &Item) -> Result<String> {
    let mail = reply_mail(item)?;
    if item.text.trim().is_empty() {
        bail!("尚無本機正文，無法引用原文；請等正文載入後再回覆");
    }
    let sender = if item.subtitle.trim().is_empty() {
        "原寄件者"
    } else {
        item.subtitle.trim()
    };
    let when = item
        .date
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|date| {
            date.with_timezone(&chrono_tz::Asia::Taipei)
                .format("%Y/%m/%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "先前".into());
    let quoted = item
        .text
        .lines()
        .map(|line| {
            if line.is_empty() {
                ">".to_owned()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "To: {}\nSubject: {}\n\n\n\nOn {when}, {sender} wrote:\n{quoted}\n",
        mail.to.join(", "),
        mail.subject
    ))
}

/// 解析編輯後的草稿；回傳可直接寄送的標頭與正文。
pub fn parse_draft(text: &str) -> Result<(ReplyHeaders, String)> {
    let text = text.replace("\r\n", "\n");
    let (head, body) = match text.split_once("\n\n") {
        Some(parts) => parts,
        None => (text.trim_end_matches('\n'), ""),
    };
    let mut to = None;
    let mut subject = None;
    for line in head.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            bail!("標頭格式錯誤：{line}");
        };
        {
            match name.trim().to_ascii_lowercase().as_str() {
                "to" => to = Some(value.trim().to_owned()),
                "subject" => subject = Some(value.trim().to_owned()),
                other => bail!("不支援的標頭 {other}:（只能編輯 To 與 Subject）"),
            }
        }
    }
    let to = addresses(&to.context("缺少 To: 標頭")?).context("To: 收件人無法解析")?;
    if to.is_empty() || to.iter().any(|address| !address.contains('@')) {
        bail!("To: 至少需要一位有效收件人（含 @）");
    }
    let subject = subject.unwrap_or_default();
    if subject.is_empty() || subject.contains('\n') {
        bail!("Subject: 不可為空");
    }
    let has_reply = body.lines().any(|line| {
        let line = line.trim();
        !line.is_empty()
            && !line.starts_with('>')
            && !(line.starts_with("On ") && line.ends_with("wrote:"))
    });
    if !has_reply {
        bail!("回覆正文是空的（只有引用）");
    }
    Ok((
        ReplyHeaders { to, subject },
        body.trim_end().to_owned() + "\n",
    ))
}

/// 保存草稿到 `<data-dir>/drafts/`；`existing` 有值時覆寫同一檔。
pub fn save_draft_file(data_dir: &Path, existing: Option<&Path>, text: &str) -> Result<PathBuf> {
    let root = data_dir.join("drafts");
    local_state::private_directory(&root)?;
    let path = match existing {
        Some(path) => path.to_owned(),
        None => root.join(format!(
            "reply-{}-{}.txt",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        )),
    };
    local_state::write_atomic(&path, text.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail(raw: &str, text: &str) -> Item {
        Item {
            id: "m1".into(),
            kind: Kind::Mail,
            collection: "INBOX".into(),
            remote_id: "1".into(),
            etag: None,
            uid_validity: None,
            title: "週會".into(),
            subtitle: "王小明 <ming@example.test>".into(),
            date: Some("2026-10-19T02:00:00Z".into()),
            end_date: None,
            text: text.into(),
            raw: raw.into(),
            flags: vec![],
            emails: vec![],
            phones: vec![],
            attachments: vec![],
        }
    }

    const RAW: &str = "From: ming@example.test\r\nReply-To: Ming <reply@example.test>\r\nMessage-ID: <a@x>\r\nSubject: x\r\n\r\nhi\r\n";

    #[test]
    fn draft_has_re_subject_reply_to_recipient_and_quoted_original() {
        let draft = compose_draft(&mail(RAW, "第一行\n\n第三行")).unwrap();
        assert!(draft.starts_with("To: reply@example.test\nSubject: Re: 週會\n\n"));
        assert!(draft.contains(
            "On 2026/10/19 10:00, 王小明 <ming@example.test> wrote:\n> 第一行\n>\n> 第三行\n"
        ));
    }

    #[test]
    fn subject_is_not_double_prefixed() {
        assert_eq!(reply_subject("RE: x"), "RE: x");
        assert_eq!(reply_subject("x"), "Re: x");
    }

    #[test]
    fn draft_without_body_or_headers_is_refused() {
        assert!(compose_draft(&mail(RAW, "  ")).is_err());
        assert!(compose_draft(&mail("Subject: x\r\n\r\nhi", "hi")).is_err());
    }

    #[test]
    fn parse_round_trips_edited_headers_and_requires_a_real_reply() {
        let draft = compose_draft(&mail(RAW, "原文")).unwrap();
        assert!(parse_draft(&draft).is_err(), "只有引用不可寄");
        let edited = draft.replacen("\n\n\n\n", "\n\n好的，週三見。\n\n", 1);
        let (headers, body) = parse_draft(&edited).unwrap();
        assert_eq!(headers.to, vec!["reply@example.test"]);
        assert_eq!(headers.subject, "Re: 週會");
        assert!(body.starts_with("好的，週三見。\n"));
        assert!(body.contains("> 原文"));
        assert!(parse_draft("To: nobody\nSubject: s\n\nhi").is_err());
        assert!(parse_draft("To: a@b.test\nSubject:\n\nhi").is_err());
        assert!(parse_draft("Subject: s\n\nhi").is_err());
        let cc = parse_draft("To: a@b.test\nCc: c@b.test\nSubject: s\n\nhi").unwrap_err();
        assert!(format!("{cc:#}").contains("不支援的標頭 cc"));
    }

    #[test]
    fn save_draft_file_is_private_and_overwrites_same_path() {
        let temp = tempfile::tempdir().unwrap();
        let first = save_draft_file(temp.path(), None, "a").unwrap();
        assert!(first.starts_with(temp.path().join("drafts")));
        let again = save_draft_file(temp.path(), Some(&first), "b").unwrap();
        assert_eq!(first, again);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "b");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&first).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn check_order_matches_old_cli_body_is_read_last() {
        let temp = tempfile::tempdir().unwrap();
        crate::demo::seed(temp.path()).unwrap();
        // 找不到郵件優先於讀正文。
        let error = send_reply(
            temp.path(),
            "no-such-id",
            || panic!("正文不該先讀"),
            vec![],
            None,
        )
        .unwrap_err();
        assert!(!format!("{error:#}").contains("展示資料"), "{error:#}");
        // demo 拒絕優先於讀正文。
        let error = send_reply(
            temp.path(),
            "demo-mail-2",
            || panic!("正文不該先讀"),
            vec![],
            None,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("展示資料不連線"));
    }

    #[test]
    fn short_passwords_never_scramble_messages() {
        assert_eq!(
            mask_secret("找不到 /nonexistent/missing.pdf", "o"),
            "找不到 /nonexistent/missing.pdf"
        );
        assert_eq!(
            mask_secret("auth hunter22 failed", "hunter22"),
            "auth [已隱藏] failed"
        );
    }

    #[test]
    fn demo_data_dir_refuses_to_send() {
        let temp = tempfile::tempdir().unwrap();
        crate::demo::seed(temp.path()).unwrap();
        let id = "demo-mail-2";
        let error = send_reply(temp.path(), id, || Ok("hi\n".into()), vec![], None).unwrap_err();
        assert!(format!("{error:#}").contains("展示資料不連線"), "{error:#}");
    }
}
