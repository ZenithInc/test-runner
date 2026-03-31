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

#[test]
fn unknown_top_level_case_field_reports_clear_error() {
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
name: extra top field
api: user/get-user
bogus_top_level: 123
steps:
  - sleep:
      ms: 1
"#,
    )
    .expect("write invalid case");

    Command::new(binary())
        .current_dir(temp.path())
        .args(["test", "all", "--dry-run"])
        .assert()
        .failure()
        .stderr(contains("failed to parse"))
        .stderr(contains("bogus_top_level"));
}

#[test]
fn unknown_request_field_reports_clear_error() {
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
name: extra request field
api: user/get-user
steps:
  - request:
      api: user/get-user
      bogus_request_key: 1
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
        .stderr(contains("bogus_request_key"));
}

#[test]
fn sql_step_rejects_sql_and_file_together() {
    let temp = tempdir().expect("tempdir");

    Command::new(binary())
        .args(["init", "--root", temp.path().to_str().expect("utf8")])
        .assert()
        .success();

    let datasources_dir = temp.path().join(".testrunner/datasources");
    fs::create_dir_all(&datasources_dir).expect("create datasources dir");
    fs::write(
        datasources_dir.join("mysql.yaml"),
        r#"
datasources:
  mysql.main:
    kind: mysql
    url: mysql://root:password@127.0.0.1:3306/app
"#,
    )
    .expect("write datasource config");

    let bad_case = temp
        .path()
        .join(".testrunner/cases/user/get-user/smoke.yaml");
    fs::write(
        &bad_case,
        r#"
name: invalid sql
api: user/get-user
steps:
  - sql:
      datasource: mysql.main
      sql: "select 1"
      file: data/sql/check.sql
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
        .stderr(contains("exactly one of `sql` or `file`"));
}

#[test]
fn unknown_case_api_is_reported_during_load() {
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
name: missing api
api: user/not-found
steps:
  - sleep:
      ms: 1
"#,
    )
    .expect("write invalid case");

    Command::new(binary())
        .current_dir(temp.path())
        .args(["test", "all", "--dry-run"])
        .assert()
        .failure()
        .stderr(contains("invalid case definition"))
        .stderr(contains("case.api references unknown API `user/not-found`"));
}

#[test]
fn workflow_unknown_case_reference_reports_clear_error() {
    let temp = tempdir().expect("tempdir");

    Command::new(binary())
        .args(["init", "--root", temp.path().to_str().expect("utf8")])
        .assert()
        .success();

    let workflows_dir = temp.path().join(".testrunner/workflows");
    fs::create_dir_all(&workflows_dir).expect("create workflows dir");
    fs::write(
        workflows_dir.join("broken.yaml"),
        r#"
name: broken workflow
steps:
  - run_case:
      id: step-1
      case: user/get-user/missing
"#,
    )
    .expect("write invalid workflow");

    Command::new(binary())
        .current_dir(temp.path())
        .args(["test", "workflow", "--all", "--dry-run"])
        .assert()
        .failure()
        .stderr(contains("invalid workflow definition"))
        .stderr(contains(
            "steps[0].run_case.case references unknown case `user/get-user/missing`",
        ));
}

#[test]
fn runtime_if_condition_failure_reports_step_path_and_reason() {
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
name: bad if
api: user/get-user
steps:
  - if: "${missing_flag == true}"
    then:
      - sleep:
          ms: 1
"#,
    )
    .expect("write invalid runtime case");

    Command::new(binary())
        .current_dir(temp.path())
        .args([
            "test",
            "api",
            "user/get-user",
            "--root",
            temp.path().to_str().expect("utf8"),
        ])
        .assert()
        .failure()
        .stdout(contains("steps[0] `if` step failed"))
        .stdout(contains("failed to evaluate if condition"))
        .stdout(contains("missing_flag"))
        .stderr(contains("1 of 1 case(s) failed"));
}

#[test]
fn runtime_request_assertion_failure_reports_missing_path_details() {
    let temp = tempdir().expect("tempdir");

    Command::new(binary())
        .args(["init", "--root", temp.path().to_str().expect("utf8")])
        .assert()
        .success();

    fs::write(
        temp.path().join(".testrunner/project.yaml"),
        r#"
version: 1
project:
  name: cli-runtime-test
defaults:
  env: local
  execution_mode: serial
  timeout_ms: 30000
mock:
  enabled: true
  host: 127.0.0.1
  port: 18091
"#,
    )
    .expect("write project config");
    fs::write(
        temp.path().join(".testrunner/env/local.yaml"),
        "base_url: http://127.0.0.1:18091\n",
    )
    .expect("write local env");
    fs::write(
        temp.path().join(".testrunner/apis/user/get-user.yaml"),
        r#"
name: Mock user profile
method: GET
path: /profiles/u-001
headers:
  accept: application/json
query: {}
"#,
    )
    .expect("write mock api");
    fs::write(
        temp.path()
            .join(".testrunner/cases/user/get-user/smoke.yaml"),
        r#"
name: bad assertion
api: user/get-user
steps:
  - request:
      api: user/get-user
    assert:
      - eq: [response.json.missing_name, Alice Runner]
"#,
    )
    .expect("write bad assertion case");

    Command::new(binary())
        .current_dir(temp.path())
        .args([
            "test",
            "api",
            "user/get-user",
            "--root",
            temp.path().to_str().expect("utf8"),
        ])
        .assert()
        .failure()
        .stdout(contains("steps[0] `request` step failed"))
        .stdout(contains("request assertions failed"))
        .stdout(contains(
            "failed to evaluate assertion #1 `eq(response.json.missing_name, Alice Runner)`",
        ))
        .stdout(contains(
            "could not find key `missing_name` under `response.json`",
        ))
        .stdout(contains("available keys: [display_name, id]"))
        .stderr(contains("1 of 1 case(s) failed"));
}
