use assert_cmd::Command;
use m2k::skills::{self, M2K_SKILL};
use predicates::str::contains;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_m2k"))
}

#[test]
fn skill_installer_is_discoverable_from_cli_help() {
    command()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("skills"));
    command()
        .args(["skills", "--help"])
        .assert()
        .success()
        .stdout(contains("install"));
    let output = command()
        .args(["skills", "install", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let help = String::from_utf8(output).unwrap();
    assert!(help.contains("~/.agents/skills"));
    assert!(help.contains("--path"));
    assert!(help.contains("--dest"));
    assert!(help.contains("m2k/SKILL.md"));
}

#[test]
fn install_prints_path_and_needs_no_account_configuration() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("custom agent skills");
    let data_dir = dir.path().join("unused-account");
    let expected = root.join("m2k/SKILL.md");

    let output = command()
        .arg("--data-dir")
        .arg(&data_dir)
        .args(["skills", "install", "--path"])
        .arg(&root)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(output).unwrap().trim(),
        expected.to_str().unwrap()
    );
    assert_eq!(fs::read_to_string(&expected).unwrap(), M2K_SKILL);
    assert!(!data_dir.exists());
}

#[test]
fn repeated_cli_install_is_idempotent_and_dest_alias_uses_same_root() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join(".cursor/skills");
    let first = skills::install(Some(&root)).unwrap();
    assert!(first.changed);
    let modified = fs::metadata(&first.path).unwrap().modified().unwrap();

    let output = command()
        .args(["--json", "skills", "install", "--dest"])
        .arg(&root)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let result: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(result["path"], first.path.to_str().unwrap());
    assert_eq!(result["changed"], false);
    assert_eq!(
        fs::metadata(&first.path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(fs::read_to_string(first.path).unwrap(), M2K_SKILL);
}

#[test]
fn install_updates_bundled_skill_and_preserves_other_files() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("skills");
    let folder = root.join("m2k");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("SKILL.md"), "older bundled skill").unwrap();
    fs::write(folder.join("notes.txt"), "personal notes").unwrap();
    fs::create_dir(root.join("another-skill")).unwrap();
    fs::write(root.join("another-skill/SKILL.md"), "other skill").unwrap();

    let result = skills::install(Some(&root)).unwrap();
    assert!(result.changed);
    assert_eq!(result.path, folder.join("SKILL.md"));
    assert_eq!(fs::read_to_string(&result.path).unwrap(), M2K_SKILL);
    assert_eq!(
        fs::read_to_string(folder.join("notes.txt")).unwrap(),
        "personal notes"
    );
    assert_eq!(
        fs::read_to_string(root.join("another-skill/SKILL.md")).unwrap(),
        "other skill"
    );
    assert!(!skills::install(Some(&root)).unwrap().changed);
}

#[test]
fn install_fails_cleanly_when_destination_is_a_file() {
    let dir = TempDir::new().unwrap();
    let destination = dir.path().join("not-a-directory");
    fs::write(&destination, "keep this file").unwrap();
    command()
        .args(["skills", "install", "--path"])
        .arg(&destination)
        .assert()
        .failure()
        .stdout("")
        .stderr(contains("錯誤"));
    assert_eq!(fs::read_to_string(destination).unwrap(), "keep this file");
}
