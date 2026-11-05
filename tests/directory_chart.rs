use chrono::Utc;
use m2k::directory::{address_book_tree, merge_contact_items};
use m2k::directory_chart::{build, render_markdown, render_mermaid, OrgChart};
use m2k::model::{Collection, Item, Kind, Snapshot};
use m2k::store::Store;
use std::process::Command;
use tempfile::TempDir;

const BASE: &str = "https://mail.example.test/carddav/";

fn collection(path: &str) -> Collection {
    let name = path.rsplit('/').next().unwrap();
    Collection {
        kind: Kind::Contacts,
        id: format!("{BASE}{path}/"),
        name: format!("{name}(/{path})"),
    }
}

fn contact(id: &str, path: &str, uid: &str, name: &str, extra: &str) -> Item {
    let email = format!("{uid}@example.test");
    Item {
        id: id.into(),
        kind: Kind::Contacts,
        collection: format!("{BASE}{path}/"),
        remote_id: format!("{id}.vcf"),
        etag: None,
        uid_validity: None,
        title: name.into(),
        subtitle: String::new(),
        date: None,
        end_date: None,
        text: String::new(),
        raw: format!(
            "BEGIN:VCARD\nVERSION:3.0\nUID:{uid}\nFN:{name}\nNICKNAME:{uid}\nORG:;;\n{extra}EMAIL:{email}\nEND:VCARD\n"
        ),
        flags: vec![],
        emails: vec![email],
        phones: vec![],
        attachments: vec![],
    }
}

/// ACME 是全公司名單（每個人都在），ENG／ENG/TOOLS／MKT 才是部門；集合只放直屬成員。
fn directory() -> (Vec<Collection>, Vec<Item>) {
    let collections = ["ACME", "ACME/ENG", "ACME/ENG/TOOLS", "ACME/MKT"]
        .into_iter()
        .map(collection)
        .collect();
    let items = vec![
        contact("1", "ACME", "ceo", "王大同", ""),
        contact("2", "ACME", "lin", "林工程", ""),
        contact("3", "ACME/ENG", "lin", "林工程", "TITLE:協理\n"),
        contact("4", "ACME", "chen", "陳工具", ""),
        contact("5", "ACME/ENG/TOOLS", "chen", "陳工具", ""),
        // 一人兼兩個部門，兩邊都要列出，但總人數只算一次。
        contact("6", "ACME", "wu", "吳兼任", ""),
        contact("7", "ACME/ENG/TOOLS", "wu", "吳兼任", ""),
        contact("8", "ACME/MKT", "wu", "吳兼任", ""),
        contact("9", "ACME", "mkt", "許行銷", ""),
        contact("10", "ACME/MKT", "mkt", "許行銷", ""),
    ];
    (collections, items)
}

fn chart(collections: &[Collection], items: &[Item], branch: Option<&str>) -> OrgChart {
    let nodes = address_book_tree(collections, Some(BASE));
    let contacts = merge_contact_items(items, &nodes);
    build(&nodes, &contacts, branch).unwrap()
}

fn names(members: &[m2k::directory_chart::Member]) -> Vec<&str> {
    members.iter().map(|member| member.name.as_str()).collect()
}

#[test]
fn chart_is_identical_regardless_of_collection_and_contact_order() {
    // 代理程式拿圖當脈絡，同一份快取每次都要得到逐字相同的輸出，否則會被誤判成組織異動。
    let (collections, items) = directory();
    let expected = chart(&collections, &items, None);
    let mut reversed_collections = collections.clone();
    reversed_collections.reverse();
    let mut reversed_items = items.clone();
    reversed_items.reverse();
    let mut rotated_items = items.clone();
    rotated_items.rotate_left(4);
    for (cols, its) in [
        (&reversed_collections, &reversed_items),
        (&collections, &rotated_items),
    ] {
        let nodes = address_book_tree(cols, Some(BASE));
        let mut contacts = merge_contact_items(its, &nodes);
        // merge_contact_items 本身已排序；直接打亂 build 的輸入，才驗得到 build 自己的排序。
        contacts.reverse();
        let mut nodes = nodes;
        nodes.reverse();
        let actual = build(&nodes, &contacts, None).unwrap();
        assert_eq!(
            serde_json::to_string(&actual).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
        assert_eq!(render_markdown(&actual), render_markdown(&expected));
        assert_eq!(render_mermaid(&actual), render_mermaid(&expected));
    }
}

#[test]
fn members_sit_in_their_departments_not_in_the_company_wide_list() {
    let (collections, items) = directory();
    let chart = chart(&collections, &items, None);
    assert_eq!(chart.departments.len(), 1);
    let acme = &chart.departments[0];
    // 只在全公司名單、沒有部門的人才留在最上層。
    assert_eq!(names(&acme.members), ["王大同"]);
    let child_paths = acme
        .children
        .iter()
        .map(|department| department.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(child_paths, ["ACME/ENG", "ACME/MKT"]);
    let eng = &acme.children[0];
    assert_eq!(names(&eng.members), ["林工程"]);
    assert_eq!(eng.members[0].title.as_deref(), Some("協理"));
    assert_eq!(names(&eng.children[0].members), ["吳兼任", "陳工具"]);
    assert_eq!(names(&acme.children[1].members), ["吳兼任", "許行銷"]);
    assert_eq!(chart.department_count, 4);
    assert_eq!(chart.member_count, 5);
}

#[test]
fn branch_keeps_only_that_subtree() {
    let (collections, items) = directory();
    let eng = chart(&collections, &items, Some("/ACME/ENG"));
    assert_eq!(eng.branch.as_deref(), Some("ACME/ENG"));
    assert_eq!(eng.departments.len(), 1);
    assert_eq!(eng.departments[0].path, "ACME/ENG");
    assert_eq!(eng.departments[0].children[0].path, "ACME/ENG/TOOLS");
    assert_eq!(eng.department_count, 2);
    assert_eq!(eng.member_count, 3);
    let markdown = render_markdown(&eng);
    assert!(markdown.contains("吳兼任"));
    assert!(
        !markdown.contains("許行銷"),
        "sibling branch leaked: {markdown}"
    );
    assert!(
        !markdown.contains("王大同"),
        "company list leaked: {markdown}"
    );

    // 用部門名稱指定也行，結果與 path 相同。
    let by_name = chart(&collections, &items, Some("TOOLS"));
    assert_eq!(by_name.branch.as_deref(), Some("ACME/ENG/TOOLS"));
    assert_eq!(names(&by_name.departments[0].members), ["吳兼任", "陳工具"]);

    let nodes = address_book_tree(&collections, Some(BASE));
    let contacts = merge_contact_items(&items, &nodes);
    let error = build(&nodes, &contacts, Some("ACME/SALES")).unwrap_err();
    assert!(error.to_string().contains("ACME/SALES"));
}

#[test]
fn outputs_say_there_are_no_reporting_lines_and_draw_department_edges() {
    // vCard 沒有主管欄位；圖若不講明，代理程式會把部門樹誤讀成匯報關係。
    let (collections, items) = directory();
    let chart = chart(&collections, &items, None);
    let json = serde_json::to_value(&chart).unwrap();
    assert!(json["reporting_note"]
        .as_str()
        .unwrap()
        .contains("不含匯報線"));
    assert!(render_markdown(&chart).contains("不含匯報線"));
    let mermaid = render_mermaid(&chart);
    assert!(mermaid.starts_with("flowchart TD\n"));
    assert!(mermaid.contains("d0[\"ACME<br/>1 人\"]"));
    assert!(mermaid.contains("d0 --> d1"));
    assert!(mermaid.contains("d1 --> d2"));
    assert!(mermaid.contains("d0 --> d3"));
}

#[test]
fn cli_reads_only_the_local_cache() {
    // 沒有 config 也沒有密碼：指令若嘗試連線就會失敗，成功即代表只讀本機。
    let dir = TempDir::new().unwrap();
    let (collections, items) = directory();
    Store::open(dir.path())
        .unwrap()
        .upsert_snapshot(
            &Snapshot {
                kind: Kind::Contacts,
                collections,
                items,
            },
            Utc::now(),
        )
        .unwrap();
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_m2k"))
            .arg("--data-dir")
            .arg(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let markdown = run(&["directory", "chart", "--branch", "ACME/MKT"]);
    assert!(markdown.starts_with("# 部門組織圖"));
    assert!(markdown.contains("  - 許行銷 · mkt · mkt@example.test"));
    let json: serde_json::Value =
        serde_json::from_str(&run(&["--json", "directory", "chart"])).unwrap();
    assert_eq!(json["member_count"], 5);
    assert!(run(&["directory", "chart", "--format", "mermaid"]).starts_with("flowchart TD"));
}

/// Mail2000 的實際形狀：FN 與 NICKNAME 都是帳號，中文全名只在 N 的名字欄。
fn mail2000_contact(id: &str, path: &str, uid: &str, full_name: &str, phone: &str) -> Item {
    let mut item = contact(id, path, uid, uid, &format!("N:;{full_name};;;\n"));
    if !phone.is_empty() {
        item.phones = vec![phone.into()];
    }
    item
}

#[test]
fn members_show_the_chinese_full_name_not_just_the_account() {
    // 只有帳號時代理程式對不上信件裡的中文署名；全名在 vCard N，帳號仍要留著才能寄信。
    let collections = ["ACME", "ACME/ENG"]
        .into_iter()
        .map(collection)
        .collect::<Vec<_>>();
    let mut other = mail2000_contact("3", "ACME/ENG", "lee", "李外包", "0912345678");
    other.raw = other
        .raw
        .replace("EMAIL:lee@example.test", "EMAIL:lee@partner.test");
    other.emails = vec!["lee@partner.test".into()];
    let items = vec![
        mail2000_contact("1", "ACME/ENG", "ming_wang", "王小明", "1234"),
        mail2000_contact("2", "ACME/ENG", "nobody", "", ""),
        other,
    ];
    let chart = chart(&collections, &items, None);
    let eng = &chart.departments[0].children[0];
    let ming = eng.members.iter().find(|m| m.name == "王小明").unwrap();
    assert_eq!(ming.username.as_deref(), Some("ming_wang"));
    assert_eq!(
        ming.nickname, None,
        "NICKNAME equal to the account is noise"
    );
    assert_eq!(ming.phones, ["1234"]);
    let nobody = eng.members.iter().find(|m| m.name == "nobody").unwrap();
    assert_eq!(
        nobody.username, None,
        "without N the account is the name, not repeated"
    );
    assert_eq!(chart.email_domain.as_deref(), Some("example.test"));

    let markdown = render_markdown(&chart);
    assert!(markdown.contains("- Email：括號內帳號 @example.test"));
    assert!(
        markdown.contains("  - 王小明（ming_wang） · 分機 1234\n"),
        "{markdown}"
    );
    assert!(markdown.contains("  - nobody\n"), "{markdown}");
    // 網域不同的 email 推不出來，必須照列。
    assert!(
        markdown.contains("  - 李外包（lee） · 電話 0912345678 · lee@partner.test\n"),
        "{markdown}"
    );
    assert!(!markdown.contains("ming_wang@example.test"));
}
