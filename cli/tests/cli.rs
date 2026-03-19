use assert_cmd::Command;
use predicates::str::contains;
use tempfile::tempdir;

fn binary() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin!("test-runner").to_path_buf()
}

#[test]
fn init_creates_scaffold() {
    let temp = tempdir().expect("tempdir");

    Command::new(binary())
        .args(["init", "--root", temp.path().to_str().expect("utf8")])
        .assert()
        .success()
        .stdout(contains("Initialized .testrunner"));

    assert!(temp.path().join(".testrunner/project.yaml").exists());
}

#[test]
fn dry_run_works_after_init() {
    let temp = tempdir().expect("tempdir");

    Command::new(binary())
        .args(["init", "--root", temp.path().to_str().expect("utf8")])
        .assert()
        .success();

    Command::new(binary())
        .current_dir(temp.path())
        .args(["test", "all", "--dry-run"])
        .assert()
        .success()
        .stdout(contains("Selected 2 case(s)"));
}
