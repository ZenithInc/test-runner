use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use indexmap::IndexMap;
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::mysql::{MySqlPool, MySqlRow};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Column, ColumnIndex, Decode, Row, Type, TypeInfo};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs;
use std::future::Future;
use std::io::{self, IsTerminal};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{Duration, sleep};
use url::Url;

use crate::callback::{
    CallbackReport, CallbackRuntime, CallbackSummaryReport, PreparedRequest, PreparedRequestBody,
    RequestPreparationContext, ScheduledCallback, prepare_callback_request, prepare_case_request,
};
use crate::cli::{CommonTestArgs, ReportFormat, TestCommand, TestWorkflowArgs};
use crate::config::{
    DatasourceDefinition, EnvironmentConfig, EnvironmentRuntimeKind, LoadedApi, LoadedCase,
    LoadedProject, LoadedWorkflow, TESTRUNNER_DIR, environment_context_value, load_data_tree,
    load_project,
};
use crate::dsl::{
    CallbackStep, ConditionalStep, ForeachStep, QueryDbStep, QueryRedisStep, RedisCommandStep,
    RequestSpec, RequestStep, SleepStep, SqlExecStep, Step,
};
use crate::environment::{EnvironmentArtifactsReport, EnvironmentSession};
use crate::mock;
use crate::runtime::{RuntimeContext, apply_assertions, value_to_string};
use crate::url_rewrite::{rewrite_url_base_in_place, rewrite_value_url_bases};
use crate::workflow::{CleanupPolicy, WorkflowStep};

pub async fn run(command: TestCommand) -> Result<()> {
    let command = match command {
        TestCommand::Workflow(args) => return run_workflow(args).await,
        other => other,
    };

    let (target, options) = match &command {
        TestCommand::Api(args) => (
            TargetSelection::Api(args.api_id.clone()),
            args.common.clone(),
        ),
        TestCommand::Dir(args) => (TargetSelection::Dir(args.dir.clone()), args.common.clone()),
        TestCommand::All(args) => (TargetSelection::All, args.common.clone()),
        TestCommand::Workflow(_) => unreachable!(),
    };

    let project = load_project(&options.root, options.env.as_deref())?;
    let selected_cases = select_cases(&project, &target, &options)?;

    if selected_cases.is_empty() {
        bail!(
            "no test cases matched the requested target in {}",
            TESTRUNNER_DIR
        );
    }

    if options.dry_run {
        println!(
            "Selected {} case(s) for {} in env `{}`:",
            selected_cases.len(),
            target.display(),
            project.environment_name
        );
        for case in &selected_cases {
            println!("  - {} ({})", case.id, case.definition.name);
        }
        return Ok(());
    }

    let parallel_jobs = resolve_parallel_jobs(&project, &options, selected_cases.len())?;
    let console = SummaryConsole::new(options.report_format);
    console.run_started(
        &target,
        &project.environment_name,
        selected_cases.len(),
        parallel_jobs,
    );
    let should_start_mock = options
        .mock_override()
        .unwrap_or(project.project.mock.enabled)
        && !project.mock_routes.is_empty();
    let manages_environment = manages_environment(&project.environment);

    let mut environment_session = match EnvironmentSession::new(
        &project,
        parallel_jobs.unwrap_or(1),
        options.follow_env_logs,
    ) {
        Ok(session) => session,
        Err(error) => {
            if manages_environment {
                console.environment_failed(&project.environment_name, &error);
            }
            return Err(error);
        }
    };
    if manages_environment {
        console.environment_starting(&project.environment_name, &project.environment);
        if options.follow_env_logs && !project.environment.logs.is_empty() {
            console.environment_logs_following(&project.environment_name);
        }
    }
    let reserved_mock_endpoints = match prepare_slot_mock_endpoints(
        &project,
        should_start_mock,
        parallel_jobs.unwrap_or(1),
        &mut environment_session,
    )
    .await
    {
        Ok(endpoints) => endpoints,
        Err(error) => {
            if manages_environment {
                console.environment_failed(&project.environment_name, &error);
            }
            return Err(error);
        }
    };
    let mut mock_servers: Vec<mock::MockServerHandle> = Vec::new();
    let execution_result = match environment_session.prepare().await {
        Ok(()) => {
            if manages_environment {
                console.environment_ready(&project.environment_name);
            }
            match build_slot_execution_contexts(
                &environment_session,
                should_start_mock,
                &console,
                reserved_mock_endpoints,
            )
            .await
            {
                Ok((slot_contexts, servers)) => {
                    mock_servers = servers;
                    if let Some(jobs) = parallel_jobs {
                        execute_cases_parallel(
                            slot_contexts,
                            &selected_cases,
                            &options,
                            &target,
                            &console,
                            jobs,
                        )
                        .await
                    } else {
                        let slot_context = slot_contexts
                            .into_iter()
                            .next()
                            .context("no execution slot was prepared")?;
                        let mut runner = Runner::new_with_callback_runtime(
                            slot_context.project,
                            options.report_format,
                            slot_context.callback_runtime,
                            None,
                        );
                        runner.execute(&selected_cases, &options, &target).await
                    }
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => {
            if manages_environment {
                console.environment_failed(&project.environment_name, &error);
            }
            Err(error)
        }
    };
    let execution_succeeded = match &execution_result {
        Ok(report) => report.summary.failed == 0 && report.callback_summary.failed == 0,
        Err(_) => false,
    };
    let environment_artifacts = environment_session.finish(execution_succeeded).await;

    for server in mock_servers.drain(..) {
        server.shutdown().await;
    }

    let mut report = execution_result?;
    if !environment_artifacts.is_empty() {
        report.environment_artifacts = Some(environment_artifacts);
    }
    let report_path = write_report(&project.runner_root, &report)?;
    print_report(&report, &report_path, options.report_format)?;

    if report.summary.failed > 0 {
        bail!(
            "{} of {} case(s) failed",
            report.summary.failed,
            report.summary.total
        );
    }
    if report.callback_summary.failed > 0 {
        bail!(
            "{} of {} callback(s) failed",
            report.callback_summary.failed,
            report.callback_summary.total
        );
    }

    Ok(())
}

async fn run_workflow(args: TestWorkflowArgs) -> Result<()> {
    let options = &args.common;

    if !options.tag.is_empty() {
        bail!(
            "--tag is not supported for `test workflow`; workflow steps are selected by the workflow definition"
        );
    }
    if options.case_pattern.is_some() {
        bail!(
            "--case is not supported for `test workflow`; workflow steps are selected by the workflow definition"
        );
    }
    let project = load_project(&options.root, options.env.as_deref())?;
    let workflows = select_workflows(&project, &args)?;

    if options.dry_run {
        if workflows.len() == 1 {
            print_workflow_dry_run(&workflows[0], &project.environment_name);
        } else {
            print_workflows_dry_run(&workflows, &project.environment_name);
        }
        return Ok(());
    }

    let parallel_jobs = resolve_parallel_jobs(&project, options, workflows.len())?;
    let console = SummaryConsole::new(options.report_format);
    if workflows.len() == 1 && parallel_jobs.is_none() {
        console.workflow_started(&workflows[0].id, &project.environment_name);
    } else {
        console.workflows_started(workflows.len(), &project.environment_name, parallel_jobs);
    }
    let should_start_mock = options
        .mock_override()
        .unwrap_or(project.project.mock.enabled)
        && !project.mock_routes.is_empty();
    let manages_environment = manages_environment(&project.environment);

    let mut environment_session = match EnvironmentSession::new(
        &project,
        parallel_jobs.unwrap_or(1),
        options.follow_env_logs,
    ) {
        Ok(session) => session,
        Err(error) => {
            if manages_environment {
                console.environment_failed(&project.environment_name, &error);
            }
            return Err(error);
        }
    };
    if manages_environment {
        console.environment_starting(&project.environment_name, &project.environment);
        if options.follow_env_logs && !project.environment.logs.is_empty() {
            console.environment_logs_following(&project.environment_name);
        }
    }
    let reserved_mock_endpoints = match prepare_slot_mock_endpoints(
        &project,
        should_start_mock,
        parallel_jobs.unwrap_or(1),
        &mut environment_session,
    )
    .await
    {
        Ok(endpoints) => endpoints,
        Err(error) => {
            if manages_environment {
                console.environment_failed(&project.environment_name, &error);
            }
            return Err(error);
        }
    };
    let mut mock_servers: Vec<mock::MockServerHandle> = Vec::new();
    let execution_result = match environment_session.prepare().await {
        Ok(()) => {
            if manages_environment {
                console.environment_ready(&project.environment_name);
            }
            match build_slot_execution_contexts(
                &environment_session,
                should_start_mock,
                &console,
                reserved_mock_endpoints,
            )
            .await
            {
                Ok((slot_contexts, servers)) => {
                    mock_servers = servers;
                    if workflows.len() == 1 && parallel_jobs.is_none() {
                        let slot_context = slot_contexts
                            .into_iter()
                            .next()
                            .context("no execution slot was prepared")?;
                        let mut runner = Runner::new_with_callback_runtime(
                            slot_context.project,
                            options.report_format,
                            slot_context.callback_runtime,
                            None,
                        );
                        runner
                            .execute_workflow(&workflows[0].id, &workflows[0], options)
                            .await
                    } else if let Some(jobs) = parallel_jobs {
                        let report = execute_workflows_parallel(
                            slot_contexts,
                            &workflows,
                            options,
                            &console,
                            jobs,
                        )
                        .await?;
                        Err(anyhow::anyhow!(WorkflowBatchError(report)))
                    } else {
                        let slot_context = slot_contexts
                            .into_iter()
                            .next()
                            .context("no execution slot was prepared")?;
                        let report =
                            execute_workflows_serial(slot_context, &workflows, options, &console)
                                .await?;
                        Err(anyhow::anyhow!(WorkflowBatchError(report)))
                    }
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => {
            if manages_environment {
                console.environment_failed(&project.environment_name, &error);
            }
            Err(error)
        }
    };

    let batch_report = execution_result
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<WorkflowBatchError>())
        .cloned()
        .map(|error| error.0);
    let execution_succeeded = match (&execution_result, &batch_report) {
        (Ok(report), _) => report.status == "passed" && report.callback_summary.failed == 0,
        (_, Some(report)) => {
            report.summary.failed_workflows == 0 && report.callback_summary.failed == 0
        }
        _ => false,
    };
    let environment_artifacts = environment_session.finish(execution_succeeded).await;

    for server in mock_servers.drain(..) {
        server.shutdown().await;
    }

    if let Some(mut report) = batch_report {
        if !environment_artifacts.is_empty() {
            report.environment_artifacts = Some(environment_artifacts);
        }
        let report_path = write_workflow_batch_report(&project.runner_root, &report)?;
        print_workflow_batch_report(&report, &report_path, options.report_format)?;
        if report.summary.failed_workflows > 0 {
            bail!(
                "{} of {} workflow(s) failed",
                report.summary.failed_workflows,
                report.summary.total_workflows
            );
        }
        return Ok(());
    }

    let mut report = execution_result?;
    if !environment_artifacts.is_empty() {
        report.environment_artifacts = Some(environment_artifacts);
    }
    let report_path = write_workflow_report(&project.runner_root, &report)?;
    print_workflow_report(&report, &report_path, options.report_format)?;

    if report.status == "failed" {
        bail!("workflow `{}` failed", report.workflow_id);
    }

    Ok(())
}

#[derive(Debug, Clone)]
enum TargetSelection {
    Api(String),
    Dir(String),
    All,
}

impl TargetSelection {
    fn display(&self) -> String {
        match self {
            Self::Api(api) => format!("api `{api}`"),
            Self::Dir(dir) => format!("dir `{dir}`"),
            Self::All => "all cases".to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
struct RunReport {
    project: String,
    environment: String,
    target: String,
    started_at: String,
    finished_at: String,
    summary: SummaryReport,
    callback_summary: CallbackSummaryReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel: Option<ParallelRunMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    callbacks: Vec<CallbackReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment_artifacts: Option<EnvironmentArtifactsReport>,
    cases: Vec<CaseReport>,
}

#[derive(Debug, Serialize, Clone)]
struct SummaryReport {
    total: usize,
    passed: usize,
    failed: usize,
    duration_ms: u128,
}

#[derive(Debug, Serialize, Clone)]
struct CaseReport {
    id: String,
    name: String,
    api: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    slot_id: Option<usize>,
    status: String,
    duration_ms: u128,
    error: Option<String>,
    steps: Vec<StepReport>,
}

#[derive(Debug, Serialize, Clone)]
struct StepReport {
    kind: String,
    status: String,
    duration_ms: u128,
    details: Value,
    error: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
struct WorkflowRunReport {
    project: String,
    environment: String,
    workflow_id: String,
    workflow_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    slot_id: Option<usize>,
    started_at: String,
    finished_at: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    summary: WorkflowSummaryReport,
    callback_summary: CallbackSummaryReport,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    callbacks: Vec<CallbackReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment_artifacts: Option<EnvironmentArtifactsReport>,
    steps: Vec<WorkflowStepReport>,
}

#[derive(Debug, Serialize, Clone)]
struct ParallelRunMetadata {
    jobs: usize,
    slots: usize,
    unit: String,
}

#[derive(Debug, Serialize, Clone)]
struct WorkflowBatchRunReport {
    project: String,
    environment: String,
    started_at: String,
    finished_at: String,
    summary: WorkflowBatchSummaryReport,
    callback_summary: CallbackSummaryReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel: Option<ParallelRunMetadata>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    callbacks: Vec<CallbackReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment_artifacts: Option<EnvironmentArtifactsReport>,
    workflows: Vec<WorkflowRunReport>,
}

#[derive(Debug, Serialize, Clone)]
struct WorkflowBatchSummaryReport {
    total_workflows: usize,
    passed_workflows: usize,
    failed_workflows: usize,
    duration_ms: u128,
}

#[derive(Debug, Serialize, Clone)]
struct WorkflowSummaryReport {
    executed_steps: usize,
    passed_steps: usize,
    failed_steps: usize,
    duration_ms: u128,
}

#[derive(Debug, Serialize, Clone)]
struct WorkflowStepReport {
    id: String,
    case_id: String,
    status: String,
    passed: bool,
    duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    exports: serde_json::Map<String, Value>,
    case_steps: Vec<StepReport>,
    deferred_teardown_steps: Vec<StepReport>,
}

struct WorkflowState {
    vars: serde_json::Map<String, Value>,
    steps: IndexMap<String, WorkflowStepState>,
}

struct WorkflowStepState {
    status: String,
    passed: bool,
    error: Option<Value>,
    exports: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Copy)]
struct SummaryConsole {
    enabled: bool,
    styler: TerminalStyler,
}

#[derive(Debug, Clone, Copy)]
struct TerminalStyler {
    enabled: bool,
}

impl TerminalStyler {
    fn detect() -> Self {
        let stdout_is_terminal = io::stdout().is_terminal();
        let no_color = env::var_os("NO_COLOR").is_some();
        let term_is_dumb = env::var("TERM")
            .map(|term| term.eq_ignore_ascii_case("dumb"))
            .unwrap_or(false);
        Self {
            enabled: stdout_is_terminal && !no_color && !term_is_dumb,
        }
    }

    fn paint(&self, text: impl AsRef<str>, code: &str) -> String {
        let text = text.as_ref();
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn phase(&self, text: impl AsRef<str>) -> String {
        self.paint(text, "1;36")
    }

    fn section(&self, text: impl AsRef<str>) -> String {
        self.paint(text, "1;34")
    }

    fn success(&self, text: impl AsRef<str>) -> String {
        self.paint(text, "1;32")
    }

    fn failure(&self, text: impl AsRef<str>) -> String {
        self.paint(text, "1;31")
    }

    fn warning(&self, text: impl AsRef<str>) -> String {
        self.paint(text, "1;33")
    }

    fn info(&self, text: impl AsRef<str>) -> String {
        self.paint(text, "1;34")
    }

    fn muted(&self, text: impl AsRef<str>) -> String {
        self.paint(text, "2")
    }

    fn status(&self, status: &str) -> String {
        match status {
            "passed" => self.success(status_label(status)),
            "failed" => self.failure(status_label(status)),
            "skipped" => self.warning(status_label(status)),
            _ => self.info(status_label(status)),
        }
    }
}

impl SummaryConsole {
    fn new(format: ReportFormat) -> Self {
        Self {
            enabled: format == ReportFormat::Summary,
            styler: TerminalStyler::detect(),
        }
    }

    fn run_started(
        &self,
        target: &TargetSelection,
        env_name: &str,
        case_count: usize,
        parallel_jobs: Option<usize>,
    ) {
        if !self.enabled {
            return;
        }
        let parallel_suffix = parallel_jobs
            .map(|jobs| format!(" (parallel: {jobs} jobs)"))
            .unwrap_or_default();
        println!(
            "{}",
            self.styler.phase(format!(
                "==> Running {case_count} case(s) for {} in env `{env_name}`{parallel_suffix}",
                target.display(),
            ))
        );
    }

    fn workflow_started(&self, workflow_id: &str, env_name: &str) {
        if !self.enabled {
            return;
        }
        println!(
            "{}",
            self.styler.phase(format!(
                "==> Running workflow `{workflow_id}` in env `{env_name}`"
            ))
        );
    }

    fn workflows_started(
        &self,
        workflow_count: usize,
        env_name: &str,
        parallel_jobs: Option<usize>,
    ) {
        if !self.enabled {
            return;
        }
        let parallel_suffix = parallel_jobs
            .map(|jobs| format!(" (parallel: {jobs} jobs)"))
            .unwrap_or_default();
        println!(
            "{}",
            self.styler.phase(format!(
                "==> Running {workflow_count} workflow(s) in env `{env_name}`{parallel_suffix}"
            ))
        );
    }

    fn mock_starting(&self) {
        if !self.enabled {
            return;
        }
        println!("{}", self.styler.phase("==> Starting embedded mock server"));
    }

    fn mock_ready(&self, base_url: &str) {
        if !self.enabled {
            return;
        }
        println!(
            "{}",
            self.styler.phase(format!("==> Mock ready at {base_url}"))
        );
    }

    fn mock_pool_ready(&self, slot_count: usize) {
        if !self.enabled {
            return;
        }
        println!(
            "{}",
            self.styler.phase(format!(
                "==> {slot_count} embedded mock server slot(s) ready"
            ))
        );
    }

    fn environment_starting(&self, env_name: &str, environment: &EnvironmentConfig) {
        if !self.enabled {
            return;
        }

        let mut parts = Vec::new();
        if let Some(runtime) = &environment.runtime {
            let label = match runtime.kind {
                crate::config::EnvironmentRuntimeKind::DockerCompose => "docker compose",
                crate::config::EnvironmentRuntimeKind::Containers => "containers",
            };
            parts.push(label.to_string());
        }
        if !environment.readiness.is_empty() {
            parts.push(format!(
                "{} readiness check(s)",
                environment.readiness.len()
            ));
        }
        if !environment.logs.is_empty() {
            parts.push(format!("{} log source(s)", environment.logs.len()));
        }

        if parts.is_empty() {
            println!(
                "{}",
                self.styler
                    .phase(format!("==> Preparing environment `{env_name}`"))
            );
        } else {
            println!(
                "{}",
                self.styler.phase(format!(
                    "==> Preparing environment `{env_name}` ({})",
                    parts.join(" | ")
                ))
            );
        }
    }

    fn environment_ready(&self, env_name: &str) {
        if !self.enabled {
            return;
        }
        println!(
            "{}",
            self.styler
                .phase(format!("==> Environment `{env_name}` ready"))
        );
    }

    fn environment_logs_following(&self, env_name: &str) {
        if !self.enabled {
            return;
        }
        println!(
            "{}",
            self.styler.phase(format!(
                "==> Following environment logs for `{env_name}` on stderr"
            ))
        );
    }

    fn environment_failed(&self, env_name: &str, error: impl std::fmt::Display) {
        if !self.enabled {
            return;
        }
        println!(
            "{}",
            self.styler
                .failure(format!("FAIL environment `{env_name}`: {error}"))
        );
    }

    fn case_finished(&self, index: usize, total: usize, report: &CaseReport) {
        if !self.enabled {
            return;
        }
        let slot_suffix = report
            .slot_id
            .map(|slot_id| format!(" [slot {slot_id}]"))
            .unwrap_or_default();
        println!(
            "{} [{index}/{total}] {}{} ({})",
            self.styler.status(&report.status),
            report.id,
            slot_suffix,
            self.styler.muted(format_duration(report.duration_ms))
        );
        if let Some(error) = &report.error {
            println!("    {error}");
        }
    }

    fn workflow_finished(&self, index: usize, total: usize, report: &WorkflowRunReport) {
        if !self.enabled {
            return;
        }
        let slot_suffix = report
            .slot_id
            .map(|slot_id| format!(" [slot {slot_id}]"))
            .unwrap_or_default();
        println!(
            "{} [{index}/{total}] {}{} ({})",
            self.styler.status(&report.status),
            report.workflow_id,
            slot_suffix,
            self.styler
                .muted(format_duration(report.summary.duration_ms))
        );
        if let Some(error) = &report.error {
            println!("    {error}");
        }
    }

    fn workflow_step_finished(&self, index: usize, report: &WorkflowStepReport) {
        if !self.enabled {
            return;
        }
        println!(
            "{} [{index}] {} -> {} ({})",
            self.styler.status(&report.status),
            report.id,
            report.case_id,
            self.styler.muted(format_duration(report.duration_ms))
        );
        if let Some(error) = &report.error {
            println!("    {error}");
        }
    }
}

struct DeferredTeardown {
    step_id: String,
    case: LoadedCase,
    saved_root: serde_json::Map<String, Value>,
    teardown_steps: Vec<Step>,
}

struct WorkflowCaseOutcome {
    step_report: WorkflowStepReport,
    deferred: Option<DeferredTeardown>,
}

#[derive(Clone)]
struct SlotExecutionContext {
    slot_id: usize,
    project: LoadedProject,
    callback_runtime: CallbackRuntime,
}

struct SlotReservedMockEndpoint {
    slot_id: usize,
    endpoint: mock::ReservedMockEndpoint,
}

#[derive(Clone)]
struct SlotAllocator {
    slots: Arc<Vec<SlotExecutionContext>>,
    available: Arc<Mutex<VecDeque<usize>>>,
    semaphore: Arc<Semaphore>,
}

struct SlotLease {
    slot: SlotExecutionContext,
    _permit: OwnedSemaphorePermit,
    available: Arc<Mutex<VecDeque<usize>>>,
}

#[derive(Debug, Clone)]
struct WorkflowBatchError(WorkflowBatchRunReport);

struct CaseTaskResult {
    index: usize,
    report: CaseReport,
    callbacks: Vec<CallbackReport>,
}

struct WorkflowTaskResult {
    index: usize,
    report: WorkflowRunReport,
}

impl SlotAllocator {
    fn new(slots: Vec<SlotExecutionContext>) -> Self {
        let slot_count = slots.len();
        Self {
            slots: Arc::new(slots),
            available: Arc::new(Mutex::new((0..slot_count).collect())),
            semaphore: Arc::new(Semaphore::new(slot_count)),
        }
    }

    fn slot_count(&self) -> usize {
        self.slots.len()
    }

    async fn acquire(&self) -> Result<SlotLease> {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .context("slot allocator closed")?;
        let slot_id = self
            .available
            .lock()
            .expect("slot allocator mutex poisoned")
            .pop_front()
            .context("no slot available after acquiring permit")?;
        let slot = self
            .slots
            .get(slot_id)
            .cloned()
            .with_context(|| format!("slot `{slot_id}` does not exist"))?;
        Ok(SlotLease {
            slot,
            _permit: permit,
            available: self.available.clone(),
        })
    }
}

impl SlotLease {
    fn slot(&self) -> &SlotExecutionContext {
        &self.slot
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        self.available
            .lock()
            .expect("slot allocator mutex poisoned")
            .push_back(self.slot.slot_id);
    }
}

impl std::fmt::Display for WorkflowBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "workflow batch completed with {} workflow(s)",
            self.0.summary.total_workflows
        )
    }
}

impl std::error::Error for WorkflowBatchError {}

struct Runner {
    project: LoadedProject,
    http_client: reqwest::Client,
    request_context: RequestPreparationContext,
    callback_runtime: CallbackRuntime,
    db_pools: HashMap<String, DatabasePool>,
    redis_clients: HashMap<String, redis::Client>,
    console: SummaryConsole,
    slot_id: Option<usize>,
}

impl Runner {
    fn new_with_callback_runtime(
        project: LoadedProject,
        report_format: ReportFormat,
        callback_runtime: CallbackRuntime,
        slot_id: Option<usize>,
    ) -> Self {
        let request_context = RequestPreparationContext::from_project(&project);
        let http_client = build_http_client(&project);
        Self {
            project,
            http_client,
            request_context,
            callback_runtime,
            db_pools: HashMap::new(),
            redis_clients: HashMap::new(),
            console: SummaryConsole::new(report_format),
            slot_id,
        }
    }

    async fn execute(
        &mut self,
        cases: &[LoadedCase],
        options: &CommonTestArgs,
        target: &TargetSelection,
    ) -> Result<RunReport> {
        let started_at = Utc::now().to_rfc3339();
        let started = Instant::now();
        let mut reports = Vec::new();
        let mut passed = 0usize;
        let mut failed = 0usize;

        for (index, case) in cases.iter().enumerate() {
            let report = self.execute_case(case).await;
            self.console.case_finished(index + 1, cases.len(), &report);
            let failed_now = report.error.is_some();
            if failed_now {
                failed += 1;
            } else {
                passed += 1;
            }
            reports.push(report);
            if failed_now && options.fail_fast {
                break;
            }
        }

        let callbacks = self.callback_runtime.flush().await;
        let callback_summary = CallbackSummaryReport::from_reports(&callbacks);
        let finished_at = Utc::now().to_rfc3339();
        Ok(RunReport {
            project: self.project.project.project.name.clone(),
            environment: self.project.environment_name.clone(),
            target: target.display(),
            started_at,
            finished_at,
            summary: SummaryReport {
                total: reports.len(),
                passed,
                failed,
                duration_ms: started.elapsed().as_millis(),
            },
            callback_summary,
            parallel: None,
            callbacks,
            environment_artifacts: None,
            cases: reports,
        })
    }

    async fn execute_workflow(
        &mut self,
        workflow_id: &str,
        workflow: &LoadedWorkflow,
        options: &CommonTestArgs,
    ) -> Result<WorkflowRunReport> {
        let started_at = Utc::now().to_rfc3339();
        let started = Instant::now();
        let mut state = WorkflowState {
            vars: serde_json::Map::new(),
            steps: IndexMap::new(),
        };
        for (key, value) in &workflow.definition.vars {
            let workflow_runtime = build_workflow_runtime(&self.project, &state)?;
            let resolved = workflow_runtime
                .resolve_value(value)
                .with_context(|| format!("failed to resolve workflow var `{key}`"))?;
            state.vars.insert(key.clone(), resolved);
        }

        let mut step_reports = Vec::new();
        let mut deferred_teardowns = Vec::new();
        let mut any_case_failed = false;
        let mut stop_execution = false;
        let mut executed_run_case_steps = 0usize;

        self.execute_workflow_steps(
            &workflow.definition.steps,
            &mut state,
            &mut step_reports,
            &mut deferred_teardowns,
            &mut any_case_failed,
            options.fail_fast,
            &mut stop_execution,
            &mut executed_run_case_steps,
        )
        .await?;

        deferred_teardowns.reverse();
        for deferred in deferred_teardowns {
            let (step_id, teardown_reports, teardown_error) =
                self.run_deferred_teardown(deferred).await;
            if let Some(report) = step_reports.iter_mut().find(|report| report.id == step_id) {
                report.deferred_teardown_steps = teardown_reports;
                if let Some(error) = teardown_error {
                    any_case_failed = true;
                    report.passed = false;
                    report.status = "failed".to_string();
                    report.error = Some(match report.error.take() {
                        Some(existing) => {
                            format!("{existing}; deferred teardown failed: {error}")
                        }
                        None => format!("deferred teardown failed: {error}"),
                    });
                }
            }
        }

        let callbacks = self.callback_runtime.flush().await;
        let callback_summary = CallbackSummaryReport::from_reports(&callbacks);
        if callback_summary.failed > 0 {
            any_case_failed = true;
        }
        let status = if any_case_failed { "failed" } else { "passed" };
        let passed_steps = step_reports.iter().filter(|report| report.passed).count();
        let failed_steps = step_reports.iter().filter(|report| !report.passed).count();
        let mut error_parts = Vec::new();
        if failed_steps > 0 {
            error_parts.push(format!(
                "{failed_steps} of {} step(s) failed",
                step_reports.len()
            ));
        }
        if callback_summary.failed > 0 {
            error_parts.push(format!(
                "{} of {} callback(s) failed",
                callback_summary.failed, callback_summary.total
            ));
        }

        Ok(WorkflowRunReport {
            project: self.project.project.project.name.clone(),
            environment: self.project.environment_name.clone(),
            workflow_id: workflow_id.to_string(),
            workflow_name: workflow.definition.name.clone(),
            slot_id: self.slot_id,
            started_at,
            finished_at: Utc::now().to_rfc3339(),
            status: status.to_string(),
            error: if error_parts.is_empty() {
                None
            } else {
                Some(error_parts.join("; "))
            },
            summary: WorkflowSummaryReport {
                executed_steps: step_reports.len(),
                passed_steps,
                failed_steps,
                duration_ms: started.elapsed().as_millis(),
            },
            callback_summary,
            callbacks,
            environment_artifacts: None,
            steps: step_reports,
        })
    }

    fn execute_workflow_steps<'a>(
        &'a mut self,
        steps: &'a [WorkflowStep],
        state: &'a mut WorkflowState,
        step_reports: &'a mut Vec<WorkflowStepReport>,
        deferred_teardowns: &'a mut Vec<DeferredTeardown>,
        any_case_failed: &'a mut bool,
        fail_fast: bool,
        stop_execution: &'a mut bool,
        executed_run_case_steps: &'a mut usize,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            for step in steps {
                if *stop_execution {
                    break;
                }
                match step {
                    WorkflowStep::RunCase(run_case) => {
                        let wf_runtime = build_workflow_runtime(&self.project, state)?;
                        let resolved_inputs: IndexMap<String, Value> = run_case
                            .inputs
                            .iter()
                            .map(|(key, value)| {
                                Ok((
                                    key.clone(),
                                    wf_runtime.resolve_value(value).with_context(|| {
                                        format!(
                                            "workflow step `{}` failed to resolve input `{key}`",
                                            run_case.id
                                        )
                                    })?,
                                ))
                            })
                            .collect::<Result<_>>()?;

                        let case = self
                            .project
                            .cases
                            .iter()
                            .find(|case| case.id == run_case.case_id)
                            .with_context(|| {
                                format!(
                                    "workflow step `{}`: case `{}` not found",
                                    run_case.id, run_case.case_id
                                )
                            })?
                            .clone();

                        let mut outcome = self
                            .run_case_in_workflow(
                                &run_case.id,
                                &case,
                                &resolved_inputs,
                                &run_case.exports,
                                run_case.cleanup,
                            )
                            .await;

                        let passed = outcome.step_report.passed;
                        if !passed {
                            *any_case_failed = true;
                            if fail_fast {
                                *stop_execution = true;
                            }
                        }
                        state.steps.insert(
                            run_case.id.clone(),
                            WorkflowStepState {
                                status: outcome.step_report.status.clone(),
                                passed,
                                error: outcome
                                    .step_report
                                    .error
                                    .as_ref()
                                    .map(|error| Value::String(error.clone())),
                                exports: outcome.step_report.exports.clone(),
                            },
                        );

                        if let Some(mut deferred) = outcome.deferred.take() {
                            deferred.step_id = run_case.id.clone();
                            deferred_teardowns.push(deferred);
                        }

                        *executed_run_case_steps += 1;
                        self.console
                            .workflow_step_finished(*executed_run_case_steps, &outcome.step_report);
                        step_reports.push(outcome.step_report);
                    }
                    WorkflowStep::Conditional(cond) => {
                        let wf_runtime = build_workflow_runtime(&self.project, state)?;
                        let condition = wf_runtime
                            .evaluate_condition(&cond.condition)
                            .with_context(|| {
                                format!(
                                    "workflow conditional step failed to evaluate `{}`",
                                    cond.condition
                                )
                            })?;
                        let branch = if condition {
                            &cond.then_steps
                        } else {
                            &cond.else_steps
                        };
                        self.execute_workflow_steps(
                            branch,
                            state,
                            step_reports,
                            deferred_teardowns,
                            any_case_failed,
                            fail_fast,
                            stop_execution,
                            executed_run_case_steps,
                        )
                        .await?;
                    }
                }
            }
            Ok(())
        })
    }

    async fn run_case_in_workflow(
        &mut self,
        step_id: &str,
        case: &LoadedCase,
        inputs: &IndexMap<String, Value>,
        export_specs: &IndexMap<String, String>,
        cleanup: CleanupPolicy,
    ) -> WorkflowCaseOutcome {
        let started = Instant::now();

        if !self.project.apis.contains_key(&case.definition.api) {
            return WorkflowCaseOutcome {
                step_report: WorkflowStepReport {
                    id: step_id.to_string(),
                    case_id: case.id.clone(),
                    status: "failed".to_string(),
                    passed: false,
                    duration_ms: started.elapsed().as_millis(),
                    error: Some(format!("API `{}` was not found", case.definition.api)),
                    exports: serde_json::Map::new(),
                    case_steps: Vec::new(),
                    deferred_teardown_steps: Vec::new(),
                },
                deferred: None,
            };
        }

        let mut context = match ExecutionContext::new(&self.project, case) {
            Ok(context) => context,
            Err(error) => {
                return WorkflowCaseOutcome {
                    step_report: WorkflowStepReport {
                        id: step_id.to_string(),
                        case_id: case.id.clone(),
                        status: "failed".to_string(),
                        passed: false,
                        duration_ms: started.elapsed().as_millis(),
                        error: Some(error.to_string()),
                        exports: serde_json::Map::new(),
                        case_steps: Vec::new(),
                        deferred_teardown_steps: Vec::new(),
                    },
                    deferred: None,
                };
            }
        };

        let mut case_steps = Vec::new();
        let mut case_error: Option<String> = None;
        let mut should_run_cleanup = false;

        for (name, value) in inputs {
            context.set_var(name, value.clone());
        }

        for (name, value) in &case.definition.vars {
            if context.lookup_var(name).is_some() {
                continue;
            }
            match context.resolve_value(value) {
                Ok(resolved) => context.set_var(name, resolved),
                Err(error) => {
                    case_error = Some(format!("failed to resolve var `{name}`: {error}"));
                    break;
                }
            }
        }

        if case_error.is_none() {
            if let Err(error) = self
                .execute_step_list(&case.definition.setup, &mut context, &mut case_steps)
                .await
            {
                case_error = Some(format!("setup failed: {error}"));
            }
        }

        if case_error.is_none() {
            should_run_cleanup = true;
            if let Err(error) = self
                .execute_step_list(&case.definition.steps, &mut context, &mut case_steps)
                .await
            {
                case_error = Some(format!("steps failed: {error}"));
            }
        }

        let saved_root = context.root().clone();

        let exports = if case_error.is_none() {
            match export_specs
                .iter()
                .map(|(name, path)| {
                    Ok((
                        name.clone(),
                        context.evaluate_expr_value(path).with_context(|| {
                            format!("failed to resolve workflow export `{name}` from `{path}`")
                        })?,
                    ))
                })
                .collect::<Result<serde_json::Map<_, _>>>()
            {
                Ok(exports) => exports,
                Err(error) => {
                    case_error = Some(format!("failed to evaluate workflow exports: {error}"));
                    serde_json::Map::new()
                }
            }
        } else {
            serde_json::Map::new()
        };

        let deferred = if should_run_cleanup {
            match cleanup {
                CleanupPolicy::Immediate => {
                    if let Err(teardown_error) = self
                        .execute_step_list(&case.definition.teardown, &mut context, &mut case_steps)
                        .await
                    {
                        let message = format!("teardown failed: {teardown_error}");
                        match &mut case_error {
                            Some(existing) => *existing = format!("{existing}; {message}"),
                            None => case_error = Some(message),
                        }
                    }
                    None
                }
                CleanupPolicy::Defer => {
                    if case.definition.teardown.is_empty() {
                        None
                    } else {
                        Some(DeferredTeardown {
                            step_id: step_id.to_string(),
                            case: case.clone(),
                            saved_root,
                            teardown_steps: case.definition.teardown.clone(),
                        })
                    }
                }
                CleanupPolicy::Skip => None,
            }
        } else {
            None
        };

        let passed = case_error.is_none();
        WorkflowCaseOutcome {
            step_report: WorkflowStepReport {
                id: step_id.to_string(),
                case_id: case.id.clone(),
                status: if passed { "passed" } else { "failed" }.to_string(),
                passed,
                duration_ms: started.elapsed().as_millis(),
                error: case_error,
                exports,
                case_steps,
                deferred_teardown_steps: Vec::new(),
            },
            deferred,
        }
    }

    async fn run_deferred_teardown(
        &mut self,
        deferred: DeferredTeardown,
    ) -> (String, Vec<StepReport>, Option<String>) {
        let mut context =
            match ExecutionContext::from_saved_root(&deferred.case, deferred.saved_root) {
                Ok(context) => context,
                Err(error) => {
                    return (
                        deferred.step_id.clone(),
                        Vec::new(),
                        Some(format!("context init failed: {error}")),
                    );
                }
            };

        let mut reports = Vec::new();
        let error = self
            .execute_step_list(&deferred.teardown_steps, &mut context, &mut reports)
            .await
            .err()
            .map(|error| error.to_string());

        (deferred.step_id.clone(), reports, error)
    }

    async fn execute_case(&mut self, case: &LoadedCase) -> CaseReport {
        let started = Instant::now();
        let api = match self.project.apis.get(&case.definition.api) {
            Some(api) => api.clone(),
            None => {
                return CaseReport {
                    id: case.id.clone(),
                    name: case.definition.name.clone(),
                    api: case.definition.api.clone(),
                    slot_id: self.slot_id,
                    status: "failed".to_string(),
                    duration_ms: started.elapsed().as_millis(),
                    error: Some(format!("API `{}` was not found", case.definition.api)),
                    steps: Vec::new(),
                };
            }
        };

        match self.execute_case_inner(case, &api).await {
            Ok(steps) => CaseReport {
                id: case.id.clone(),
                name: case.definition.name.clone(),
                api: case.definition.api.clone(),
                slot_id: self.slot_id,
                status: "passed".to_string(),
                duration_ms: started.elapsed().as_millis(),
                error: None,
                steps,
            },
            Err((steps, error)) => CaseReport {
                id: case.id.clone(),
                name: case.definition.name.clone(),
                api: case.definition.api.clone(),
                slot_id: self.slot_id,
                status: "failed".to_string(),
                duration_ms: started.elapsed().as_millis(),
                error: Some(error.to_string()),
                steps,
            },
        }
    }

    async fn execute_case_inner(
        &mut self,
        case: &LoadedCase,
        _api: &LoadedApi,
    ) -> std::result::Result<Vec<StepReport>, (Vec<StepReport>, anyhow::Error)> {
        let mut context = match ExecutionContext::new(&self.project, case) {
            Ok(context) => context,
            Err(error) => return Err((Vec::new(), error)),
        };
        let mut reports = Vec::new();

        for (name, value) in &case.definition.vars {
            match context.resolve_value(value) {
                Ok(resolved) => context.set_var(name, resolved),
                Err(error) => {
                    return Err((
                        reports,
                        error.context(format!("failed to resolve var `{name}`")),
                    ));
                }
            }
        }

        if let Err(error) = self
            .execute_step_list(&case.definition.setup, &mut context, &mut reports)
            .await
        {
            return Err((reports, error.context("setup failed")));
        }
        if let Err(error) = self
            .execute_step_list(&case.definition.steps, &mut context, &mut reports)
            .await
        {
            let teardown_result = self
                .execute_step_list(&case.definition.teardown, &mut context, &mut reports)
                .await;
            if let Err(teardown_error) = teardown_result {
                return Err((
                    reports,
                    error.context(format!(
                        "steps failed; teardown also failed: {teardown_error}"
                    )),
                ));
            }
            return Err((reports, error.context("steps failed")));
        }
        if let Err(error) = self
            .execute_step_list(&case.definition.teardown, &mut context, &mut reports)
            .await
        {
            return Err((reports, error.context("teardown failed")));
        }

        Ok(reports)
    }

    fn execute_step_list<'a>(
        &'a mut self,
        steps: &'a [Step],
        context: &'a mut ExecutionContext<'_>,
        reports: &'a mut Vec<StepReport>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            for step in steps {
                self.execute_single_step(step, context, reports).await?;
            }
            Ok(())
        })
    }

    async fn execute_single_step(
        &mut self,
        step: &Step,
        context: &mut ExecutionContext<'_>,
        reports: &mut Vec<StepReport>,
    ) -> Result<()> {
        match step {
            Step::UseData { path } => {
                let started = Instant::now();
                let value = load_specific_data_file(&self.project.runner_root.join("data"), path)?;
                context.insert_data_path(path, value)?;
                reports.push(StepReport {
                    kind: "use_data".to_string(),
                    status: "passed".to_string(),
                    duration_ms: started.elapsed().as_millis(),
                    details: json!({ "path": path }),
                    error: None,
                });
                Ok(())
            }
            Step::Set { values } => {
                let started = Instant::now();
                for (key, value) in values {
                    let resolved = context.resolve_value(value)?;
                    context.set_var(key, resolved);
                }
                reports.push(StepReport {
                    kind: "set".to_string(),
                    status: "passed".to_string(),
                    duration_ms: started.elapsed().as_millis(),
                    details: json!({ "keys": values.keys().collect::<Vec<_>>() }),
                    error: None,
                });
                Ok(())
            }
            Step::Sql(step) => self.run_sql_step(step, context, reports).await,
            Step::Redis(step) => self.run_redis_step(step, context, reports).await,
            Step::Request(step) => self.run_request_step(step, context, reports).await,
            Step::Callback(step) => self.run_callback_step(step, context, reports).await,
            Step::Sleep(step) => self.run_sleep_step(step, context, reports).await,
            Step::QueryDb(step) => self.run_query_db_step(step, context, reports).await,
            Step::QueryRedis(step) => self.run_query_redis_step(step, context, reports).await,
            Step::Conditional(step) => self.run_conditional_step(step, context, reports).await,
            Step::Foreach(step) => self.run_foreach_step(step, context, reports).await,
        }
    }

    async fn run_sql_step(
        &mut self,
        step: &SqlExecStep,
        context: &mut ExecutionContext<'_>,
        reports: &mut Vec<StepReport>,
    ) -> Result<()> {
        let started = Instant::now();
        let script = self.load_sql(step.sql.as_ref(), step.file.as_ref())?;
        let rendered = context.render_string(&script)?;
        let result = self.execute_sql_script(&step.datasource, &rendered).await?;
        context.set_result(result.clone());
        reports.push(StepReport {
            kind: "sql".to_string(),
            status: "passed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            details: json!({ "datasource": step.datasource, "result": result }),
            error: None,
        });
        Ok(())
    }

    async fn run_redis_step(
        &mut self,
        step: &RedisCommandStep,
        context: &mut ExecutionContext<'_>,
        reports: &mut Vec<StepReport>,
    ) -> Result<()> {
        let started = Instant::now();
        let args = self.resolve_args(&step.args, context)?;
        let result = self
            .execute_redis_command(&step.datasource, &step.command, &args)
            .await?;
        context.set_result(result.clone());
        reports.push(StepReport {
            kind: "redis".to_string(),
            status: "passed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            details: json!({ "datasource": step.datasource, "command": step.command, "result": result }),
            error: None,
        });
        Ok(())
    }

    async fn run_request_step(
        &mut self,
        step: &RequestStep,
        context: &mut ExecutionContext<'_>,
        reports: &mut Vec<StepReport>,
    ) -> Result<()> {
        let started = Instant::now();
        let response = self.execute_request(&step.request, context).await?;
        context.set_response(response.clone());
        context.set_result(response.clone());
        context
            .apply_extracts(&step.extract)
            .context("request extract failed")?;
        apply_assertions(&step.assertions, context).context("request assertions failed")?;
        reports.push(StepReport {
            kind: "request".to_string(),
            status: "passed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            details: json!({
                "api": step.request.api.clone().unwrap_or_else(|| context.case.definition.api.clone()),
                "status": response.get("status").cloned().unwrap_or(Value::Null)
            }),
            error: None,
        });
        Ok(())
    }

    async fn run_callback_step(
        &mut self,
        step: &CallbackStep,
        context: &mut ExecutionContext<'_>,
        reports: &mut Vec<StepReport>,
    ) -> Result<()> {
        let started = Instant::now();
        let request = prepare_callback_request(&self.request_context, &step.request, context)?;
        let scheduled = self.callback_runtime.schedule(ScheduledCallback {
            source: format!("case:{}", context.case.id),
            after_ms: step.after_ms,
            request,
        });
        let details = json!({
            "id": scheduled.id,
            "after_ms": scheduled.after_ms,
            "request": scheduled.request.to_json(),
        });
        context.set_result(details.clone());
        reports.push(StepReport {
            kind: "callback".to_string(),
            status: "passed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            details,
            error: None,
        });
        Ok(())
    }

    async fn run_sleep_step(
        &mut self,
        step: &SleepStep,
        context: &mut ExecutionContext<'_>,
        reports: &mut Vec<StepReport>,
    ) -> Result<()> {
        let started = Instant::now();
        sleep(Duration::from_millis(step.ms)).await;
        let details = json!({ "ms": step.ms });
        context.set_result(details.clone());
        reports.push(StepReport {
            kind: "sleep".to_string(),
            status: "passed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            details,
            error: None,
        });
        Ok(())
    }

    async fn run_query_db_step(
        &mut self,
        step: &QueryDbStep,
        context: &mut ExecutionContext<'_>,
        reports: &mut Vec<StepReport>,
    ) -> Result<()> {
        let started = Instant::now();
        let sql = self.load_sql(step.query.sql.as_ref(), step.query.file.as_ref())?;
        let rendered = context.render_string(&sql)?;
        let result = self
            .query_database(&step.query.datasource, &rendered)
            .await?;
        context.set_result(result.clone());
        context
            .apply_extracts(&step.extract)
            .context("query_db extract failed")?;
        apply_assertions(&step.assertions, context).context("query_db assertions failed")?;
        reports.push(StepReport {
            kind: "query_db".to_string(),
            status: "passed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            details: json!({ "datasource": step.query.datasource, "sql": rendered }),
            error: None,
        });
        Ok(())
    }

    async fn run_query_redis_step(
        &mut self,
        step: &QueryRedisStep,
        context: &mut ExecutionContext<'_>,
        reports: &mut Vec<StepReport>,
    ) -> Result<()> {
        let started = Instant::now();
        let args = self.resolve_args(&step.query.args, context)?;
        let result = self
            .execute_redis_command(&step.query.datasource, &step.query.command, &args)
            .await?;
        context.set_result(json!({ "value": result.clone() }));
        context
            .apply_extracts(&step.extract)
            .context("query_redis extract failed")?;
        apply_assertions(&step.assertions, context).context("query_redis assertions failed")?;
        reports.push(StepReport {
            kind: "query_redis".to_string(),
            status: "passed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            details: json!({ "datasource": step.query.datasource, "command": step.query.command, "args": args }),
            error: None,
        });
        Ok(())
    }

    async fn run_conditional_step(
        &mut self,
        step: &ConditionalStep,
        context: &mut ExecutionContext<'_>,
        reports: &mut Vec<StepReport>,
    ) -> Result<()> {
        let started = Instant::now();
        let condition = context
            .evaluate_condition(&step.condition)
            .with_context(|| format!("failed to evaluate if condition `{}`", step.condition))?;
        reports.push(StepReport {
            kind: "if".to_string(),
            status: "passed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            details: json!({ "condition": step.condition, "value": condition }),
            error: None,
        });
        if condition {
            self.execute_step_list(&step.then_steps, context, reports)
                .await?;
        } else {
            self.execute_step_list(&step.else_steps, context, reports)
                .await?;
        }
        Ok(())
    }

    async fn run_foreach_step(
        &mut self,
        step: &ForeachStep,
        context: &mut ExecutionContext<'_>,
        reports: &mut Vec<StepReport>,
    ) -> Result<()> {
        let started = Instant::now();
        let values = context
            .resolve_value(&Value::String(step.expression.clone()))
            .with_context(|| {
                format!(
                    "failed to evaluate foreach expression `{}`",
                    step.expression
                )
            })?;
        let Some(items) = values.as_array().cloned() else {
            bail!(
                "foreach expression {} did not resolve to an array",
                step.expression
            );
        };

        reports.push(StepReport {
            kind: "foreach".to_string(),
            status: "passed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            details: json!({ "expression": step.expression, "items": items.len() }),
            error: None,
        });

        let previous = context.lookup_var(&step.binding);
        for item in items {
            context.set_var(&step.binding, item);
            self.execute_step_list(&step.steps, context, reports)
                .await?;
        }
        context.restore_var(&step.binding, previous);
        Ok(())
    }

    fn load_sql(&self, inline: Option<&String>, file: Option<&String>) -> Result<String> {
        if let Some(sql) = inline {
            return Ok(sql.clone());
        }
        let file = file.context("expected file path")?;
        fs::read_to_string(self.project.runner_root.join(file))
            .with_context(|| format!("failed to read SQL file {file}"))
    }

    fn resolve_args(&self, args: &[Value], context: &ExecutionContext<'_>) -> Result<Vec<String>> {
        args.iter()
            .map(|value| context.resolve_value(value).map(value_to_string))
            .collect()
    }

    async fn execute_request(
        &self,
        step: &RequestSpec,
        context: &ExecutionContext<'_>,
    ) -> Result<Value> {
        let request = prepare_case_request(
            &self.request_context,
            &context.case.definition.api,
            step,
            context,
        )?;
        self.send_prepared_request(&request).await
    }

    async fn send_prepared_request(&self, request: &PreparedRequest) -> Result<Value> {
        let mut builder = self
            .http_client
            .request(request.method.clone(), &request.url);
        for (key, value) in &request.headers {
            builder = builder.header(key, value);
        }
        match &request.body {
            Some(PreparedRequestBody::Text(body)) => {
                builder = builder.body(body.clone());
            }
            Some(PreparedRequestBody::Json(body)) => {
                builder = builder.json(body);
            }
            None => {}
        }
        let response = builder.send().await?;
        response_to_json(response).await
    }

    async fn execute_sql_script(&mut self, datasource: &str, script: &str) -> Result<Value> {
        let statements = split_sql_statements(script);
        if statements.is_empty() {
            return Ok(json!({ "affected_rows": 0 }));
        }

        let pool = self.database_pool(datasource).await?;
        let mut affected_rows = 0u64;
        for statement in statements {
            affected_rows += pool.execute(&statement).await?;
        }
        Ok(json!({ "affected_rows": affected_rows }))
    }

    async fn query_database(&mut self, datasource: &str, sql: &str) -> Result<Value> {
        let pool = self.database_pool(datasource).await?;
        pool.query(sql).await
    }

    async fn execute_redis_command(
        &mut self,
        datasource: &str,
        command: &str,
        args: &[String],
    ) -> Result<Value> {
        let datasource_config = self
            .project
            .datasources
            .get(datasource)
            .with_context(|| format!("unknown datasource `{datasource}`"))?;
        let DatasourceDefinition::Redis(redis_config) = datasource_config else {
            bail!("datasource `{datasource}` is not a Redis datasource");
        };

        let client = if let Some(client) = self.redis_clients.get(datasource) {
            client.clone()
        } else {
            let client = redis::Client::open(redis_config.url.as_str())?;
            self.redis_clients
                .insert(datasource.to_string(), client.clone());
            client
        };

        let mut connection = client.get_multiplexed_tokio_connection().await?;
        let mut cmd = redis::cmd(command);
        for arg in args {
            cmd.arg(arg);
        }
        let result: redis::Value = cmd.query_async(&mut connection).await?;
        Ok(redis_value_to_json(result))
    }

    async fn database_pool(&mut self, datasource: &str) -> Result<&mut DatabasePool> {
        if !self.db_pools.contains_key(datasource) {
            let definition = self
                .project
                .datasources
                .get(datasource)
                .with_context(|| format!("unknown datasource `{datasource}`"))?
                .clone();
            let pool = match definition {
                DatasourceDefinition::Mysql(config) => {
                    DatabasePool::Mysql(MySqlPool::connect(&config.url).await?)
                }
                DatasourceDefinition::Postgres(config) => {
                    DatabasePool::Postgres(PgPool::connect(&config.url).await?)
                }
                DatasourceDefinition::Redis(_) => {
                    bail!("datasource `{datasource}` is not a SQL datasource")
                }
            };
            self.db_pools.insert(datasource.to_string(), pool);
        }

        self.db_pools
            .get_mut(datasource)
            .with_context(|| format!("failed to create datasource `{datasource}`"))
    }
}

fn build_http_client(project: &LoadedProject) -> reqwest::Client {
    let timeout = std::time::Duration::from_millis(project.project.defaults.timeout_ms);
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("failed to build reqwest client")
}

fn resolve_parallel_jobs(
    project: &LoadedProject,
    options: &CommonTestArgs,
    unit_count: usize,
) -> Result<Option<usize>> {
    if !options.parallel_requested() || unit_count <= 1 {
        return Ok(None);
    }
    let runtime = project
        .environment
        .runtime
        .as_ref()
        .context("--parallel requires environment.runtime to be configured")?;
    if runtime.kind != crate::config::EnvironmentRuntimeKind::Containers {
        bail!("--parallel requires environment.runtime.kind = containers");
    }
    let jobs = match options.jobs {
        Some(0) => bail!("--jobs must be greater than zero"),
        Some(jobs) => jobs,
        None => runtime
            .parallel
            .as_ref()
            .map(|parallel| parallel.slots)
            .context("--parallel requires environment.runtime.parallel.slots or --jobs")?,
    };
    if jobs == 0 {
        bail!("--jobs must be greater than zero");
    }
    Ok(Some(jobs.min(unit_count)))
}

fn select_workflows(
    project: &LoadedProject,
    args: &TestWorkflowArgs,
) -> Result<Vec<LoadedWorkflow>> {
    if args.all {
        let workflows = project.workflows.values().cloned().collect::<Vec<_>>();
        if workflows.is_empty() {
            bail!("no workflow definitions were found under {TESTRUNNER_DIR}/workflows");
        }
        return Ok(workflows);
    }

    let workflow_id = args
        .workflow_id
        .as_deref()
        .context("workflow id is required unless --all is specified")?;
    let workflow = project
        .workflows
        .get(workflow_id)
        .with_context(|| format!("workflow `{workflow_id}` not found in {TESTRUNNER_DIR}"))?
        .clone();
    Ok(vec![workflow])
}

fn print_workflows_dry_run(workflows: &[LoadedWorkflow], env_name: &str) {
    println!(
        "Selected {} workflow(s) in env `{env_name}`:",
        workflows.len()
    );
    for workflow in workflows {
        println!(
            "  - {} ({}) — {} step(s)",
            workflow.id,
            workflow.definition.name,
            count_run_case_steps(&workflow.definition.steps)
        );
    }
}

async fn build_slot_execution_contexts(
    session: &EnvironmentSession,
    should_start_mock: bool,
    console: &SummaryConsole,
    reserved_mock_endpoints: Vec<SlotReservedMockEndpoint>,
) -> Result<(Vec<SlotExecutionContext>, Vec<mock::MockServerHandle>)> {
    let slot_ids = if session.slots().is_empty() {
        vec![0]
    } else {
        session
            .slots()
            .iter()
            .map(|slot| slot.slot_id)
            .collect::<Vec<_>>()
    };

    let mut contexts = Vec::new();
    let mut mock_servers: Vec<mock::MockServerHandle> = Vec::new();
    let mut first_mock_base_url = None;

    if should_start_mock {
        console.mock_starting();
    }

    let multi_slot_mock = should_start_mock && slot_ids.len() > 1;
    let using_reserved_mock_endpoints = !reserved_mock_endpoints.is_empty();
    let mut reserved_mock_endpoints = reserved_mock_endpoints
        .into_iter()
        .map(|reservation| (reservation.slot_id, reservation.endpoint))
        .collect::<HashMap<_, _>>();

    for slot_id in slot_ids {
        let base_project = match session.project_for_slot(slot_id) {
            Ok(project) => project,
            Err(error) => {
                for server in mock_servers.drain(..) {
                    server.shutdown().await;
                }
                return Err(error);
            }
        };
        let callback_runtime = CallbackRuntime::new(build_http_client(&base_project));
        let mut execution_project = base_project.clone();

        if should_start_mock {
            let reserved_endpoint = reserved_mock_endpoints.remove(&slot_id);
            if using_reserved_mock_endpoints && reserved_endpoint.is_none() {
                for server in mock_servers.drain(..) {
                    server.shutdown().await;
                }
                return Err(anyhow!(
                    "no reserved mock endpoint found for slot `{slot_id}`"
                ));
            }

            let mut mock_project = base_project.clone();
            if let Some(endpoint) = reserved_endpoint.as_ref() {
                mock_project.project.mock.port = endpoint.port;
            } else if multi_slot_mock {
                mock_project.project.mock.port = 0;
            }
            let handle = match if let Some(endpoint) = reserved_endpoint {
                mock::start_reserved(
                    &mock_project,
                    RequestPreparationContext::from_project(&mock_project),
                    callback_runtime.clone(),
                    endpoint,
                )
                .await
            } else {
                mock::start(
                    &mock_project,
                    RequestPreparationContext::from_project(&mock_project),
                    callback_runtime.clone(),
                )
                .await
            } {
                Ok(handle) => handle,
                Err(error) => {
                    for server in mock_servers.drain(..) {
                        server.shutdown().await;
                    }
                    return Err(error);
                }
            };
            if first_mock_base_url.is_none() {
                first_mock_base_url = Some(handle.base_url.clone());
            }
            apply_mock_base_url(&mut execution_project, &handle)?;
            mock_servers.push(handle);
        }

        contexts.push(SlotExecutionContext {
            slot_id,
            project: execution_project,
            callback_runtime,
        });
    }

    if let Some(base_url) = first_mock_base_url {
        if mock_servers.len() == 1 {
            console.mock_ready(&base_url);
        } else {
            console.mock_pool_ready(mock_servers.len());
        }
    }

    Ok((contexts, mock_servers))
}

async fn prepare_slot_mock_endpoints(
    project: &LoadedProject,
    should_start_mock: bool,
    slot_count: usize,
    session: &mut EnvironmentSession,
) -> Result<Vec<SlotReservedMockEndpoint>> {
    let endpoints = reserve_slot_mock_endpoints(project, should_start_mock, slot_count).await?;
    if endpoints.is_empty() {
        return Ok(endpoints);
    }

    session.set_slot_mock_base_urls(
        project.project.mock.port,
        container_slot_mock_base_urls(&endpoints)?,
    )?;
    Ok(endpoints)
}

async fn reserve_slot_mock_endpoints(
    project: &LoadedProject,
    should_start_mock: bool,
    slot_count: usize,
) -> Result<Vec<SlotReservedMockEndpoint>> {
    let uses_containers_runtime = matches!(
        project
            .environment
            .runtime
            .as_ref()
            .map(|runtime| runtime.kind),
        Some(EnvironmentRuntimeKind::Containers)
    );
    if !should_start_mock || !uses_containers_runtime {
        return Ok(Vec::new());
    }

    let mut endpoints = Vec::new();
    for slot_id in 0..slot_count {
        let port = if slot_count > 1 {
            0
        } else {
            project.project.mock.port
        };
        let endpoint = mock::reserve_endpoint(&project.project.mock.host, port).await?;
        endpoints.push(SlotReservedMockEndpoint { slot_id, endpoint });
    }
    Ok(endpoints)
}

fn container_slot_mock_base_urls(
    reserved_mock_endpoints: &[SlotReservedMockEndpoint],
) -> Result<HashMap<usize, String>> {
    reserved_mock_endpoints
        .iter()
        .map(|endpoint| {
            Ok((
                endpoint.slot_id,
                container_visible_mock_base_url(&endpoint.endpoint.base_url)?,
            ))
        })
        .collect()
}

fn apply_mock_base_url(project: &mut LoadedProject, handle: &mock::MockServerHandle) -> Result<()> {
    let local_base_url = handle.base_url.clone();
    project.environment.variables.insert(
        "mock_base_url".to_string(),
        Value::String(local_base_url.clone()),
    );

    let replacement_url = if project.environment.runtime.is_some() {
        container_visible_mock_base_url(&local_base_url)?
    } else {
        local_base_url.clone()
    };
    rewrite_mock_urls(project, project.project.mock.port, &replacement_url);
    Ok(())
}

fn container_visible_mock_base_url(base_url: &str) -> Result<String> {
    let url =
        Url::parse(base_url).with_context(|| format!("invalid mock base URL `{base_url}`"))?;
    let port = url
        .port()
        .with_context(|| format!("mock base URL `{base_url}` does not contain a port"))?;
    Ok(format!("http://host.docker.internal:{port}"))
}

fn rewrite_mock_urls(project: &mut LoadedProject, original_port: u16, replacement_base_url: &str) {
    rewrite_url_base_in_place(
        &mut project.environment.base_url,
        original_port,
        replacement_base_url,
    );
    for value in project.environment.variables.values_mut() {
        rewrite_value_url_bases(value, original_port, replacement_base_url);
    }
    for api in project.apis.values_mut() {
        if let Some(base_url) = api.definition.base_url.as_mut() {
            rewrite_url_base_in_place(base_url, original_port, replacement_base_url);
        }
    }
    if let Some(runtime) = project.environment.runtime.as_mut()
        && runtime.kind == EnvironmentRuntimeKind::Containers
    {
        for service in &mut runtime.services {
            for value in service.environment.values_mut() {
                rewrite_url_base_in_place(value, original_port, replacement_base_url);
            }
        }
    }
}

async fn execute_cases_parallel(
    slot_contexts: Vec<SlotExecutionContext>,
    cases: &[LoadedCase],
    options: &CommonTestArgs,
    target: &TargetSelection,
    console: &SummaryConsole,
    jobs: usize,
) -> Result<RunReport> {
    let first_slot = slot_contexts
        .first()
        .context("parallel case run requires at least one execution slot")?;
    let project_name = first_slot.project.project.project.name.clone();
    let environment_name = first_slot.project.environment_name.clone();
    let allocator = SlotAllocator::new(slot_contexts);
    let started_at = Utc::now().to_rfc3339();
    let started = Instant::now();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut join_set = JoinSet::new();
    let mut next_index = 0usize;
    let mut completed = 0usize;
    let mut reports = std::iter::repeat_with(|| None)
        .take(cases.len())
        .collect::<Vec<Option<CaseTaskResult>>>();

    while next_index < cases.len() && join_set.len() < jobs {
        spawn_case_task(
            &mut join_set,
            next_index,
            cases[next_index].clone(),
            options.clone(),
            target.clone(),
            allocator.clone(),
        )
        .await?;
        next_index += 1;
    }

    while let Some(joined) = join_set.join_next().await {
        let result = joined.context("case task failed to join")??;
        completed += 1;
        console.case_finished(completed, cases.len(), &result.report);
        if result.report.status == "failed" && options.fail_fast {
            cancel.store(true, Ordering::SeqCst);
        }
        let should_spawn_more = !(options.fail_fast && cancel.load(Ordering::SeqCst));
        let result_index = result.index;
        reports[result_index] = Some(result);
        if should_spawn_more && next_index < cases.len() {
            spawn_case_task(
                &mut join_set,
                next_index,
                cases[next_index].clone(),
                options.clone(),
                target.clone(),
                allocator.clone(),
            )
            .await?;
            next_index += 1;
        }
    }

    let mut callbacks = Vec::new();
    let mut ordered_reports = Vec::new();
    let mut passed = 0usize;
    let mut failed = 0usize;
    for task_result in reports.into_iter().flatten() {
        if task_result.report.status == "failed" {
            failed += 1;
        } else {
            passed += 1;
        }
        callbacks.extend(task_result.callbacks);
        ordered_reports.push(task_result.report);
    }
    let callback_summary = CallbackSummaryReport::from_reports(&callbacks);

    Ok(RunReport {
        project: project_name,
        environment: environment_name,
        target: target.display(),
        started_at,
        finished_at: Utc::now().to_rfc3339(),
        summary: SummaryReport {
            total: ordered_reports.len(),
            passed,
            failed,
            duration_ms: started.elapsed().as_millis(),
        },
        callback_summary,
        parallel: Some(ParallelRunMetadata {
            jobs,
            slots: allocator.slot_count(),
            unit: "case".to_string(),
        }),
        callbacks,
        environment_artifacts: None,
        cases: ordered_reports,
    })
}

async fn spawn_case_task(
    join_set: &mut JoinSet<Result<CaseTaskResult>>,
    index: usize,
    case: LoadedCase,
    options: CommonTestArgs,
    target: TargetSelection,
    allocator: SlotAllocator,
) -> Result<()> {
    join_set.spawn(async move {
        let lease = allocator.acquire().await?;
        let slot = lease.slot().clone();
        let mut runner = Runner::new_with_callback_runtime(
            slot.project,
            ReportFormat::Json,
            slot.callback_runtime,
            Some(slot.slot_id),
        );
        let run_report = runner
            .execute(std::slice::from_ref(&case), &options, &target)
            .await?;
        let case_report = run_report
            .cases
            .into_iter()
            .next()
            .context("expected single case report")?;
        Ok(CaseTaskResult {
            index,
            report: case_report,
            callbacks: run_report.callbacks,
        })
    });
    Ok(())
}

async fn execute_workflows_serial(
    slot_context: SlotExecutionContext,
    workflows: &[LoadedWorkflow],
    options: &CommonTestArgs,
    console: &SummaryConsole,
) -> Result<WorkflowBatchRunReport> {
    let started_at = Utc::now().to_rfc3339();
    let started = Instant::now();
    let mut reports = Vec::new();
    let mut callbacks = Vec::new();
    let mut passed = 0usize;
    let mut failed = 0usize;

    for (index, workflow) in workflows.iter().enumerate() {
        let mut runner = Runner::new_with_callback_runtime(
            slot_context.project.clone(),
            ReportFormat::Json,
            slot_context.callback_runtime.clone(),
            None,
        );
        let report = runner
            .execute_workflow(&workflow.id, workflow, options)
            .await?;
        console.workflow_finished(index + 1, workflows.len(), &report);
        if report.status == "failed" {
            failed += 1;
        } else {
            passed += 1;
        }
        callbacks.extend(report.callbacks.clone());
        reports.push(report);
        if failed > 0 && options.fail_fast {
            break;
        }
    }

    Ok(WorkflowBatchRunReport {
        project: slot_context.project.project.project.name.clone(),
        environment: slot_context.project.environment_name.clone(),
        started_at,
        finished_at: Utc::now().to_rfc3339(),
        summary: WorkflowBatchSummaryReport {
            total_workflows: reports.len(),
            passed_workflows: passed,
            failed_workflows: failed,
            duration_ms: started.elapsed().as_millis(),
        },
        callback_summary: CallbackSummaryReport::from_reports(&callbacks),
        parallel: None,
        callbacks,
        environment_artifacts: None,
        workflows: reports,
    })
}

async fn execute_workflows_parallel(
    slot_contexts: Vec<SlotExecutionContext>,
    workflows: &[LoadedWorkflow],
    options: &CommonTestArgs,
    console: &SummaryConsole,
    jobs: usize,
) -> Result<WorkflowBatchRunReport> {
    let allocator = SlotAllocator::new(slot_contexts);
    let started_at = Utc::now().to_rfc3339();
    let started = Instant::now();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut join_set = JoinSet::new();
    let mut next_index = 0usize;
    let mut completed = 0usize;
    let mut reports = std::iter::repeat_with(|| None)
        .take(workflows.len())
        .collect::<Vec<Option<WorkflowTaskResult>>>();

    while next_index < workflows.len() && join_set.len() < jobs {
        spawn_workflow_task(
            &mut join_set,
            next_index,
            workflows[next_index].clone(),
            options.clone(),
            allocator.clone(),
        )
        .await?;
        next_index += 1;
    }

    while let Some(joined) = join_set.join_next().await {
        let result = joined.context("workflow task failed to join")??;
        completed += 1;
        console.workflow_finished(completed, workflows.len(), &result.report);
        if result.report.status == "failed" && options.fail_fast {
            cancel.store(true, Ordering::SeqCst);
        }
        let should_spawn_more = !(options.fail_fast && cancel.load(Ordering::SeqCst));
        let result_index = result.index;
        reports[result_index] = Some(result);
        if should_spawn_more && next_index < workflows.len() {
            spawn_workflow_task(
                &mut join_set,
                next_index,
                workflows[next_index].clone(),
                options.clone(),
                allocator.clone(),
            )
            .await?;
            next_index += 1;
        }
    }

    let mut callbacks = Vec::new();
    let mut ordered_reports = Vec::new();
    let mut passed = 0usize;
    let mut failed = 0usize;
    for task_result in reports.into_iter().flatten() {
        if task_result.report.status == "failed" {
            failed += 1;
        } else {
            passed += 1;
        }
        callbacks.extend(task_result.report.callbacks.clone());
        ordered_reports.push(task_result.report);
    }
    let first = ordered_reports
        .first()
        .context("parallel workflow run did not produce any reports")?;
    Ok(WorkflowBatchRunReport {
        project: first.project.clone(),
        environment: first.environment.clone(),
        started_at,
        finished_at: Utc::now().to_rfc3339(),
        summary: WorkflowBatchSummaryReport {
            total_workflows: ordered_reports.len(),
            passed_workflows: passed,
            failed_workflows: failed,
            duration_ms: started.elapsed().as_millis(),
        },
        callback_summary: CallbackSummaryReport::from_reports(&callbacks),
        parallel: Some(ParallelRunMetadata {
            jobs,
            slots: allocator.slot_count(),
            unit: "workflow".to_string(),
        }),
        callbacks,
        environment_artifacts: None,
        workflows: ordered_reports,
    })
}

async fn spawn_workflow_task(
    join_set: &mut JoinSet<Result<WorkflowTaskResult>>,
    index: usize,
    workflow: LoadedWorkflow,
    options: CommonTestArgs,
    allocator: SlotAllocator,
) -> Result<()> {
    join_set.spawn(async move {
        let lease = allocator.acquire().await?;
        let slot = lease.slot().clone();
        let mut runner = Runner::new_with_callback_runtime(
            slot.project,
            ReportFormat::Json,
            slot.callback_runtime,
            Some(slot.slot_id),
        );
        let report = runner
            .execute_workflow(&workflow.id, &workflow, &options)
            .await?;
        Ok(WorkflowTaskResult { index, report })
    });
    Ok(())
}

enum DatabasePool {
    Mysql(MySqlPool),
    Postgres(PgPool),
}

impl DatabasePool {
    async fn execute(&self, sql: &str) -> Result<u64> {
        match self {
            Self::Mysql(pool) => Ok(sqlx::query(sql).execute(pool).await?.rows_affected()),
            Self::Postgres(pool) => Ok(sqlx::query(sql).execute(pool).await?.rows_affected()),
        }
    }

    async fn query(&self, sql: &str) -> Result<Value> {
        match self {
            Self::Mysql(pool) => {
                let rows = sqlx::query(sql).fetch_all(pool).await?;
                let rows = rows
                    .iter()
                    .map(mysql_row_to_json)
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!({ "row_count": rows.len(), "rows": rows }))
            }
            Self::Postgres(pool) => {
                let rows = sqlx::query(sql).fetch_all(pool).await?;
                let rows = rows
                    .iter()
                    .map(postgres_row_to_json)
                    .collect::<Result<Vec<_>>>()?;
                Ok(json!({ "row_count": rows.len(), "rows": rows }))
            }
        }
    }
}

struct ExecutionContext<'a> {
    case: &'a LoadedCase,
    runtime: RuntimeContext,
}

impl<'a> ExecutionContext<'a> {
    fn new(project: &LoadedProject, case: &'a LoadedCase) -> Result<Self> {
        let api = project
            .apis
            .get(&case.definition.api)
            .with_context(|| format!("API `{}` was not found", case.definition.api))?;
        let mut root = serde_json::Map::new();
        root.insert(
            "env".to_string(),
            environment_context_value(&project.environment_name, &project.environment)?,
        );
        root.insert(
            "project".to_string(),
            json!({ "name": project.project.project.name, "root": project.root }),
        );
        root.insert(
            "case".to_string(),
            json!({ "id": case.id, "name": case.definition.name, "api": case.definition.api }),
        );
        root.insert(
            "api".to_string(),
            json!({
                "id": api.id,
                "name": api.definition.name,
                "method": api.definition.method,
                "path": api.definition.path,
                "file": api.relative_path,
            }),
        );
        root.insert("vars".to_string(), Value::Object(serde_json::Map::new()));
        root.insert(
            "data".to_string(),
            load_data_tree(&project.runner_root.join("data"))?,
        );

        Ok(Self {
            case,
            runtime: RuntimeContext::new(root)?,
        })
    }

    fn from_saved_root(case: &'a LoadedCase, root: serde_json::Map<String, Value>) -> Result<Self> {
        Ok(Self {
            case,
            runtime: RuntimeContext::new(root)?,
        })
    }

    fn set_response(&mut self, value: Value) {
        self.runtime.set_root_value("response", value);
    }

    fn set_result(&mut self, value: Value) {
        self.runtime.set_root_value("result", value);
    }

    fn insert_data_path(&mut self, relative_path: &str, value: Value) -> Result<()> {
        let mut segments = relative_path
            .split('/')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        if let Some(last) = segments.last_mut()
            && let Some((stem, _)) = last.rsplit_once('.')
        {
            *last = stem.to_string();
        }
        let data = self
            .runtime
            .root_mut()
            .get_mut("data")
            .and_then(Value::as_object_mut)
            .context("data tree is not available")?;
        insert_nested(data, &segments, value)
    }
}

impl Deref for ExecutionContext<'_> {
    type Target = RuntimeContext;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl DerefMut for ExecutionContext<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.runtime
    }
}

fn select_cases(
    project: &LoadedProject,
    target: &TargetSelection,
    options: &CommonTestArgs,
) -> Result<Vec<LoadedCase>> {
    let mut selected = Vec::new();
    for case in &project.cases {
        let matches_target = match target {
            TargetSelection::Api(api_id) => case.definition.api == *api_id,
            TargetSelection::Dir(dir) => {
                case.definition.api.starts_with(dir)
                    || path_prefix_matches(&case.relative_path, dir)
            }
            TargetSelection::All => true,
        };
        if !matches_target {
            continue;
        }
        if !options.tag.is_empty()
            && !options.tag.iter().all(|tag| {
                case.definition
                    .tags
                    .iter()
                    .any(|candidate| candidate == tag)
            })
        {
            continue;
        }
        if let Some(pattern) = &options.case_pattern
            && !case.id.contains(pattern)
            && !case.definition.name.contains(pattern)
        {
            continue;
        }
        selected.push(case.clone());
    }
    Ok(selected)
}

fn path_prefix_matches(path: &Path, prefix: &str) -> bool {
    let normalized = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/");
    normalized.starts_with(prefix)
}

async fn response_to_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status().as_u16();
    let headers = header_map_to_json(response.headers());
    let body = response.text().await?;
    let json_body = serde_json::from_str::<Value>(&body).ok();

    Ok(json!({
        "status": status,
        "headers": headers,
        "body": body,
        "json": json_body,
    }))
}

fn header_map_to_json(headers: &HeaderMap) -> Value {
    let map = headers
        .iter()
        .map(|(key, value)| {
            (
                key.as_str().to_ascii_lowercase(),
                Value::String(value.to_str().unwrap_or_default().to_string()),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    Value::Object(map)
}

fn insert_nested(
    root: &mut serde_json::Map<String, Value>,
    path: &[String],
    value: Value,
) -> Result<()> {
    if path.is_empty() {
        bail!("cannot insert into an empty path");
    }
    let mut current = root;
    for segment in &path[..path.len() - 1] {
        let entry = current
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        let Some(object) = entry.as_object_mut() else {
            bail!("{segment} is not an object node");
        };
        current = object;
    }
    current.insert(path[path.len() - 1].clone(), value);
    Ok(())
}

fn load_specific_data_file(data_root: &Path, relative_path: &str) -> Result<Value> {
    let path = data_root.join(relative_path);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read data file {}", path.display()))?;
    match extension {
        "json" => Ok(serde_json::from_str(&raw)?),
        "yaml" | "yml" => Ok(serde_yaml::from_str(&raw)?),
        _ => bail!("unsupported data file extension for {}", path.display()),
    }
}

fn write_report(root: &Path, report: &RunReport) -> Result<PathBuf> {
    let report_dir = root.join("reports");
    fs::create_dir_all(&report_dir)?;
    let report_path = report_dir.join("last-run.json");
    fs::write(&report_path, serde_json::to_string_pretty(report)?)?;
    Ok(report_path)
}

fn build_workflow_runtime(
    project: &LoadedProject,
    state: &WorkflowState,
) -> Result<RuntimeContext> {
    let mut steps_map = serde_json::Map::new();
    for (id, step_state) in &state.steps {
        steps_map.insert(
            id.clone(),
            json!({
                "status": step_state.status.clone(),
                "passed": step_state.passed,
                "error": step_state.error.clone(),
                "exports": step_state.exports.clone(),
            }),
        );
    }

    let mut root = serde_json::Map::new();
    root.insert(
        "env".to_string(),
        environment_context_value(&project.environment_name, &project.environment)?,
    );
    root.insert(
        "project".to_string(),
        json!({ "name": project.project.project.name, "root": project.root }),
    );
    root.insert(
        "data".to_string(),
        load_data_tree(&project.runner_root.join("data"))?,
    );
    root.insert(
        "workflow".to_string(),
        json!({
            "vars": state.vars.clone(),
            "steps": steps_map,
        }),
    );
    RuntimeContext::new(root)
}

fn print_workflow_dry_run(workflow: &LoadedWorkflow, env_name: &str) {
    let step_count = count_run_case_steps(&workflow.definition.steps);
    println!(
        "Workflow `{}` ({}) — {} step(s) in env `{}`:",
        workflow.id, workflow.definition.name, step_count, env_name,
    );
    print_workflow_steps_dry_run(&workflow.definition.steps, 0);
}

fn print_workflow_steps_dry_run(steps: &[WorkflowStep], indent: usize) {
    let pad = "  ".repeat(indent);
    for step in steps {
        match step {
            WorkflowStep::RunCase(run_case) => {
                println!("{pad}  - [{}] case: {}", run_case.id, run_case.case_id);
            }
            WorkflowStep::Conditional(cond) => {
                println!("{pad}  - if: {}", cond.condition);
                if !cond.then_steps.is_empty() {
                    println!("{pad}    then:");
                    print_workflow_steps_dry_run(&cond.then_steps, indent + 2);
                }
                if !cond.else_steps.is_empty() {
                    println!("{pad}    else:");
                    print_workflow_steps_dry_run(&cond.else_steps, indent + 2);
                }
            }
        }
    }
}

fn write_workflow_report(root: &Path, report: &WorkflowRunReport) -> Result<PathBuf> {
    let report_dir = root.join("reports");
    fs::create_dir_all(&report_dir)?;
    let report_path = report_dir.join("last-workflow-run.json");
    fs::write(&report_path, serde_json::to_string_pretty(report)?)?;
    Ok(report_path)
}

fn write_workflow_batch_report(root: &Path, report: &WorkflowBatchRunReport) -> Result<PathBuf> {
    let report_dir = root.join("reports");
    fs::create_dir_all(&report_dir)?;
    let report_path = report_dir.join("last-workflows-run.json");
    fs::write(&report_path, serde_json::to_string_pretty(report)?)?;
    Ok(report_path)
}

fn print_report(report: &RunReport, report_path: &Path, format: ReportFormat) -> Result<()> {
    match format {
        ReportFormat::Summary => {
            let styler = TerminalStyler::detect();
            print_summary_header(&styler);
            println!(
                "  Cases: {} passed, {} failed, {} total",
                styler.success(report.summary.passed.to_string()),
                styler.failure(report.summary.failed.to_string()),
                styler.info(report.summary.total.to_string())
            );
            if report.callback_summary.total > 0 {
                println!(
                    "  Callbacks: {} passed, {} failed, {} total",
                    styler.success(report.callback_summary.passed.to_string()),
                    styler.failure(report.callback_summary.failed.to_string()),
                    styler.info(report.callback_summary.total.to_string())
                );
            }
            if let Some(parallel) = &report.parallel {
                println!(
                    "  Parallel: {} {}(s) across {} slot(s)",
                    styler.info(parallel.jobs.to_string()),
                    parallel.unit,
                    styler.info(parallel.slots.to_string())
                );
            }
            print_environment_summary(&styler, report.environment_artifacts.as_ref());
            print_summary_metadata(&styler, report_path, report.summary.duration_ms);
            print_callback_details(&styler, &report.callbacks);
        }
        ReportFormat::Json => {
            println!("{}", serde_json::to_string_pretty(report)?);
            println!("Report written to {}", report_path.display());
        }
    }
    Ok(())
}

fn print_workflow_report(
    report: &WorkflowRunReport,
    report_path: &Path,
    format: ReportFormat,
) -> Result<()> {
    match format {
        ReportFormat::Summary => {
            let styler = TerminalStyler::detect();
            print_summary_header(&styler);
            println!("  Status: {}", styler.status(&report.status));
            println!(
                "  Steps: {} passed, {} failed, {} total",
                styler.success(report.summary.passed_steps.to_string()),
                styler.failure(report.summary.failed_steps.to_string()),
                styler.info(report.summary.executed_steps.to_string())
            );
            if report.callback_summary.total > 0 {
                println!(
                    "  Callbacks: {} passed, {} failed, {} total",
                    styler.success(report.callback_summary.passed.to_string()),
                    styler.failure(report.callback_summary.failed.to_string()),
                    styler.info(report.callback_summary.total.to_string())
                );
            }
            print_environment_summary(&styler, report.environment_artifacts.as_ref());
            print_summary_metadata(&styler, report_path, report.summary.duration_ms);
            print_callback_details(&styler, &report.callbacks);
        }
        ReportFormat::Json => {
            println!("{}", serde_json::to_string_pretty(report)?);
            println!("Report written to {}", report_path.display());
        }
    }
    Ok(())
}

fn print_workflow_batch_report(
    report: &WorkflowBatchRunReport,
    report_path: &Path,
    format: ReportFormat,
) -> Result<()> {
    match format {
        ReportFormat::Summary => {
            let styler = TerminalStyler::detect();
            print_summary_header(&styler);
            println!(
                "  Workflows: {} passed, {} failed, {} total",
                styler.success(report.summary.passed_workflows.to_string()),
                styler.failure(report.summary.failed_workflows.to_string()),
                styler.info(report.summary.total_workflows.to_string())
            );
            if report.callback_summary.total > 0 {
                println!(
                    "  Callbacks: {} passed, {} failed, {} total",
                    styler.success(report.callback_summary.passed.to_string()),
                    styler.failure(report.callback_summary.failed.to_string()),
                    styler.info(report.callback_summary.total.to_string())
                );
            }
            if let Some(parallel) = &report.parallel {
                println!(
                    "  Parallel: {} {}(s) across {} slot(s)",
                    styler.info(parallel.jobs.to_string()),
                    parallel.unit,
                    styler.info(parallel.slots.to_string())
                );
            }
            print_environment_summary(&styler, report.environment_artifacts.as_ref());
            print_summary_metadata(&styler, report_path, report.summary.duration_ms);
            print_callback_details(&styler, &report.callbacks);
        }
        ReportFormat::Json => {
            println!("{}", serde_json::to_string_pretty(report)?);
            println!("Report written to {}", report_path.display());
        }
    }
    Ok(())
}

fn manages_environment(environment: &EnvironmentConfig) -> bool {
    environment.runtime.is_some()
        || !environment.readiness.is_empty()
        || !environment.logs.is_empty()
}

fn count_run_case_steps(steps: &[WorkflowStep]) -> usize {
    steps
        .iter()
        .map(|step| match step {
            WorkflowStep::RunCase(_) => 1,
            WorkflowStep::Conditional(cond) => {
                count_run_case_steps(&cond.then_steps) + count_run_case_steps(&cond.else_steps)
            }
        })
        .sum()
}

fn print_summary_header(styler: &TerminalStyler) {
    println!();
    println!("{}", styler.section("==> Summary"));
}

fn print_summary_metadata(styler: &TerminalStyler, report_path: &Path, duration_ms: u128) {
    println!("  Duration: {}", styler.muted(format_duration(duration_ms)));
    println!(
        "  Report: {}",
        styler.muted(report_path.display().to_string())
    );
}

fn print_environment_summary(
    styler: &TerminalStyler,
    environment_artifacts: Option<&EnvironmentArtifactsReport>,
) {
    let Some(environment_artifacts) = environment_artifacts else {
        return;
    };

    let readiness_passed = environment_artifacts
        .readiness
        .iter()
        .filter(|report| report.status == "passed")
        .count();
    let readiness_failed = environment_artifacts.readiness.len() - readiness_passed;
    let logs_passed = environment_artifacts
        .logs
        .iter()
        .filter(|report| report.status == "passed")
        .count();
    let logs_failed = environment_artifacts.logs.len() - logs_passed;

    println!("  {}", styler.section("Environment:"));
    if let Some(runtime) = &environment_artifacts.runtime {
        let runtime_label = match runtime.slots {
            Some(slots) => format!("{} x{slots}", runtime.kind),
            None => runtime.kind.clone(),
        };
        println!(
            "    Runtime: {} (startup: {}, shutdown: {})",
            styler.info(runtime_label),
            styler.status(runtime.startup_status.as_deref().unwrap_or("n/a")),
            styler.status(runtime.shutdown_status.as_deref().unwrap_or("n/a"))
        );
    }
    if !environment_artifacts.readiness.is_empty() {
        println!(
            "    Readiness: {} passed, {} failed",
            styler.success(readiness_passed.to_string()),
            styler.failure(readiness_failed.to_string())
        );
    }
    if !environment_artifacts.logs.is_empty() {
        println!(
            "    Logs: {} collected, {} failed",
            styler.success(logs_passed.to_string()),
            styler.failure(logs_failed.to_string())
        );
    }
}

fn print_callback_details(styler: &TerminalStyler, callbacks: &[CallbackReport]) {
    if callbacks.is_empty() {
        return;
    }

    println!();
    println!("{}", styler.section("==> Callbacks"));
    for callback in callbacks {
        println!(
            "  {} #{} {} -> {} ({})",
            styler.status(&callback.status),
            callback.id,
            callback.source,
            callback.url,
            styler.muted(format_duration(callback.duration_ms))
        );
        if let Some(error) = &callback.error {
            println!("    {error}");
        }
    }
}

fn status_label(status: &str) -> &'static str {
    match status {
        "passed" => "PASS",
        "failed" => "FAIL",
        "skipped" => "SKIP",
        "n/a" => "N/A",
        _ => "INFO",
    }
}

fn format_duration(duration_ms: u128) -> String {
    if duration_ms >= 1_000 {
        let seconds = duration_ms as f64 / 1_000.0;
        format!("{seconds:.2}s")
    } else {
        format!("{duration_ms}ms")
    }
}

fn split_sql_statements(script: &str) -> Vec<String> {
    script
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .map(|statement| statement.to_string())
        .collect()
}

fn redis_value_to_json(value: redis::Value) -> Value {
    match value {
        redis::Value::Nil => Value::Null,
        redis::Value::Int(number) => json!(number),
        redis::Value::Data(bytes) => match String::from_utf8(bytes.clone()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
            Err(_) => json!(bytes),
        },
        redis::Value::Bulk(values) => {
            Value::Array(values.into_iter().map(redis_value_to_json).collect())
        }
        redis::Value::Status(text) => Value::String(text),
        redis::Value::Okay => Value::String("OK".to_string()),
    }
}

fn mysql_row_to_json(row: &MySqlRow) -> Result<Value> {
    row_to_json(row)
}

fn postgres_row_to_json(row: &PgRow) -> Result<Value> {
    row_to_json(row)
}

fn row_to_json<R>(row: &R) -> Result<Value>
where
    R: Row,
    usize: ColumnIndex<R>,
    for<'r> Option<bool>: Decode<'r, R::Database> + Type<R::Database>,
    for<'r> Option<i64>: Decode<'r, R::Database> + Type<R::Database>,
    for<'r> Option<f64>: Decode<'r, R::Database> + Type<R::Database>,
    for<'r> Option<String>: Decode<'r, R::Database> + Type<R::Database>,
    for<'r> Option<Vec<u8>>: Decode<'r, R::Database> + Type<R::Database>,
{
    let mut object = serde_json::Map::new();
    for (index, column) in row.columns().iter().enumerate() {
        let value = if let Ok(value) = row.try_get::<Option<bool>, _>(index) {
            value.map(Value::Bool).unwrap_or(Value::Null)
        } else if let Ok(value) = row.try_get::<Option<i64>, _>(index) {
            value.map(|value| json!(value)).unwrap_or(Value::Null)
        } else if let Ok(value) = row.try_get::<Option<f64>, _>(index) {
            value.map(|value| json!(value)).unwrap_or(Value::Null)
        } else if let Ok(value) = row.try_get::<Option<String>, _>(index) {
            match value {
                Some(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
                None => Value::Null,
            }
        } else if let Ok(value) = row.try_get::<Option<Vec<u8>>, _>(index) {
            match value {
                Some(bytes) => match String::from_utf8(bytes.clone()) {
                    Ok(text) => Value::String(text),
                    Err(_) => json!(bytes),
                },
                None => Value::Null,
            }
        } else {
            Value::String(format!("<unsupported:{}>", column.type_info().name()))
        };
        object.insert(column.name().to_string(), value);
    }
    Ok(Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{EnvTemplate, InitArgs};
    use crate::config::{
        ContainerServiceConfig, EnvironmentRuntimeCleanupPolicy, EnvironmentRuntimeConfig,
        MockServerConfig, ProjectConfig, ProjectDefaults, ProjectMetadata,
    };
    use crate::init;
    use tempfile::tempdir;

    #[tokio::test]
    async fn dry_run_selects_cases() {
        let temp = tempdir().expect("tempdir");
        init::run(InitArgs {
            root: temp.path().to_path_buf(),
            force: false,
            env_template: EnvTemplate::Local,
            with_mock: true,
        })
        .await
        .expect("init");

        let project = load_project(temp.path(), None).expect("load project");
        let cases = select_cases(
            &project,
            &TargetSelection::All,
            &CommonTestArgs {
                root: temp.path().to_path_buf(),
                env: None,
                tag: vec![],
                case_pattern: None,
                fail_fast: false,
                parallel: false,
                jobs: None,
                dry_run: true,
                mock: false,
                no_mock: false,
                follow_env_logs: false,
                report_format: ReportFormat::Summary,
            },
        )
        .expect("select cases");

        assert_eq!(cases.len(), 2);
    }

    #[tokio::test]
    async fn dry_run_selects_workflow_steps() {
        let temp = tempdir().expect("tempdir");
        init::run(InitArgs {
            root: temp.path().to_path_buf(),
            force: false,
            env_template: EnvTemplate::Local,
            with_mock: true,
        })
        .await
        .expect("init");

        let wf_dir = temp.path().join(".testrunner/workflows");
        std::fs::create_dir_all(&wf_dir).expect("create workflows dir");
        std::fs::write(
            wf_dir.join("smoke-flow.yaml"),
            r#"name: smoke flow
steps:
  - run_case:
      id: health
      case: user/create-user/happy-path
      cleanup: immediate
"#,
        )
        .expect("write workflow");

        let project = load_project(temp.path(), None).expect("load project");
        assert!(
            project.workflows.contains_key("smoke-flow"),
            "workflow should be loaded"
        );
    }

    #[tokio::test]
    async fn select_workflows_supports_all_flag() {
        let temp = tempdir().expect("tempdir");
        init::run(InitArgs {
            root: temp.path().to_path_buf(),
            force: false,
            env_template: EnvTemplate::Local,
            with_mock: true,
        })
        .await
        .expect("init");

        let wf_dir = temp.path().join(".testrunner/workflows");
        std::fs::create_dir_all(&wf_dir).expect("create workflows dir");
        std::fs::write(
            wf_dir.join("smoke-flow.yaml"),
            r#"name: smoke flow
steps:
  - run_case:
      id: health
      case: user/create-user/happy-path
      cleanup: immediate
"#,
        )
        .expect("write smoke workflow");
        std::fs::write(
            wf_dir.join("login-flow.yaml"),
            r#"name: login flow
steps:
  - run_case:
      id: login
      case: user/login/happy-path
      cleanup: immediate
"#,
        )
        .expect("write login workflow");

        let project = load_project(temp.path(), None).expect("load project");
        let workflows = select_workflows(
            &project,
            &TestWorkflowArgs {
                workflow_id: None,
                all: true,
                common: CommonTestArgs {
                    root: temp.path().to_path_buf(),
                    env: None,
                    tag: vec![],
                    case_pattern: None,
                    fail_fast: false,
                    parallel: false,
                    jobs: None,
                    dry_run: true,
                    mock: false,
                    no_mock: false,
                    follow_env_logs: false,
                    report_format: ReportFormat::Summary,
                },
            },
        )
        .expect("select workflows");

        assert!(workflows.len() >= 2);
        let workflow_ids = workflows
            .iter()
            .map(|workflow| workflow.id.as_str())
            .collect::<Vec<_>>();
        assert!(workflow_ids.contains(&"login-flow"));
        assert!(workflow_ids.contains(&"smoke-flow"));
    }

    #[test]
    fn terminal_styler_keeps_plain_text_when_disabled() {
        let styler = TerminalStyler { enabled: false };
        assert_eq!(styler.success("PASS"), "PASS");
        assert_eq!(styler.section("==> Summary"), "==> Summary");
    }

    #[test]
    fn terminal_styler_wraps_text_when_enabled() {
        let styler = TerminalStyler { enabled: true };
        assert_eq!(styler.failure("FAIL"), "\u{1b}[1;31mFAIL\u{1b}[0m");
        assert_eq!(styler.muted("12ms"), "\u{1b}[2m12ms\u{1b}[0m");
    }

    fn test_project_with_container_service_env(
        service_env: IndexMap<String, String>,
    ) -> LoadedProject {
        LoadedProject {
            root: PathBuf::from("/tmp/project"),
            runner_root: PathBuf::from("/tmp/project/.testrunner"),
            project: ProjectConfig {
                version: 1,
                project: ProjectMetadata {
                    name: "sample".to_string(),
                },
                defaults: ProjectDefaults::default(),
                mock: MockServerConfig {
                    enabled: true,
                    host: "127.0.0.1".to_string(),
                    port: 18081,
                },
            },
            environment_name: "containers".to_string(),
            environment: EnvironmentConfig {
                name: Some("containers".to_string()),
                base_url: "http://127.0.0.1:18080".to_string(),
                headers: Default::default(),
                variables: Default::default(),
                runtime: Some(EnvironmentRuntimeConfig {
                    kind: EnvironmentRuntimeKind::Containers,
                    project_directory: ".".to_string(),
                    files: Vec::new(),
                    project_name: None,
                    up: Vec::new(),
                    down: Vec::new(),
                    cleanup: EnvironmentRuntimeCleanupPolicy::Always,
                    services: vec![ContainerServiceConfig {
                        name: "app".to_string(),
                        image: "sample/app:latest".to_string(),
                        build: None,
                        ports: vec!["18080:3000".to_string()],
                        environment: service_env,
                        command: Vec::new(),
                        volumes: Vec::new(),
                        extra_hosts: Vec::new(),
                        wait_for: None,
                    }],
                    network_name: None,
                    parallel: None,
                }),
                readiness: Vec::new(),
                logs: Vec::new(),
            },
            datasources: Default::default(),
            apis: Default::default(),
            cases: Vec::new(),
            workflows: Default::default(),
            mock_routes: Vec::new(),
        }
    }

    #[test]
    fn rewrite_mock_urls_updates_runtime_service_env_values() {
        let mut project = test_project_with_container_service_env(IndexMap::from([(
            "SMS_PROVIDER_BASE_URL".to_string(),
            "http://host.docker.internal:18081".to_string(),
        )]));

        rewrite_mock_urls(&mut project, 18081, "http://host.docker.internal:29001");

        let runtime = project
            .environment
            .runtime
            .as_ref()
            .expect("runtime should exist");
        assert_eq!(
            runtime.services[0]
                .environment
                .get("SMS_PROVIDER_BASE_URL")
                .expect("service env"),
            "http://host.docker.internal:29001"
        );
    }
}
