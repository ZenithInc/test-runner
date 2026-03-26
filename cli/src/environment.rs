use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::container::{
    Config as ContainerConfig, CreateContainerOptions, InspectContainerOptions, LogOutput,
    LogsOptions, RemoveContainerOptions, StartContainerOptions,
};
use bollard::errors::Error as BollardError;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::network::{CreateNetworkOptions, InspectNetworkOptions};
use chrono::Utc;
use flate2::Compression;
use flate2::write::GzEncoder;
use futures_util::StreamExt;
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::io::IsTerminal;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, sleep, timeout};
use url::Url;

use crate::config::{
    ComposeLogStream, ContainerBuildConfig, ContainerServiceConfig, ContainerWaitFor,
    DatasourceDefinition, EnvironmentConfig, EnvironmentLogSource, EnvironmentReadinessCheck,
    EnvironmentRuntimeCleanupPolicy, EnvironmentRuntimeKind, LoadedProject,
    environment_context_value,
};
use crate::runtime::{RuntimeContext, value_to_string};
use crate::url_rewrite::{
    is_rewritable_host, render_url_without_root_slash, rewrite_url_base_in_place,
};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slots: Option<usize>,
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
    pub slot_id: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentLogArtifactReport {
    pub kind: String,
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<usize>,
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

#[derive(Debug, Clone)]
pub struct EnvironmentSlot {
    pub slot_id: usize,
    pub port_mappings: HashMap<String, HashMap<u16, u16>>,
}

pub struct EnvironmentSession {
    project: LoadedProject,
    runner_root: PathBuf,
    run_id: String,
    runtime: Option<ResolvedRuntime>,
    runtime_invoked: bool,
    follow_logs: bool,
    live_logs: Vec<LiveLogHandle>,
    active_logs: Vec<ActiveLogHandle>,
    report: EnvironmentArtifactsReport,
    slots: Vec<EnvironmentSlot>,
}

#[derive(Debug, Clone)]
enum ResolvedRuntime {
    DockerCompose(ResolvedDockerComposeRuntime),
    Containers(ResolvedContainersRuntime),
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
struct ResolvedContainersRuntime {
    services: Vec<ContainerServiceConfig>,
    network_name_prefix: String,
    cleanup: EnvironmentRuntimeCleanupPolicy,
    slot_count: usize,
    slots: Vec<ResolvedContainerSlot>,
    slot_mock_rewrite: Option<SlotMockRewrite>,
}

#[derive(Debug, Clone)]
struct ResolvedContainerSlot {
    slot_id: usize,
    network_name: String,
    container_ids: Vec<(String, String)>,
    port_mappings: HashMap<String, HashMap<u16, u16>>,
}

#[derive(Debug, Clone)]
struct SlotMockRewrite {
    original_port: u16,
    base_urls: HashMap<usize, String>,
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
    RedisMonitor {
        service: String,
        output: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolvedLiveLogSpec {
    ComposeService {
        slot_id: usize,
        report_slot_id: Option<usize>,
        service: String,
        stream: ComposeLogStream,
    },
    ContainerFile {
        slot_id: usize,
        report_slot_id: Option<usize>,
        service: String,
        path: String,
    },
}

impl ResolvedLiveLogSpec {
    fn slot_id(&self) -> usize {
        match self {
            Self::ComposeService { slot_id, .. } | Self::ContainerFile { slot_id, .. } => *slot_id,
        }
    }

    fn service(&self) -> &str {
        match self {
            Self::ComposeService { service, .. } | Self::ContainerFile { service, .. } => service,
        }
    }

    fn display_label(&self) -> String {
        let base = match self {
            Self::ComposeService {
                report_slot_id,
                service,
                ..
            }
            | Self::ContainerFile {
                report_slot_id,
                service,
                ..
            } => match report_slot_id {
                Some(slot_id) => format!("[env:slot-{slot_id}:{service}]"),
                None => format!("[env:{service}]"),
            },
        };
        match self {
            Self::ComposeService { stream, .. } => match stream {
                ComposeLogStream::Combined => base,
                _ => format!("{base}:{}", stream_label(*stream)),
            },
            Self::ContainerFile { path, .. } => {
                let file_name = Path::new(path)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("file");
                format!("{base}:{file_name}")
            }
        }
    }

    fn color(&self) -> LiveLogColor {
        match self {
            Self::ComposeService { service, .. } => live_log_color_for_service(service, None),
            Self::ContainerFile { service, path, .. } => {
                live_log_color_for_service(service, Some(path))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedActiveLogSpec {
    slot_id: usize,
    report_slot_id: Option<usize>,
    service: String,
    output: String,
}

impl ResolvedActiveLogSpec {
    fn display_label(&self) -> String {
        match self.report_slot_id {
            Some(slot_id) => format!("[env:slot-{slot_id}:{}:monitor]", self.service),
            None => format!("[env:{}:monitor]", self.service),
        }
    }

    fn color(&self) -> LiveLogColor {
        live_log_color_for_service(&self.service, None)
    }
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

struct LiveLogHandle {
    label: String,
    task: JoinHandle<()>,
}

struct ActiveLogHandle {
    kind: String,
    service: String,
    slot_id: Option<usize>,
    source_path: Option<String>,
    output: String,
    output_path: PathBuf,
    task: JoinHandle<Result<(), String>>,
}

const LIVE_TAIL_READY_MARKER: &str = "__TEST_RUNNER_LIVE_TAIL_READY__";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveLogColor {
    Default,
    App,
    Mysql,
    Redis,
}

impl LiveLogColor {
    fn label_code(self) -> &'static str {
        match self {
            Self::Default => "1;34",
            Self::App => "1;36",
            Self::Mysql => "1;35",
            Self::Redis => "1;33",
        }
    }

    fn text_code(self) -> Option<&'static str> {
        match self {
            Self::Mysql => Some("35"),
            Self::Redis => Some("33"),
            Self::Default | Self::App => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct LiveLogStyler {
    enabled: bool,
}

impl LiveLogStyler {
    fn detect() -> Self {
        let stderr_is_terminal = io::stderr().is_terminal();
        let no_color = env::var_os("NO_COLOR").is_some();
        let term_is_dumb = env::var("TERM")
            .map(|term| term.eq_ignore_ascii_case("dumb"))
            .unwrap_or(false);
        Self {
            enabled: stderr_is_terminal && !no_color && !term_is_dumb,
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

    fn format_line(&self, color: LiveLogColor, label: &str, line: &str) -> String {
        let label = self.paint(label, color.label_code());
        let line = match color.text_code() {
            Some(code) => self.paint(line, code),
            None => line.to_string(),
        };
        format!("{label} {line}")
    }

    fn format_error(&self, label: &str, message: &str) -> String {
        if self.enabled {
            format!(
                "{} {}",
                self.paint(label, "1;31"),
                self.paint(format!("[error] {message}"), "1;31")
            )
        } else {
            format!("{label} [error] {message}")
        }
    }
}

impl EnvironmentSession {
    pub fn new(project: &LoadedProject, requested_slots: usize, follow_logs: bool) -> Result<Self> {
        let run_id = build_run_id(&project.project.project.name);
        let render_context = build_render_context(project, &run_id)?;
        let runtime = project
            .environment
            .runtime
            .as_ref()
            .map(|runtime_config| {
                resolve_runtime(
                    project,
                    runtime_config,
                    &render_context,
                    &run_id,
                    requested_slots,
                )
            })
            .transpose()?;
        let report = runtime.as_ref().map(|resolved| match resolved {
            ResolvedRuntime::DockerCompose(dc) => EnvironmentRuntimeReport {
                kind: "docker_compose".to_string(),
                project_name: dc.project_name.clone(),
                project_directory: dc.project_directory.display().to_string(),
                slots: None,
                files: dc
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
            },
            ResolvedRuntime::Containers(ct) => EnvironmentRuntimeReport {
                kind: "containers".to_string(),
                project_name: ct.network_name_prefix.clone(),
                project_directory: String::new(),
                slots: (ct.slot_count > 1).then_some(ct.slot_count),
                files: ct
                    .services
                    .iter()
                    .map(|service| {
                        if service.image.is_empty() {
                            format!("{}:<built>", service.name)
                        } else {
                            format!("{}:{}", service.name, service.image)
                        }
                    })
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
            },
        });

        Ok(Self {
            project: project.clone(),
            runner_root: project.runner_root.clone(),
            run_id,
            runtime,
            runtime_invoked: false,
            follow_logs,
            live_logs: Vec::new(),
            active_logs: Vec::new(),
            report: EnvironmentArtifactsReport {
                runtime: report,
                readiness: Vec::new(),
                logs: Vec::new(),
            },
            slots: Vec::new(),
        })
    }

    pub fn slots(&self) -> &[EnvironmentSlot] {
        &self.slots
    }

    pub fn set_slot_mock_base_urls(
        &mut self,
        original_port: u16,
        base_urls: HashMap<usize, String>,
    ) -> Result<()> {
        if base_urls.is_empty() {
            return Ok(());
        }

        let Some(ResolvedRuntime::Containers(runtime)) = self.runtime.as_mut() else {
            return Ok(());
        };

        if base_urls.len() != runtime.slot_count {
            bail!(
                "expected {} slot mock base URL(s), got {}",
                runtime.slot_count,
                base_urls.len()
            );
        }

        for slot_id in 0..runtime.slot_count {
            if !base_urls.contains_key(&slot_id) {
                bail!("missing slot mock base URL for slot `{slot_id}`");
            }
        }

        runtime.slot_mock_rewrite = Some(SlotMockRewrite {
            original_port,
            base_urls,
        });
        Ok(())
    }

    pub fn project_for_slot(&self, slot_id: usize) -> Result<LoadedProject> {
        let mut project = self.project.clone();
        let Some(runtime) = self.runtime.as_ref() else {
            return if slot_id == 0 {
                Ok(project)
            } else {
                bail!("slot `{slot_id}` is not available")
            };
        };

        let Some(slot) = self.slots.iter().find(|slot| slot.slot_id == slot_id) else {
            return if matches!(runtime, ResolvedRuntime::DockerCompose(_)) && slot_id == 0 {
                Ok(project)
            } else {
                bail!("slot `{slot_id}` is not available")
            };
        };

        inject_port_mappings(&slot.port_mappings, &mut project.environment);
        apply_slot_endpoint_overrides(&mut project, runtime, slot);
        Ok(project)
    }

    fn slot_report_id(&self, slot_id: usize) -> Option<usize> {
        (self.slots.len() > 1).then_some(slot_id)
    }

    fn readiness_slot_ids(&self) -> Vec<usize> {
        if self.slots.is_empty() {
            vec![0]
        } else {
            self.slots.iter().map(|slot| slot.slot_id).collect()
        }
    }

    fn resolve_live_log_specs(&self) -> Result<Vec<ResolvedLiveLogSpec>> {
        let mut specs = Vec::new();
        for slot_id in self.readiness_slot_ids() {
            let slot_project = self.project_for_slot(slot_id)?;
            specs.extend(resolve_live_log_specs_for_project(
                &slot_project,
                &self.run_id,
                slot_id,
                self.slot_report_id(slot_id),
            )?);
        }
        Ok(specs)
    }

    fn resolve_active_log_specs(&self) -> Result<Vec<ResolvedActiveLogSpec>> {
        let mut specs = Vec::new();
        for slot_id in self.readiness_slot_ids() {
            let slot_project = self.project_for_slot(slot_id)?;
            specs.extend(resolve_active_log_specs_for_project(
                &slot_project,
                &self.run_id,
                slot_id,
                self.slot_report_id(slot_id),
            )?);
        }
        Ok(specs)
    }

    async fn run_readiness_checks(&mut self) -> Result<()> {
        for slot_id in self.readiness_slot_ids() {
            let project = self.project_for_slot(slot_id)?;
            let context = build_render_context(&project, &self.run_id)?;
            for check in &project.environment.readiness {
                let resolved = resolve_readiness(check, &context)?;
                let mut report = run_readiness_check(&resolved).await;
                report.slot_id = self.slot_report_id(slot_id);
                let failed = report.status == "failed";
                let error = report.error.clone();
                self.report.readiness.push(report);
                if failed {
                    bail!(
                        error.unwrap_or_else(|| "environment readiness check failed".to_string())
                    );
                }
            }
        }
        Ok(())
    }

    async fn start_live_logs(&mut self) -> Result<()> {
        if !self.follow_logs {
            return Ok(());
        }
        let Some(runtime) = self.runtime.clone() else {
            return Ok(());
        };
        let specs = self.resolve_live_log_specs()?;
        if specs.is_empty() {
            return Ok(());
        }

        let docker = connect_docker()?;
        let mut handles = Vec::new();
        let start_result = match runtime {
            ResolvedRuntime::DockerCompose(dc) => {
                for spec in specs {
                    let container_id = compose_container_id(&dc, spec.service()).await?;
                    handles.push(start_live_log_task(docker.clone(), container_id, spec).await?);
                }
                Ok(())
            }
            ResolvedRuntime::Containers(ct) => {
                for spec in specs {
                    let slot = ct
                        .slots
                        .iter()
                        .find(|slot| slot.slot_id == spec.slot_id())
                        .with_context(|| {
                            format!("no slot runtime found for slot `{}`", spec.slot_id())
                        })?;
                    let container_id = slot_container_id(slot, spec.service())?;
                    handles.push(start_live_log_task(docker.clone(), container_id, spec).await?);
                }
                Ok(())
            }
        };

        if let Err(error) = start_result {
            stop_live_log_handles(handles).await;
            return Err(error);
        }

        self.live_logs = handles;
        Ok(())
    }

    async fn stop_live_logs(&mut self) {
        stop_live_log_handles(std::mem::take(&mut self.live_logs)).await;
    }

    async fn start_active_logs(&mut self) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        let specs = match self.resolve_active_log_specs() {
            Ok(specs) => specs,
            Err(error) => {
                self.report.logs.push(EnvironmentLogArtifactReport {
                    kind: "environment".to_string(),
                    service: "environment".to_string(),
                    slot_id: None,
                    stream: None,
                    source_path: None,
                    output: String::new(),
                    status: "failed".to_string(),
                    size_bytes: None,
                    error: Some(error.to_string()),
                });
                return;
            }
        };
        if specs.is_empty() {
            return;
        }
        let docker = match connect_docker() {
            Ok(docker) => docker,
            Err(error) => {
                for spec in specs {
                    self.report.logs.push(failed_active_log_report(
                        "redis_monitor",
                        &spec.service,
                        spec.report_slot_id,
                        &spec.output,
                        None,
                        error.to_string(),
                    ));
                }
                return;
            }
        };

        match runtime {
            ResolvedRuntime::DockerCompose(dc) => {
                for spec in specs {
                    let container_id = match compose_container_id(&dc, &spec.service).await {
                        Ok(container_id) => container_id,
                        Err(error) => {
                            self.report.logs.push(failed_active_log_report(
                                "redis_monitor",
                                &spec.service,
                                spec.report_slot_id,
                                &spec.output,
                                None,
                                error.to_string(),
                            ));
                            continue;
                        }
                    };
                    match prepare_active_output_path(&self.runner_root, &spec.output) {
                        Ok(output_path) => {
                            let report_slot_id = spec.report_slot_id;
                            let service = spec.service.clone();
                            let output = spec.output.clone();
                            match start_redis_monitor_capture(
                                docker.clone(),
                                container_id,
                                spec,
                                output_path,
                                self.follow_logs,
                            )
                            .await
                            {
                                Ok(handle) => self.active_logs.push(handle),
                                Err(error) => self.report.logs.push(failed_active_log_report(
                                    "redis_monitor",
                                    &service,
                                    report_slot_id,
                                    &output,
                                    None,
                                    error,
                                )),
                            }
                        }
                        Err(error) => {
                            self.report.logs.push(failed_active_log_report(
                                "redis_monitor",
                                &spec.service,
                                spec.report_slot_id,
                                &spec.output,
                                None,
                                error.to_string(),
                            ));
                        }
                    }
                }
            }
            ResolvedRuntime::Containers(ct) => {
                for spec in specs {
                    let slot = match ct.slots.iter().find(|slot| slot.slot_id == spec.slot_id) {
                        Some(slot) => slot,
                        None => {
                            self.report.logs.push(failed_active_log_report(
                                "redis_monitor",
                                &spec.service,
                                spec.report_slot_id,
                                &spec.output,
                                None,
                                format!("no slot runtime found for slot `{}`", spec.slot_id),
                            ));
                            continue;
                        }
                    };
                    let container_id = match slot_container_id(slot, &spec.service) {
                        Ok(container_id) => container_id,
                        Err(error) => {
                            self.report.logs.push(failed_active_log_report(
                                "redis_monitor",
                                &spec.service,
                                spec.report_slot_id,
                                &spec.output,
                                None,
                                error.to_string(),
                            ));
                            continue;
                        }
                    };
                    match prepare_active_output_path(&self.runner_root, &spec.output) {
                        Ok(output_path) => {
                            let report_slot_id = spec.report_slot_id;
                            let service = spec.service.clone();
                            let output = spec.output.clone();
                            match start_redis_monitor_capture(
                                docker.clone(),
                                container_id,
                                spec,
                                output_path,
                                self.follow_logs,
                            )
                            .await
                            {
                                Ok(handle) => self.active_logs.push(handle),
                                Err(error) => self.report.logs.push(failed_active_log_report(
                                    "redis_monitor",
                                    &service,
                                    report_slot_id,
                                    &output,
                                    None,
                                    error,
                                )),
                            }
                        }
                        Err(error) => {
                            self.report.logs.push(failed_active_log_report(
                                "redis_monitor",
                                &spec.service,
                                spec.report_slot_id,
                                &spec.output,
                                None,
                                error.to_string(),
                            ));
                        }
                    }
                }
            }
        }
    }

    async fn finalize_active_logs(&mut self) {
        let reports = stop_active_log_handles(std::mem::take(&mut self.active_logs)).await;
        self.report.logs.extend(reports);
    }

    pub async fn prepare(&mut self) -> Result<()> {
        match self.runtime.clone() {
            Some(ResolvedRuntime::DockerCompose(runtime)) => {
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
            Some(ResolvedRuntime::Containers(ref containers_config)) => {
                let started = Instant::now();
                match start_containers(containers_config, &self.runner_root).await {
                    Ok(slots) => {
                        self.runtime_invoked = true;
                        if let Some(ResolvedRuntime::Containers(ref mut ct)) = self.runtime {
                            ct.slots = slots.clone();
                        }
                        self.slots = slots
                            .iter()
                            .map(|slot| EnvironmentSlot {
                                slot_id: slot.slot_id,
                                port_mappings: slot.port_mappings.clone(),
                            })
                            .collect();
                        if let Some(report) = self.report.runtime.as_mut() {
                            report.startup_duration_ms = Some(started.elapsed().as_millis());
                            report.startup_status = Some("passed".to_string());
                            report.startup_command = Some(vec!["bollard::containers".to_string()]);
                        }
                    }
                    Err(error) => {
                        if let Some(report) = self.report.runtime.as_mut() {
                            report.startup_duration_ms = Some(started.elapsed().as_millis());
                            report.startup_status = Some("failed".to_string());
                            report.startup_error = Some(error.to_string());
                        }
                        bail!(error);
                    }
                }
            }
            None => {}
        }
        self.run_readiness_checks().await?;
        self.start_active_logs().await;
        self.start_live_logs().await?;

        Ok(())
    }

    pub async fn finish(mut self, success: bool) -> EnvironmentArtifactsReport {
        if self.runtime_invoked {
            self.collect_logs().await;
        }
        self.shutdown(success).await;
        self.finalize_active_logs().await;
        self.stop_live_logs().await;
        self.report
    }

    async fn collect_logs(&mut self) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        let slot_ids = self.readiness_slot_ids();
        for slot_id in slot_ids {
            let slot_project = match self.project_for_slot(slot_id) {
                Ok(project) => project,
                Err(error) => {
                    self.report.logs.push(EnvironmentLogArtifactReport {
                        kind: "environment".to_string(),
                        service: "environment".to_string(),
                        slot_id: self.slot_report_id(slot_id),
                        stream: None,
                        source_path: None,
                        output: String::new(),
                        status: "failed".to_string(),
                        size_bytes: None,
                        error: Some(error.to_string()),
                    });
                    continue;
                }
            };
            let context = match build_render_context(&slot_project, &self.run_id) {
                Ok(context) => context,
                Err(error) => {
                    self.report.logs.push(EnvironmentLogArtifactReport {
                        kind: "environment".to_string(),
                        service: "environment".to_string(),
                        slot_id: self.slot_report_id(slot_id),
                        stream: None,
                        source_path: None,
                        output: String::new(),
                        status: "failed".to_string(),
                        size_bytes: None,
                        error: Some(error.to_string()),
                    });
                    continue;
                }
            };

            for log in &slot_project.environment.logs {
                let spec = match resolve_log_spec(log, &context) {
                    Ok(spec) => spec,
                    Err(error) => {
                        self.report.logs.push(EnvironmentLogArtifactReport {
                            kind: "environment".to_string(),
                            service: "environment".to_string(),
                            slot_id: self.slot_report_id(slot_id),
                            stream: None,
                            source_path: None,
                            output: String::new(),
                            status: "failed".to_string(),
                            size_bytes: None,
                            error: Some(error.to_string()),
                        });
                        continue;
                    }
                };
                let output = slot_output_path(spec.output(), slot_id, self.slots.len());
                let artifact = match (&spec, &runtime) {
                    (
                        ResolvedLogSpec::ComposeService {
                            service, stream, ..
                        },
                        ResolvedRuntime::DockerCompose(dc),
                    ) => {
                        collect_compose_service_logs(
                            &self.runner_root,
                            dc,
                            service,
                            *stream,
                            &output,
                        )
                        .await
                    }
                    (
                        ResolvedLogSpec::ContainerFile { service, path, .. },
                        ResolvedRuntime::DockerCompose(dc),
                    ) => {
                        collect_container_file(&self.runner_root, dc, service, path, &output).await
                    }
                    (
                        ResolvedLogSpec::ComposeService { service, .. },
                        ResolvedRuntime::Containers(ct),
                    ) => match ct.slots.iter().find(|slot| slot.slot_id == slot_id) {
                        Some(slot) => {
                            collect_container_logs_bollard(
                                &self.runner_root,
                                slot,
                                service,
                                &output,
                            )
                            .await
                        }
                        None => EnvironmentLogArtifactReport {
                            kind: "containers".to_string(),
                            service: service.clone(),
                            slot_id: self.slot_report_id(slot_id),
                            stream: Some("combined".to_string()),
                            source_path: None,
                            output,
                            status: "failed".to_string(),
                            size_bytes: None,
                            error: Some(format!("no slot runtime found for slot `{slot_id}`")),
                        },
                    },
                    (
                        ResolvedLogSpec::ContainerFile { service, path, .. },
                        ResolvedRuntime::Containers(ct),
                    ) => match ct.slots.iter().find(|slot| slot.slot_id == slot_id) {
                        Some(slot) => {
                            collect_container_file_bollard(
                                &self.runner_root,
                                slot,
                                service,
                                path,
                                &output,
                            )
                            .await
                        }
                        None => EnvironmentLogArtifactReport {
                            kind: "container_file".to_string(),
                            service: service.clone(),
                            slot_id: self.slot_report_id(slot_id),
                            stream: None,
                            source_path: Some(path.clone()),
                            output,
                            status: "failed".to_string(),
                            size_bytes: None,
                            error: Some(format!("no slot runtime found for slot `{slot_id}`")),
                        },
                    },
                    (ResolvedLogSpec::RedisMonitor { .. }, _) => {
                        continue;
                    }
                };
                self.report.logs.push(artifact);
            }
        }
    }

    async fn shutdown(&mut self, success: bool) {
        let Some(runtime) = self.runtime.clone() else {
            return;
        };
        let Some(report) = self.report.runtime.as_mut() else {
            return;
        };

        let cleanup_policy = match &runtime {
            ResolvedRuntime::DockerCompose(dc) => dc.cleanup,
            ResolvedRuntime::Containers(ct) => ct.cleanup,
        };
        let should_cleanup = match cleanup_policy {
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

        match runtime {
            ResolvedRuntime::DockerCompose(dc) => {
                let started = Instant::now();
                match run_compose_command(&dc, "down", &dc.down_args).await {
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
            ResolvedRuntime::Containers(ct) => {
                let started = Instant::now();
                match stop_containers(&ct.slots).await {
                    Ok(()) => {
                        report.shutdown_duration_ms = Some(started.elapsed().as_millis());
                        report.shutdown_status = Some("passed".to_string());
                        report.shutdown_command =
                            Some(vec!["bollard::containers::stop+remove".to_string()]);
                    }
                    Err(error) => {
                        report.shutdown_duration_ms = Some(started.elapsed().as_millis());
                        report.shutdown_status = Some("failed".to_string());
                        report.shutdown_error = Some(error.to_string());
                    }
                }
            }
        }
    }
}

impl ResolvedLogSpec {
    fn output(&self) -> &str {
        match self {
            Self::ComposeService { output, .. }
            | Self::ContainerFile { output, .. }
            | Self::RedisMonitor { output, .. } => output,
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
    requested_slots: usize,
) -> Result<ResolvedRuntime> {
    match runtime.kind {
        EnvironmentRuntimeKind::DockerCompose => {
            resolve_docker_compose_runtime(project, runtime, context, run_id)
                .map(ResolvedRuntime::DockerCompose)
        }
        EnvironmentRuntimeKind::Containers => {
            resolve_containers_runtime(project, runtime, context, run_id, requested_slots)
                .map(ResolvedRuntime::Containers)
        }
    }
}

fn resolve_docker_compose_runtime(
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
                project.project.project.name, run_id
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

fn resolve_containers_runtime(
    project: &LoadedProject,
    runtime: &crate::config::EnvironmentRuntimeConfig,
    context: &RuntimeContext,
    run_id: &str,
    requested_slots: usize,
) -> Result<ResolvedContainersRuntime> {
    let network_name_prefix = runtime
        .network_name
        .as_ref()
        .map(|value| resolve_string(context, value))
        .transpose()?
        .map(|value| sanitize_project_name(&value))
        .unwrap_or_else(|| {
            sanitize_project_name(&format!(
                "test-runner-{}-{}",
                project.project.project.name, run_id
            ))
        });

    Ok(ResolvedContainersRuntime {
        services: runtime.services.clone(),
        network_name_prefix,
        cleanup: runtime.cleanup,
        slot_count: requested_slots.max(1),
        slots: Vec::new(),
        slot_mock_rewrite: None,
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

fn resolve_log_spec(
    log: &EnvironmentLogSource,
    context: &RuntimeContext,
) -> Result<ResolvedLogSpec> {
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
        EnvironmentLogSource::RedisMonitor { service, output } => ResolvedLogSpec::RedisMonitor {
            service: resolve_string(context, service)?,
            output: resolve_string(context, output)?,
        },
    })
}

fn slot_network_name(prefix: &str, slot_id: usize, slot_count: usize) -> String {
    if slot_count <= 1 {
        prefix.to_string()
    } else {
        format!("{prefix}-slot-{slot_id}")
    }
}

fn slot_output_path(output: &str, slot_id: usize, slot_count: usize) -> String {
    if slot_count <= 1 {
        output.to_string()
    } else {
        format!("slot-{slot_id}/{output}")
    }
}

fn inject_port_mappings(
    port_mappings: &HashMap<String, HashMap<u16, u16>>,
    environment: &mut EnvironmentConfig,
) {
    if port_mappings.is_empty() {
        return;
    }
    let mut ports_map = serde_json::Map::new();
    for (service, mapping) in port_mappings {
        let mut service_map = serde_json::Map::new();
        for (container_port, host_port) in mapping {
            service_map.insert(container_port.to_string(), json!(host_port));
        }
        ports_map.insert(service.clone(), Value::Object(service_map));
    }
    environment
        .variables
        .insert("runtime_ports".to_string(), Value::Object(ports_map));
}

fn apply_slot_endpoint_overrides(
    project: &mut LoadedProject,
    runtime: &ResolvedRuntime,
    slot: &EnvironmentSlot,
) {
    let ResolvedRuntime::Containers(runtime) = runtime else {
        return;
    };
    let Ok(rewrite_rules) = configured_port_rewrite_rules(runtime) else {
        return;
    };

    rewrite_local_url_in_place(&mut project.environment.base_url, &rewrite_rules, slot);
    for value in project.environment.variables.values_mut() {
        rewrite_value_urls(value, &rewrite_rules, slot);
    }
    for check in &mut project.environment.readiness {
        match check {
            EnvironmentReadinessCheck::Http { url, .. } => {
                rewrite_local_url_in_place(url, &rewrite_rules, slot);
            }
            EnvironmentReadinessCheck::Tcp { host, port, .. } => {
                if is_rewritable_host(host)
                    && let Some((service, container_port)) = rewrite_rules.get(port)
                    && let Some(host_port) = slot
                        .port_mappings
                        .get(service)
                        .and_then(|mapping| mapping.get(container_port))
                {
                    *port = *host_port;
                }
            }
        }
    }
    for api in project.apis.values_mut() {
        if let Some(base_url) = api.definition.base_url.as_mut() {
            rewrite_local_url_in_place(base_url, &rewrite_rules, slot);
        }
    }
    for datasource in project.datasources.values_mut() {
        match datasource {
            DatasourceDefinition::Mysql(config) | DatasourceDefinition::Postgres(config) => {
                rewrite_local_url_in_place(&mut config.url, &rewrite_rules, slot);
            }
            DatasourceDefinition::Redis(config) => {
                rewrite_local_url_in_place(&mut config.url, &rewrite_rules, slot);
            }
        }
    }
    if let Some(runtime_config) = project.environment.runtime.as_mut()
        && runtime_config.kind == EnvironmentRuntimeKind::Containers
    {
        for service in &mut runtime_config.services {
            for value in service.environment.values_mut() {
                rewrite_local_url_in_place(value, &rewrite_rules, slot);
            }
        }
    }
}

fn configured_port_rewrite_rules(
    runtime: &ResolvedContainersRuntime,
) -> Result<HashMap<u16, (String, u16)>> {
    let mut rules = HashMap::new();
    for service in &runtime.services {
        for port_spec in &service.ports {
            let (container_port, host_port) = parse_port_mapping(port_spec)?;
            if let Some(host_port) = host_port {
                rules.insert(host_port, (service.name.clone(), container_port));
            }
        }
    }
    Ok(rules)
}

fn rewrite_value_urls(
    value: &mut Value,
    rewrite_rules: &HashMap<u16, (String, u16)>,
    slot: &EnvironmentSlot,
) {
    match value {
        Value::String(text) => rewrite_local_url_in_place(text, rewrite_rules, slot),
        Value::Array(items) => {
            for item in items {
                rewrite_value_urls(item, rewrite_rules, slot);
            }
        }
        Value::Object(object) => {
            for item in object.values_mut() {
                rewrite_value_urls(item, rewrite_rules, slot);
            }
        }
        _ => {}
    }
}

fn rewrite_local_url_in_place(
    text: &mut String,
    rewrite_rules: &HashMap<u16, (String, u16)>,
    slot: &EnvironmentSlot,
) {
    let Ok(mut url) = Url::parse(text) else {
        return;
    };
    let Some(host) = url.host_str() else {
        return;
    };
    if !is_rewritable_host(host) {
        return;
    }
    let Some(original_port) = url.port() else {
        return;
    };
    let Some((service, container_port)) = rewrite_rules.get(&original_port) else {
        return;
    };
    let Some(host_port) = slot
        .port_mappings
        .get(service)
        .and_then(|mapping| mapping.get(container_port))
    else {
        return;
    };
    if url.set_port(Some(*host_port)).is_ok() {
        *text = render_url_without_root_slash(&url);
    }
}

fn rewrite_service_environment_mock_urls(
    environment: &mut IndexMap<String, String>,
    original_port: u16,
    replacement_base_url: &str,
) {
    for value in environment.values_mut() {
        rewrite_url_base_in_place(value, original_port, replacement_base_url);
    }
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
                    slot_id: None,
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
        slot_id: None,
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
                    slot_id: None,
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
        slot_id: None,
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
                slot_id: None,
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
                    slot_id: None,
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
                    slot_id: None,
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
                    slot_id: None,
                    stream: Some(stream_label(stream).to_string()),
                    source_path: None,
                    output: output.to_string(),
                    status: "failed".to_string(),
                    size_bytes: Some(size_bytes),
                    error: Some(format_command_failure(
                        "docker compose logs",
                        &command_output,
                    )),
                },
                (false, Err(error)) => EnvironmentLogArtifactReport {
                    kind: "compose_service".to_string(),
                    service: service.to_string(),
                    slot_id: None,
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
            slot_id: None,
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
                slot_id: None,
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
                slot_id: None,
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
            slot_id: None,
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
                slot_id: None,
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
                slot_id: None,
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
            slot_id: None,
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
            slot_id: None,
            stream: None,
            source_path: Some(path.to_string()),
            output: output.to_string(),
            status: "failed".to_string(),
            size_bytes: None,
            error: Some(error.to_string()),
        },
    }
}

async fn compose_container_id(
    runtime: &ResolvedDockerComposeRuntime,
    service: &str,
) -> Result<String> {
    let output =
        run_compose_command(runtime, "ps", &["-q".to_string(), service.to_string()]).await?;
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
    let mut args = vec![
        "compose".to_string(),
        "-p".to_string(),
        runtime.project_name.clone(),
    ];
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

// ---------------------------------------------------------------------------
// Containers runtime (bollard-based)
// ---------------------------------------------------------------------------

fn connect_docker() -> Result<Docker> {
    Docker::connect_with_local_defaults().context("failed to connect to Docker daemon")
}

fn parse_port_mapping(spec: &str) -> Result<(u16, Option<u16>)> {
    let parts: Vec<&str> = spec.split(':').collect();
    match parts.len() {
        1 => {
            let container_port = parts[0]
                .parse::<u16>()
                .with_context(|| format!("invalid port `{spec}`"))?;
            Ok((container_port, None))
        }
        2 => {
            let host_port = parts[0]
                .parse::<u16>()
                .with_context(|| format!("invalid host port in `{spec}`"))?;
            let container_port = parts[1]
                .parse::<u16>()
                .with_context(|| format!("invalid container port in `{spec}`"))?;
            Ok((container_port, Some(host_port)))
        }
        _ => bail!("invalid port mapping `{spec}`"),
    }
}

/// Resolve the image for a service: build from Dockerfile or pull from registry.
async fn resolve_service_image(
    docker: &Docker,
    service: &ContainerServiceConfig,
    runner_root: &Path,
) -> Result<String> {
    if let Some(build_config) = &service.build {
        // Build image from Dockerfile
        let image_tag = if service.image.is_empty() {
            format!("testrunner-{}", service.name)
        } else {
            service.image.clone()
        };
        build_container_image(docker, build_config, &image_tag, runner_root).await?;
        Ok(image_tag)
    } else {
        match docker.inspect_image(&service.image).await {
            Ok(_) => Ok(service.image.clone()),
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                let mut pull_stream = docker.create_image(
                    Some(CreateImageOptions {
                        from_image: service.image.as_str(),
                        ..Default::default()
                    }),
                    None,
                    None,
                );
                while let Some(result) = pull_stream.next().await {
                    result.with_context(|| format!("failed to pull image `{}`", service.image))?;
                }
                Ok(service.image.clone())
            }
            Err(error) => {
                Err(error).with_context(|| format!("failed to inspect image `{}`", service.image))
            }
        }
    }
}

/// Build a Docker image from a build context directory.
async fn build_container_image(
    docker: &Docker,
    build_config: &ContainerBuildConfig,
    tag: &str,
    runner_root: &Path,
) -> Result<()> {
    // The runner_root is .testrunner/, context is relative to its parent (project root)
    let project_root = runner_root.parent().unwrap_or(runner_root);
    let context_path = project_root.join(&build_config.context);
    if !context_path.is_dir() {
        bail!(
            "build context directory `{}` does not exist",
            context_path.display()
        );
    }

    // Create a tar archive of the build context
    let tar_bytes = create_build_context_tar(&context_path).with_context(|| {
        format!(
            "failed to create tar for build context `{}`",
            context_path.display()
        )
    })?;

    let mut build_opts = bollard::image::BuildImageOptions {
        t: tag.to_string(),
        rm: true,
        ..Default::default()
    };
    if let Some(dockerfile) = &build_config.dockerfile {
        build_opts.dockerfile = dockerfile.clone();
    }

    let mut build_stream = docker.build_image(build_opts, None, Some(tar_bytes.into()));

    while let Some(result) = build_stream.next().await {
        let info = result.with_context(|| format!("failed to build image `{tag}`"))?;
        if let Some(error) = info.error {
            bail!("image build error for `{tag}`: {error}");
        }
    }

    Ok(())
}

/// Create a gzip-compressed tar archive of the build context directory.
fn create_build_context_tar(context_path: &Path) -> Result<Vec<u8>> {
    let buf = Vec::new();
    let enc = GzEncoder::new(buf, Compression::fast());
    let mut archive = tar::Builder::new(enc);
    archive
        .append_dir_all(".", context_path)
        .with_context(|| "failed to add files to build context tar")?;
    let enc = archive
        .into_inner()
        .with_context(|| "failed to finalize build context tar")?;
    enc.finish()
        .with_context(|| "failed to compress build context tar")
}

/// Start all containers for the containers runtime.
async fn start_containers(
    runtime: &ResolvedContainersRuntime,
    runner_root: &Path,
) -> Result<Vec<ResolvedContainerSlot>> {
    let docker = connect_docker()?;
    let mut slots = Vec::new();

    for slot_id in 0..runtime.slot_count {
        let network_name =
            slot_network_name(&runtime.network_name_prefix, slot_id, runtime.slot_count);
        let slot_mock_rewrite = runtime.slot_mock_rewrite.as_ref().and_then(|rewrite| {
            rewrite
                .base_urls
                .get(&slot_id)
                .map(|base_url| (rewrite.original_port, base_url.clone()))
        });
        let network_exists = docker
            .inspect_network(&network_name, None::<InspectNetworkOptions<String>>)
            .await
            .is_ok();
        if !network_exists {
            docker
                .create_network(CreateNetworkOptions {
                    name: network_name.as_str(),
                    driver: "bridge",
                    ..Default::default()
                })
                .await
                .with_context(|| format!("failed to create network `{network_name}`"))?;
        }

        let mut container_ids = Vec::new();
        let mut port_mappings = HashMap::new();

        for service in &runtime.services {
            let image_name = resolve_service_image(&docker, service, runner_root).await?;

            let mut exposed_ports = HashMap::new();
            let mut port_bindings: HashMap<String, Option<Vec<bollard::models::PortBinding>>> =
                HashMap::new();
            for port_spec in &service.ports {
                let (container_port, host_port) = parse_port_mapping(port_spec)?;
                let key = format!("{container_port}/tcp");
                exposed_ports.insert(key.clone(), HashMap::new());
                let host_port = if runtime.slot_count > 1 {
                    None
                } else {
                    host_port
                };
                port_bindings.insert(
                    key,
                    Some(vec![bollard::models::PortBinding {
                        host_ip: Some("0.0.0.0".to_string()),
                        host_port: host_port.map(|port| port.to_string()),
                    }]),
                );
            }

            let mut service_environment = service.environment.clone();
            if let Some((original_port, replacement_base_url)) = &slot_mock_rewrite {
                rewrite_service_environment_mock_urls(
                    &mut service_environment,
                    *original_port,
                    replacement_base_url,
                );
            }

            let env_vars: Vec<String> = service_environment
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            let binds = service.volumes.clone();
            let cmd = service.command.clone();
            let container_name = format!("{network_name}-{}", service.name);

            let host_config = bollard::models::HostConfig {
                port_bindings: Some(port_bindings),
                binds: if binds.is_empty() { None } else { Some(binds) },
                network_mode: Some(network_name.clone()),
                extra_hosts: if service.extra_hosts.is_empty() {
                    None
                } else {
                    Some(service.extra_hosts.clone())
                },
                ..Default::default()
            };

            let networking_config = bollard::container::NetworkingConfig {
                endpoints_config: HashMap::from([(
                    network_name.clone(),
                    bollard::models::EndpointSettings {
                        aliases: Some(vec![service.name.clone()]),
                        ..Default::default()
                    },
                )]),
            };

            let config = ContainerConfig {
                image: Some(image_name),
                exposed_ports: if exposed_ports.is_empty() {
                    None
                } else {
                    Some(exposed_ports)
                },
                env: if env_vars.is_empty() {
                    None
                } else {
                    Some(env_vars)
                },
                cmd: if cmd.is_empty() { None } else { Some(cmd) },
                host_config: Some(host_config),
                networking_config: Some(networking_config),
                ..Default::default()
            };

            let create_result = docker
                .create_container(
                    Some(CreateContainerOptions {
                        name: container_name.as_str(),
                        platform: None,
                    }),
                    config,
                )
                .await
                .with_context(|| format!("failed to create container `{}`", service.name))?;

            let container_id = create_result.id;

            docker
                .start_container(&container_id, None::<StartContainerOptions<String>>)
                .await
                .with_context(|| format!("failed to start container `{}`", service.name))?;

            if let Some(wait_for) = &service.wait_for {
                wait_for_container(&docker, &container_id, &service.name, wait_for).await?;
            }

            let inspect = docker
                .inspect_container(&container_id, None::<InspectContainerOptions>)
                .await
                .with_context(|| format!("failed to inspect container `{}`", service.name))?;

            let mut service_ports = HashMap::new();
            if let Some(network_settings) = &inspect.network_settings {
                if let Some(ports) = &network_settings.ports {
                    for (container_key, host_bindings) in ports {
                        if let Some(container_port) = container_key
                            .split('/')
                            .next()
                            .and_then(|port| port.parse::<u16>().ok())
                        {
                            if let Some(Some(bindings)) = host_bindings.as_ref().map(Some) {
                                if let Some(binding) = bindings.first() {
                                    if let Some(host_port) = binding
                                        .host_port
                                        .as_ref()
                                        .and_then(|port| port.parse::<u16>().ok())
                                    {
                                        service_ports.insert(container_port, host_port);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            container_ids.push((service.name.clone(), container_id));
            port_mappings.insert(service.name.clone(), service_ports);
        }

        slots.push(ResolvedContainerSlot {
            slot_id,
            network_name,
            container_ids,
            port_mappings,
        });
    }

    Ok(slots)
}

/// Wait for a container to become ready based on the configured strategy.
async fn wait_for_container(
    docker: &Docker,
    container_id: &str,
    service_name: &str,
    wait_for: &ContainerWaitFor,
) -> Result<()> {
    match wait_for {
        ContainerWaitFor::LogMessage {
            pattern,
            timeout_ms,
        } => wait_for_log_message(docker, container_id, service_name, pattern, *timeout_ms).await,
        ContainerWaitFor::Tcp {
            port,
            timeout_ms,
            interval_ms,
        } => {
            // We need to get the host port mapping first
            let host_port = resolve_host_port(docker, container_id, *port).await?;
            wait_for_tcp(service_name, host_port, *timeout_ms, *interval_ms).await
        }
        ContainerWaitFor::Http {
            port,
            path,
            expect_status,
            timeout_ms,
            interval_ms,
        } => {
            let host_port = resolve_host_port(docker, container_id, *port).await?;
            let url = format!("http://127.0.0.1:{host_port}{path}");
            wait_for_http(
                service_name,
                &url,
                *expect_status,
                *timeout_ms,
                *interval_ms,
            )
            .await
        }
    }
}

async fn resolve_host_port(
    docker: &Docker,
    container_id: &str,
    container_port: u16,
) -> Result<u16> {
    let inspect = docker
        .inspect_container(container_id, None::<InspectContainerOptions>)
        .await
        .context("failed to inspect container for port resolution")?;

    let key = format!("{container_port}/tcp");
    let port = inspect
        .network_settings
        .as_ref()
        .and_then(|ns| ns.ports.as_ref())
        .and_then(|ports| ports.get(&key))
        .and_then(|bindings| bindings.as_ref())
        .and_then(|bindings| bindings.first())
        .and_then(|binding| binding.host_port.as_ref())
        .and_then(|p| p.parse::<u16>().ok())
        .with_context(|| {
            format!("no host port mapping found for container port {container_port}")
        })?;

    Ok(port)
}

async fn wait_for_log_message(
    docker: &Docker,
    container_id: &str,
    service_name: &str,
    pattern: &str,
    timeout_ms: u64,
) -> Result<()> {
    let started = Instant::now();
    let regex = regex::Regex::new(pattern)
        .with_context(|| format!("invalid log_message pattern `{pattern}`"))?;

    let mut stream = docker.logs::<String>(
        container_id,
        Some(LogsOptions {
            follow: true,
            stdout: true,
            stderr: true,
            ..Default::default()
        }),
    );

    loop {
        let remaining = Duration::from_millis(timeout_ms)
            .checked_sub(started.elapsed())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            bail!(
                "timed out waiting for log message matching `{pattern}` on container `{service_name}` \
                 after {timeout_ms}ms"
            );
        }

        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(log_output))) => {
                let line = match &log_output {
                    LogOutput::StdOut { message } | LogOutput::StdErr { message } => {
                        String::from_utf8_lossy(message).to_string()
                    }
                    _ => continue,
                };
                if regex.is_match(&line) {
                    return Ok(());
                }
            }
            Ok(Some(Err(error))) => {
                bail!("error reading logs from container `{service_name}`: {error}");
            }
            Ok(None) => {
                bail!("container `{service_name}` log stream ended before matching `{pattern}`");
            }
            Err(_) => {
                bail!(
                    "timed out waiting for log message matching `{pattern}` on container `{service_name}` \
                     after {timeout_ms}ms"
                );
            }
        }
    }
}

async fn wait_for_tcp(
    service_name: &str,
    host_port: u16,
    timeout_ms: u64,
    interval_ms: u64,
) -> Result<()> {
    let started = Instant::now();
    let target = format!("127.0.0.1:{host_port}");
    let mut last_error = None;

    while started.elapsed().as_millis() < u128::from(timeout_ms) {
        match tokio::net::TcpStream::connect(&target).await {
            Ok(_) => return Ok(()),
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
        sleep(Duration::from_millis(interval_ms)).await;
    }

    bail!(
        "timed out waiting for TCP readiness on container `{service_name}` port {host_port} \
         after {timeout_ms}ms: {}",
        last_error.unwrap_or_else(|| "unknown".to_string())
    )
}

async fn wait_for_http(
    service_name: &str,
    url: &str,
    expect_status: u16,
    timeout_ms: u64,
    interval_ms: u64,
) -> Result<()> {
    let client = reqwest::Client::new();
    let started = Instant::now();
    let mut last_error = None;

    while started.elapsed().as_millis() < u128::from(timeout_ms) {
        match client.get(url).send().await {
            Ok(response) if response.status().as_u16() == expect_status => return Ok(()),
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

    bail!(
        "timed out waiting for HTTP readiness on container `{service_name}` at {url} \
         after {timeout_ms}ms: {}",
        last_error.unwrap_or_else(|| "unknown".to_string())
    )
}

/// Stop and remove all containers, then remove the network.
async fn stop_containers(slots: &[ResolvedContainerSlot]) -> Result<()> {
    let docker = connect_docker()?;
    let mut errors = Vec::new();

    for slot in slots.iter().rev() {
        for (service_name, container_id) in slot.container_ids.iter().rev() {
            if let Err(error) = docker
                .remove_container(
                    container_id,
                    Some(RemoveContainerOptions {
                        force: true,
                        v: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                errors.push(format!(
                    "failed to remove container `{service_name}` from slot `{}`: {error}",
                    slot.slot_id
                ));
            }
        }
        if let Err(error) = docker.remove_network(&slot.network_name).await {
            errors.push(format!(
                "failed to remove network `{}`: {error}",
                slot.network_name
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

/// Collect logs from a container using bollard.
async fn collect_container_logs_bollard(
    runner_root: &Path,
    slot: &ResolvedContainerSlot,
    service: &str,
    output: &str,
) -> EnvironmentLogArtifactReport {
    let output_path = match resolve_report_output_path(runner_root, output) {
        Ok(path) => path,
        Err(error) => {
            return EnvironmentLogArtifactReport {
                kind: "containers".to_string(),
                service: service.to_string(),
                slot_id: Some(slot.slot_id),
                stream: Some("combined".to_string()),
                source_path: None,
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(error.to_string()),
            };
        }
    };

    let container_id = match slot_container_id(slot, service) {
        Ok(id) => id,
        Err(error) => {
            return EnvironmentLogArtifactReport {
                kind: "containers".to_string(),
                service: service.to_string(),
                slot_id: Some(slot.slot_id),
                stream: Some("combined".to_string()),
                source_path: None,
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(error.to_string()),
            };
        }
    };

    let docker = match connect_docker() {
        Ok(d) => d,
        Err(error) => {
            return EnvironmentLogArtifactReport {
                kind: "containers".to_string(),
                service: service.to_string(),
                slot_id: Some(slot.slot_id),
                stream: Some("combined".to_string()),
                source_path: None,
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(error.to_string()),
            };
        }
    };

    let mut logs = String::new();
    let mut stream = docker.logs::<String>(
        &container_id,
        Some(LogsOptions {
            follow: false,
            stdout: true,
            stderr: true,
            timestamps: true,
            ..Default::default()
        }),
    );

    while let Some(result) = stream.next().await {
        match result {
            Ok(log_output) => match log_output {
                LogOutput::StdOut { message } | LogOutput::StdErr { message } => {
                    logs.push_str(&String::from_utf8_lossy(&message));
                }
                _ => {}
            },
            Err(error) => {
                return EnvironmentLogArtifactReport {
                    kind: "containers".to_string(),
                    service: service.to_string(),
                    slot_id: Some(slot.slot_id),
                    stream: Some("combined".to_string()),
                    source_path: None,
                    output: output.to_string(),
                    status: "failed".to_string(),
                    size_bytes: None,
                    error: Some(format!("error reading logs: {error}")),
                };
            }
        }
    }

    match write_artifact_file(&output_path, &logs) {
        Ok(size_bytes) => EnvironmentLogArtifactReport {
            kind: "containers".to_string(),
            service: service.to_string(),
            slot_id: Some(slot.slot_id),
            stream: Some("combined".to_string()),
            source_path: None,
            output: output.to_string(),
            status: "passed".to_string(),
            size_bytes: Some(size_bytes),
            error: None,
        },
        Err(error) => EnvironmentLogArtifactReport {
            kind: "containers".to_string(),
            service: service.to_string(),
            slot_id: Some(slot.slot_id),
            stream: Some("combined".to_string()),
            source_path: None,
            output: output.to_string(),
            status: "failed".to_string(),
            size_bytes: None,
            error: Some(error.to_string()),
        },
    }
}

/// Copy a file from a container using the docker cp command.
async fn collect_container_file_bollard(
    runner_root: &Path,
    slot: &ResolvedContainerSlot,
    service: &str,
    path: &str,
    output: &str,
) -> EnvironmentLogArtifactReport {
    let output_path = match resolve_report_output_path(runner_root, output) {
        Ok(p) => p,
        Err(error) => {
            return EnvironmentLogArtifactReport {
                kind: "container_file".to_string(),
                service: service.to_string(),
                slot_id: Some(slot.slot_id),
                stream: None,
                source_path: Some(path.to_string()),
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(error.to_string()),
            };
        }
    };

    let container_id = match slot_container_id(slot, service) {
        Ok(id) => id,
        Err(error) => {
            return EnvironmentLogArtifactReport {
                kind: "container_file".to_string(),
                service: service.to_string(),
                slot_id: Some(slot.slot_id),
                stream: None,
                source_path: Some(path.to_string()),
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(error.to_string()),
            };
        }
    };

    if let Some(parent) = output_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if output_path.exists() && output_path.is_file() {
        let _ = fs::remove_file(&output_path);
    }

    let source = format!("{container_id}:{path}");
    match run_command(
        "docker",
        &["cp".to_string(), source, output_path.display().to_string()],
        None,
    )
    .await
    {
        Ok(command_output) if command_output.success() => match fs::metadata(&output_path) {
            Ok(metadata) => EnvironmentLogArtifactReport {
                kind: "container_file".to_string(),
                service: service.to_string(),
                slot_id: Some(slot.slot_id),
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
                slot_id: Some(slot.slot_id),
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
            slot_id: Some(slot.slot_id),
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
            slot_id: Some(slot.slot_id),
            stream: None,
            source_path: Some(path.to_string()),
            output: output.to_string(),
            status: "failed".to_string(),
            size_bytes: None,
            error: Some(error.to_string()),
        },
    }
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

fn prepare_active_output_path(runner_root: &Path, output: &str) -> Result<PathBuf> {
    let output_path = resolve_report_output_path(runner_root, output)?;
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create artifact directory {}", parent.display()))?;
    }
    fs::write(&output_path, b"").with_context(|| {
        format!(
            "failed to initialize artifact file {}",
            output_path.display()
        )
    })?;
    Ok(output_path)
}

fn failed_active_log_report(
    kind: &str,
    service: &str,
    slot_id: Option<usize>,
    output: &str,
    source_path: Option<String>,
    error: String,
) -> EnvironmentLogArtifactReport {
    EnvironmentLogArtifactReport {
        kind: kind.to_string(),
        service: service.to_string(),
        slot_id,
        stream: None,
        source_path,
        output: output.to_string(),
        status: "failed".to_string(),
        size_bytes: None,
        error: Some(error),
    }
}

fn resolve_string(context: &RuntimeContext, raw: &str) -> Result<String> {
    Ok(value_to_string(
        context.resolve_value(&Value::String(raw.to_string()))?,
    ))
}

fn resolve_live_log_specs_for_project(
    project: &LoadedProject,
    run_id: &str,
    slot_id: usize,
    report_slot_id: Option<usize>,
) -> Result<Vec<ResolvedLiveLogSpec>> {
    let context = build_render_context(project, run_id)?;
    let specs = project
        .environment
        .logs
        .iter()
        .map(|log| resolve_log_spec(log, &context))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|log| match log {
            ResolvedLogSpec::ComposeService {
                service, stream, ..
            } => Some(ResolvedLiveLogSpec::ComposeService {
                slot_id,
                report_slot_id,
                service,
                stream,
            }),
            ResolvedLogSpec::ContainerFile { service, path, .. } => {
                Some(ResolvedLiveLogSpec::ContainerFile {
                    slot_id,
                    report_slot_id,
                    service,
                    path,
                })
            }
            ResolvedLogSpec::RedisMonitor { .. } => None,
        })
        .collect::<Vec<_>>();
    Ok(specs)
}

fn resolve_active_log_specs_for_project(
    project: &LoadedProject,
    run_id: &str,
    slot_id: usize,
    report_slot_id: Option<usize>,
) -> Result<Vec<ResolvedActiveLogSpec>> {
    let context = build_render_context(project, run_id)?;
    let specs = project
        .environment
        .logs
        .iter()
        .map(|log| resolve_log_spec(log, &context))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|log| match log {
            ResolvedLogSpec::RedisMonitor { service, output } => Some(ResolvedActiveLogSpec {
                slot_id,
                report_slot_id,
                service,
                output,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    Ok(specs)
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

fn slot_container_id(slot: &ResolvedContainerSlot, service: &str) -> Result<String> {
    slot.container_ids
        .iter()
        .find(|(name, _)| name == service)
        .map(|(_, id)| id.clone())
        .with_context(|| {
            format!(
                "no container found for service `{service}` in slot `{}`",
                slot.slot_id
            )
        })
}

fn stream_label(stream: ComposeLogStream) -> &'static str {
    match stream {
        ComposeLogStream::Stdout => "stdout",
        ComposeLogStream::Stderr => "stderr",
        ComposeLogStream::Combined => "combined",
    }
}

fn live_log_color_for_service(service: &str, path: Option<&str>) -> LiveLogColor {
    let service = service.to_ascii_lowercase();
    let path = path.map(|value| value.to_ascii_lowercase());
    if service.contains("redis") {
        LiveLogColor::Redis
    } else if service.contains("mysql")
        || service.contains("mariadb")
        || path
            .as_deref()
            .map(|value| value.contains("general.log") || value.contains("mysql"))
            .unwrap_or(false)
    {
        LiveLogColor::Mysql
    } else if service.contains("app") {
        LiveLogColor::App
    } else {
        LiveLogColor::Default
    }
}

fn live_log_styler() -> &'static LiveLogStyler {
    static STYLER: OnceLock<LiveLogStyler> = OnceLock::new();
    STYLER.get_or_init(LiveLogStyler::detect)
}

fn emit_live_log_text(color: LiveLogColor, label: &str, text: &str) {
    let styler = live_log_styler();
    for line in text.lines() {
        let line = line.trim_end();
        if !line.is_empty() {
            eprintln!("{}", styler.format_line(color, label, line));
        }
    }
}

fn emit_live_log_error(label: &str, message: &str) {
    eprintln!("{}", live_log_styler().format_error(label, message));
}

fn shell_escape_single_quotes(value: &str) -> String {
    value.replace('\'', r#"'\''"#)
}

fn filter_redis_monitor_text(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed == "OK"
            || trimmed == "Error: Server closed the connection"
            || trimmed.contains(r#""CLIENT" "SETINFO""#)
        {
            continue;
        }
        lines.push(line.to_string());
    }
    if lines.is_empty() {
        String::new()
    } else {
        let mut filtered = lines.join("\n");
        if text.ends_with('\n') {
            filtered.push('\n');
        }
        filtered
    }
}

async fn docker_exec_stream(
    docker: Docker,
    container_id: String,
    command: Vec<String>,
) -> Result<impl futures_util::Stream<Item = Result<LogOutput, BollardError>>, String> {
    let exec = docker
        .create_exec(
            &container_id,
            CreateExecOptions {
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(command),
                ..Default::default()
            },
        )
        .await
        .map_err(|error| format!("failed to create exec session: {error}"))?;

    match docker
        .start_exec(&exec.id, None)
        .await
        .map_err(|error| format!("failed to start exec session: {error}"))?
    {
        StartExecResults::Attached { output, .. } => Ok(output),
        StartExecResults::Detached => Err("exec session detached unexpectedly".to_string()),
    }
}

async fn start_live_log_task(
    docker: Docker,
    container_id: String,
    spec: ResolvedLiveLogSpec,
) -> Result<LiveLogHandle> {
    let label = spec.display_label();
    let color = spec.color();
    let task_label = label.clone();
    let task = match spec {
        ResolvedLiveLogSpec::ComposeService { stream, .. } => {
            let stdout = stream != ComposeLogStream::Stderr;
            let stderr = stream != ComposeLogStream::Stdout;
            let mut stream = docker.logs::<String>(
                &container_id,
                Some(LogsOptions {
                    follow: true,
                    stdout,
                    stderr,
                    timestamps: true,
                    ..Default::default()
                }),
            );

            tokio::spawn(async move {
                while let Some(result) = stream.next().await {
                    match result {
                        Ok(LogOutput::StdOut { message } | LogOutput::StdErr { message }) => {
                            let text = String::from_utf8_lossy(&message);
                            emit_live_log_text(color, &task_label, text.as_ref());
                        }
                        Ok(_) => {}
                        Err(error) => {
                            emit_live_log_error(&task_label, &format!("log stream error: {error}"));
                            break;
                        }
                    }
                }
            })
        }
        ResolvedLiveLogSpec::ContainerFile { path, .. } => {
            let escaped = shell_escape_single_quotes(&path);
            let command = vec![
                "sh".to_string(),
                "-lc".to_string(),
                format!(
                    "while [ ! -f '{escaped}' ]; do sleep 1; done; offset=$(wc -c < '{escaped}' 2>/dev/null || echo 0); printf '%s\\n' '{LIVE_TAIL_READY_MARKER}'; exec tail -c +$((offset + 1)) -F '{escaped}'"
                ),
            ];
            let mut stream = docker_exec_stream(docker, container_id, command)
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
            let initial_text =
                await_live_tail_ready(&mut stream, &task_label, LIVE_TAIL_READY_MARKER).await?;

            tokio::spawn(async move {
                if !initial_text.is_empty() {
                    emit_live_log_text(color, &task_label, &initial_text);
                }
                while let Some(result) = stream.next().await {
                    match result {
                        Ok(LogOutput::StdOut { message } | LogOutput::StdErr { message }) => {
                            let text = String::from_utf8_lossy(&message);
                            emit_live_log_text(color, &task_label, text.as_ref());
                        }
                        Ok(_) => {}
                        Err(error) => {
                            emit_live_log_error(
                                &task_label,
                                &format!("file tail stream error: {error}"),
                            );
                            break;
                        }
                    }
                }
            })
        }
    };
    Ok(LiveLogHandle { label, task })
}

async fn stop_live_log_handles(handles: Vec<LiveLogHandle>) {
    for handle in handles {
        handle.task.abort();
        match handle.task.await {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) => {
                emit_live_log_error(&handle.label, &format!("live log task failed: {error}"));
            }
        }
    }
}

async fn start_redis_monitor_capture(
    docker: Docker,
    container_id: String,
    spec: ResolvedActiveLogSpec,
    output_path: PathBuf,
    mirror_to_stderr: bool,
) -> Result<ActiveLogHandle, String> {
    let label = spec.display_label();
    let color = spec.color();
    let task_label = label.clone();
    let task_output_path = output_path.clone();
    let mut stream = docker_exec_stream(
        docker,
        container_id,
        vec![
            "sh".to_string(),
            "-lc".to_string(),
            "command -v redis-cli >/dev/null 2>&1 || exit 127; until redis-cli --raw MONITOR; do sleep 1; done".to_string(),
        ],
    )
    .await?;
    let task = tokio::spawn(async move {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&task_output_path)
            .await
            .map_err(|error| {
                format!(
                    "failed to open monitor artifact {}: {error}",
                    task_output_path.display()
                )
            })?;

        while let Some(result) = stream.next().await {
            match result {
                Ok(LogOutput::StdOut { message } | LogOutput::StdErr { message }) => {
                    let text = String::from_utf8_lossy(&message);
                    let filtered = filter_redis_monitor_text(text.as_ref());
                    if filtered.is_empty() {
                        continue;
                    }
                    file.write_all(filtered.as_bytes())
                        .await
                        .map_err(|error| format!("failed to write monitor artifact: {error}"))?;
                    if mirror_to_stderr {
                        emit_live_log_text(color, &task_label, &filtered);
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    return Err(format!("redis monitor stream error: {error}"));
                }
            }
        }

        file.flush()
            .await
            .map_err(|error| format!("failed to flush monitor artifact: {error}"))?;
        Ok(())
    });

    Ok(ActiveLogHandle {
        kind: "redis_monitor".to_string(),
        service: spec.service,
        slot_id: spec.report_slot_id,
        source_path: None,
        output: spec.output,
        output_path,
        task,
    })
}

async fn await_live_tail_ready<S>(stream: &mut S, label: &str, marker: &str) -> Result<String>
where
    S: futures_util::Stream<Item = Result<LogOutput, BollardError>> + Unpin,
{
    timeout(Duration::from_secs(5), async {
        let mut buffered = String::new();
        loop {
            match stream.next().await {
                Some(Ok(LogOutput::StdOut { message } | LogOutput::StdErr { message })) => {
                    buffered.push_str(&String::from_utf8_lossy(&message));
                    if let Some(pending) = extract_live_tail_pending_text(&buffered, marker) {
                        return Ok(pending);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    bail!("file tail stream error before ready: {error}");
                }
                None => {
                    bail!("file tail stream ended before ready");
                }
            }
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for live tail to attach for {label}"))?
}

fn extract_live_tail_pending_text(buffered: &str, marker: &str) -> Option<String> {
    buffered.find(marker).map(|index| {
        let mut pending = &buffered[index + marker.len()..];
        if let Some(stripped) = pending.strip_prefix("\r\n") {
            pending = stripped;
        } else if let Some(stripped) = pending.strip_prefix('\n') {
            pending = stripped;
        }
        pending.to_string()
    })
}

async fn stop_active_log_handles(
    handles: Vec<ActiveLogHandle>,
) -> Vec<EnvironmentLogArtifactReport> {
    let mut reports = Vec::new();
    for handle in handles {
        let ActiveLogHandle {
            kind,
            service,
            slot_id,
            source_path,
            output,
            output_path,
            task,
            ..
        } = handle;
        let mut task = task;
        let task_result = match timeout(Duration::from_secs(2), &mut task).await {
            Ok(result) => match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error),
                Err(error) if error.is_cancelled() => None,
                Err(error) => Some(format!("active log task failed: {error}")),
            },
            Err(_) => {
                task.abort();
                match task.await {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error),
                    Err(error) if error.is_cancelled() => None,
                    Err(error) => Some(format!("active log task failed: {error}")),
                }
            }
        };

        let size_bytes = fs::metadata(&output_path)
            .ok()
            .map(|metadata| metadata.len());
        reports.push(EnvironmentLogArtifactReport {
            kind,
            service,
            slot_id,
            stream: None,
            source_path,
            output,
            status: if task_result.is_some() {
                "failed".to_string()
            } else {
                "passed".to_string()
            },
            size_bytes,
            error: task_result,
        });
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        EnvironmentLogSource, EnvironmentRuntimeConfig, MockServerConfig, ProjectConfig,
        ProjectDefaults, ProjectMetadata,
    };

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

    #[test]
    fn live_log_color_detects_mysql_and_redis_sources() {
        assert_eq!(
            live_log_color_for_service("mysql", Some("/var/lib/mysql/general.log")),
            LiveLogColor::Mysql
        );
        assert_eq!(
            live_log_color_for_service("redis", None),
            LiveLogColor::Redis
        );
        assert_eq!(live_log_color_for_service("app", None), LiveLogColor::App);
    }

    #[test]
    fn live_log_styler_keeps_plain_text_when_disabled() {
        let styler = LiveLogStyler { enabled: false };
        assert_eq!(
            styler.format_line(LiveLogColor::Mysql, "[env:mysql]", "SELECT 1"),
            "[env:mysql] SELECT 1"
        );
        assert_eq!(
            styler.format_error("[env:mysql]", "boom"),
            "[env:mysql] [error] boom"
        );
    }

    #[test]
    fn live_log_styler_colors_labels_and_source_text_when_enabled() {
        let styler = LiveLogStyler { enabled: true };
        assert_eq!(
            styler.format_line(LiveLogColor::Mysql, "[env:mysql]", "SELECT 1"),
            "\u{1b}[1;35m[env:mysql]\u{1b}[0m \u{1b}[35mSELECT 1\u{1b}[0m"
        );
        assert_eq!(
            styler.format_line(LiveLogColor::App, "[env:app]", "listening"),
            "\u{1b}[1;36m[env:app]\u{1b}[0m listening"
        );
    }

    fn test_project_with_container_runtime(service_env: IndexMap<String, String>) -> LoadedProject {
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
    fn apply_slot_endpoint_overrides_rewrites_runtime_service_env_urls() {
        let mut project = test_project_with_container_runtime(IndexMap::from([(
            "APP_BASE_URL".to_string(),
            "http://127.0.0.1:18080".to_string(),
        )]));
        let runtime = ResolvedRuntime::Containers(ResolvedContainersRuntime {
            services: project
                .environment
                .runtime
                .as_ref()
                .expect("runtime")
                .services
                .clone(),
            network_name_prefix: "sample".to_string(),
            cleanup: EnvironmentRuntimeCleanupPolicy::Always,
            slot_count: 2,
            slots: Vec::new(),
            slot_mock_rewrite: None,
        });
        let slot = EnvironmentSlot {
            slot_id: 1,
            port_mappings: HashMap::from([("app".to_string(), HashMap::from([(3000, 28080)]))]),
        };

        apply_slot_endpoint_overrides(&mut project, &runtime, &slot);

        let runtime = project
            .environment
            .runtime
            .as_ref()
            .expect("runtime should exist");
        assert_eq!(
            runtime.services[0]
                .environment
                .get("APP_BASE_URL")
                .expect("service env"),
            "http://127.0.0.1:28080"
        );
    }

    #[test]
    fn rewrite_service_environment_mock_urls_updates_local_targets_only() {
        let mut service_env = IndexMap::from([
            (
                "SMS_PROVIDER_BASE_URL".to_string(),
                "http://host.docker.internal:18081".to_string(),
            ),
            (
                "EXTERNAL_PROVIDER_BASE_URL".to_string(),
                "http://example.com:18081".to_string(),
            ),
        ]);

        rewrite_service_environment_mock_urls(
            &mut service_env,
            18081,
            "http://host.docker.internal:29001",
        );

        assert_eq!(
            service_env.get("SMS_PROVIDER_BASE_URL"),
            Some(&"http://host.docker.internal:29001".to_string())
        );
        assert_eq!(
            service_env.get("EXTERNAL_PROVIDER_BASE_URL"),
            Some(&"http://example.com:18081".to_string())
        );
    }

    #[test]
    fn live_log_specs_follow_declared_sources_without_fallbacks() {
        let project = test_project_with_docker_compose_runtime(vec![
            EnvironmentLogSource::ContainerFile {
                service: "mysql".to_string(),
                path: "/var/lib/mysql/general.log".to_string(),
                output: "env/mysql-query.log".to_string(),
            },
            EnvironmentLogSource::RedisMonitor {
                service: "redis".to_string(),
                output: "env/redis-monitor.log".to_string(),
            },
        ]);

        let specs =
            resolve_live_log_specs_for_project(&project, "run-1", 0, None).expect("live specs");

        assert_eq!(
            specs,
            vec![ResolvedLiveLogSpec::ContainerFile {
                slot_id: 0,
                report_slot_id: None,
                service: "mysql".to_string(),
                path: "/var/lib/mysql/general.log".to_string(),
            }]
        );
    }

    #[test]
    fn active_log_specs_capture_redis_monitor_sources() {
        let project = test_project_with_docker_compose_runtime(vec![
            EnvironmentLogSource::ContainerFile {
                service: "mysql".to_string(),
                path: "/var/lib/mysql/general.log".to_string(),
                output: "env/mysql-query.log".to_string(),
            },
            EnvironmentLogSource::RedisMonitor {
                service: "redis".to_string(),
                output: "env/redis-monitor.log".to_string(),
            },
        ]);

        let specs =
            resolve_active_log_specs_for_project(&project, "run-1", 0, None).expect("active specs");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].service, "redis");
        assert_eq!(specs[0].output, "env/redis-monitor.log");
        assert_eq!(specs[0].display_label(), "[env:redis:monitor]");
    }

    #[test]
    fn live_log_specs_keep_multiple_sources_for_same_service() {
        let project = test_project_with_docker_compose_runtime(vec![
            EnvironmentLogSource::ContainerFile {
                service: "mysql".to_string(),
                path: "/var/lib/mysql/general.log".to_string(),
                output: "env/mysql-query.log".to_string(),
            },
            EnvironmentLogSource::ComposeService {
                service: "mysql".to_string(),
                stream: ComposeLogStream::Stdout,
                output: "env/mysql.log".to_string(),
            },
            EnvironmentLogSource::ContainerFile {
                service: "mysql".to_string(),
                path: "/var/lib/mysql/slow.log".to_string(),
                output: "env/mysql-slow.log".to_string(),
            },
        ]);

        let specs =
            resolve_live_log_specs_for_project(&project, "run-1", 0, None).expect("live specs");

        assert_eq!(
            specs,
            vec![
                ResolvedLiveLogSpec::ContainerFile {
                    slot_id: 0,
                    report_slot_id: None,
                    service: "mysql".to_string(),
                    path: "/var/lib/mysql/general.log".to_string(),
                },
                ResolvedLiveLogSpec::ComposeService {
                    slot_id: 0,
                    report_slot_id: None,
                    service: "mysql".to_string(),
                    stream: ComposeLogStream::Stdout,
                },
                ResolvedLiveLogSpec::ContainerFile {
                    slot_id: 0,
                    report_slot_id: None,
                    service: "mysql".to_string(),
                    path: "/var/lib/mysql/slow.log".to_string(),
                }
            ]
        );
    }

    #[test]
    fn live_log_labels_include_slot_and_stream_context() {
        assert_eq!(
            ResolvedLiveLogSpec::ComposeService {
                slot_id: 0,
                report_slot_id: None,
                service: "redis".to_string(),
                stream: ComposeLogStream::Combined,
            }
            .display_label(),
            "[env:redis]"
        );
        assert_eq!(
            ResolvedLiveLogSpec::ComposeService {
                slot_id: 2,
                report_slot_id: Some(2),
                service: "mysql".to_string(),
                stream: ComposeLogStream::Stdout,
            }
            .display_label(),
            "[env:slot-2:mysql]:stdout"
        );
        assert_eq!(
            ResolvedLiveLogSpec::ContainerFile {
                slot_id: 1,
                report_slot_id: Some(1),
                service: "mysql".to_string(),
                path: "/var/lib/mysql/general.log".to_string(),
            }
            .display_label(),
            "[env:slot-1:mysql]:general.log"
        );
    }

    #[test]
    fn redis_monitor_filter_removes_connection_noise() {
        let filtered = filter_redis_monitor_text(
            "OK\n177 foo \"CLIENT\" \"SETINFO\" \"LIB-NAME\" \"redis-rs\"\n177 bar \"SETEX\" \"sms\" \"300\" \"1234\"\nError: Server closed the connection\n",
        );
        assert_eq!(filtered, "177 bar \"SETEX\" \"sms\" \"300\" \"1234\"\n");
    }

    fn test_project_with_docker_compose_runtime(logs: Vec<EnvironmentLogSource>) -> LoadedProject {
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
            environment_name: "docker".to_string(),
            environment: EnvironmentConfig {
                name: Some("docker".to_string()),
                base_url: "http://127.0.0.1:18080".to_string(),
                headers: Default::default(),
                variables: Default::default(),
                runtime: Some(EnvironmentRuntimeConfig {
                    kind: EnvironmentRuntimeKind::DockerCompose,
                    project_directory: ".".to_string(),
                    files: vec!["docker-compose.yml".to_string()],
                    project_name: Some("sample".to_string()),
                    up: vec!["-d".to_string()],
                    down: vec!["-v".to_string()],
                    cleanup: EnvironmentRuntimeCleanupPolicy::Always,
                    services: Vec::new(),
                    network_name: None,
                    parallel: None,
                }),
                readiness: Vec::new(),
                logs,
            },
            datasources: Default::default(),
            apis: Default::default(),
            cases: Vec::new(),
            workflows: Default::default(),
            mock_routes: Vec::new(),
        }
    }
}
