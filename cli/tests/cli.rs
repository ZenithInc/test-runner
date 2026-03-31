use assert_cmd::Command;
use predicates::str::contains;
use std::fs;
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

#[test]
fn schema_command_prints_case_schema() {
    Command::new(binary())
        .args(["schema", "case"])
        .assert()
        .success()
        .stdout(contains("\"$schema\""))
        .stdout(contains("\"title\": \"CaseFile\""))
        .stdout(contains("\"steps\""));
}

#[test]
fn schema_command_writes_all_schema_files() {
    let temp = tempdir().expect("tempdir");

    Command::new(binary())
        .args([
            "schema",
            "all",
            "--output",
            temp.path().to_str().expect("utf8"),
        ])
        .assert()
        .success()
        .stdout(contains("Wrote 7 schema file(s)"));

    assert!(temp.path().join("project.schema.json").exists());
    assert!(temp.path().join("environment.schema.json").exists());
    assert!(temp.path().join("datasources.schema.json").exists());
    assert!(temp.path().join("api.schema.json").exists());
    assert!(temp.path().join("case.schema.json").exists());
    assert!(temp.path().join("workflow.schema.json").exists());
    assert!(temp.path().join("mock-route.schema.json").exists());
}

#[test]
fn invalid_step_shape_reports_path_and_supported_keys() {
    let temp = tempdir().expect("tempdir");

    Command::new(binary())
        .args(["init", "--root", temp.path().to_str().expect("utf8")])
        .assert()
        .success();

    let bad_case = temp
        .path()
        .join(".testrunner/cases/user/get-user/smoke.yaml");
    fs::write(
        &bad_case,
        r#"
name: invalid step
api: user/get-user
steps:
  - nope:
      value: 1
"#,
    )
    .expect("write invalid case");

    Command::new(binary())
        .current_dir(temp.path())
        .args(["test", "all", "--dry-run"])
        .assert()
        .failure()
        .stderr(contains("failed to parse"))
        .stderr(contains("steps[0]"))
        .stderr(contains("expected exactly one of [use_data, set, sql, redis, request, callback, sleep, query_db, query_redis, if, foreach]"));
}
