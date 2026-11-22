use assert_cmd::Command;
use m2k::{
    config::{self, Config},
    model::{Item, Kind, Snapshot},
    store::Store,
    trust,
};
use serde_json::Value;

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    config::save(
        dir.path(),
        &Config::new("me@example.test".into(), "mail.example.test").unwrap(),
    )
    .unwrap();
    dir
}
fn mail(id: &str, headers: &str) -> Item {
    Item {
        id: id.into(),
        kind: Kind::Mail,
        collection: "INBOX".into(),
        remote_id: id.into(),
        etag: None,
        uid_validity: Some(1),
        title: "Trust fixture".into(),
        subtitle: "unreliable@example.test".into(),
        date: Some(chrono::Utc::now().to_rfc3339()),
        end_date: None,
        text: "SECRET_BODY".into(),
        raw: format!(
            "{headers}\r\nSubject: Trust fixture\r\nContent-Type: text/plain\r\n\r\nSECRET_BODY"
        ),
        flags: vec![],
        emails: vec![],
        phones: vec![],
        attachments: vec![],
    }
}
fn cli(dir: &std::path::Path, args: &[&str]) -> Value {
    let out = Command::cargo_bin("m2k")
        .unwrap()
        .arg("--data-dir")
        .arg(dir)
        .arg("--json")
        .args(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}
#[test]
fn identity_domain_boundaries_and_idna() {
    let dir = fixture();
    for headers in [
        "From: a@EXAMPLE.TEST",
        "From: a@sub.example.test\r\nReturn-Path: <b@example.test>",
        "From: a@example.test\r\nReturn-Path: <b@sub.example.test>",
    ] {
        assert!(trust::check(dir.path(), &mail("1", headers))
            .unwrap()
            .reason
            .is_none());
    }
    for headers in [
        "From: a@example.test, b@example.test",
        "From: a@example.test\r\nFrom: b@example.test",
        "From: a@example.test\r\nReturn-Path: <b@evil.test>",
        "From: a@example.test\r\nReturn-Path: <>",
        "Subject: no from",
        "From: a@example.test\r\nReturn-Path: <b@example.test>\r\nReturn-Path: <c@example.test>",
    ] {
        assert_eq!(
            trust::check(dir.path(), &mail("1", headers))
                .unwrap()
                .reason,
            Some("identity_unverified")
        );
    }
    assert_eq!(
        trust::check(
            dir.path(),
            &mail(
                "1",
                "From: a@evilexample.test\r\nAuthentication-Results: trusted; dkim=pass"
            )
        )
        .unwrap()
        .reason,
        Some("domain_untrusted")
    );
    assert_eq!(
        trust::domain("例子.公司").unwrap(),
        "xn--fsqu00a.xn--55qx5d"
    );
    for invalid in [
        "https://example.com",
        "a@example.com",
        "*.example.com",
        "a..com",
        "-a.com",
        "a/b",
        "",
    ] {
        assert!(trust::domain(invalid).is_err());
    }
}
#[test]
fn cli_all_body_exits_revoke_and_account_isolation() {
    let dir = fixture();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .ensure_account(&config::load(dir.path()).unwrap())
        .unwrap();
    let items = vec![
        mail("1", "From: a@vendor.test"),
        mail("2", "From: a@example.test"),
        mail("3", "From: a@example.test\r\nReturn-Path: <a@evil.test>"),
    ];
    store
        .apply_snapshot(
            &Snapshot {
                kind: Kind::Mail,
                collections: vec![],
                items,
            },
            chrono::Utc::now(),
        )
        .unwrap();
    for args in [
        vec!["mail", "list", "--full"],
        vec!["mail", "search", "SECRET_BODY", "--full"],
        vec!["mail", "thread", "--id", "1", "--full"],
    ] {
        let value = cli(dir.path(), &args);
        assert_eq!(
            value["needs_trust_domains"],
            serde_json::json!(["vendor.test"])
        );
        assert_eq!(value["identity_review_ids"], serde_json::json!(["3"]));
        for item in value["items"].as_array().unwrap() {
            if item["id"] == "2" {
                assert!(item["text"].as_str().unwrap().contains("SECRET_BODY"));
            } else {
                assert!(item.get("text").is_none());
                assert!(item.get("raw").is_none());
            }
        }
    }
    for extra in [vec![], vec!["--raw"], vec!["--full"]] {
        let mut args = vec!["mail", "show", "1"];
        args.extend(extra);
        let value = cli(dir.path(), &args);
        assert_eq!(value["body_withheld"], "domain_untrusted");
        assert_eq!(value["subtitle"], "a@vendor.test");
        assert!(!value.to_string().contains("SECRET_BODY"));
    }
    for args in [
        vec!["mail", "show", "1", "--raw"],
        vec!["mail", "thread", "--id", "1"],
        vec!["mail", "graph"],
    ] {
        let output = Command::cargo_bin("m2k")
            .unwrap()
            .arg("--data-dir")
            .arg(dir.path())
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        assert!(!String::from_utf8(output).unwrap().contains("SECRET_BODY"));
    }
    let graph = cli(dir.path(), &["mail", "graph"]);
    assert!(!graph.to_string().contains("SECRET_BODY")); // latest is inconsistent identity
    cli(dir.path(), &["trust", "add", "VENDOR.TEST"]);
    assert_eq!(
        cli(dir.path(), &["trust", "list"])["domains"],
        serde_json::json!(["vendor.test"])
    );
    assert!(cli(dir.path(), &["mail", "show", "1", "--raw"])["raw"]
        .as_str()
        .unwrap()
        .contains("SECRET_BODY"));
    cli(dir.path(), &["trust", "remove", "vendor.test"]);
    assert!(cli(dir.path(), &["mail", "show", "1"])["text"].is_null());
    assert!(store.get("1").unwrap().raw.contains("SECRET_BODY"));
    cli(dir.path(), &["trust", "add", "vendor.test"]);
    config::save(
        dir.path(),
        &Config::new("other@different.test".into(), "mail.different.test").unwrap(),
    )
    .unwrap();
    assert_eq!(
        cli(dir.path(), &["trust", "list"])["domains"],
        serde_json::json!([])
    );
    assert_eq!(
        trust::check(dir.path(), &store.get("1").unwrap())
            .unwrap()
            .reason,
        Some("domain_untrusted")
    );
}
#[test]
fn corruption_fails_closed_and_subdomains_revoke_immediately() {
    let dir = fixture();
    let item = mail("1", "From: a@sub.vendor.test");
    trust::execute(
        dir.path(),
        trust::Command::Add {
            domain: "vendor.test".into(),
        },
    )
    .unwrap();
    assert!(trust::check(dir.path(), &item).unwrap().reason.is_none());
    trust::execute(
        dir.path(),
        trust::Command::Remove {
            domain: "vendor.test".into(),
        },
    )
    .unwrap();
    assert_eq!(
        trust::check(dir.path(), &item).unwrap().reason,
        Some("domain_untrusted")
    );
    std::fs::write(dir.path().join("trust/domains.json"), b"invalid json").unwrap();
    assert!(trust::check(dir.path(), &item).is_err());
    assert!(trust::check(dir.path(), &mail("2", "From: a@example.test")).is_err());
    Command::cargo_bin("m2k")
        .unwrap()
        .arg("--data-dir")
        .arg(dir.path())
        .args(["--json", "trust", "list"])
        .assert()
        .failure();
}

#[test]
fn attachment_download_stays_available_for_untrusted_mail() {
    let dir = tempfile::tempdir().unwrap();
    m2k::demo::seed_without_rooms(dir.path()).unwrap();
    let mut store = Store::open(dir.path()).unwrap();
    let mut item = store.get("demo-mail-1").unwrap();
    item.raw = item
        .raw
        .replace("From: team@example.test", "From: team@vendor.test");
    store.upsert(&item).unwrap();
    let show = cli(dir.path(), &["mail", "show", "demo-mail-1"]);
    assert_eq!(show["body_withheld"], "domain_untrusted");
    let output = dir.path().join("attachment.txt");
    cli(
        dir.path(),
        &[
            "mail",
            "attachment",
            "demo-mail-1",
            "--index",
            "0",
            "--output",
            output.to_str().unwrap(),
        ],
    );
    assert!(std::fs::read_to_string(output)
        .unwrap()
        .contains("Offline attachment demo"));
}

#[test]
fn graph_omits_untrusted_excerpt_and_restores_it_only_after_explicit_trust() {
    let dir = fixture();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .ensure_account(&config::load(dir.path()).unwrap())
        .unwrap();
    store.upsert(&mail("1", "From: a@vendor.test")).unwrap();
    let graph = cli(dir.path(), &["mail", "graph"]);
    assert_eq!(
        graph["needs_trust_domains"],
        serde_json::json!(["vendor.test"])
    );
    assert_eq!(graph["threads"][0]["body_withheld"], "domain_untrusted");
    assert!(graph["threads"][0].get("latest_excerpt").is_none());
    cli(dir.path(), &["trust", "add", "vendor.test"]);
    assert!(
        cli(dir.path(), &["mail", "graph"])["threads"][0]["latest_excerpt"]
            .as_str()
            .unwrap()
            .contains("SECRET_BODY")
    );
}

#[test]
fn list_keeps_metadata_for_malformed_identity_and_sorts_unique_domains() {
    let dir = fixture();
    let mut store = Store::open(dir.path()).unwrap();
    store
        .ensure_account(&config::load(dir.path()).unwrap())
        .unwrap();
    for (id, from) in [
        ("1", "z.vendor.test"),
        ("2", "a.vendor.test"),
        ("3", "z.vendor.test"),
    ] {
        store.upsert(&mail(id, &format!("From: a@{from}"))).unwrap();
    }
    let mut broken = mail("4", "From: a@example.test");
    broken.raw = "base64:invalid".into();
    store.upsert(&broken).unwrap();
    let value = cli(dir.path(), &["mail", "list"]);
    assert_eq!(
        value["needs_trust_domains"],
        serde_json::json!(["a.vendor.test", "z.vendor.test"])
    );
    assert_eq!(value["identity_review_ids"], serde_json::json!(["4"]));
    assert_eq!(value["items"].as_array().unwrap().len(), 4);
    assert!(!value.to_string().contains("SECRET_BODY"));
}
