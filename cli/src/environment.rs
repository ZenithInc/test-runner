use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};
use tokio::process::Command;
use tokio::time::{Duration, Instant, sleep};

use crate::config::{
    ComposeLogStream, EnvironmentLogSource, EnvironmentReadinessCheck, EnvironmentRuntimeCleanupPolicy,
    LoadedProject, environment_context_value,
};
use crate::runtime::{RuntimeContext, value_to_string};

#[derive(Debug, Clone, Serialize, Default)]
pub struct EnvironmentArtifactsReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<EnvironmentRuntimeReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readiness: Vec<EnvironmentReadinessReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<EnvironmentLogArtifactReport>,
}

impl EnvironmentArtifactsReport {
    pub fn is_empty(&self) -> bool {
        self.runtime.is_none() && self.readiness.is_empty() && self.logs.is_empty()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentRuntimeReport {
    pub kind: String,
    pub project_name: String,
    pub project_directory: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_duration_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutdown_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutdown_duration_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutdown_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutdown_command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutdown_stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutdown_stderr: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentReadinessReport {
    pub kind: String,
    pub target: String,
    pub status: String,
    pub attempts: u32,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentLogArtifactReport {
    pub kind: String,
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    pub output: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct EnvironmentSession {
    runner_root: PathBuf,
    runtime: Option<ResolvedDockerComposeRuntime>,
    readiness: Vec<ResolvedReadinessCheck>,
    logs: Vec<ResolvedLogSpec>,
    runtime_invoked: bool,
    report: EnvironmentArtifactsReport,
}

#[derive(Debug, Clone)]
struct ResolvedDockerComposeRuntime {
    project_directory: PathBuf,
    files: Vec<PathBuf>,
    file_displays: Vec<String>,
    project_name: String,
    up_args: Vec<String>,
    down_args: Vec<String>,
    cleanup: EnvironmentRuntimeCleanupPolicy,
}

#[derive(Debug, Clone)]
enum ResolvedReadinessCheck {
    Http {
        url: String,
        expect_status: u16,
        timeout_ms: u64,
        interval_ms: u64,
    },
    Tcp {
        host: String,
        port: u16,
        timeout_ms: u64,
        interval_ms: u64,
    },
}

#[derive(Debug, Clone)]
enum ResolvedLogSpec {
    ComposeService {
        service: String,
        stream: ComposeLogStream,
        output: String,
    },
    ContainerFile {
        service: String,
        path: String,
        output: String,
    },
}

#[derive(Debug, Clone)]
struct CommandCapture {
    command: Vec<String>,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
}

impl CommandCapture {
    fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

impl EnvironmentSession {
    pub fn new(project: &LoadedProject) -> Result<Self> {
        let run_id = build_run_id(&project.project.project.name);
        let render_context = build_render_context(project, &run_id)?;
        let runtime = project
            .environment
            .runtime
            .as_ref()
            .map(|runtime| resolve_runtime(project, runtime, &render_context, &run_id))
            .transpose()?;
        let readiness = project
            .environment
            .readiness
            .iter()
            .map(|check| resolve_readiness(check, &render_context))
            .collect::<Result<Vec<_>>>()?;
        let logs = project
            .environment
            .logs
            .iter()
            .map(|log| resolve_log_spec(log, &render_context))
            .collect::<Result<Vec<_>>>()?;
        let report = runtime.as_ref().map(|runtime| EnvironmentRuntimeReport {
            kind: "docker_compose".to_string(),
            project_name: runtime.project_name.clone(),
            project_directory: runtime.project_directory.display().to_string(),
            files: runtime
                .file_displays
                .iter()
                .map(|path| display_relative_to_root(&project.root, path))
                .collect(),
            startup_status: None,
            startup_duration_ms: None,
            startup_error: None,
            startup_command: None,
            startup_stdout: None,
            startup_stderr: None,
            shutdown_status: None,
            shutdown_duration_ms: None,
            shutdown_error: None,
            shutdown_command: None,
            shutdown_stdout: None,
            shutdown_stderr: None,
        });

        Ok(Self {
            runner_root: project.runner_root.clone(),
            runtime,
            readiness,
            logs,
            runtime_invoked: false,
            report: EnvironmentArtifactsReport {
                runtime: report,
                readiness: Vec::new(),
                logs: Vec::new(),
            },
        })
    }

    pub async fn prepare(&mut self) -> Result<()> {
        if let Some(runtime) = self.runtime.clone() {
            ensure_docker_compose_available().await?;
            let started = Instant::now();
            let output = run_compose_command(&runtime, "up", &runtime.up_args).await?;
            self.runtime_invoked = true;
            if let Some(report) = self.report.runtime.as_mut() {
                report.startup_duration_ms = Some(started.elapsed().as_millis());
                report.startup_command = Some(output.command.clone());
            }
            if output.success() {
                if let Some(report) = self.report.runtime.as_mut() {
                    report.startup_status = Some("passed".to_string());
                    report.startup_stdout = None;
                    report.startup_stderr = None;
                }
            } else {
                if let Some(report) = self.report.runtime.as_mut() {
                    report.startup_status = Some("failed".to_string());
                    report.startup_stdout = non_empty(output.stdout.clone());
                    report.startup_stderr = non_empty(output.stderr.clone());
                    report.startup_error =
                        Some(format_command_failure("docker compose up", &output));
                }
                bail!(format_command_failure("docker compose up", &output));
            }
        }

        for check in self.readiness.clone() {
            let report = run_readiness_check(&check).await;
            let failed = report.status == "failed";
            let error = report.error.clone();
            self.report.readiness.push(report);
            if failed {
                bail!(error.unwrap_or_else(|| "environment readiness check failed".to_string()));
            }
        }

        Ok(())
    }

    pub async fn finish(mut self, success: bool) -> EnvironmentArtifactsReport {
        if self.runtime_invoked {
            self.collect_logs().await;
        }
        self.shutdown(success).await;
        self.report
    }

    async fn collect_logs(&mut self) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        for spec in self.logs.clone() {
            let artifact = match spec {
                ResolvedLogSpec::ComposeService {
                    service,
                    stream,
                    output,
                } => {
                    collect_compose_service_logs(&self.runner_root, &runtime, &service, stream, &output)
                        .await
                }
                ResolvedLogSpec::ContainerFile {
                    service,
                    path,
                    output,
                } => {
                    collect_container_file(&self.runner_root, &runtime, &service, &path, &output).await
                }
            };
            self.report.logs.push(artifact);
        }
    }

    async fn shutdown(&mut self, success: bool) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        let Some(report) = self.report.runtime.as_mut() else {
            return;
        };

        let should_cleanup = match runtime.cleanup {
            EnvironmentRuntimeCleanupPolicy::Always => true,
            EnvironmentRuntimeCleanupPolicy::OnSuccess => success,
            EnvironmentRuntimeCleanupPolicy::Never => false,
        };

        if !should_cleanup {
            report.shutdown_status = Some("skipped".to_string());
            return;
        }
        if !self.runtime_invoked {
            return;
        }

        let started = Instant::now();
        match run_compose_command(&runtime, "down", &runtime.down_args).await {
            Ok(output) => {
                report.shutdown_duration_ms = Some(started.elapsed().as_millis());
                report.shutdown_command = Some(output.command.clone());
                if output.success() {
                    report.shutdown_status = Some("passed".to_string());
                    report.shutdown_stdout = None;
                    report.shutdown_stderr = None;
                } else {
                    report.shutdown_status = Some("failed".to_string());
                    report.shutdown_stdout = non_empty(output.stdout.clone());
                    report.shutdown_stderr = non_empty(output.stderr.clone());
                    report.shutdown_error =
                        Some(format_command_failure("docker compose down", &output));
                }
            }
            Err(error) => {
                report.shutdown_duration_ms = Some(started.elapsed().as_millis());
                report.shutdown_status = Some("failed".to_string());
                report.shutdown_error = Some(error.to_string());
            }
        }
    }
}

fn build_render_context(project: &LoadedProject, run_id: &str) -> Result<RuntimeContext> {
    let mut root = Map::new();
    root.insert(
        "env".to_string(),
        environment_context_value(&project.environment_name, &project.environment)?,
    );
    root.insert(
        "project".to_string(),
        json!({
            "name": project.project.project.name,
            "root": project.root,
        }),
    );
    root.insert(
        "run".to_string(),
        json!({
            "id": run_id,
            "started_at": Utc::now().to_rfc3339(),
        }),
    );
    RuntimeContext::new(root)
}

fn resolve_runtime(
    project: &LoadedProject,
    runtime: &crate::config::EnvironmentRuntimeConfig,
    context: &RuntimeContext,
    run_id: &str,
) -> Result<ResolvedDockerComposeRuntime> {
    let project_directory_value = resolve_string(context, &runtime.project_directory)?;
    let project_directory = resolve_project_path(&project.root, &project_directory_value)
        .with_context(|| {
            format!(
                "failed to resolve environment.runtime.project_directory `{project_directory_value}`"
            )
        })?;
    let project_directory = project_directory
        .canonicalize()
        .unwrap_or(project_directory);
    if !project_directory.exists() {
        bail!(
            "environment runtime project directory {} does not exist",
            project_directory.display()
        );
    }

    let mut files = Vec::new();
    let mut file_displays = Vec::new();
    for file in &runtime.files {
        let resolved = resolve_string(context, file)?;
        let path = resolve_project_path(&project_directory, &resolved)
            .with_context(|| format!("failed to resolve compose file `{resolved}`"))?;
        if !path.exists() {
            bail!("compose file {} does not exist", path.display());
        }
        file_displays.push(path.display().to_string());
        files.push(path);
    }

    let project_name = runtime
        .project_name
        .as_ref()
        .map(|value| resolve_string(context, value))
        .transpose()?
        .map(|value| sanitize_project_name(&value))
        .unwrap_or_else(|| {
            sanitize_project_name(&format!(
                "test-runner-{}-{}",
                project.project.project.name,
                run_id
            ))
        });
    let project_name = if project_name.is_empty() {
        sanitize_project_name(&format!(
            "test-runner-{}-{}",
            project.project.project.name,
            build_run_id(&project.project.project.name)
        ))
    } else {
        project_name
    };

    Ok(ResolvedDockerComposeRuntime {
        project_directory,
        files,
        file_displays,
        project_name,
        up_args: runtime.up.clone(),
        down_args: runtime.down.clone(),
        cleanup: runtime.cleanup,
    })
}

fn resolve_readiness(
    readiness: &EnvironmentReadinessCheck,
    context: &RuntimeContext,
) -> Result<ResolvedReadinessCheck> {
    Ok(match readiness {
        EnvironmentReadinessCheck::Http {
            url,
            expect_status,
            timeout_ms,
            interval_ms,
        } => ResolvedReadinessCheck::Http {
            url: resolve_string(context, url)?,
            expect_status: *expect_status,
            timeout_ms: *timeout_ms,
            interval_ms: *interval_ms,
        },
        EnvironmentReadinessCheck::Tcp {
            host,
            port,
            timeout_ms,
            interval_ms,
        } => ResolvedReadinessCheck::Tcp {
            host: resolve_string(context, host)?,
            port: *port,
            timeout_ms: *timeout_ms,
            interval_ms: *interval_ms,
        },
    })
}

fn resolve_log_spec(log: &EnvironmentLogSource, context: &RuntimeContext) -> Result<ResolvedLogSpec> {
    Ok(match log {
        EnvironmentLogSource::ComposeService {
            service,
            stream,
            output,
        } => ResolvedLogSpec::ComposeService {
            service: resolve_string(context, service)?,
            stream: *stream,
            output: resolve_string(context, output)?,
        },
        EnvironmentLogSource::ContainerFile {
            service,
            path,
            output,
        } => ResolvedLogSpec::ContainerFile {
            service: resolve_string(context, service)?,
            path: resolve_string(context, path)?,
            output: resolve_string(context, output)?,
        },
    })
}

async fn ensure_docker_compose_available() -> Result<()> {
    let output = run_command(
        "docker",
        &["compose".to_string(), "version".to_string()],
        None,
    )
    .await
    .context("failed to execute `docker compose version`")?;
    if output.success() {
        return Ok(());
    }
    bail!(format_command_failure("docker compose version", &output));
}

async fn run_readiness_check(check: &ResolvedReadinessCheck) -> EnvironmentReadinessReport {
    match check {
        ResolvedReadinessCheck::Http {
            url,
            expect_status,
            timeout_ms,
            interval_ms,
        } => run_http_readiness_check(url, *expect_status, *timeout_ms, *interval_ms).await,
        ResolvedReadinessCheck::Tcp {
            host,
            port,
            timeout_ms,
            interval_ms,
        } => run_tcp_readiness_check(host, *port, *timeout_ms, *interval_ms).await,
    }
}

async fn run_http_readiness_check(
    url: &str,
    expect_status: u16,
    timeout_ms: u64,
    interval_ms: u64,
) -> EnvironmentReadinessReport {
    let client = reqwest::Client::new();
    let started = Instant::now();
    let mut attempts = 0u32;
    let mut last_error = None;

    while started.elapsed().as_millis() < u128::from(timeout_ms) {
        attempts += 1;
        match client.get(url).send().await {
            Ok(response) if response.status().as_u16() == expect_status => {
                return EnvironmentReadinessReport {
                    kind: "http".to_string(),
                    target: url.to_string(),
                    status: "passed".to_string(),
                    attempts,
                    duration_ms: started.elapsed().as_millis(),
                    error: None,
                };
            }
            Ok(response) => {
                last_error = Some(format!(
                    "expected HTTP {expect_status}, got HTTP {}",
                    response.status().as_u16()
                ));
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
        sleep(Duration::from_millis(interval_ms)).await;
    }

    EnvironmentReadinessReport {
        kind: "http".to_string(),
        target: url.to_string(),
        status: "failed".to_string(),
        attempts,
        duration_ms: started.elapsed().as_millis(),
        error: Some(last_error.unwrap_or_else(|| "HTTP readiness check timed out".to_string())),
    }
}

async fn run_tcp_readiness_check(
    host: &str,
    port: u16,
    timeout_ms: u64,
    interval_ms: u64,
) -> EnvironmentReadinessReport {
    let started = Instant::now();
    let mut attempts = 0u32;
    let mut last_error = None;
    let target = format!("{host}:{port}");

    while started.elapsed().as_millis() < u128::from(timeout_ms) {
        attempts += 1;
        match tokio::net::TcpStream::connect(&target).await {
            Ok(_) => {
                return EnvironmentReadinessReport {
                    kind: "tcp".to_string(),
                    target,
                    status: "passed".to_string(),
                    attempts,
                    duration_ms: started.elapsed().as_millis(),
                    error: None,
                };
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
        sleep(Duration::from_millis(interval_ms)).await;
    }

    EnvironmentReadinessReport {
        kind: "tcp".to_string(),
        target,
        status: "failed".to_string(),
        attempts,
        duration_ms: started.elapsed().as_millis(),
        error: Some(last_error.unwrap_or_else(|| "TCP readiness check timed out".to_string())),
    }
}

async fn collect_compose_service_logs(
    runner_root: &Path,
    runtime: &ResolvedDockerComposeRuntime,
    service: &str,
    stream: ComposeLogStream,
    output: &str,
) -> EnvironmentLogArtifactReport {
    let args = vec![
        "--no-color".to_string(),
        "--timestamps".to_string(),
        service.to_string(),
    ];
    let output_path = match resolve_report_output_path(runner_root, output) {
        Ok(path) => path,
        Err(error) => {
            return EnvironmentLogArtifactReport {
                kind: "compose_service".to_string(),
                service: service.to_string(),
                stream: Some(stream_label(stream).to_string()),
                source_path: None,
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(error.to_string()),
            };
        }
    };

    match run_compose_command(runtime, "logs", &args).await {
        Ok(command_output) => {
            let write_result = write_artifact_file(&output_path, &command_output.stdout);
            match (command_output.success(), write_result) {
                (true, Ok(size_bytes)) => EnvironmentLogArtifactReport {
                    kind: "compose_service".to_string(),
                    service: service.to_string(),
                    stream: Some(stream_label(stream).to_string()),
                    source_path: None,
                    output: output.to_string(),
                    status: "passed".to_string(),
                    size_bytes: Some(size_bytes),
                    error: None,
                },
                (true, Err(error)) => EnvironmentLogArtifactReport {
                    kind: "compose_service".to_string(),
                    service: service.to_string(),
                    stream: Some(stream_label(stream).to_string()),
                    source_path: None,
                    output: output.to_string(),
                    status: "failed".to_string(),
                    size_bytes: None,
                    error: Some(error.to_string()),
                },
                (false, Ok(size_bytes)) => EnvironmentLogArtifactReport {
                    kind: "compose_service".to_string(),
                    service: service.to_string(),
                    stream: Some(stream_label(stream).to_string()),
                    source_path: None,
                    output: output.to_string(),
                    status: "failed".to_string(),
                    size_bytes: Some(size_bytes),
                    error: Some(format_command_failure("docker compose logs", &command_output)),
                },
                (false, Err(error)) => EnvironmentLogArtifactReport {
                    kind: "compose_service".to_string(),
                    service: service.to_string(),
                    stream: Some(stream_label(stream).to_string()),
                    source_path: None,
                    output: output.to_string(),
                    status: "failed".to_string(),
                    size_bytes: None,
                    error: Some(format!(
                        "{}; failed to write artifact: {error}",
                        format_command_failure("docker compose logs", &command_output)
                    )),
                },
            }
        }
        Err(error) => EnvironmentLogArtifactReport {
            kind: "compose_service".to_string(),
            service: service.to_string(),
            stream: Some(stream_label(stream).to_string()),
            source_path: None,
            output: output.to_string(),
            status: "failed".to_string(),
            size_bytes: None,
            error: Some(error.to_string()),
        },
    }
}

async fn collect_container_file(
    runner_root: &Path,
    runtime: &ResolvedDockerComposeRuntime,
    service: &str,
    path: &str,
    output: &str,
) -> EnvironmentLogArtifactReport {
    let output_path = match resolve_report_output_path(runner_root, output) {
        Ok(path) => path,
        Err(error) => {
            return EnvironmentLogArtifactReport {
                kind: "container_file".to_string(),
                service: service.to_string(),
                stream: None,
                source_path: Some(path.to_string()),
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(error.to_string()),
            };
        }
    };

    let container_id = match compose_container_id(runtime, service).await {
        Ok(container_id) => container_id,
        Err(error) => {
            return EnvironmentLogArtifactReport {
                kind: "container_file".to_string(),
                service: service.to_string(),
                stream: None,
                source_path: Some(path.to_string()),
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(error.to_string()),
            };
        }
    };

    if let Some(parent) = output_path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        return EnvironmentLogArtifactReport {
            kind: "container_file".to_string(),
            service: service.to_string(),
            stream: None,
            source_path: Some(path.to_string()),
            output: output.to_string(),
            status: "failed".to_string(),
            size_bytes: None,
            error: Some(error.to_string()),
        };
    }
    if output_path.exists() && output_path.is_file() {
        let _ = fs::remove_file(&output_path);
    }

    let source = format!("{container_id}:{path}");
    match run_command(
        "docker",
        &[
            "cp".to_string(),
            source.clone(),
            output_path.display().to_string(),
        ],
        None,
    )
    .await
    {
        Ok(command_output) if command_output.success() => match fs::metadata(&output_path) {
            Ok(metadata) => EnvironmentLogArtifactReport {
                kind: "container_file".to_string(),
                service: service.to_string(),
                stream: None,
                source_path: Some(path.to_string()),
                output: output.to_string(),
                status: "passed".to_string(),
                size_bytes: Some(metadata.len()),
                error: None,
            },
            Err(error) => EnvironmentLogArtifactReport {
                kind: "container_file".to_string(),
                service: service.to_string(),
                stream: None,
                source_path: Some(path.to_string()),
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(error.to_string()),
            },
        },
        Ok(command_output) => EnvironmentLogArtifactReport {
            kind: "container_file".to_string(),
            service: service.to_string(),
            stream: None,
            source_path: Some(path.to_string()),
            output: output.to_string(),
            status: "failed".to_string(),
            size_bytes: None,
            error: Some(format_command_failure("docker cp", &command_output)),
        },
        Err(error) => EnvironmentLogArtifactReport {
            kind: "container_file".to_string(),
            service: service.to_string(),
            stream: None,
            source_path: Some(path.to_string()),
            output: output.to_string(),
            status: "failed".to_string(),
            size_bytes: None,
            error: Some(error.to_string()),
        },
    }
}

async fn compose_container_id(runtime: &ResolvedDockerComposeRuntime, service: &str) -> Result<String> {
    let output = run_compose_command(runtime, "ps", &["-q".to_string(), service.to_string()]).await?;
    if !output.success() {
        bail!(format_command_failure("docker compose ps", &output));
    }
    let container_id = output.stdout.trim();
    if container_id.is_empty() {
        bail!("docker compose ps returned no container id for service `{service}`");
    }
    Ok(container_id.to_string())
}

async fn run_compose_command(
    runtime: &ResolvedDockerComposeRuntime,
    subcommand: &str,
    extra_args: &[String],
) -> Result<CommandCapture> {
    let mut args = vec!["compose".to_string(), "-p".to_string(), runtime.project_name.clone()];
    for file in &runtime.files {
        args.push("-f".to_string());
        args.push(file.display().to_string());
    }
    args.push(subcommand.to_string());
    args.extend(extra_args.iter().cloned());
    run_command("docker", &args, Some(&runtime.project_directory)).await
}

async fn run_command(
    program: &str,
    args: &[String],
    current_dir: Option<&Path>,
) -> Result<CommandCapture> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let output = command
        .output()
        .await
        .with_context(|| format!("failed to spawn `{program}`"))?;
    Ok(CommandCapture {
        command: std::iter::once(program.to_string())
            .chain(args.iter().cloned())
            .collect(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code(),
    })
}

fn resolve_report_output_path(runner_root: &Path, output: &str) -> Result<PathBuf> {
    let relative = Path::new(output);
    if relative.is_absolute() {
        bail!("artifact output must be relative to .testrunner/reports");
    }
    if relative
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("artifact output cannot escape .testrunner/reports");
    }
    Ok(runner_root.join("reports").join(relative))
}

fn write_artifact_file(path: &Path, contents: &str) -> Result<u64> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create artifact directory {}", parent.display()))?;
    }
    fs::write(path, contents)
        .with_context(|| format!("failed to write artifact file {}", path.display()))?;
    Ok(fs::metadata(path)?.len())
}

fn resolve_string(context: &RuntimeContext, raw: &str) -> Result<String> {
    Ok(value_to_string(
        context.resolve_value(&Value::String(raw.to_string()))?,
    ))
}

fn resolve_project_path(base: &Path, raw: &str) -> Result<PathBuf> {
    let path = Path::new(raw);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    Ok(resolved)
}

fn build_run_id(project_name: &str) -> String {
    sanitize_project_name(&format!(
        "{}-{}-{}",
        project_name,
        std::process::id(),
        Utc::now().timestamp_millis()
    ))
}

fn sanitize_project_name(raw: &str) -> String {
    let mut sanitized = raw
        .chars()
        .map(|ch| match ch {
            'a'..='z' | '0'..='9' => ch,
            'A'..='Z' => ch.to_ascii_lowercase(),
            _ => '-',
        })
        .collect::<String>();
    while sanitized.contains("--") {
        sanitized = sanitized.replace("--", "-");
    }
    sanitized = sanitized.trim_matches('-').to_string();
    if sanitized.is_empty() {
        "test-runner".to_string()
    } else {
        sanitized
    }
}

fn display_relative_to_root(root: &Path, display: &str) -> String {
    let path = Path::new(display);
    path.strip_prefix(root)
        .map(|value| value.display().to_string())
        .unwrap_or_else(|_| display.to_string())
}

fn format_command_failure(action: &str, output: &CommandCapture) -> String {
    let mut message = format!(
        "{action} failed with exit code {}",
        output
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );
    if let Some(stdout) = non_empty(output.stdout.clone()) {
        message.push_str(&format!("\nstdout:\n{stdout}"));
    }
    if let Some(stderr) = non_empty(output.stderr.clone()) {
        message.push_str(&format!("\nstderr:\n{stderr}"));
    }
    message
}

fn non_empty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn stream_label(stream: ComposeLogStream) -> &'static str {
    match stream {
        ComposeLogStream::Stdout => "stdout",
        ComposeLogStream::Stderr => "stderr",
        ComposeLogStream::Combined => "combined",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_project_name_normalizes_runtime_identifiers() {
        assert_eq!(
            sanitize_project_name("Sample Project / 1"),
            "sample-project-1"
        );
        assert_eq!(sanitize_project_name("___"), "test-runner");
    }

    #[test]
    fn report_output_paths_stay_under_reports_directory() {
        let root = PathBuf::from("/tmp/project/.testrunner");
        let path = resolve_report_output_path(&root, "env/mysql.log").expect("valid path");
        assert_eq!(path, root.join("reports").join("env/mysql.log"));
        let error = resolve_report_output_path(&root, "../escape.log")
            .expect_err("parent traversal must fail");
        assert!(error.to_string().contains("cannot escape"));
    }
}
