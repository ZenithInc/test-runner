use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, State},
    http::{StatusCode, header},
    response::{
        Html, IntoResponse, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use chrono::Utc;
use futures_util::{Stream, stream};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    convert::Infallible,
    env,
    fs,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
    sync::{RwLock, broadcast},
};

use crate::{
    cli::WebArgs,
    config::{LoadedProject, TESTRUNNER_DIR, load_project},
};

const INDEX_HTML: &str = include_str!("web/index.html");
const MAX_STORED_EVENTS: usize = 10_000;

#[derive(Clone)]
struct AppState {
    current_exe: PathBuf,
    working_dir: PathBuf,
    runs: Arc<RwLock<HashMap<String, Arc<RunHandle>>>>,
}

struct RunHandle {
    next_seq: AtomicU64,
    events: Mutex<Vec<RunEvent>>,
    tx: broadcast::Sender<RunEvent>,
}

impl RunHandle {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            next_seq: AtomicU64::new(1),
            events: Mutex::new(Vec::new()),
            tx,
        }
    }

    fn push_event(&self, kind: impl Into<String>, message: impl Into<String>) {
        let event = RunEvent {
            seq: self.next_seq.fetch_add(1, Ordering::Relaxed),
            timestamp: Utc::now().to_rfc3339(),
            kind: kind.into(),
            message: message.into(),
        };

        {
            let mut events = self.events.lock().expect("run event mutex poisoned");
            events.push(event.clone());
            if events.len() > MAX_STORED_EVENTS {
                let overflow = events.len() - MAX_STORED_EVENTS;
                events.drain(0..overflow);
            }
        }

        let _ = self.tx.send(event);
    }

    fn snapshot_and_subscribe(&self) -> (Vec<RunEvent>, broadcast::Receiver<RunEvent>) {
        let receiver = self.tx.subscribe();
        let events = self.events.lock().expect("run event mutex poisoned").clone();
        (events, receiver)
    }
}

#[derive(Debug, Clone, Serialize)]
struct RunEvent {
    seq: u64,
    timestamp: String,
    kind: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct BrowsePathQuery {
    path: Option<String>,
}

#[derive(Debug, Serialize)]
struct BrowsePathResponse {
    current_path: String,
    parent_path: Option<String>,
    directories: Vec<DirectoryEntry>,
}

#[derive(Debug, Serialize)]
struct DirectoryEntry {
    name: String,
    path: String,
}

#[derive(Debug, Deserialize)]
struct ProjectMetadataQuery {
    root: String,
    env: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProjectMetadataResponse {
    root: String,
    project_name: String,
    default_env: String,
    selected_env: String,
    envs: Vec<String>,
    apis: Vec<NamedItem>,
    workflows: Vec<NamedItem>,
    dirs: Vec<String>,
    case_count: usize,
    workflow_count: usize,
    default_mock_enabled: bool,
    parallel_slots: Option<usize>,
}

#[derive(Debug, Serialize)]
struct NamedItem {
    id: String,
    name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WebTarget {
    Api,
    Dir,
    All,
    Workflow,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum MockMode {
    #[default]
    Default,
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Deserialize)]
struct StartRunRequest {
    root: String,
    env: Option<String>,
    target: WebTarget,
    target_value: Option<String>,
    #[serde(default)]
    workflow_all: bool,
    #[serde(default)]
    tags: Vec<String>,
    case_pattern: Option<String>,
    #[serde(default)]
    fail_fast: bool,
    #[serde(default)]
    parallel: bool,
    jobs: Option<usize>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    mock_mode: MockMode,
    #[serde(default)]
    follow_env_logs: bool,
}

#[derive(Debug, Serialize)]
struct StartRunResponse {
    run_id: String,
    command: String,
    stream_path: String,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

#[derive(Debug, Serialize)]
struct ApiErrorBody {
    error: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            serde_json::to_string(&ApiErrorBody {
                error: self.message,
            })
            .unwrap_or_else(|_| "{\"error\":\"internal serialization error\"}".to_string()),
        )
            .into_response()
    }
}

pub async fn run(args: WebArgs) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(format!("{}:{}", args.host, args.port))
        .await
        .with_context(|| {
            format!(
                "failed to bind Web UI server on {}:{}",
                args.host, args.port
            )
        })?;
    let local_addr = listener
        .local_addr()
        .context("failed to resolve bound Web UI address")?;
    let current_exe = env::current_exe().context("failed to resolve current executable path")?;
    let working_dir = env::current_dir().context("failed to resolve current working directory")?;
    let state = AppState {
        current_exe,
        working_dir,
        runs: Arc::new(RwLock::new(HashMap::new())),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/fs/children", get(browse_children))
        .route("/api/project", get(project_metadata))
        .route("/api/runs", post(start_run))
        .route("/api/runs/:run_id/events", get(stream_run_events))
        .with_state(state);

    println!("Web UI listening on http://{local_addr}");
    println!("Press Ctrl+C to stop the server.");

    axum::serve(listener, app)
        .await
        .context("web UI server exited unexpectedly")
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn browse_children(
    State(state): State<AppState>,
    Query(query): Query<BrowsePathQuery>,
) -> Result<Json<BrowsePathResponse>, ApiError> {
    let path = resolve_browse_path(query.path.as_deref(), &state.working_dir)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to access {}", path.display()))
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    if !canonical.is_dir() {
        return Err(ApiError::bad_request(format!(
            "{} is not a directory",
            canonical.display()
        )));
    }

    let directories =
        list_directory_entries(&canonical).map_err(|error| ApiError::bad_request(error.to_string()))?;
    let parent_path = canonical.parent().map(|parent| parent.display().to_string());
    Ok(Json(BrowsePathResponse {
        current_path: canonical.display().to_string(),
        parent_path,
        directories,
    }))
}

async fn project_metadata(
    State(state): State<AppState>,
    Query(query): Query<ProjectMetadataQuery>,
) -> Result<Json<ProjectMetadataResponse>, ApiError> {
    let root = resolve_project_root(&query.root, &state.working_dir)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let requested_env = trimmed_option(query.env.as_deref());
    let project =
        load_project(&root, requested_env).map_err(|error| ApiError::bad_request(error.to_string()))?;
    let envs = list_environment_names(&project.root)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let apis = project
        .apis
        .values()
        .map(|api| NamedItem {
            id: api.id.clone(),
            name: api.definition.name.clone(),
        })
        .collect();
    let workflows = project
        .workflows
        .values()
        .map(|workflow| NamedItem {
            id: workflow.id.clone(),
            name: workflow.definition.name.clone(),
        })
        .collect();
    let parallel_slots = project
        .environment
        .runtime
        .as_ref()
        .and_then(|runtime| runtime.parallel.as_ref().map(|parallel| parallel.slots));

    Ok(Json(ProjectMetadataResponse {
        root: project.root.display().to_string(),
        project_name: project.project.project.name.clone(),
        default_env: project.project.defaults.env.clone(),
        selected_env: project.environment_name.clone(),
        envs,
        apis,
        workflows,
        dirs: collect_directory_options(&project),
        case_count: project.cases.len(),
        workflow_count: project.workflows.len(),
        default_mock_enabled: project.project.mock.enabled,
        parallel_slots,
    }))
}

async fn start_run(
    State(state): State<AppState>,
    Json(request): Json<StartRunRequest>,
) -> Result<Json<StartRunResponse>, ApiError> {
    let args = build_test_command_args(&request, &state.working_dir)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let run_id = next_run_id();
    let run = Arc::new(RunHandle::new());
    let command = format_command(&state.current_exe, &args);
    run.push_event("command", command.clone());
    run.push_event("status", "Starting run");

    state.runs.write().await.insert(run_id.clone(), run.clone());

    let executable = state.current_exe.clone();
    tokio::spawn(async move {
        execute_child_run(executable, args, run).await;
    });

    Ok(Json(StartRunResponse {
        run_id: run_id.clone(),
        command,
        stream_path: format!("/api/runs/{run_id}/events"),
    }))
}

async fn stream_run_events(
    State(state): State<AppState>,
    AxumPath(run_id): AxumPath<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let run = state
        .runs
        .read()
        .await
        .get(&run_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(format!("run `{run_id}` was not found")))?;
    let (backlog, receiver) = run.snapshot_and_subscribe();

    let stream = stream::unfold(
        EventStreamState {
            backlog,
            next_index: 0,
            last_seq: None,
            receiver,
        },
        |mut state| async move {
            if state.next_index < state.backlog.len() {
                let event = state.backlog[state.next_index].clone();
                state.next_index += 1;
                state.last_seq = Some(event.seq);
                return Some((Ok(to_sse_event(event)), state));
            }

            loop {
                match state.receiver.recv().await {
                    Ok(event) => {
                        if state.last_seq.is_some_and(|last_seq| event.seq <= last_seq) {
                            continue;
                        }
                        state.last_seq = Some(event.seq);
                        return Some((Ok(to_sse_event(event)), state));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

struct EventStreamState {
    backlog: Vec<RunEvent>,
    next_index: usize,
    last_seq: Option<u64>,
    receiver: broadcast::Receiver<RunEvent>,
}

fn to_sse_event(event: RunEvent) -> Event {
    let payload = serde_json::to_string(&event).expect("run event should serialize");
    Event::default()
        .event(event.kind.clone())
        .id(event.seq.to_string())
        .data(payload)
}

async fn execute_child_run(executable: PathBuf, args: Vec<String>, run: Arc<RunHandle>) {
    let mut command = Command::new(&executable);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            run.push_event("error", format!("failed to spawn child process: {error}"));
            run.push_event("finished", "Run did not start");
            return;
        }
    };

    if let Some(pid) = child.id() {
        run.push_event("status", format!("Child process started (pid {pid})"));
    }

    let stdout_task = child
        .stdout
        .take()
        .map(|stdout| tokio::spawn(forward_output(stdout, "stdout", run.clone())));
    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| tokio::spawn(forward_output(stderr, "stderr", run.clone())));

    let status = child.wait().await;
    join_output_task(stdout_task, "stdout", &run).await;
    join_output_task(stderr_task, "stderr", &run).await;

    match status {
        Ok(status) if status.success() => {
            run.push_event(
                "finished",
                format!("Run completed successfully ({})", describe_exit_status(&status)),
            );
        }
        Ok(status) => {
            run.push_event(
                "finished",
                format!("Run failed ({})", describe_exit_status(&status)),
            );
        }
        Err(error) => {
            run.push_event("error", format!("failed while waiting for child process: {error}"));
            run.push_event("finished", "Run ended with an internal error");
        }
    }
}

async fn forward_output<R>(reader: R, kind: &'static str, run: Arc<RunHandle>) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        run.push_event(kind, line);
    }
    Ok(())
}

async fn join_output_task(
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    stream_name: &str,
    run: &RunHandle,
) {
    let Some(task) = task else {
        return;
    };

    match task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => run.push_event(
            "error",
            format!("failed while reading {stream_name} output: {error}"),
        ),
        Err(error) => run.push_event(
            "error",
            format!("output task for {stream_name} panicked or was cancelled: {error}"),
        ),
    }
}

fn build_test_command_args(request: &StartRunRequest, working_dir: &Path) -> Result<Vec<String>> {
    let root = resolve_project_root(&request.root, working_dir)?;
    if matches!(request.target, WebTarget::Workflow) {
        if !clean_tags(&request.tags).is_empty() {
            bail!("workflow runs do not support tag filters");
        }
        if trimmed_option(request.case_pattern.as_deref()).is_some() {
            bail!("workflow runs do not support case filters");
        }
    }
    if request.workflow_all && !matches!(request.target, WebTarget::Workflow) {
        bail!("workflow_all is only supported when target is workflow");
    }
    if let Some(jobs) = request.jobs
        && jobs == 0
    {
        bail!("jobs must be greater than 0");
    }

    let mut args = vec!["test".to_string()];
    match request.target {
        WebTarget::Api => {
            args.push("api".to_string());
            args.push(required_value(request.target_value.as_deref(), "api id")?);
        }
        WebTarget::Dir => {
            args.push("dir".to_string());
            args.push(required_value(request.target_value.as_deref(), "directory prefix")?);
        }
        WebTarget::All => args.push("all".to_string()),
        WebTarget::Workflow => {
            args.push("workflow".to_string());
            if request.workflow_all {
                args.push("--all".to_string());
            } else {
                args.push(required_value(
                    request.target_value.as_deref(),
                    "workflow id",
                )?);
            }
        }
    }

    args.push("--root".to_string());
    args.push(root.display().to_string());

    if let Some(env) = trimmed_option(request.env.as_deref()) {
        args.push("--env".to_string());
        args.push(env.to_string());
    }
    for tag in clean_tags(&request.tags) {
        args.push("--tag".to_string());
        args.push(tag);
    }
    if let Some(case_pattern) = trimmed_option(request.case_pattern.as_deref()) {
        args.push("--case".to_string());
        args.push(case_pattern.to_string());
    }
    if request.fail_fast {
        args.push("--fail-fast".to_string());
    }
    if request.parallel {
        args.push("--parallel".to_string());
    }
    if let Some(jobs) = request.jobs {
        args.push("--jobs".to_string());
        args.push(jobs.to_string());
    }
    if request.dry_run {
        args.push("--dry-run".to_string());
    }
    match request.mock_mode {
        MockMode::Default => {}
        MockMode::Enabled => args.push("--mock".to_string()),
        MockMode::Disabled => args.push("--no-mock".to_string()),
    }
    if request.follow_env_logs {
        args.push("--follow-env-logs".to_string());
    }
    args.push("--report-format".to_string());
    args.push("summary".to_string());
    Ok(args)
}

fn resolve_browse_path(raw: Option<&str>, working_dir: &Path) -> Result<PathBuf> {
    let path = resolve_input_path(raw, working_dir);
    if !path.exists() {
        bail!("{} does not exist", path.display());
    }
    Ok(path)
}

fn resolve_project_root(raw: &str, working_dir: &Path) -> Result<PathBuf> {
    let path = resolve_input_path(Some(raw), working_dir);
    if trimmed_option(Some(raw)).is_none() {
        bail!("root path is required");
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to access {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn resolve_input_path(raw: Option<&str>, working_dir: &Path) -> PathBuf {
    let raw = trimmed_option(raw);
    let Some(raw) = raw else {
        return working_dir.to_path_buf();
    };

    let expanded = expand_home(raw);
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        path
    } else {
        working_dir.join(path)
    }
}

fn expand_home(path: &str) -> String {
    if path == "~" {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).display().to_string();
        }
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest).display().to_string();
    }
    path.to_string()
}

fn list_directory_entries(path: &Path) -> Result<Vec<DirectoryEntry>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let child_path = entry.path().canonicalize().unwrap_or_else(|_| entry.path());
        entries.push(DirectoryEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            path: child_path.display().to_string(),
        });
    }
    entries.sort_by_key(|entry| entry.name.to_ascii_lowercase());
    Ok(entries)
}

fn list_environment_names(root: &Path) -> Result<Vec<String>> {
    let env_dir = root.join(TESTRUNNER_DIR).join("env");
    let mut envs = BTreeSet::new();
    for entry in
        fs::read_dir(&env_dir).with_context(|| format!("failed to read {}", env_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("yaml") | Some("yml") => {}
            _ => continue,
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            envs.insert(stem.to_string());
        }
    }
    Ok(envs.into_iter().collect())
}

fn collect_directory_options(project: &LoadedProject) -> Vec<String> {
    let mut dirs = BTreeSet::new();
    for case in &project.cases {
        if let Some(parent) = case.relative_path.parent() {
            insert_path_prefixes(parent, &mut dirs);
        }
        insert_slash_prefixes(&case.definition.api, &mut dirs);
    }
    dirs.into_iter().collect()
}

fn insert_path_prefixes(path: &Path, dirs: &mut BTreeSet<String>) {
    let mut current = String::new();
    for component in path.components() {
        let Component::Normal(segment) = component else {
            continue;
        };
        let segment = segment.to_string_lossy();
        if segment.is_empty() {
            continue;
        }
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(&segment);
        dirs.insert(current.clone());
    }
}

fn insert_slash_prefixes(value: &str, dirs: &mut BTreeSet<String>) {
    let mut current = String::new();
    for segment in value.split('/').filter(|segment| !segment.trim().is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        dirs.insert(current.clone());
    }
}

fn clean_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .filter_map(|tag| trimmed_option(Some(tag)).map(ToOwned::to_owned))
        .collect()
}

fn required_value(value: Option<&str>, label: &str) -> Result<String> {
    trimmed_option(value)
        .map(ToOwned::to_owned)
        .with_context(|| format!("{label} is required"))
}

fn trimmed_option(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn format_command(executable: &Path, args: &[String]) -> String {
    let mut parts = vec![shell_quote(&executable.display().to_string())];
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r#"'\''"#))
    }
}

fn describe_exit_status(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "terminated by signal".to_string(),
    }
}

fn next_run_id() -> String {
    static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1);
    format!("run-{}", NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn build_test_command_args_for_api_target() {
        let cwd = Path::new("/workspace");
        let root = tempdir().expect("temp dir");
        let root_display = root
            .path()
            .canonicalize()
            .expect("canonical root")
            .display()
            .to_string();
        let request = StartRunRequest {
            root: root_display.clone(),
            env: Some("docker".to_string()),
            target: WebTarget::Api,
            target_value: Some("user/get-user".to_string()),
            workflow_all: false,
            tags: vec!["smoke".to_string(), " happy ".to_string()],
            case_pattern: Some("user".to_string()),
            fail_fast: true,
            parallel: true,
            jobs: Some(2),
            dry_run: false,
            mock_mode: MockMode::Enabled,
            follow_env_logs: true,
        };

        let args = build_test_command_args(&request, cwd).expect("args should build");
        assert_eq!(
            args,
            vec![
                "test".to_string(),
                "api".to_string(),
                "user/get-user".to_string(),
                "--root".to_string(),
                root_display,
                "--env".to_string(),
                "docker".to_string(),
                "--tag".to_string(),
                "smoke".to_string(),
                "--tag".to_string(),
                "happy".to_string(),
                "--case".to_string(),
                "user".to_string(),
                "--fail-fast".to_string(),
                "--parallel".to_string(),
                "--jobs".to_string(),
                "2".to_string(),
                "--mock".to_string(),
                "--follow-env-logs".to_string(),
                "--report-format".to_string(),
                "summary".to_string(),
            ]
        );
    }

    #[test]
    fn build_test_command_args_for_workflow_all() {
        let cwd = Path::new("/workspace");
        let root = tempdir().expect("temp dir");
        let root_display = root
            .path()
            .canonicalize()
            .expect("canonical root")
            .display()
            .to_string();
        let request = StartRunRequest {
            root: root_display.clone(),
            env: None,
            target: WebTarget::Workflow,
            target_value: None,
            workflow_all: true,
            tags: Vec::new(),
            case_pattern: None,
            fail_fast: false,
            parallel: false,
            jobs: None,
            dry_run: true,
            mock_mode: MockMode::Disabled,
            follow_env_logs: false,
        };

        let args = build_test_command_args(&request, cwd).expect("args should build");
        assert_eq!(
            args,
            vec![
                "test".to_string(),
                "workflow".to_string(),
                "--all".to_string(),
                "--root".to_string(),
                root_display,
                "--dry-run".to_string(),
                "--no-mock".to_string(),
                "--report-format".to_string(),
                "summary".to_string(),
            ]
        );
    }

    #[test]
    fn collect_directory_options_includes_nested_prefixes() {
        let sample_projects_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("sample-projects");
        let project =
            load_project(&sample_projects_root, None).expect("sample project should load");
        let dirs = collect_directory_options(&project);
        assert!(dirs.contains(&"system".to_string()));
        assert!(dirs.contains(&"user".to_string()));
    }

    #[test]
    fn list_directory_entries_returns_only_directories() {
        let root = tempdir().expect("temp dir");
        fs::create_dir(root.path().join("alpha")).expect("create alpha");
        fs::create_dir(root.path().join("beta")).expect("create beta");
        fs::write(root.path().join("README.txt"), "hello").expect("create file");

        let entries = list_directory_entries(root.path()).expect("entries should load");
        let names = entries.into_iter().map(|entry| entry.name).collect::<Vec<_>>();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }
}
