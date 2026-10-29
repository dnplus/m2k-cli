mod cli;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{CommandFactory, Parser};
use cli::*;
use m2k::{
    calendar_event, config, dav_write, demo,
    model::{Item, Kind, OutgoingMail, SyncResult},
    provider,
    store::Store,
    sync, tui,
};
use std::{
    io::{IsTerminal, Read},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if std::env::args_os().any(|arg| arg == "--json")
                && !matches!(
                    error.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
        {
            println!(
                "{}",
                serde_json::to_string(&m2k::error::ErrorOutput::invalid_arguments(
                    error.to_string()
                ))
                .unwrap()
            );
            std::process::exit(2);
        }
        Err(error) => error.exit(),
    };
    let json = cli.json;
    if let Err(error) = execute(cli) {
        if json {
            if let Some(receipt) =
                error.downcast_ref::<provider::dav::RemoteWritePendingReconcile>()
            {
                println!(
                    "{}",
                    serde_json::json!({"state":"partial", "remote_write":receipt})
                );
            } else {
                println!("{}", m2k::error::ErrorOutput::json(&error));
            }
        } else {
            eprintln!("錯誤：{error:#}");
        }
        std::process::exit(1);
    }
}

fn execute(cli: Cli) -> Result<()> {
    let dir = cli.data_dir.unwrap_or_else(|| {
        if matches!(cli.command.as_ref(), Some(Command::Demo { .. })) {
            config::default_data_dir().join("demo")
        } else {
            config::default_data_dir()
        }
    });
    m2k::site::set_data_dir(&dir);
    let json = cli.json;
    match cli.command {
        Some(Command::DateRange {
            expression,
            timezone,
        }) => emit(
            &m2k::date::resolve(&expression, &timezone, Utc::now())?,
            json,
        )?,
        None => {
            if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
                tui::run(&dir)?
            } else {
                Cli::command().print_help()?;
                println!();
            }
        }
        Some(Command::Init(args)) => {
            let _lock = sync::lock(&dir)?;
            let mut cfg = config::Config::with_site_profile(
                args.username,
                args.server.as_deref().unwrap_or_default(),
                args.site_profile,
            )?;
            if let Some(email) = args.email {
                cfg.email = email;
            }
            if let Some(url) = args.caldav_url {
                cfg.caldav_url = url;
            }
            if let Some(url) = args.carddav_url {
                cfg.carddav_url = url;
            }
            if dir.join(".demo").exists() {
                bail!("展示資料目錄不能改成正式帳號，請指定新的 --data-dir");
            }
            if dir.join("config.json").exists() {
                let old = config::load(&dir)?;
                if old.username != cfg.username
                    || old.imap_host != cfg.imap_host
                    || old.caldav_url != cfg.caldav_url
                    || old.carddav_url != cfg.carddav_url
                {
                    bail!("此目錄已綁定其他帳號或端點；請使用新的 --data-dir");
                }
                bail!("此帳號已完成設定；請使用 config set 調整設定，auth login 更新密碼");
            }
            cfg.validate()?;
            Store::open(&dir)?.ensure_account(&cfg)?;
            config::save(&dir, &cfg)?;
            emit(
                &serde_json::json!({"data_dir":dir,"config":cfg,"next":"m2k auth login"}),
                json,
            )?;
        }
        Some(Command::Config { command }) => {
            let mut cfg = config::load(&dir)?;
            match command {
                ConfigCommand::Show => emit(&cfg, true)?,
                ConfigCommand::Set {
                    sync_interval_minutes,
                    mail_retention_days,
                    calendar_retention_days,
                    contacts_retention_days,
                    imap_host,
                    imap_port,
                    smtp_host,
                    smtp_port,
                    caldav_url,
                    carddav_url,
                } => {
                    let _lock = sync::lock(&dir)?;
                    if let Some(v) = sync_interval_minutes {
                        cfg.sync_interval_minutes = v;
                    }
                    if let Some(v) = mail_retention_days {
                        cfg.mail_retention_days = v;
                    }
                    if let Some(v) = calendar_retention_days {
                        cfg.calendar_retention_days = v;
                    }
                    if let Some(v) = contacts_retention_days {
                        cfg.contacts_retention_days = v;
                    }
                    if let Some(v) = imap_host {
                        cfg.imap_host = v;
                    }
                    if let Some(v) = imap_port {
                        cfg.imap_port = v;
                    }
                    if let Some(v) = smtp_host {
                        cfg.smtp_host = v;
                    }
                    if let Some(v) = smtp_port {
                        cfg.smtp_port = v;
                    }
                    if let Some(v) = caldav_url {
                        cfg.caldav_url = v;
                    }
                    if let Some(v) = carddav_url {
                        cfg.carddav_url = v;
                    }
                    cfg.validate()?;
                    Store::open(&dir)?.ensure_account(&cfg)?;
                    config::save(&dir, &cfg)?;
                    emit(&cfg, true)?;
                }
            }
        }
        Some(Command::Auth { command }) => {
            if dir.join(".demo").exists() {
                bail!("展示資料不提供登入");
            }
            let cfg = if dir.join("config.json").exists() {
                config::load(&dir)?
            } else if let AuthCommand::Login {
                username,
                email,
                server,
                site_profile,
                ..
            } = &command
            {
                let username = match username {
                    Some(v) => v.clone(),
                    None if std::io::stdin().is_terminal() => {
                        eprint!("Mail2000 帳號：");
                        let mut line = String::new();
                        std::io::stdin().read_line(&mut line)?;
                        line
                    }
                    None => bail!("尚未設定帳號；請加 --username 或先執行 init"),
                };
                let mut cfg = config::Config::with_site_profile(
                    username,
                    server.as_deref().unwrap_or_default(),
                    site_profile.clone(),
                )?;
                if let Some(email) = email {
                    cfg.email = email.clone();
                }
                cfg
            } else {
                config::load(&dir)?
            };
            match command {
                AuthCommand::Logout => {
                    config::delete_password(&dir, &cfg)?;
                    emit(&serde_json::json!({"credential_removed":true}), json)?;
                }
                AuthCommand::Login { no_store, .. } => {
                    let password = match std::env::var("M2K_PASSWORD") {
                        Ok(v) if !v.is_empty() => v,
                        _ => rpassword::prompt_password("Mail2000 密碼：")?,
                    };
                    let fallback = check_credentials(&cfg, &password)?;
                    if !dir.join("config.json").exists() {
                        let _lock = sync::lock(&dir)?;
                        cfg.validate()?;
                        Store::open(&dir)?.ensure_account(&cfg)?;
                        config::save(&dir, &cfg)?;
                    }
                    if !no_store {
                        config::save_password(&dir, &cfg, &password)?;
                    }
                    emit(&auth_verification_output(fallback, Some(!no_store)), json)?;
                }
                AuthCommand::Check => {
                    let pass = config::password(&dir, &cfg)?;
                    let fallback = check_credentials(&cfg, &pass)?;
                    emit(&auth_verification_output(fallback, None), json)?;
                }
            }
        }
        Some(Command::Sync { kind }) => {
            let results = if kind == Some(Kind::Contacts) {
                let result = m2k::directory::sync_contacts(
                    &dir,
                    m2k::directory::SyncArgs {
                        scope: m2k::directory::DirectoryScope::Company,
                        addressbook: None,
                    },
                )?;
                vec![SyncResult {
                    kind: Kind::Contacts,
                    count: result.count,
                    error: None,
                }]
            } else {
                sync::run(&dir, &kinds(kind))?
            };
            if results.iter().any(|r| r.error.is_some()) {
                return Err(m2k::error::PartialSync(results).into());
            }
            emit(&results, json)?;
        }
        Some(Command::Watch { kind }) => watch(&dir, kind, json)?,
        Some(Command::Doctor) => {
            let report = m2k::doctor::run(&dir);
            if !report.ok() {
                if !json {
                    eprint!("{}", report.render());
                }
                return Err(m2k::doctor::DoctorFailed(report).into());
            }
            if json {
                emit(&report, true)?;
            } else {
                print!("{}", report.render());
            }
        }
        Some(Command::Status) => status(&dir, json)?,
        Some(Command::Prune) => {
            let _lock = sync::lock(&dir)?;
            let cfg = config::load(&dir)?;
            let deleted = checked_store(&dir)?.prune(&cfg, Utc::now())?;
            emit(&serde_json::json!({"local_items_removed":deleted}), json)?;
        }
        Some(Command::Mail { command }) => mail_command(&dir, command, json)?,
        Some(Command::Calendar { command }) => calendar_command(&dir, command, json)?,
        Some(Command::Contacts { command }) => match command {
            ContactsCommand::Create { file, collection } => emit(
                &dav_write::create(&dir, Kind::Contacts, collection.as_deref(), |_, _| {
                    read_body(&file)
                })?,
                json,
            )?,
            ContactsCommand::Update { id, file } => emit(
                &dav_write::update(
                    &dir,
                    || get(&dir, &id, Kind::Contacts),
                    |_, _| read_body(&file),
                )?,
                json,
            )?,
            ContactsCommand::Dav(command) => dav_command(&dir, Kind::Contacts, command, json)?,
        },
        Some(Command::Directory { command }) => m2k::directory::execute(&dir, command, json)?,
        Some(Command::Meeting { command }) => m2k::meeting::execute(&dir, command, json)?,
        Some(Command::Skills { command }) => match command {
            SkillsCommand::Install { path } => {
                let result = m2k::skills::install(path.as_deref())?;
                if json {
                    emit(&result, true)?;
                } else {
                    println!("{}", result.path.display());
                }
            }
        },
        Some(Command::Tui {
            snapshot,
            width,
            height,
        }) => {
            if snapshot {
                print!("{}", tui::render_snapshot(&dir, width, height)?);
            } else {
                tui::run(&dir)?;
            }
        }
        Some(Command::Demo { no_tui }) => {
            demo::seed(&dir)?;
            if no_tui {
                status(&dir, json)?;
            } else {
                tui::run(&dir)?;
            }
        }
    }
    Ok(())
}

fn emit(value: &impl serde::Serialize, _json: bool) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn kinds(kind: Option<Kind>) -> Vec<Kind> {
    kind.map_or_else(|| vec![Kind::Mail, Kind::Calendar], |k| vec![k])
}

fn online(dir: &Path) -> Result<(config::Config, String)> {
    if dir.join(".demo").exists() {
        bail!("展示資料不連線；請指定正式帳號的 --data-dir");
    }
    let cfg = config::load(dir)?;
    Store::open(dir)?.ensure_account(&cfg)?;
    let pass = config::password(dir, &cfg)?;
    Ok((cfg, pass))
}

fn checked_store(dir: &Path) -> Result<Store> {
    let mut store = Store::open(dir)?;
    let cfg = config::load(dir)?;
    store.ensure_account(&cfg)?;
    Ok(store)
}

fn check_credentials(cfg: &config::Config, pass: &str) -> Result<Option<u16>> {
    let fallback = provider::mail::verify(cfg, pass)
        .map_err(|e| anyhow::anyhow!("{}", format!("{e:#}").replace(pass, "[已隱藏]")))?;
    for kind in [Kind::Calendar, Kind::Contacts] {
        provider::dav::verify(cfg, pass, kind)
            .map_err(|e| anyhow::anyhow!("{}", format!("{e:#}").replace(pass, "[已隱藏]")))?;
    }
    Ok(fallback)
}

fn auth_verification_output(fallback: Option<u16>, stored: Option<bool>) -> serde_json::Value {
    let mut output = serde_json::json!({"verified":["imap","smtp","caldav","carddav"]});
    if let Some(stored) = stored {
        output["stored"] = stored.into();
    }
    if let Some(port) = fallback {
        output["smtp_port_fallback"] = port.into();
        output["suggestion"] =
            "587 TCP 連線失敗，已改用 465；建議執行 m2k config set --smtp-port 465".into();
    }
    output
}

fn status(dir: &Path, json: bool) -> Result<()> {
    let store = checked_store(dir)?;
    emit(
        &serde_json::json!({"data_dir":dir,"demo":dir.join(".demo").exists(),"counts":store.counts()?,"sync":store.sync_status()?,"config":config::load(dir).ok(),"progress":m2k::sync_progress::read(dir)?,"mail_webhooks":store.mail_webhook_status()?,"webhook_signing":m2k::mail_webhook::signing_secret().ok().flatten().is_some(),"version":m2k::version::VERSION,"build_commit":m2k::version::COMMIT}),
        json,
    )
}

fn watch(dir: &Path, kind: Option<Kind>, json: bool) -> Result<()> {
    if kind == Some(Kind::Contacts) {
        bail!("聯絡人採手動全量：請執行 directory sync --scope company，或 --scope department --addressbook 部門");
    }
    let _ = online(dir)?;
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
    while !stop.load(Ordering::Relaxed) {
        let output = match sync::run_periodic(dir, &kinds(kind)) {
            Ok(results) if results.iter().any(|result| result.error.is_some()) => Some(
                m2k::error::ErrorOutput::json(&m2k::error::PartialSync(results).into()),
            ),
            Ok(results) => Some(serde_json::json!({"state":"complete","results":results})),
            Err(error) if json => Some(m2k::error::ErrorOutput::json(&error)),
            Err(error) => {
                eprintln!("同步失敗：{error:#}");
                None
            }
        };
        if let Some(mut output) = output {
            output["time"] = serde_json::to_value(Utc::now())?;
            println!("{output}");
        }
        let interval =
            Duration::from_secs(config::load(dir)?.sync_interval_minutes.saturating_mul(60));
        let start = Instant::now();
        while start.elapsed() < interval && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    Ok(())
}

fn read_body(file: &Path) -> Result<String> {
    if file == Path::new("-") {
        let mut body = String::new();
        std::io::stdin().read_to_string(&mut body)?;
        Ok(body)
    } else {
        std::fs::read_to_string(file).with_context(|| format!("無法讀取 {}", file.display()))
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("無法建立檔案 {}；不覆寫既有檔案", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

fn list(dir: &Path, kind: Kind, args: ListArgs, full: bool, json: bool) -> Result<()> {
    let store = checked_store(dir)?;
    let mut items = if json {
        store.list(
            kind,
            args.query.as_deref(),
            args.collection.as_deref(),
            args.limit,
        )?
    } else {
        store.list_summaries(
            kind,
            args.query.as_deref(),
            args.collection.as_deref(),
            args.limit,
        )?
    };
    if json {
        if kind == Kind::Mail && !full {
            for item in &mut items {
                provider::mail::normalize_cached(item)?;
                item.text = m2k::mail_read::latest_reply(&item.text);
                item.raw.clear();
            }
        }
        return emit(&items, true);
    }
    println!("{}", list_row("ID", "時間", "主旨 / 姓名", "來源 / 詳情"));
    for mut item in items {
        if kind == Kind::Mail {
            item.title = provider::mail::display_header(&item.title);
            item.subtitle = provider::mail::display_header(&item.subtitle);
        }
        println!(
            "{}",
            list_row(
                &item.id,
                item.date.as_deref().unwrap_or_default(),
                &item.title,
                &item.subtitle,
            )
        );
    }
    Ok(())
}

fn list_row(id: &str, time: &str, title: &str, detail: &str) -> String {
    m2k::plain_output::row(&[(id, 32), (time, 25), (title, 30), (detail, 28)])
}

fn get(dir: &Path, id: &str, kind: Kind) -> Result<Item> {
    let item = checked_store(dir)?.get(id)?;
    if item.kind != kind {
        bail!("資料類型不符：{}", item.kind);
    }
    Ok(item)
}

fn show(dir: &Path, id: &str, kind: Kind, raw: bool, full: bool, json: bool) -> Result<()> {
    let mut item = get(dir, id, kind)?;
    if raw {
        use std::io::Write;
        if kind == Kind::Mail {
            std::io::stdout().write_all(&provider::mail::raw_bytes(&item)?)?;
        } else {
            print!("{}", item.raw);
        }
    } else {
        if kind == Kind::Mail {
            provider::mail::normalize_cached(&mut item)?;
            if !full {
                item.text = m2k::mail_read::latest_reply(&item.text);
                item.raw.clear();
            }
        }
        if json {
            emit(&item, true)?;
        } else {
            println!(
                "{}\n{}\n{}\n\n{}",
                item.title,
                item.subtitle,
                item.date.unwrap_or_default(),
                item.text
            );
        }
    }
    Ok(())
}

fn mail_command(dir: &Path, cmd: MailCommand, json: bool) -> Result<()> {
    match cmd {
        MailCommand::Watch(args) => m2k::mail_webhook::run(dir, args)?,
        MailCommand::Workflow(command) => m2k::mail_workflow::execute(dir, command, json)?,
        MailCommand::Folders => emit(&checked_store(dir)?.collections(Kind::Mail)?, json)?,
        MailCommand::List { args, full } => list(dir, Kind::Mail, args, full, json)?,
        MailCommand::Search { query, limit, full } => list(
            dir,
            Kind::Mail,
            ListArgs {
                query: Some(query),
                collection: None,
                limit,
            },
            full,
            json,
        )?,
        MailCommand::Show { id, raw, full } => show(dir, &id, Kind::Mail, raw, full, json)?,
        MailCommand::Attachment { id, index, output } => {
            let item = get(dir, &id, Kind::Mail)?;
            let (name, bytes) = provider::mail::attachment(&item, index)?;
            write_new(&output, &bytes)?;
            emit(
                &serde_json::json!({"saved":output,"original_filename":name,"bytes":bytes.len()}),
                json,
            )?;
        }
        MailCommand::Send(args) => {
            let _lock = sync::lock(dir)?;
            let (cfg, pass) = online(dir)?;
            let mail = OutgoingMail {
                to: args.to,
                cc: args.cc,
                subject: args.subject,
                body: read_body(&args.body_file)?,
                attachments: args.attach,
                in_reply_to: None,
            };
            emit(
                &serde_json::json!({"message_id":provider::mail::send(&cfg,&pass,&mail)?}),
                json,
            )?;
        }
        MailCommand::Reply {
            id,
            body_file,
            attach,
        } => {
            emit(
                &serde_json::json!({"message_id":m2k::mail_reply::send_reply(dir,&id,|| read_body(&body_file),attach,None)?}),
                json,
            )?;
        }
        MailCommand::Forward { id, to, body_file } => {
            let _lock = sync::lock(dir)?;
            let item = get(dir, &id, Kind::Mail)?;
            let (cfg, pass) = online(dir)?;
            let introduction = body_file
                .map(|p| read_body(&p))
                .transpose()?
                .unwrap_or_default();
            emit(
                &serde_json::json!({"message_id":provider::mail::forward(&cfg,&pass,&item,&to,&introduction)?}),
                json,
            )?;
        }
        MailCommand::Flag { id, flag, clear } => {
            let _lock = sync::lock(dir)?;
            let mut item = get(dir, &id, Kind::Mail)?;
            let (cfg, pass) = online(dir)?;
            let flag = match flag.as_str() {
                "seen" => "\\Seen",
                "flagged" => "\\Flagged",
                _ => "\\Answered",
            };
            provider::mail::flag(&cfg, &pass, &item, flag, !clear)?;
            item.flags.retain(|f| f != flag);
            if !clear {
                item.flags.push(flag.into());
            }
            Store::open(dir)?.upsert(&item)?;
            emit(&serde_json::json!({"updated":id}), json)?;
        }
        MailCommand::Move {
            ids,
            folder,
            dry_run,
        } => {
            let _lock = sync::lock(dir)?;
            let items = ids
                .iter()
                .map(|id| get(dir, id, Kind::Mail))
                .collect::<Result<Vec<_>>>()?;
            let (cfg, pass) = online(dir)?;
            let plan = provider::mail::move_many(&cfg, &pass, &items, &folder, dry_run)?;
            if dry_run {
                emit(&plan, json)?;
            } else {
                let mut store = Store::open(dir)?;
                for id in &ids {
                    store.remove(id)?;
                }
                if ids.len() == 1 {
                    emit(
                        &serde_json::json!({"moved":ids[0],"folder":folder,"next":"m2k sync --kind mail"}),
                        json,
                    )?;
                } else {
                    let results = ids
                        .iter()
                        .map(|id| serde_json::json!({"moved":id,"folder":folder}))
                        .collect::<Vec<_>>();
                    emit(
                        &serde_json::json!({"results":results,"next":"m2k sync --kind mail"}),
                        json,
                    )?;
                }
            }
        }
        MailCommand::Delete { id } => {
            let _lock = sync::lock(dir)?;
            let item = get(dir, &id, Kind::Mail)?;
            let (cfg, pass) = online(dir)?;
            provider::mail::delete(&cfg, &pass, &item)?;
            Store::open(dir)?.remove(&id)?;
            emit(&serde_json::json!({"deleted":id}), json)?;
        }
    }
    Ok(())
}

fn dav_command(dir: &Path, kind: Kind, cmd: DavCommand, json: bool) -> Result<()> {
    match cmd {
        DavCommand::Collections => emit(&checked_store(dir)?.collections(kind)?, json)?,
        DavCommand::List(args) => list(dir, kind, args, true, json)?,
        DavCommand::Search { query, limit } => list(
            dir,
            kind,
            ListArgs {
                query: Some(query),
                collection: None,
                limit,
            },
            true,
            json,
        )?,
        DavCommand::Show { id, raw } => show(dir, &id, kind, raw, false, json)?,
        DavCommand::Export { id, output } => {
            let item = get(dir, &id, kind)?;
            write_new(&output, item.raw.as_bytes())?;
            emit(&serde_json::json!({"saved":output}), json)?;
        }
        DavCommand::Delete { id } => {
            dav_write::delete(dir, || get(dir, &id, kind))?;
            emit(&serde_json::json!({"deleted":id}), json)?;
        }
    }
    Ok(())
}

fn calendar_command(dir: &Path, cmd: CalendarCommand, json: bool) -> Result<()> {
    match cmd {
        CalendarCommand::Create {
            file,
            collection,
            fields,
        } => {
            // 欄位在連線前就驗證；--file 維持原本「鎖與連線檢查之後才讀檔」的順序。
            let built = match file {
                Some(_) => None,
                None => Some(calendar_event::build_new(&fields.into_input(), Utc::now())?),
            };
            let item = dav_write::create(
                dir,
                Kind::Calendar,
                collection.as_deref(),
                |cfg, url| match (built, file) {
                    (Some(raw), _) => {
                        if !calendar_event::own_collection(cfg, url) {
                            bail!("指定的 collection 是會議室或其他人的行事曆；欄位建立只寫入自己的行事曆，訂房請用 meeting book");
                        }
                        Ok(raw)
                    }
                    (None, Some(file)) => read_body(&file),
                    (None, None) => unreachable!("clap requires --file or --title"),
                },
            )?;
            emit(&item, json)?
        }
        CalendarCommand::Update { id, file, fields } => {
            let item = dav_write::update(
                dir,
                || get(dir, &id, Kind::Calendar),
                |cfg, base| match file {
                    Some(file) => read_body(&file),
                    None => {
                        if let Err(reason) = calendar_event::check_editable(cfg, base) {
                            bail!("{reason}");
                        }
                        calendar_event::apply_update(&base.raw, &fields.into_input(), Utc::now())
                    }
                },
            )?;
            emit(&item, json)?
        }
        CalendarCommand::Dav(command) => dav_command(dir, Kind::Calendar, command, json)?,
    }
    Ok(())
}

#[cfg(test)]
mod auth_output_tests {
    use super::auth_verification_output;

    #[test]
    fn fallback_is_visible_and_normal_output_stays_compatible() {
        let output = auth_verification_output(Some(465), Some(false));
        assert_eq!(output["smtp_port_fallback"], 465);
        assert_eq!(output["stored"], false);
        assert!(output["suggestion"]
            .as_str()
            .unwrap()
            .contains("m2k config set --smtp-port 465"));
        let output = auth_verification_output(None, None);
        assert!(output.get("smtp_port_fallback").is_none());
        assert!(output.get("suggestion").is_none());
        assert!(output.get("stored").is_none());
    }
}
