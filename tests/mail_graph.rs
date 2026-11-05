use chrono::{DateTime, Duration, Utc};
use m2k::mail_graph::{build, render_markdown, GraphArgs, ThreadStatus};
use m2k::model::{Item, Kind};

const ME: &str = "me@example.test";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-05T12:00:00+00:00")
        .unwrap()
        .with_timezone(&Utc)
}

fn build_with(
    items: Vec<Item>,
    me: &str,
    now: DateTime<Utc>,
    args: &GraphArgs,
) -> m2k::mail_graph::MailGraph {
    build(items, me, &[], now, args)
}

fn args() -> GraphArgs {
    GraphArgs {
        days: 14,
        person: None,
        people: 20,
        threads: 30,
        excerpt_chars: 280,
    }
}

fn mail(id: &str, folder: &str, hours_ago: i64, headers: &[&str], text: &str) -> Item {
    let subject = headers
        .iter()
        .find_map(|line| line.strip_prefix("Subject: "))
        .unwrap_or_default();
    let from = headers
        .iter()
        .find_map(|line| line.strip_prefix("From: "))
        .unwrap_or_default();
    Item {
        id: id.into(),
        kind: Kind::Mail,
        collection: folder.into(),
        remote_id: id.into(),
        etag: None,
        uid_validity: Some(1),
        title: subject.into(),
        subtitle: from.into(),
        date: Some((now() - Duration::hours(hours_ago)).to_rfc3339()),
        end_date: None,
        text: text.into(),
        raw: format!("{}\r\n\r\n{text}\r\n", headers.join("\r\n")),
        flags: vec![],
        emails: vec![],
        phones: vec![],
        attachments: vec![],
    }
}

fn conversation() -> Vec<Item> {
    vec![
        mail(
            "a1",
            "INBOX",
            48,
            &[
                "From: Alice Chen <Alice@Example.test>",
                "To: me@example.test",
                "Cc: bob@example.test",
                "Subject: 報價確認",
                "Message-ID: <a1@example.test>",
            ],
            "請確認附件報價。",
        ),
        mail(
            "a2",
            "Sent",
            24,
            &[
                "From: me@example.test",
                "To: alice@example.test",
                "Cc: bob@example.test",
                "Subject: 報價確認（修正版）",
                "Message-ID: <a2@example.test>",
                "In-Reply-To: <a1@example.test>",
                "References: <a1@example.test>",
            ],
            "已修正，請再看一次。\n\nOn Tue, Alice <alice@example.test> wrote:\n> 請確認附件報價。",
        ),
        mail(
            "a2-copy",
            "INBOX",
            24,
            &[
                "From: me@example.test",
                "To: alice@example.test",
                "Cc: bob@example.test",
                "Subject: 報價確認（修正版）",
                "Message-ID: <a2@example.test>",
            ],
            "已修正，請再看一次。",
        ),
        mail(
            "c1",
            "INBOX",
            2,
            &[
                "From: Carol <carol@example.test>",
                "To: me@example.test",
                "Subject: 下週 Workshop 場地",
                "Message-ID: <c1@example.test>",
            ],
            "場地還沒定，你可以幫忙訂嗎？",
        ),
        mail(
            "n1",
            "INBOX",
            5,
            &[
                "From: news@example.test",
                "To: all@example.test",
                "Cc: me@example.test",
                "Subject: 部門公告",
                "Message-ID: <n1@example.test>",
            ],
            "本週五停電。",
        ),
        mail(
            "old",
            "INBOX",
            24 * 30,
            &[
                "From: Alice Chen <alice@example.test>",
                "To: me@example.test",
                "Subject: 很久以前",
                "Message-ID: <old@example.test>",
            ],
            "舊信",
        ),
    ]
}

#[test]
fn replies_with_changed_subject_join_their_thread_and_duplicate_copies_count_once() {
    let graph = build_with(conversation(), ME, now(), &args());
    assert_eq!(
        graph.message_count, 4,
        "old mail and the INBOX copy are excluded"
    );
    let quote = graph
        .threads
        .iter()
        .find(|thread| thread.subject == "報價確認")
        .expect("thread keeps the root subject");
    assert_eq!(quote.message_count, 2);
    assert_eq!(quote.ids, ["a2", "a1"], "newest first, copy deduplicated");
    assert_eq!(
        quote.participants,
        ["alice@example.test", "bob@example.test"]
    );
    assert_eq!(quote.status, ThreadStatus::AwaitingThem);
    assert_eq!(
        quote.latest_excerpt, "已修正，請再看一次。",
        "quoted history is stripped"
    );
}

#[test]
fn status_tells_the_agent_who_owes_the_next_reply() {
    let mut items = conversation();
    let graph = build_with(items.clone(), ME, now(), &args());
    let status = |subject: &str| {
        graph
            .threads
            .iter()
            .find(|thread| thread.subject == subject)
            .unwrap()
            .status
    };
    assert_eq!(status("下週 Workshop 場地"), ThreadStatus::AwaitingMe);
    assert_eq!(
        status("部門公告"),
        ThreadStatus::Fyi,
        "cc-only mail is not a request"
    );

    items
        .iter_mut()
        .find(|item| item.id == "c1")
        .unwrap()
        .flags
        .push("\\Answered".into());
    let graph = build_with(items, ME, now(), &args());
    let workshop = graph
        .threads
        .iter()
        .find(|t| t.subject == "下週 Workshop 場地")
        .unwrap();
    assert_eq!(workshop.status, ThreadStatus::Replied);
}

#[test]
fn people_are_ranked_by_interaction_and_never_include_me() {
    let graph = build_with(conversation(), ME, now(), &args());
    assert!(graph.people.iter().all(|person| person.email != ME));
    let alice = &graph.people[0];
    assert_eq!(
        alice.email, "alice@example.test",
        "address case is normalized"
    );
    assert_eq!(alice.name.as_deref(), Some("Alice Chen"));
    assert_eq!((alice.messages, alice.from_them, alice.from_me), (2, 1, 1));
    assert_eq!(alice.threads, 1);
    assert_eq!(alice.often_with, ["bob@example.test"]);
    assert_eq!(alice.recent_subjects.len(), 2);
}

#[test]
fn person_filter_keeps_only_threads_involving_that_person() {
    let mut options = args();
    options.person = Some("carol".into());
    let graph = build_with(conversation(), ME, now(), &options);
    assert_eq!(graph.threads.len(), 1);
    assert_eq!(graph.threads[0].subject, "下週 Workshop 場地");
    assert_eq!(graph.message_count, 1);
    assert_eq!(graph.people[0].email, "carol@example.test");
}

#[test]
fn output_is_deterministic_regardless_of_input_order() {
    let forward = build_with(conversation(), ME, now(), &args());
    let mut reversed = conversation();
    reversed.reverse();
    let backward = build_with(reversed, ME, now(), &args());
    assert_eq!(
        serde_json::to_string(&forward).unwrap(),
        serde_json::to_string(&backward).unwrap()
    );
    assert_eq!(render_markdown(&forward), render_markdown(&backward));
}

#[test]
fn summary_only_cache_without_raw_headers_still_contributes() {
    let mut item = mail("s1", "INBOX", 1, &[], "");
    item.raw.clear();
    item.title = "Re: 合約".into();
    item.subtitle = "Dave <dave@example.test>".into();
    item.emails = vec!["dave@example.test".into(), ME.into()];
    let graph = build_with(vec![item], ME, now(), &args());
    assert_eq!(graph.threads[0].last_from, "Dave <dave@example.test>");
    assert_eq!(graph.threads[0].status, ThreadStatus::AwaitingMe);
}

#[test]
fn limits_truncate_people_threads_and_excerpts() {
    let mut options = args();
    options.people = 1;
    options.threads = 2;
    options.excerpt_chars = 4;
    let graph = build_with(conversation(), ME, now(), &options);
    assert_eq!(graph.people.len(), 1);
    assert_eq!(graph.threads.len(), 2);
    assert_eq!(graph.threads[0].latest_excerpt, "場地還沒…");
}

#[test]
fn display_name_comes_from_the_newest_mail_whatever_the_input_order() {
    let older = mail(
        "x1",
        "INBOX",
        10,
        &[
            "From: \"Smith, John\" <john@example.test>",
            "To: me@example.test",
            "Subject: 甲",
            "Message-ID: <x1@example.test>",
        ],
        "",
    );
    let newer = mail(
        "x2",
        "INBOX",
        1,
        &[
            "From: John Smith <john@example.test>",
            "To: me@example.test",
            "Subject: 乙",
            "Message-ID: <x2@example.test>",
        ],
        "",
    );
    for items in [vec![older.clone(), newer.clone()], vec![newer, older]] {
        let graph = build_with(items, ME, now(), &args());
        assert_eq!(graph.people[0].name.as_deref(), Some("John Smith"));
    }
}

#[test]
fn mail_without_a_usable_sender_never_becomes_an_empty_contact() {
    let item = mail(
        "y1",
        "INBOX",
        1,
        &[
            "From: undisclosed",
            "To: me@example.test",
            "Subject: 無寄件者",
            "Message-ID: <y1@example.test>",
        ],
        "內容",
    );
    let graph = build_with(vec![item], ME, now(), &args());
    assert!(graph.people.iter().all(|person| !person.email.is_empty()));
    assert!(graph.threads[0]
        .participants
        .iter()
        .all(|email| !email.is_empty()));
    assert_eq!(graph.threads[0].last_from, "(未知寄件者)");
}

fn latest_status(item: Item) -> ThreadStatus {
    build_with(vec![item], ME, now(), &args()).threads[0].status
}

#[test]
fn automated_mail_addressed_to_me_is_fyi_not_a_reply_request() {
    for header in [
        "Auto-Submitted: auto-generated",
        "Precedence: bulk",
        "List-Unsubscribe: <mailto:unsubscribe@example.test>",
        "List-Id: <team.example.test>",
        "X-Auto-Response-Suppress: All",
    ] {
        let item = mail(
            "auto",
            "INBOX",
            1,
            &[
                "From: Portal <portal@example.test>",
                "To: me@example.test",
                "Subject: 通知",
                header,
            ],
            "",
        );
        assert_eq!(latest_status(item), ThreadStatus::Fyi, "{header}");
    }
    for sender in [
        "no-reply@example.test",
        "DoNotReply@example.test",
        "kmbot@example.test",
        "edm03@example.test",
    ] {
        let item = mail(
            "auto",
            "INBOX",
            1,
            &[
                &format!("From: {sender}"),
                "To: me@example.test",
                "Subject: 通知",
            ],
            "",
        );
        assert_eq!(latest_status(item), ThreadStatus::Fyi, "{sender}");
    }
}

#[test]
fn people_who_merely_look_automated_by_name_are_still_people() {
    for (sender, header) in [
        ("j.talbot@example.test", "X-Note: none"),
        ("alice@example.test", "Auto-Submitted: no"),
    ] {
        let item = mail(
            "human",
            "INBOX",
            1,
            &[
                &format!("From: {sender}"),
                "To: me@example.test",
                "Subject: 請回覆",
                header,
            ],
            "",
        );
        assert_eq!(latest_status(item), ThreadStatus::AwaitingMe, "{sender}");
    }
}

#[test]
fn automated_senders_rank_after_humans_even_when_louder() {
    let mut items = (0..5)
        .map(|n| {
            mail(
                &format!("bot{n}"),
                "INBOX",
                n + 1,
                &[
                    "From: Report <report@example.test>",
                    "To: me@example.test",
                    &format!("Subject: 日報 {n}"),
                    "Precedence: bulk",
                ],
                "",
            )
        })
        .collect::<Vec<_>>();
    items.push(mail(
        "h1",
        "INBOX",
        10,
        &[
            "From: Carol <carol@example.test>",
            "To: me@example.test",
            "Subject: 問題",
        ],
        "",
    ));
    let graph = build_with(items, ME, now(), &args());
    assert_eq!(graph.people[0].email, "carol@example.test");
    assert!(!graph.people[0].automated);
    assert!(graph.people[1].automated);
    assert!(render_markdown(&graph).contains("report@example.test>（自動）"));
}

#[test]
fn configured_senders_without_automation_headers_are_treated_as_automated() {
    let item = |id: &str, sender: &str| {
        mail(
            id,
            "INBOX",
            1,
            &[
                &format!("From: {sender}"),
                "To: me@example.test",
                &format!("Subject: {id}"),
            ],
            "",
        )
    };
    let items = vec![
        item("by-local", "mis@corp.test"),
        item("by-domain", "billing@vendor.test"),
        item("by-address", "Ops@Example.test"),
        item("human", "dana@corp.test"),
    ];
    let senders = [
        "MIS".to_owned(),
        "@vendor.test".to_owned(),
        "ops@example.test".to_owned(),
    ];
    let graph = build(items.clone(), ME, &senders, now(), &args());
    let status = |subject: &str| {
        graph
            .threads
            .iter()
            .find(|t| t.subject == subject)
            .unwrap()
            .status
    };
    assert_eq!(status("by-local"), ThreadStatus::Fyi);
    assert_eq!(status("by-domain"), ThreadStatus::Fyi);
    assert_eq!(status("by-address"), ThreadStatus::Fyi);
    assert_eq!(
        status("human"),
        ThreadStatus::AwaitingMe,
        "same domain as a listed local part is not enough"
    );
    assert_eq!(graph.people[0].email, "dana@corp.test");
    assert!(graph.people[1..].iter().all(|person| person.automated));

    let unlisted = build(items, ME, &[], now(), &args());
    assert_eq!(
        unlisted
            .threads
            .iter()
            .filter(|t| t.status == ThreadStatus::AwaitingMe)
            .count(),
        4
    );
}
