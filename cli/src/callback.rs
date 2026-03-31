use anyhow::{Context, Result, bail};
use chrono::Utc;
use indexmap::IndexMap;
use reqwest::Method;
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};
use url::Url;

use crate::config::{LoadedApi, LoadedProject};
use crate::dsl::RequestSpec;
use crate::runtime::{RuntimeContext, format_error_chain, value_to_string};

#[derive(Debug, Clone)]
pub struct RequestPreparationContext {
    pub(crate) apis: Arc<IndexMap<String, LoadedApi>>,
    pub(crate) environment_base_url: String,
    pub(crate) environment_headers: IndexMap<String, String>,
}

impl RequestPreparationContext {
    pub fn from_project(project: &LoadedProject) -> Self {
        Self {
            apis: Arc::new(project.apis.clone()),
            environment_base_url: project.environment.base_url.clone(),
            environment_headers: project.environment.headers.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum PreparedRequestBody {
    Text(String),
    Json(Value),
}

#[derive(Debug, Clone)]
pub struct PreparedRequest {
    pub api_id: String,
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<PreparedRequestBody>,
}

impl PreparedRequest {
    pub fn to_json(&self) -> Value {
        json!({
            "api": self.api_id,
            "method": self.method.as_str(),
            "url": self.url,
            "headers": self.headers.iter().map(|(key, value)| (key.clone(), Value::String(value.clone()))).collect::<serde_json::Map<_, _>>(),
            "body_kind": match &self.body {
                Some(PreparedRequestBody::Text(_)) => Value::String("text".to_string()),
                Some(PreparedRequestBody::Json(_)) => Value::String("json".to_string()),
                None => Value::String("none".to_string()),
            },
        })
    }
}

pub fn prepare_callback_request(
    request_context: &RequestPreparationContext,
    request: &RequestSpec,
    runtime: &RuntimeContext,
) -> Result<PreparedRequest> {
    let api_id = request
        .api
        .clone()
        .context("callback.request.api is required")?;
    prepare_request(request_context, &api_id, request, runtime)
}

pub fn prepare_case_request(
    request_context: &RequestPreparationContext,
    default_api_id: &str,
    request: &RequestSpec,
    runtime: &RuntimeContext,
) -> Result<PreparedRequest> {
    let api_id = request
        .api
        .clone()
        .unwrap_or_else(|| default_api_id.to_string());
    prepare_request(request_context, &api_id, request, runtime)
}

fn prepare_request(
    request_context: &RequestPreparationContext,
    api_id: &str,
    request: &RequestSpec,
    runtime: &RuntimeContext,
) -> Result<PreparedRequest> {
    let api = request_context
        .apis
        .get(api_id)
        .with_context(|| format!("unknown API `{api_id}`"))?;
    let method = Method::from_bytes(api.definition.method.as_bytes())?;
    let base_url = match &request.base_url {
        Some(base_url) => value_to_string(
            runtime
                .resolve_value(&Value::String(base_url.clone()))
                .context("failed to resolve request.base_url")?,
        ),
        None => api
            .definition
            .base_url
            .clone()
            .unwrap_or_else(|| request_context.environment_base_url.clone()),
    };
    let path = render_api_path(&api.definition.path, &request.path_params, runtime)?;
    let mut url = Url::parse(&format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        path.trim_start_matches('/')
    ))
    .with_context(|| format!("failed to build request URL for API `{api_id}`"))?;

    let mut query = api.definition.query.clone();
    query.extend(request.query.clone());
    if !query.is_empty() {
        let mut query_pairs = url.query_pairs_mut();
        for (key, value) in query {
            let resolved = runtime
                .resolve_value(&value)
                .with_context(|| format!("failed to resolve request.query.{key}"))?;
            query_pairs.append_pair(&key, &value_to_string(resolved));
        }
    }

    let mut headers = request_context.environment_headers.clone();
    headers.extend(api.definition.headers.clone());
    for (key, value) in &request.headers {
        let resolved = runtime
            .resolve_value(value)
            .with_context(|| format!("failed to resolve request.headers.{key}"))?;
        headers.insert(key.clone(), value_to_string(resolved));
    }

    let body = request
        .body
        .clone()
        .or_else(|| api.definition.body.clone())
        .map(|value| {
            runtime
                .resolve_value(&value)
                .context("failed to resolve request.body")
        })
        .transpose()?
        .map(|value| {
            if value.is_string() {
                PreparedRequestBody::Text(value.as_str().unwrap_or_default().to_string())
            } else {
                PreparedRequestBody::Json(value)
            }
        });

    Ok(PreparedRequest {
        api_id: api_id.to_string(),
        method,
        url: url.to_string(),
        headers: headers.into_iter().collect(),
        body,
    })
}

pub fn render_api_path(
    path: &str,
    path_params: &IndexMap<String, Value>,
    runtime: &RuntimeContext,
) -> Result<String> {
    let mut rendered = path.to_string();
    for (key, value) in path_params {
        let replacement = value_to_string(
            runtime
                .resolve_value(value)
                .with_context(|| format!("failed to resolve request.path_params.{key}"))?,
        );
        rendered = rendered.replace(&format!("{{{key}}}"), &replacement);
    }
    runtime
        .render_string(&rendered)
        .with_context(|| format!("failed to render request path `{path}`"))
}

#[derive(Debug, Clone, Serialize)]
pub struct CallbackReport {
    pub id: u64,
    pub source: String,
    pub api: String,
    pub method: String,
    pub url: String,
    pub after_ms: u64,
    pub scheduled_at: String,
    pub status: String,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct CallbackSummaryReport {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
}

impl CallbackSummaryReport {
    pub fn from_reports(reports: &[CallbackReport]) -> Self {
        Self {
            total: reports.len(),
            passed: reports
                .iter()
                .filter(|report| report.status == "passed")
                .count(),
            failed: reports
                .iter()
                .filter(|report| report.status == "failed")
                .count(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScheduledCallback {
    pub source: String,
    pub after_ms: u64,
    pub request: PreparedRequest,
}

#[derive(Debug, Clone)]
pub struct ScheduledCallbackInfo {
    pub id: u64,
    pub after_ms: u64,
    pub request: PreparedRequest,
}

#[derive(Clone)]
pub struct CallbackRuntime {
    inner: Arc<CallbackRuntimeInner>,
}

impl std::fmt::Debug for CallbackRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackRuntime").finish()
    }
}

struct CallbackRuntimeInner {
    client: reqwest::Client,
    next_id: AtomicU64,
    pending: Mutex<Vec<PendingCallback>>,
}

struct PendingCallback {
    meta: CallbackMeta,
    join_handle: JoinHandle<CallbackReport>,
}

#[derive(Debug, Clone)]
struct CallbackMeta {
    id: u64,
    source: String,
    api: String,
    method: String,
    url: String,
    after_ms: u64,
    scheduled_at: String,
}

impl CallbackRuntime {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            inner: Arc::new(CallbackRuntimeInner {
                client,
                next_id: AtomicU64::new(1),
                pending: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn schedule(&self, callback: ScheduledCallback) -> ScheduledCallbackInfo {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let scheduled_at = Utc::now().to_rfc3339();
        let meta = CallbackMeta {
            id,
            source: callback.source.clone(),
            api: callback.request.api_id.clone(),
            method: callback.request.method.as_str().to_string(),
            url: callback.request.url.clone(),
            after_ms: callback.after_ms,
            scheduled_at: scheduled_at.clone(),
        };
        let client = self.inner.client.clone();
        let request = callback.request.clone();
        let task_meta = meta.clone();
        let join_handle = tokio::spawn(async move {
            execute_callback(client, task_meta, callback.after_ms, request).await
        });

        self.inner
            .pending
            .lock()
            .expect("callback runtime mutex poisoned")
            .push(PendingCallback { meta, join_handle });

        ScheduledCallbackInfo {
            id,
            after_ms: callback.after_ms,
            request: callback.request,
        }
    }

    pub async fn flush(&self) -> Vec<CallbackReport> {
        let pending = {
            let mut guard = self
                .inner
                .pending
                .lock()
                .expect("callback runtime mutex poisoned");
            std::mem::take(&mut *guard)
        };

        let mut reports = Vec::with_capacity(pending.len());
        for pending_callback in pending {
            let PendingCallback { meta, join_handle } = pending_callback;
            match join_handle.await {
                Ok(report) => reports.push(report),
                Err(error) => reports.push(CallbackReport {
                    id: meta.id,
                    source: meta.source,
                    api: meta.api,
                    method: meta.method,
                    url: meta.url,
                    after_ms: meta.after_ms,
                    scheduled_at: meta.scheduled_at,
                    status: "failed".to_string(),
                    duration_ms: 0,
                    response_status: None,
                    error: Some(format!("callback task failed to join: {error}")),
                }),
            }
        }
        reports.sort_by_key(|report| report.id);
        reports
    }
}

async fn execute_callback(
    client: reqwest::Client,
    meta: CallbackMeta,
    after_ms: u64,
    request: PreparedRequest,
) -> CallbackReport {
    let started = Instant::now();
    if after_ms > 0 {
        sleep(Duration::from_millis(after_ms)).await;
    }

    let outcome = send_prepared_request(&client, &request).await;
    match outcome {
        Ok(response_status) => CallbackReport {
            id: meta.id,
            source: meta.source,
            api: meta.api,
            method: meta.method,
            url: meta.url,
            after_ms,
            scheduled_at: meta.scheduled_at,
            status: "passed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            response_status: Some(response_status),
            error: None,
        },
        Err(error) => CallbackReport {
            id: meta.id,
            source: meta.source,
            api: meta.api,
            method: meta.method,
            url: meta.url,
            after_ms,
            scheduled_at: meta.scheduled_at,
            status: "failed".to_string(),
            duration_ms: started.elapsed().as_millis(),
            response_status: None,
            error: Some(format_error_chain(&error)),
        },
    }
}

async fn send_prepared_request(client: &reqwest::Client, request: &PreparedRequest) -> Result<u16> {
    let mut builder = client.request(request.method.clone(), &request.url);
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
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        if body.is_empty() {
            bail!("callback returned HTTP {}", status.as_u16());
        }
        bail!("callback returned HTTP {}: {}", status.as_u16(), body);
    }
    Ok(status.as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    async fn start_callback_target() -> (
        SocketAddr,
        tokio::task::JoinHandle<()>,
        Arc<Mutex<Vec<Value>>>,
    ) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let state = received.clone();
        let app = Router::new().route(
            "/callbacks/payment",
            post(move |Json(payload): Json<Value>| {
                let state = state.clone();
                async move {
                    state.lock().expect("received lock").push(payload);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind target");
        let addr = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve callback target");
        });
        (addr, join, received)
    }

    #[tokio::test]
    async fn callback_runtime_delivers_scheduled_requests() {
        let (addr, join, received) = start_callback_target().await;
        let runtime = CallbackRuntime::new(reqwest::Client::new());
        let info = runtime.schedule(ScheduledCallback {
            source: "case:callback/direct-payment-success".to_string(),
            after_ms: 10,
            request: PreparedRequest {
                api_id: "callback/payment/status".to_string(),
                method: Method::POST,
                url: format!("http://{addr}/callbacks/payment"),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: Some(PreparedRequestBody::Json(json!({
                    "order_no": "order-1",
                    "status": "SUCCESS"
                }))),
            },
        });
        assert_eq!(info.id, 1);

        let reports = runtime.flush().await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].status, "passed");
        assert_eq!(reports[0].response_status, Some(204));

        let values = received.lock().expect("received lock");
        assert_eq!(
            values.as_slice(),
            &[json!({
                "order_no": "order-1",
                "status": "SUCCESS"
            })]
        );

        join.abort();
    }
}
