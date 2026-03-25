use anyhow::{Context, Result, bail};
use bollard::Docker;
use bollard::container::{
    Config as ContainerConfig, CreateContainerOptions, LogOutput, LogsOptions,
    RemoveContainerOptions, StartContainerOptions, InspectContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::network::{CreateNetworkOptions, InspectNetworkOptions};
use chrono::Utc;
use flate2::Compression;
use flate2::write::GzEncoder;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use tokio::process::Command;
use tokio::time::{Duration, Instant, sleep};
use url::Url;

use crate::config::{
    ComposeLogStream, ContainerBuildConfig, ContainerServiceConfig, ContainerWaitFor,
    DatasourceDefinition, EnvironmentConfig, EnvironmentLogSource, EnvironmentReadinessCheck,
    EnvironmentRuntimeCleanupPolicy, EnvironmentRuntimeKind, LoadedProject,
    environment_context_value,
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
}

#[derive(Debug, Clone)]
struct ResolvedContainerSlot {
    slot_id: usize,
    network_name: String,
    container_ids: Vec<(String, String)>,
    port_mappings: HashMap<String, HashMap<u16, u16>>,
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
    pub fn new(project: &LoadedProject, requested_slots: usize) -> Result<Self> {
        let run_id = build_run_id(&project.project.project.name);
        let render_context = build_render_context(project, &run_id)?;
        let runtime = project
            .environment
            .runtime
            .as_ref()
            .map(|runtime_config| {
                resolve_runtime(project, runtime_config, &render_context, &run_id, requested_slots)
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
                            service,
                            stream,
                            ..
                        },
                        ResolvedRuntime::DockerCompose(dc),
                    ) => {
                        collect_compose_service_logs(&self.runner_root, dc, service, *stream, &output)
                            .await
                    }
                    (
                        ResolvedLogSpec::ContainerFile { service, path, .. },
                        ResolvedRuntime::DockerCompose(dc),
                    ) => collect_container_file(&self.runner_root, dc, service, path, &output).await,
                    (
                        ResolvedLogSpec::ComposeService { service, .. },
                        ResolvedRuntime::Containers(ct),
                    ) => match ct.slots.iter().find(|slot| slot.slot_id == slot_id) {
                        Some(slot) => {
                            collect_container_logs_bollard(&self.runner_root, slot, service, &output)
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
                            collect_container_file_bollard(&self.runner_root, slot, service, path, &output)
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
            Self::ComposeService { output, .. } | Self::ContainerFile { output, .. } => output,
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
                project.project.project.name,
                run_id
            ))
        });

    Ok(ResolvedContainersRuntime {
        services: runtime.services.clone(),
        network_name_prefix,
        cleanup: runtime.cleanup,
        slot_count: requested_slots.max(1),
        slots: Vec::new(),
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

fn is_rewritable_host(host: &str) -> bool {
    matches!(
        host,
        "127.0.0.1" | "localhost" | "::1" | "0.0.0.0" | "host.docker.internal"
    )
}

fn render_url_without_root_slash(url: &Url) -> String {
    let rendered = url.to_string();
    if url.path() == "/" && url.query().is_none() && url.fragment().is_none() {
        rendered.trim_end_matches('/').to_string()
    } else {
        rendered
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
                    error: Some(format_command_failure("docker compose logs", &command_output)),
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
            let container_port = parts[0].parse::<u16>()
                .with_context(|| format!("invalid port `{spec}`"))?;
            Ok((container_port, None))
        }
        2 => {
            let host_port = parts[0].parse::<u16>()
                .with_context(|| format!("invalid host port in `{spec}`"))?;
            let container_port = parts[1].parse::<u16>()
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
        // Pull pre-built image
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
    let tar_bytes = create_build_context_tar(&context_path)
        .with_context(|| format!("failed to create tar for build context `{}`", context_path.display()))?;

    let mut build_opts = bollard::image::BuildImageOptions {
        t: tag.to_string(),
        rm: true,
        ..Default::default()
    };
    if let Some(dockerfile) = &build_config.dockerfile {
        build_opts.dockerfile = dockerfile.clone();
    }

    let mut build_stream = docker.build_image(
        build_opts,
        None,
        Some(tar_bytes.into()),
    );

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
    enc.finish().with_context(|| "failed to compress build context tar")
}

/// Start all containers for the containers runtime.
async fn start_containers(
    runtime: &ResolvedContainersRuntime,
    runner_root: &Path,
) -> Result<Vec<ResolvedContainerSlot>> {
    let docker = connect_docker()?;
    let mut slots = Vec::new();

    for slot_id in 0..runtime.slot_count {
        let network_name = slot_network_name(&runtime.network_name_prefix, slot_id, runtime.slot_count);
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

            let env_vars: Vec<String> = service
                .environment
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
        } => {
            wait_for_log_message(docker, container_id, service_name, pattern, *timeout_ms).await
        }
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
            wait_for_http(service_name, &url, *expect_status, *timeout_ms, *interval_ms).await
        }
    }
}

async fn resolve_host_port(docker: &Docker, container_id: &str, container_port: u16) -> Result<u16> {
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
        .with_context(|| format!("no host port mapping found for container port {container_port}"))?;

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
                bail!(
                    "error reading logs from container `{service_name}`: {error}"
                );
            }
            Ok(None) => {
                bail!(
                    "container `{service_name}` log stream ended before matching `{pattern}`"
                );
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

    let container_id = match slot
        .container_ids
        .iter()
        .find(|(name, _)| name == service)
        .map(|(_, id)| id.clone())
    {
        Some(id) => id,
        None => {
            return EnvironmentLogArtifactReport {
                kind: "containers".to_string(),
                service: service.to_string(),
                slot_id: Some(slot.slot_id),
                stream: Some("combined".to_string()),
                source_path: None,
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(format!("no container found for service `{service}`")),
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

    let container_id = match slot
        .container_ids
        .iter()
        .find(|(name, _)| name == service)
        .map(|(_, id)| id.clone())
    {
        Some(id) => id,
        None => {
            return EnvironmentLogArtifactReport {
                kind: "container_file".to_string(),
                service: service.to_string(),
                slot_id: Some(slot.slot_id),
                stream: None,
                source_path: Some(path.to_string()),
                output: output.to_string(),
                status: "failed".to_string(),
                size_bytes: None,
                error: Some(format!("no container found for service `{service}`")),
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
        &[
            "cp".to_string(),
            source,
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
