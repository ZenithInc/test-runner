use anyhow::{Context, Result, bail};
use chrono::Utc;
use indexmap::IndexMap;
use reqwest::{Method, header::HeaderMap};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::mysql::{MySqlPool, MySqlRow};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Column, ColumnIndex, Decode, Row, Type, TypeInfo};
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Instant;

use crate::cli::{CommonTestArgs, ReportFormat, TestCommand, TestWorkflowArgs};
use crate::config::{
    DatasourceDefinition, LoadedApi, LoadedCase, LoadedProject, LoadedWorkflow, TESTRUNNER_DIR,
    load_data_tree, load_project,
};
use crate::dsl::{
    ConditionalStep, ForeachStep, QueryDbStep, QueryRedisStep, RedisCommandStep, RequestSpec,
    RequestStep, SqlExecStep, Step,
};
use crate::mock;
use crate::runtime::{RuntimeContext, apply_assertions, value_to_string};
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

    let mut project = load_project(&options.root, options.env.as_deref())?;
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

    if options.report_format == ReportFormat::Junit {
        bail!("junit output is planned but not implemented yet");
    }

    let should_start_mock = options
        .mock_override()
        .unwrap_or(project.project.mock.enabled)
        && !project.mock_routes.is_empty();

    let mock_server = if should_start_mock {
        let handle = mock::start(&project).await?;
        project.environment.variables.insert(
            "mock_base_url".to_string(),
            Value::String(handle.base_url.clone()),
        );
        Some(handle)
    } else {
        None
    };

    let mut runner = Runner::new(project);
    let report = runner.execute(&selected_cases, &options, &target).await;

    if let Some(server) = mock_server {
        server.shutdown().await;
    }

    let report = report?;
    let report_path = write_report(&runner.project.runner_root, &report)?;
    print_report(&report, &report_path, options.report_format)?;

    if report.summary.failed > 0 {
        bail!(
            "{} of {} case(s) failed",
            report.summary.failed,
            report.summary.total
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
    if options.report_format == ReportFormat::Junit {
        bail!("junit output is planned but not implemented yet");
    }

    let mut project = load_project(&options.root, options.env.as_deref())?;
    let workflow = project
        .workflows
        .get(&args.workflow_id)
        .with_context(|| {
            format!(
                "workflow `{}` not found in {TESTRUNNER_DIR}",
                args.workflow_id
            )
        })?
        .clone();

    if options.dry_run {
        print_workflow_dry_run(&workflow, &project.environment_name);
        return Ok(());
    }

    let should_start_mock = options
        .mock_override()
        .unwrap_or(project.project.mock.enabled)
        && !project.mock_routes.is_empty();

    let mock_server = if should_start_mock {
        let handle = mock::start(&project).await?;
        project.environment.variables.insert(
            "mock_base_url".to_string(),
            Value::String(handle.base_url.clone()),
        );
        Some(handle)
    } else {
        None
    };

    let mut runner = Runner::new(project);
    let report = runner.execute_workflow(&workflow, &args, options).await;

    if let Some(server) = mock_server {
        server.shutdown().await;
    }

    let report = report?;
    let report_path = write_workflow_report(&runner.project.runner_root, &report)?;
    print_workflow_report(&report, &report_path, options.report_format)?;

    if report.status == "failed" {
        bail!("workflow `{}` failed", args.workflow_id);
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
    cases: Vec<CaseReport>,
}

#[derive(Debug, Serialize)]
struct SummaryReport {
    total: usize,
    passed: usize,
    failed: usize,
    duration_ms: u128,
}

#[derive(Debug, Serialize)]
struct CaseReport {
    id: String,
    name: String,
    api: String,
    status: String,
    duration_ms: u128,
    error: Option<String>,
    steps: Vec<StepReport>,
}

#[derive(Debug, Serialize)]
struct StepReport {
    kind: String,
    status: String,
    duration_ms: u128,
    details: Value,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct WorkflowRunReport {
    project: String,
    environment: String,
    workflow_id: String,
    workflow_name: String,
    started_at: String,
    finished_at: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    summary: WorkflowSummaryReport,
    steps: Vec<WorkflowStepReport>,
}

#[derive(Debug, Serialize)]
struct WorkflowSummaryReport {
    executed_steps: usize,
    passed_steps: usize,
    failed_steps: usize,
    duration_ms: u128,
}

#[derive(Debug, Serialize)]
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

struct Runner {
    project: LoadedProject,
    http_client: reqwest::Client,
    db_pools: HashMap<String, DatabasePool>,
    redis_clients: HashMap<String, redis::Client>,
}

impl Runner {
    fn new(project: LoadedProject) -> Self {
        let timeout = std::time::Duration::from_millis(project.project.defaults.timeout_ms);
        let http_client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("failed to build reqwest client");
        Self {
            project,
            http_client,
            db_pools: HashMap::new(),
            redis_clients: HashMap::new(),
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

        for case in cases {
            let report = self.execute_case(case).await;
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
            cases: reports,
        })
    }

    async fn execute_workflow(
        &mut self,
        workflow: &LoadedWorkflow,
        args: &TestWorkflowArgs,
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
            let resolved = workflow_runtime.resolve_value(value)?;
            state.vars.insert(key.clone(), resolved);
        }

        let mut step_reports = Vec::new();
        let mut deferred_teardowns = Vec::new();
        let mut any_case_failed = false;
        let mut stop_execution = false;

        self.execute_workflow_steps(
            &workflow.definition.steps,
            &mut state,
            &mut step_reports,
            &mut deferred_teardowns,
            &mut any_case_failed,
            options.fail_fast,
            &mut stop_execution,
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

        let status = if any_case_failed { "failed" } else { "passed" };
        let passed_steps = step_reports.iter().filter(|report| report.passed).count();
        let failed_steps = step_reports.iter().filter(|report| !report.passed).count();

        Ok(WorkflowRunReport {
            project: self.project.project.project.name.clone(),
            environment: self.project.environment_name.clone(),
            workflow_id: args.workflow_id.clone(),
            workflow_name: workflow.definition.name.clone(),
            started_at,
            finished_at: Utc::now().to_rfc3339(),
            status: status.to_string(),
            error: if any_case_failed {
                Some(format!(
                    "{failed_steps} of {} step(s) failed",
                    step_reports.len()
                ))
            } else {
                None
            },
            summary: WorkflowSummaryReport {
                executed_steps: step_reports.len(),
                passed_steps,
                failed_steps,
                duration_ms: started.elapsed().as_millis(),
            },
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
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
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
                            .map(|(key, value)| Ok((key.clone(), wf_runtime.resolve_value(value)?)))
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

                        step_reports.push(outcome.step_report);
                    }
                    WorkflowStep::Conditional(cond) => {
                        let wf_runtime = build_workflow_runtime(&self.project, state)?;
                        let condition = wf_runtime.evaluate_condition(&cond.condition)?;
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
                .map(|(name, path)| Ok((name.clone(), context.evaluate_expr_value(path)?)))
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
                status: "passed".to_string(),
                duration_ms: started.elapsed().as_millis(),
                error: None,
                steps,
            },
            Err((steps, error)) => CaseReport {
                id: case.id.clone(),
                name: case.definition.name.clone(),
                api: case.definition.api.clone(),
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
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
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
        context.apply_extracts(&step.extract)?;
        apply_assertions(&step.assertions, context)?;
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
        context.apply_extracts(&step.extract)?;
        apply_assertions(&step.assertions, context)?;
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
        context.apply_extracts(&step.extract)?;
        apply_assertions(&step.assertions, context)?;
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
        let condition = context.evaluate_condition(&step.condition)?;
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
        let values = context.resolve_value(&Value::String(step.expression.clone()))?;
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
        let api_id = step
            .api
            .clone()
            .unwrap_or_else(|| context.case.definition.api.clone());
        let api = self
            .project
            .apis
            .get(&api_id)
            .with_context(|| format!("unknown API `{api_id}`"))?;
        let method = Method::from_bytes(api.definition.method.as_bytes())?;
        let base_url = match &step.base_url {
            Some(base_url) => {
                value_to_string(context.resolve_value(&Value::String(base_url.clone()))?)
            }
            None => api
                .definition
                .base_url
                .clone()
                .unwrap_or_else(|| self.project.environment.base_url.clone()),
        };
        let path = render_api_path(&api.definition.path, &step.path_params, context)?;
        let url = format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        );

        let mut request_builder = self.http_client.request(method, &url);

        let mut headers = self.project.environment.headers.clone();
        headers.extend(api.definition.headers.clone());
        for (key, value) in &step.headers {
            headers.insert(key.clone(), value_to_string(context.resolve_value(value)?));
        }
        for (key, value) in headers {
            request_builder = request_builder.header(&key, value);
        }

        let mut query = api.definition.query.clone();
        query.extend(step.query.clone());
        if !query.is_empty() {
            let query_pairs = query
                .into_iter()
                .map(|(key, value)| Ok((key, value_to_string(context.resolve_value(&value)?))))
                .collect::<Result<Vec<_>>>()?;
            request_builder = request_builder.query(&query_pairs);
        }

        let body = step
            .body
            .clone()
            .or_else(|| api.definition.body.clone())
            .map(|value| context.resolve_value(&value))
            .transpose()?;
        if let Some(body) = body {
            if body.is_string() {
                request_builder =
                    request_builder.body(body.as_str().unwrap_or_default().to_string());
            } else {
                request_builder = request_builder.json(&body);
            }
        }

        let response = request_builder.send().await?;
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
            json!({
                "name": project.environment_name,
                "base_url": project.environment.base_url,
                "headers": project.environment.headers,
                "variables": project.environment.variables,
            }),
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

fn render_api_path(
    path: &str,
    path_params: &IndexMap<String, Value>,
    context: &ExecutionContext<'_>,
) -> Result<String> {
    let mut rendered = path.to_string();
    for (key, value) in path_params {
        let replacement = value_to_string(context.resolve_value(value)?);
        rendered = rendered.replace(&format!("{{{key}}}"), &replacement);
    }
    context.render_string(&rendered)
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
        json!({
            "name": project.environment_name,
            "base_url": project.environment.base_url,
            "headers": project.environment.headers,
            "variables": project.environment.variables,
        }),
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

fn print_report(report: &RunReport, report_path: &Path, format: ReportFormat) -> Result<()> {
    match format {
        ReportFormat::Summary => {
            println!(
                "Run finished: {} passed, {} failed, {} total (report: {})",
                report.summary.passed,
                report.summary.failed,
                report.summary.total,
                report_path.display()
            );
            for case in &report.cases {
                println!(
                    "  [{}] {} ({})",
                    case.status.to_uppercase(),
                    case.id,
                    case.duration_ms
                );
                if let Some(error) = &case.error {
                    println!("    -> {error}");
                }
            }
        }
        ReportFormat::Json => {
            println!("{}", serde_json::to_string_pretty(report)?);
            println!("Report written to {}", report_path.display());
        }
        ReportFormat::Junit => {}
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
            println!(
                "Workflow `{}` finished: {} passed, {} failed, {} total (report: {})",
                report.workflow_id,
                report.summary.passed_steps,
                report.summary.failed_steps,
                report.summary.executed_steps,
                report_path.display(),
            );
            for step in &report.steps {
                println!(
                    "  [{}] {} → {} ({}ms)",
                    step.status.to_uppercase(),
                    step.id,
                    step.case_id,
                    step.duration_ms,
                );
                if let Some(error) = &step.error {
                    println!("    -> {error}");
                }
            }
        }
        ReportFormat::Json => {
            println!("{}", serde_json::to_string_pretty(report)?);
            println!("Report written to {}", report_path.display());
        }
        ReportFormat::Junit => {}
    }
    Ok(())
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
                dry_run: true,
                mock: false,
                no_mock: false,
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
}
