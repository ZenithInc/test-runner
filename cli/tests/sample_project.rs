use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use redis::AsyncCommands;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tempfile::tempdir;
use tokio::time::sleep;

fn binary() -> PathBuf {
    assert_cmd::cargo::cargo_bin!("test-runner").to_path_buf()
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("cli crate should live under the workspace root")
        .to_path_buf()
}

fn sample_project_root() -> PathBuf {
    workspace_root().join("sample-projects")
}

fn sample_project_docker_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .expect("sample project docker lock")
}

#[tokio::test]
async fn sample_health_service_passes_health_case() {
    let temp = tempdir().expect("tempdir");
    let runner_root = temp.path();

    let port = reserve_port();
    prepare_it_runner(runner_root, port).expect("prepare runner root");

    let mut service = ChildGuard(start_sample_service(port));
    wait_for_service(&mut service.0, port).await;

    Command::new(binary())
        .args([
            "test",
            "api",
            "system/health",
            "--root",
            runner_root.to_str().expect("utf8"),
            "--env",
            "it",
            "--no-mock",
        ])
        .assert()
        .success()
        .stdout(contains(
            "==> Running 1 case(s) for api `system/health` in env `it`",
        ))
        .stdout(contains("PASS [1/1] system/health/smoke"))
        .stdout(contains("Cases: 1 passed, 0 failed, 1 total"));
}

#[tokio::test]
async fn sample_health_service_passes_order_expression_case() {
    let temp = tempdir().expect("tempdir");
    let runner_root = temp.path();

    let port = reserve_port();
    prepare_it_runner(runner_root, port).expect("prepare runner root");

    let mut service = ChildGuard(start_sample_service(port));
    wait_for_service(&mut service.0, port).await;

    Command::new(binary())
        .args([
            "test",
            "api",
            "order/create",
            "--root",
            runner_root.to_str().expect("utf8"),
            "--env",
            "it",
            "--no-mock",
        ])
        .assert()
        .success()
        .stdout(contains(
            "==> Running 1 case(s) for api `order/create` in env `it`",
        ))
        .stdout(contains("PASS [1/1] order/create/expression-happy-path"))
        .stdout(contains("Cases: 1 passed, 0 failed, 1 total"));
}

#[tokio::test]
async fn sample_project_all_cases_pass() {
    let temp = tempdir().expect("tempdir");
    let runner_root = temp.path();

    let app_port = reserve_port();
    let mock_port = reserve_port();
    let mysql_port = reserve_port();
    let redis_port = reserve_port();
    prepare_full_suite_runner(runner_root, app_port, mock_port, mysql_port, redis_port)
        .expect("prepare full-suite runner root");

    let _dependency_stack = start_dependency_stack(temp.path(), mysql_port, redis_port);
    let mut service = ChildGuard(start_sample_service_with_env(
        app_port,
        &[
            (
                "DATABASE_URL".to_string(),
                format!("mysql://app:app@127.0.0.1:{mysql_port}/app"),
            ),
            (
                "REDIS_URL".to_string(),
                format!("redis://127.0.0.1:{redis_port}/0"),
            ),
            (
                "SMS_PROVIDER_BASE_URL".to_string(),
                format!("http://127.0.0.1:{mock_port}"),
            ),
        ],
    ));
    wait_for_service(&mut service.0, app_port).await;

    Command::new(binary())
        .args([
            "test",
            "all",
            "--root",
            runner_root.to_str().expect("utf8"),
            "--env",
            "it",
        ])
        .assert()
        .success()
        .stdout(contains("==> Running 22 case(s) for all cases in env `it`"))
        .stdout(contains("Cases: 22 passed, 0 failed, 22 total"))
        .stdout(contains("callback/direct-payment-success"))
        .stdout(contains("payment/provider-callback-via-mock"))
        .stdout(contains("system/health/smoke"))
        .stdout(contains("order/create/expression-happy-path"))
        .stdout(contains("order/get/happy-path"))
        .stdout(contains("order/list/filter-pagination"))
        .stdout(contains("order/update/happy-path"))
        .stdout(contains("user/send-sms-code/happy-path"))
        .stdout(contains("user/register/happy-path"))
        .stdout(contains("user/login/happy-path"))
        .stdout(contains("user/login/invalid-sms-code"))
        .stdout(contains("user/me/happy-path"))
        .stdout(contains("workflow/user/me-after-login"))
        .stdout(contains("workflow/user/login-after-register"))
        .stdout(contains("workflow/order/create-after-login"))
        .stdout(contains("workflow/order/get-created-order"))
        .stdout(contains("workflow/order/update-created-order"));
}

#[tokio::test]
async fn sample_project_direct_callback_case_passes() {
    let temp = tempdir().expect("tempdir");
    let runner_root = temp.path();

    let app_port = reserve_port();
    let mysql_port = reserve_port();
    let redis_port = reserve_port();
    prepare_full_suite_runner(
        runner_root,
        app_port,
        reserve_port(),
        mysql_port,
        redis_port,
    )
    .expect("prepare callback runner root");

    let _dependency_stack = start_dependency_stack(temp.path(), mysql_port, redis_port);
    let mut service = ChildGuard(start_sample_service_with_env(
        app_port,
        &[(
            "REDIS_URL".to_string(),
            format!("redis://127.0.0.1:{redis_port}/0"),
        )],
    ));
    wait_for_service(&mut service.0, app_port).await;

    Command::new(binary())
        .args([
            "test",
            "dir",
            "callback",
            "--case",
            "direct-payment-success",
            "--root",
            runner_root.to_str().expect("utf8"),
            "--env",
            "it",
            "--no-mock",
        ])
        .assert()
        .success()
        .stdout(contains("Callbacks: 1 passed, 0 failed, 1 total"))
        .stdout(contains("PASS [1/1] callback/direct-payment-success"))
        .stdout(contains("PASS #1 case:callback/direct-payment-success ->"));
}

#[tokio::test]
async fn sample_project_mock_callback_case_passes() {
    let temp = tempdir().expect("tempdir");
    let runner_root = temp.path();

    let app_port = reserve_port();
    let mock_port = reserve_port();
    let mysql_port = reserve_port();
    let redis_port = reserve_port();
    prepare_full_suite_runner(runner_root, app_port, mock_port, mysql_port, redis_port)
        .expect("prepare mock callback runner root");

    let _dependency_stack = start_dependency_stack(temp.path(), mysql_port, redis_port);
    let mut service = ChildGuard(start_sample_service_with_env(
        app_port,
        &[(
            "REDIS_URL".to_string(),
            format!("redis://127.0.0.1:{redis_port}/0"),
        )],
    ));
    wait_for_service(&mut service.0, app_port).await;

    Command::new(binary())
        .args([
            "test",
            "api",
            "payment/provider/create",
            "--root",
            runner_root.to_str().expect("utf8"),
            "--env",
            "it",
        ])
        .assert()
        .success()
        .stdout(contains("Callbacks: 1 passed, 0 failed, 1 total"))
        .stdout(contains("PASS [1/1] payment/provider-callback-via-mock"))
        .stdout(contains("PASS #1 mock:POST /payments/create ->"));
}

#[test]
fn sample_project_dry_run_lists_all_cases() {
    Command::new(binary())
        .args([
            "test",
            "all",
            "--dry-run",
            "--root",
            sample_project_root().to_str().expect("utf8"),
        ])
        .assert()
        .success()
        .stdout(contains("Selected 22 case(s)"));
}

#[test]
fn workflow_dry_run_lists_steps() {
    Command::new(binary())
        .args([
            "test",
            "workflow",
            "auth-flow",
            "--dry-run",
            "--root",
            sample_project_root().to_str().expect("utf8"),
        ])
        .assert()
        .success()
        .stdout(contains("auth-flow"))
        .stdout(contains("send-sms"))
        .stdout(contains("user/send-sms-code/happy-path"));
}

#[test]
fn workflow_dry_run_lists_register_login_create_order_steps() {
    Command::new(binary())
        .args([
            "test",
            "workflow",
            "register-login-create-order",
            "--dry-run",
            "--root",
            sample_project_root().to_str().expect("utf8"),
        ])
        .assert()
        .success()
        .stdout(contains("register-login-create-order"))
        .stdout(contains("register"))
        .stdout(contains("send-sms"))
        .stdout(contains("login"))
        .stdout(contains("current-profile"))
        .stdout(contains("create-order"))
        .stdout(contains("get-order"))
        .stdout(contains("update-order"));
}

#[test]
fn workflow_dry_run_rejects_tag_flag() {
    Command::new(binary())
        .args([
            "test",
            "workflow",
            "auth-flow",
            "--dry-run",
            "--tag",
            "smoke",
            "--root",
            sample_project_root().to_str().expect("utf8"),
        ])
        .assert()
        .failure()
        .stderr(contains("--tag is not supported for `test workflow`"));
}

#[test]
fn workflow_dry_run_rejects_case_flag() {
    Command::new(binary())
        .args([
            "test",
            "workflow",
            "auth-flow",
            "--dry-run",
            "--case",
            "health",
            "--root",
            sample_project_root().to_str().expect("utf8"),
        ])
        .assert()
        .failure()
        .stderr(contains("--case is not supported for `test workflow`"));
}

#[tokio::test]
async fn workflow_failure_branch_is_executed() {
    let temp = tempdir().expect("tempdir");
    let runner_root = temp.path();
    let mysql_port = reserve_port();
    let redis_port = reserve_port();
    prepare_full_suite_runner(
        runner_root,
        reserve_port(),
        reserve_port(),
        mysql_port,
        redis_port,
    )
    .expect("prepare workflow runner root");
    let _dependency_stack = start_dependency_stack(temp.path(), mysql_port, redis_port);
    write_workflow_cache_cases(runner_root).expect("write workflow cache cases");

    let workflow_dir = runner_root.join(".testrunner/workflows");
    fs::create_dir_all(&workflow_dir).expect("create workflows dir");
    fs::write(
        workflow_dir.join("failure-branch-flow.yaml"),
        concat!(
            "name: failure branch flow\n",
            "steps:\n",
            "  - run_case:\n",
            "      id: seed\n",
            "      case: workflow/cache/seed\n",
            "      cleanup: defer\n",
            "      exports:\n",
            "        cache_key: vars.cache_key\n",
            "        cache_value: vars.cache_value\n",
            "  - run_case:\n",
            "      id: failing-check\n",
            "      case: workflow/cache/assert-wrong\n",
            "      inputs:\n",
            "        cache_key: \"${workflow.steps.seed.exports.cache_key}\"\n",
            "  - if: \"${workflow.steps.failing-check.passed}\"\n",
            "    then:\n",
            "      - run_case:\n",
            "          id: should-not-run\n",
            "          case: workflow/cache/assert-present\n",
            "          inputs:\n",
            "            cache_key: \"${workflow.steps.seed.exports.cache_key}\"\n",
            "            expected_value: \"${workflow.steps.seed.exports.cache_value}\"\n",
            "    else:\n",
            "      - run_case:\n",
            "          id: fallback\n",
            "          case: workflow/cache/assert-present\n",
            "          inputs:\n",
            "            cache_key: \"${workflow.steps.seed.exports.cache_key}\"\n",
            "            expected_value: \"${workflow.steps.seed.exports.cache_value}\"\n",
        ),
    )
    .expect("write failure-branch workflow");

    Command::new(binary())
        .args([
            "test",
            "workflow",
            "failure-branch-flow",
            "--root",
            runner_root.to_str().expect("utf8"),
            "--env",
            "it",
            "--no-mock",
        ])
        .assert()
        .failure()
        .stdout(contains(
            "==> Running workflow `failure-branch-flow` in env `it`",
        ))
        .stdout(contains("Status: FAIL"))
        .stdout(contains("Steps: 2 passed, 1 failed, 3 total"))
        .stdout(contains(
            "FAIL [2] failing-check -> workflow/cache/assert-wrong",
        ))
        .stdout(contains(
            "PASS [3] fallback -> workflow/cache/assert-present",
        ))
        .stdout(contains("should-not-run").not());

    assert_eq!(
        redis_get_string(redis_port, "workflow:test:key")
            .await
            .as_deref(),
        None
    );
}

#[tokio::test]
async fn workflow_executes_with_deferred_cleanup() {
    let temp = tempdir().expect("tempdir");
    let runner_root = temp.path();

    let mysql_port = reserve_port();
    let redis_port = reserve_port();
    prepare_full_suite_runner(
        runner_root,
        reserve_port(),
        reserve_port(),
        mysql_port,
        redis_port,
    )
    .expect("prepare workflow runner root");
    write_workflow_cache_cases(runner_root).expect("write workflow cache cases");
    fs::write(
        runner_root.join(".testrunner/workflows/deferred-cache-flow.yaml"),
        concat!(
            "name: deferred cache flow\n",
            "steps:\n",
            "  - run_case:\n",
            "      id: seed\n",
            "      case: workflow/cache/seed\n",
            "      cleanup: defer\n",
            "      exports:\n",
            "        cache_key: vars.cache_key\n",
            "        cache_value: vars.cache_value\n",
            "  - run_case:\n",
            "      id: verify\n",
            "      case: workflow/cache/assert-present\n",
            "      inputs:\n",
            "        cache_key: \"${workflow.steps.seed.exports.cache_key}\"\n",
            "        expected_value: \"${workflow.steps.seed.exports.cache_value}\"\n",
        ),
    )
    .expect("write deferred cache workflow");

    let _dependency_stack = start_dependency_stack(temp.path(), mysql_port, redis_port);

    Command::new(binary())
        .args([
            "test",
            "workflow",
            "deferred-cache-flow",
            "--root",
            runner_root.to_str().expect("utf8"),
            "--env",
            "it",
            "--no-mock",
        ])
        .assert()
        .success()
        .stdout(contains(
            "==> Running workflow `deferred-cache-flow` in env `it`",
        ))
        .stdout(contains("PASS [1] seed -> workflow/cache/seed"))
        .stdout(contains("PASS [2] verify -> workflow/cache/assert-present"))
        .stdout(contains("Steps: 2 passed, 0 failed, 2 total"));

    assert_eq!(
        redis_get_string(redis_port, "workflow:test:key")
            .await
            .as_deref(),
        None
    );
}

#[tokio::test]
async fn sample_project_register_login_create_order_workflow_passes() {
    let temp = tempdir().expect("tempdir");
    let runner_root = temp.path();

    let app_port = reserve_port();
    let mock_port = reserve_port();
    let mysql_port = reserve_port();
    let redis_port = reserve_port();
    prepare_full_suite_runner(runner_root, app_port, mock_port, mysql_port, redis_port)
        .expect("prepare full-suite runner root");

    let _dependency_stack = start_dependency_stack(temp.path(), mysql_port, redis_port);
    let mut service = ChildGuard(start_sample_service_with_env(
        app_port,
        &[
            (
                "DATABASE_URL".to_string(),
                format!("mysql://app:app@127.0.0.1:{mysql_port}/app"),
            ),
            (
                "REDIS_URL".to_string(),
                format!("redis://127.0.0.1:{redis_port}/0"),
            ),
            (
                "SMS_PROVIDER_BASE_URL".to_string(),
                format!("http://127.0.0.1:{mock_port}"),
            ),
        ],
    ));
    wait_for_service(&mut service.0, app_port).await;

    Command::new(binary())
        .args([
            "test",
            "workflow",
            "register-login-create-order",
            "--root",
            runner_root.to_str().expect("utf8"),
            "--env",
            "it",
        ])
        .assert()
        .success()
        .stdout(contains(
            "==> Running workflow `register-login-create-order` in env `it`",
        ))
        .stdout(contains("PASS [1] register -> user/register/happy-path"))
        .stdout(contains(
            "PASS [2] send-sms -> user/send-sms-code/happy-path",
        ))
        .stdout(contains(
            "PASS [3] login -> workflow/user/login-after-register",
        ))
        .stdout(contains(
            "PASS [4] current-profile -> workflow/user/me-after-login",
        ))
        .stdout(contains(
            "PASS [5] create-order -> workflow/order/create-after-login",
        ))
        .stdout(contains(
            "PASS [6] get-order -> workflow/order/get-created-order",
        ))
        .stdout(contains(
            "PASS [7] update-order -> workflow/order/update-created-order",
        ))
        .stdout(contains("Steps: 7 passed, 0 failed, 7 total"));
}

#[tokio::test]
async fn sample_project_payment_callback_workflow_passes() {
    let temp = tempdir().expect("tempdir");
    let runner_root = temp.path();

    let app_port = reserve_port();
    let mysql_port = reserve_port();
    let redis_port = reserve_port();
    prepare_full_suite_runner(
        runner_root,
        app_port,
        reserve_port(),
        mysql_port,
        redis_port,
    )
    .expect("prepare payment callback workflow root");

    let _dependency_stack = start_dependency_stack(temp.path(), mysql_port, redis_port);
    let mut service = ChildGuard(start_sample_service_with_env(
        app_port,
        &[(
            "REDIS_URL".to_string(),
            format!("redis://127.0.0.1:{redis_port}/0"),
        )],
    ));
    wait_for_service(&mut service.0, app_port).await;

    Command::new(binary())
        .args([
            "test",
            "workflow",
            "payment-callback-flow",
            "--root",
            runner_root.to_str().expect("utf8"),
            "--env",
            "it",
            "--no-mock",
        ])
        .assert()
        .success()
        .stdout(contains(
            "==> Running workflow `payment-callback-flow` in env `it`",
        ))
        .stdout(contains(
            "PASS [1] schedule-callback -> workflow/payment/schedule-callback",
        ))
        .stdout(contains(
            "PASS [2] verify-callback -> workflow/payment/assert-callback",
        ))
        .stdout(contains("Steps: 2 passed, 0 failed, 2 total"))
        .stdout(contains("Callbacks: 1 passed, 0 failed, 1 total"))
        .stdout(contains(
            "PASS #1 case:workflow/payment/schedule-callback ->",
        ));
}

#[test]
fn sample_project_docker_env_managed_api_run_passes() {
    let _lock = sample_project_docker_lock();
    let _cleanup = SampleProjectDockerCleanupGuard;

    cleanup_sample_project_docker_env();

    Command::new(binary())
        .args([
            "test",
            "api",
            "system/health",
            "--root",
            sample_project_root().to_str().expect("utf8"),
            "--env",
            "docker",
        ])
        .assert()
        .success()
        .stdout(contains(
            "==> Running 1 case(s) for api `system/health` in env `docker`",
        ))
        .stdout(contains("PASS [1/1] system/health/smoke"))
        .stdout(contains("Cases: 1 passed, 0 failed, 1 total"))
        .stdout(contains("Readiness: 2 passed, 0 failed"));

    let report = read_json_report(&sample_project_root().join(".testrunner/reports/last-run.json"));
    assert_eq!(report["environment"], "docker");
    assert_eq!(
        report["environment_artifacts"]["runtime"]["startup_status"],
        "passed"
    );
    assert_eq!(
        report["environment_artifacts"]["runtime"]["shutdown_status"],
        "passed"
    );
    assert!(
        report["environment_artifacts"]["readiness"]
            .as_array()
            .map(|items| !items.is_empty())
            .unwrap_or(false)
    );
}

#[test]
fn sample_project_docker_env_managed_workflow_collects_environment_logs() {
    let _lock = sample_project_docker_lock();
    let _cleanup = SampleProjectDockerCleanupGuard;

    cleanup_sample_project_docker_env();

    Command::new(binary())
        .args([
            "test",
            "workflow",
            "register-login-create-order",
            "--root",
            sample_project_root().to_str().expect("utf8"),
            "--env",
            "docker",
        ])
        .assert()
        .success()
        .stdout(contains(
            "==> Running workflow `register-login-create-order` in env `docker`",
        ))
        .stdout(contains("PASS [1] register -> user/register/happy-path"))
        .stdout(contains(
            "PASS [2] send-sms -> user/send-sms-code/happy-path",
        ))
        .stdout(contains(
            "PASS [3] login -> workflow/user/login-after-register",
        ))
        .stdout(contains(
            "PASS [4] current-profile -> workflow/user/me-after-login",
        ))
        .stdout(contains(
            "PASS [5] create-order -> workflow/order/create-after-login",
        ))
        .stdout(contains(
            "PASS [6] get-order -> workflow/order/get-created-order",
        ))
        .stdout(contains(
            "PASS [7] update-order -> workflow/order/update-created-order",
        ))
        .stdout(contains("Steps: 7 passed, 0 failed, 7 total"))
        .stdout(contains("Logs: 3 collected, 0 failed"));

    let report =
        read_json_report(&sample_project_root().join(".testrunner/reports/last-workflow-run.json"));
    let logs = report["environment_artifacts"]["logs"]
        .as_array()
        .expect("environment logs array");
    assert_eq!(logs.len(), 3);
    assert!(logs.iter().all(|log| log["status"] == "passed"));
    assert!(
        sample_project_root()
            .join(".testrunner/reports/env/app.log")
            .exists()
    );
    assert!(
        sample_project_root()
            .join(".testrunner/reports/env/mysql-query.log")
            .exists()
    );
    assert!(
        sample_project_root()
            .join(".testrunner/reports/env/redis-monitor.log")
            .exists()
    );
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct SampleProjectDockerCleanupGuard;

impl Drop for SampleProjectDockerCleanupGuard {
    fn drop(&mut self) {
        cleanup_sample_project_docker_env();
    }
}

struct ComposeGuard {
    project_name: String,
    compose_file: PathBuf,
}

impl Drop for ComposeGuard {
    fn drop(&mut self) {
        match docker_compose(
            &self.project_name,
            &self.compose_file,
            &["down", "-v", "--remove-orphans"],
        ) {
            Ok(output) if !output.status.success() => {
                eprintln!(
                    "docker compose down failed:\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
            Ok(_) => {}
            Err(error) => eprintln!("failed to run docker compose down: {error}"),
        }
    }
}

fn start_sample_service(port: u16) -> Child {
    start_sample_service_with_env(port, &[])
}

fn start_sample_service_with_env(port: u16, envs: &[(String, String)]) -> Child {
    let mut command = StdCommand::new("cargo");
    command
        .arg("run")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(sample_project_root().join("Cargo.toml"))
        .env("HOST", "127.0.0.1")
        .env("PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    for (key, value) in envs {
        command.env(key, value);
    }

    command.spawn().expect("start sample service")
}

fn start_dependency_stack(root: &Path, mysql_port: u16, redis_port: u16) -> ComposeGuard {
    let compose_file = root.join("docker-compose.integration.yaml");
    write_dependency_compose(&compose_file, mysql_port, redis_port)
        .expect("write integration docker compose file");
    let project_name = format!("test-runner-sample-{}-{mysql_port}", std::process::id());
    let output = docker_compose(&project_name, &compose_file, &["up", "-d", "--wait"])
        .expect("run docker compose up");
    if !output.status.success() {
        panic!(
            "docker compose up failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    ComposeGuard {
        project_name,
        compose_file,
    }
}

fn docker_compose(
    project_name: &str,
    compose_file: &Path,
    args: &[&str],
) -> std::io::Result<Output> {
    let mut command = StdCommand::new("docker");
    command
        .arg("compose")
        .arg("-p")
        .arg(project_name)
        .arg("-f")
        .arg(compose_file);
    command.args(args);
    command.output()
}

fn cleanup_sample_project_docker_env() {
    let compose_file = sample_project_root().join("docker-compose.yml");
    match docker_compose(
        "test-runner-sample",
        &compose_file,
        &["down", "-v", "--remove-orphans"],
    ) {
        Ok(output) if !output.status.success() => {
            eprintln!(
                "sample project docker cleanup failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        Ok(_) => {}
        Err(error) => eprintln!("failed to clean sample project docker env: {error}"),
    }
}

fn read_json_report(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read report"))
        .expect("parse report json")
}

fn write_dependency_compose(
    compose_file: &Path,
    mysql_port: u16,
    redis_port: u16,
) -> std::io::Result<()> {
    fs::write(
        compose_file,
        format!(
            "services:\n  mysql:\n    image: mysql:8.4\n    environment:\n      MYSQL_DATABASE: app\n      MYSQL_USER: app\n      MYSQL_PASSWORD: app\n      MYSQL_ROOT_PASSWORD: root\n    ports:\n      - \"127.0.0.1:{mysql_port}:3306\"\n    healthcheck:\n      test: [\"CMD-SHELL\", \"mysqladmin ping -h 127.0.0.1 -uapp -papp --silent\"]\n      interval: 5s\n      timeout: 5s\n      retries: 20\n      start_period: 10s\n\n  redis:\n    image: redis:7.2-alpine\n    command: [\"redis-server\", \"--save\", \"\", \"--appendonly\", \"no\"]\n    ports:\n      - \"127.0.0.1:{redis_port}:6379\"\n    healthcheck:\n      test: [\"CMD\", \"redis-cli\", \"ping\"]\n      interval: 5s\n      timeout: 5s\n      retries: 20\n",
        ),
    )
}

fn prepare_full_suite_runner(
    runner_root: &Path,
    app_port: u16,
    mock_port: u16,
    mysql_port: u16,
    redis_port: u16,
) -> std::io::Result<()> {
    copy_dir_all(
        &sample_project_root().join(".testrunner"),
        &runner_root.join(".testrunner"),
    )?;
    fs::write(
        runner_root.join(".testrunner/env/it.yaml"),
        format!(
            "name: it\nbase_url: http://127.0.0.1:{app_port}\nheaders:\n  x-test-env: it\nvariables:\n  service_base_url: http://127.0.0.1:{app_port}\n  sms_provider_base_url: http://127.0.0.1:{mock_port}\n  payment_provider_base_url: http://127.0.0.1:{mock_port}\n"
        ),
    )?;
    fs::write(
        runner_root.join(".testrunner/datasources/mysql.yaml"),
        format!(
            "datasources:\n  mysql.main:\n    kind: mysql\n    url: mysql://app:app@127.0.0.1:{mysql_port}/app\n"
        ),
    )?;
    fs::write(
        runner_root.join(".testrunner/datasources/redis.yaml"),
        format!(
            "datasources:\n  redis.main:\n    kind: redis\n    url: redis://127.0.0.1:{redis_port}/0\n"
        ),
    )?;

    let project_path = runner_root.join(".testrunner/project.yaml");
    let project = fs::read_to_string(&project_path)?;
    fs::write(
        &project_path,
        project.replace("port: 18081", &format!("port: {mock_port}")),
    )
}

fn prepare_it_runner(runner_root: &Path, port: u16) -> std::io::Result<()> {
    copy_dir_all(
        &sample_project_root().join(".testrunner"),
        &runner_root.join(".testrunner"),
    )?;
    fs::write(
        runner_root.join(".testrunner/env/it.yaml"),
        format!(
            "name: it\nbase_url: http://127.0.0.1:{port}\nheaders:\n  x-test-env: it\nvariables:\n  service_base_url: http://127.0.0.1:{port}\n"
        ),
    )
}

async fn wait_for_service(child: &mut Child, port: u16) {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/health");

    for _ in 0..300 {
        if let Some(status) = child.try_wait().expect("inspect child process") {
            panic!("health service exited before becoming ready: {status}");
        }

        if let Ok(response) = client.get(&url).send().await
            && response.status().is_success()
        {
            return;
        }

        sleep(Duration::from_millis(100)).await;
    }

    panic!("health service did not become ready at {url}");
}

fn reserve_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("read local addr")
        .port()
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source = entry.path();
        let destination = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_all(&source, &destination)?;
        } else {
            fs::copy(&source, &destination)?;
        }
    }

    Ok(())
}

fn write_workflow_cache_cases(runner_root: &Path) -> std::io::Result<()> {
    let case_dir = runner_root.join(".testrunner/cases/workflow/cache");
    fs::create_dir_all(&case_dir)?;
    fs::write(
        case_dir.join("seed.yaml"),
        concat!(
            "name: seed cache key\n",
            "api: system/health\n",
            "vars:\n",
            "  cache_key: workflow:test:key\n",
            "  cache_value: seeded-value\n",
            "steps:\n",
            "  - redis:\n",
            "      datasource: redis.main\n",
            "      command: SET\n",
            "      args:\n",
            "        - \"{{ cache_key }}\"\n",
            "        - \"{{ cache_value }}\"\n",
            "  - query_redis:\n",
            "      datasource: redis.main\n",
            "      command: GET\n",
            "      args:\n",
            "        - \"{{ cache_key }}\"\n",
            "    assert:\n",
            "      - eq: [result.value, cache_value]\n",
            "teardown:\n",
            "  - redis:\n",
            "      datasource: redis.main\n",
            "      command: DEL\n",
            "      args:\n",
            "        - \"{{ cache_key }}\"\n",
        ),
    )?;
    fs::write(
        case_dir.join("assert-present.yaml"),
        concat!(
            "name: assert cache key present\n",
            "api: system/health\n",
            "vars:\n",
            "  cache_key: workflow:test:key\n",
            "  expected_value: seeded-value\n",
            "steps:\n",
            "  - query_redis:\n",
            "      datasource: redis.main\n",
            "      command: GET\n",
            "      args:\n",
            "        - \"{{ cache_key }}\"\n",
            "    assert:\n",
            "      - eq: [result.value, expected_value]\n",
        ),
    )?;
    fs::write(
        case_dir.join("assert-wrong.yaml"),
        concat!(
            "name: assert cache key wrong value\n",
            "api: system/health\n",
            "vars:\n",
            "  cache_key: workflow:test:key\n",
            "  expected_value: definitely-wrong\n",
            "steps:\n",
            "  - query_redis:\n",
            "      datasource: redis.main\n",
            "      command: GET\n",
            "      args:\n",
            "        - \"{{ cache_key }}\"\n",
            "    assert:\n",
            "      - eq: [result.value, expected_value]\n",
        ),
    )?;
    Ok(())
}

async fn redis_get_string(port: u16, key: &str) -> Option<String> {
    let client =
        redis::Client::open(format!("redis://127.0.0.1:{port}/0")).expect("open redis client");
    let mut connection = client
        .get_multiplexed_tokio_connection()
        .await
        .expect("connect redis");
    connection.get(key).await.expect("read redis key")
}
