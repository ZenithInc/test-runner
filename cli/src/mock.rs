use anyhow::{Context, Result, bail};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri};
use axum::routing::any;
use axum::{Router, response::IntoResponse};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use url::form_urlencoded;

use crate::callback::{
    CallbackRuntime, RequestPreparationContext, ScheduledCallback, prepare_callback_request,
};
use crate::config::{
    LoadedProject, MockResponseDefinition, MockRouteDefinition, environment_context_value,
    load_data_tree,
};
use crate::dsl::{Assertion, CallbackStep, ConditionalStep, Step};
use crate::runtime::{RuntimeContext, assertions_match, value_to_string};

#[derive(Debug)]
pub struct MockServerHandle {
    pub base_url: String,
    shutdown: Option<oneshot::Sender<()>>,
    join_handle: JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct MockRoute {
    method: Method,
    path: String,
    priority: i32,
    when: Vec<Assertion>,
    extract: IndexMap<String, String>,
    steps: Vec<Step>,
    response: MockResponseDefinition,
}

#[derive(Debug, Clone)]
struct MockState {
    routes: Arc<Vec<MockRoute>>,
    base_root: Arc<Map<String, Value>>,
    runner_root: Arc<PathBuf>,
    request_context: RequestPreparationContext,
    callback_runtime: CallbackRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedMockResponse {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: String,
}

pub async fn start(
    project: &LoadedProject,
    request_context: RequestPreparationContext,
    callback_runtime: CallbackRuntime,
) -> Result<MockServerHandle> {
    let state = MockState {
        routes: Arc::new(load_routes(project)?),
        base_root: Arc::new(build_base_root(project)?),
        runner_root: Arc::new(project.runner_root.clone()),
        request_context,
        callback_runtime,
    };

    let address = format!(
        "{}:{}",
        project.project.mock.host, project.project.mock.port
    );
    let socket_addr: SocketAddr = address.parse()?;
    let listener = TcpListener::bind(socket_addr)
        .await
        .with_context(|| format!("failed to bind mock server on {address}"))?;
    let local_addr = listener.local_addr()?;
    let advertised_host = if local_addr.ip().is_unspecified() {
        "127.0.0.1".to_string()
    } else {
        local_addr.ip().to_string()
    };

    let app = Router::new()
        .fallback(any(handle_request))
        .with_state(state);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let join_handle = tokio::spawn(async move {
        let server = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });

        if let Err(error) = server.await {
            eprintln!("mock server stopped with error: {error}");
        }
    });

    Ok(MockServerHandle {
        base_url: format!("http://{advertised_host}:{}", local_addr.port()),
        shutdown: Some(shutdown_tx),
        join_handle,
    })
}

impl MockServerHandle {
    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = self.join_handle.await;
    }
}

async fn handle_request(
    State(state): State<MockState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    match resolve_request(&state, method, &uri, &headers, &body) {
        Ok(Some(rendered)) => build_response(rendered).unwrap_or_else(internal_error_response),
        Ok(None) => not_found_response(),
        Err(error) => internal_error_response(error),
    }
}

fn resolve_request(
    state: &MockState,
    method: Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Option<RenderedMockResponse>> {
    let request = build_request_value(&method, uri, headers, body)?;

    for route in state.routes.iter() {
        if route.method != method || route.path != uri.path() {
            continue;
        }

        let mut context = RuntimeContext::new((*state.base_root).clone())?;
        context.set_root_value("request", request.clone());
        context.set_root_value(
            "route",
            json!({
                "method": route.method.as_str(),
                "path": route.path,
                "priority": route.priority,
            }),
        );

        if !assertions_match(&route.when, &context)? {
            continue;
        }

        context.apply_extracts(&route.extract)?;
        execute_mock_steps(
            &route.steps,
            &mut context,
            &state.request_context,
            &state.callback_runtime,
            &format!("mock:{} {}", route.method.as_str(), route.path),
        )?;
        return Ok(Some(render_route_response(
            &state.runner_root,
            &route.response,
            &context,
        )?));
    }

    Ok(None)
}

fn execute_mock_steps(
    steps: &[Step],
    context: &mut RuntimeContext,
    request_context: &RequestPreparationContext,
    callback_runtime: &CallbackRuntime,
    source: &str,
) -> Result<()> {
    for step in steps {
        match step {
            Step::Set { values } => {
                for (key, value) in values {
                    let resolved = context.resolve_value(value)?;
                    context.set_var(key, resolved);
                }
            }
            Step::Callback(step) => {
                schedule_mock_callback(step, context, request_context, callback_runtime, source)?;
            }
            Step::Conditional(step) => {
                execute_conditional_step(step, context, request_context, callback_runtime, source)?
            }
            _ => bail!("mock route steps currently support only `set`, `callback` and `if`"),
        }
    }
    Ok(())
}

fn execute_conditional_step(
    step: &ConditionalStep,
    context: &mut RuntimeContext,
    request_context: &RequestPreparationContext,
    callback_runtime: &CallbackRuntime,
    source: &str,
) -> Result<()> {
    let branch = if context.evaluate_condition(&step.condition)? {
        &step.then_steps
    } else {
        &step.else_steps
    };
    execute_mock_steps(branch, context, request_context, callback_runtime, source)
}

fn schedule_mock_callback(
    step: &CallbackStep,
    context: &mut RuntimeContext,
    request_context: &RequestPreparationContext,
    callback_runtime: &CallbackRuntime,
    source: &str,
) -> Result<()> {
    let request = prepare_callback_request(request_context, &step.request, context)?;
    callback_runtime.schedule(ScheduledCallback {
        source: source.to_string(),
        after_ms: step.after_ms,
        request,
    });
    Ok(())
}

fn build_base_root(project: &LoadedProject) -> Result<Map<String, Value>> {
    let mut root = Map::new();
    root.insert(
        "project".to_string(),
        json!({
            "name": project.project.project.name,
        }),
    );
    root.insert(
        "env".to_string(),
        environment_context_value(&project.environment_name, &project.environment)?,
    );
    root.insert(
        "data".to_string(),
        load_data_tree(&project.runner_root.join("data"))?,
    );
    Ok(root)
}

fn load_routes(project: &LoadedProject) -> Result<Vec<MockRoute>> {
    let mut routes = Vec::new();
    for definition in &project.mock_routes {
        routes.push(MockRoute {
            method: Method::from_bytes(definition.method.as_bytes())?,
            path: definition.path.clone(),
            priority: definition.priority,
            when: definition.when.clone(),
            extract: definition.extract.clone(),
            steps: definition.steps.clone(),
            response: effective_response(definition),
        });
    }
    Ok(routes)
}

fn effective_response(definition: &MockRouteDefinition) -> MockResponseDefinition {
    match &definition.respond {
        Some(response) => response.clone(),
        None => MockResponseDefinition {
            status: Some(json!(definition.status)),
            headers: definition
                .headers
                .iter()
                .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                .collect(),
            body: definition.body.clone(),
            body_file: definition.body_file.clone(),
        },
    }
}

fn render_route_response(
    runner_root: &Path,
    response: &MockResponseDefinition,
    context: &RuntimeContext,
) -> Result<RenderedMockResponse> {
    Ok(RenderedMockResponse {
        status: render_status(response, context)?,
        headers: render_headers(response, context)?,
        body: render_body(runner_root, response, context)?,
    })
}

fn render_status(
    response: &MockResponseDefinition,
    context: &RuntimeContext,
) -> Result<StatusCode> {
    let raw = response.status.clone().unwrap_or_else(|| json!(200));
    let resolved = context.resolve_value(&raw)?;
    let code = match resolved {
        Value::Number(number) => number
            .as_u64()
            .context("mock response status must be a positive integer")?
            as u16,
        Value::String(text) => text
            .parse::<u16>()
            .with_context(|| format!("invalid mock response status `{text}`"))?,
        other => bail!(
            "mock response status must resolve to a number or string, got {}",
            serde_json::to_string(&other)?
        ),
    };
    Ok(StatusCode::from_u16(code)?)
}

fn render_headers(
    response: &MockResponseDefinition,
    context: &RuntimeContext,
) -> Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    for (key, value) in &response.headers {
        headers.push((key.clone(), value_to_string(context.resolve_value(value)?)));
    }
    Ok(headers)
}

fn render_body(
    runner_root: &Path,
    response: &MockResponseDefinition,
    context: &RuntimeContext,
) -> Result<String> {
    if let Some(path) = &response.body_file {
        let path = context.render_string(path)?;
        let body = fs::read_to_string(runner_root.join(&path))
            .with_context(|| format!("failed to read mock body file {path}"))?;
        return context.render_string(&body);
    }

    match &response.body {
        Some(body) => {
            let resolved = context.resolve_value(body)?;
            if resolved.is_string() {
                Ok(value_to_string(resolved))
            } else {
                Ok(serde_json::to_string(&resolved)?)
            }
        }
        None => Ok(String::new()),
    }
}

fn build_response(rendered: RenderedMockResponse) -> Result<Response<Body>> {
    let mut response = Response::builder().status(rendered.status);
    for (key, value) in rendered.headers {
        let header_name = HeaderName::try_from(key.as_str())
            .with_context(|| format!("invalid mock response header name `{key}`"))?;
        let header_value = HeaderValue::from_str(&value)
            .with_context(|| format!("invalid mock response header value for `{key}`"))?;
        response = response.header(header_name, header_value);
    }
    response
        .body(Body::from(rendered.body))
        .context("failed to construct mock response")
}

fn build_request_value(
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Value> {
    let mut query = Map::new();
    if let Some(raw_query) = uri.query() {
        for (key, value) in form_urlencoded::parse(raw_query.as_bytes()) {
            push_json_value(
                &mut query,
                key.into_owned(),
                Value::String(value.into_owned()),
            );
        }
    }

    let mut request_headers = Map::new();
    for (name, value) in headers {
        let value = Value::String(
            value
                .to_str()
                .context("request contains a non-UTF-8 header value")?
                .to_string(),
        );
        push_json_value(
            &mut request_headers,
            name.as_str().to_ascii_lowercase(),
            value,
        );
    }

    let body_text = if body.is_empty() {
        Value::Null
    } else {
        Value::String(String::from_utf8_lossy(body).into_owned())
    };
    let json_body = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);

    Ok(json!({
        "method": method.as_str(),
        "path": uri.path(),
        "query": Value::Object(query),
        "headers": Value::Object(request_headers),
        "body": body_text,
        "json": json_body,
    }))
}

fn push_json_value(root: &mut Map<String, Value>, key: String, value: Value) {
    match root.remove(&key) {
        Some(Value::Array(mut items)) => {
            items.push(value);
            root.insert(key, Value::Array(items));
        }
        Some(existing) => {
            root.insert(key, Value::Array(vec![existing, value]));
        }
        None => {
            root.insert(key, value);
        }
    }
}

fn not_found_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("mock route not found"))
        .unwrap_or_else(|_| Response::new(Body::from("mock route not found")))
}

fn internal_error_response(error: anyhow::Error) -> Response<Body> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from(format!("mock route execution failed: {error}")))
        .unwrap_or_else(|_| Response::new(Body::from("mock route execution failed")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::{Assertion, AssertionKind};
    use tempfile::tempdir;

    fn test_state(routes: Vec<MockRoute>, runner_root: PathBuf) -> MockState {
        MockState {
            routes: Arc::new(routes),
            base_root: Arc::new(Map::new()),
            runner_root: Arc::new(runner_root),
            request_context: RequestPreparationContext {
                apis: Arc::new(IndexMap::new()),
                environment_base_url: "http://127.0.0.1:3000".to_string(),
                environment_headers: IndexMap::new(),
            },
            callback_runtime: CallbackRuntime::new(reqwest::Client::new()),
        }
    }

    fn legacy_route() -> MockRoute {
        MockRoute {
            method: Method::POST,
            path: "/sms/send".to_string(),
            priority: 0,
            when: Vec::new(),
            extract: IndexMap::new(),
            steps: Vec::new(),
            response: MockResponseDefinition {
                status: Some(json!(200)),
                headers: IndexMap::from([(
                    "content-type".to_string(),
                    Value::String("application/json".to_string()),
                )]),
                body: Some(json!({
                    "accepted": true,
                    "provider": "mock-sms"
                })),
                body_file: None,
            },
        }
    }

    #[test]
    fn legacy_mock_route_renders_static_response() {
        let temp = tempdir().expect("tempdir");
        let state = test_state(vec![legacy_route()], temp.path().to_path_buf());
        let response = resolve_request(
            &state,
            Method::POST,
            &"/sms/send".parse::<Uri>().expect("uri"),
            &HeaderMap::new(),
            br#"{"phone":"13800000000"}"#,
        )
        .expect("response")
        .expect("matched route");

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.headers[0],
            ("content-type".to_string(), "application/json".to_string())
        );
        assert_eq!(response.body, r#"{"accepted":true,"provider":"mock-sms"}"#);
    }

    #[test]
    fn dynamic_mock_route_can_match_extract_and_render_response() {
        let temp = tempdir().expect("tempdir");
        let route = MockRoute {
            method: Method::POST,
            path: "/sms/send".to_string(),
            priority: 10,
            when: vec![Assertion {
                kind: AssertionKind::Eq,
                args: vec![json!("request.json.phone"), json!("13800000000")],
            }],
            extract: IndexMap::from([("phone".to_string(), "request.json.phone".to_string())]),
            steps: Vec::new(),
            response: MockResponseDefinition {
                status: Some(json!(200)),
                headers: IndexMap::from([(
                    "x-mock-phone".to_string(),
                    Value::String("{{ vars.phone }}".to_string()),
                )]),
                body: Some(json!({
                    "accepted": true,
                    "phone": "{{ vars.phone }}",
                    "provider": "mock-sms"
                })),
                body_file: None,
            },
        };
        let state = test_state(vec![route], temp.path().to_path_buf());
        let response = resolve_request(
            &state,
            Method::POST,
            &"/sms/send?channel=login".parse::<Uri>().expect("uri"),
            &HeaderMap::new(),
            br#"{"phone":"13800000000","message":"Your verification code is 123456"}"#,
        )
        .expect("response")
        .expect("matched route");

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.headers[0],
            ("x-mock-phone".to_string(), "13800000000".to_string())
        );
        assert_eq!(
            response.body,
            r#"{"accepted":true,"phone":"13800000000","provider":"mock-sms"}"#
        );
    }

    #[test]
    fn dynamic_mock_route_supports_if_steps_and_body_files() {
        let temp = tempdir().expect("tempdir");
        fs::write(
            temp.path().join("dynamic-response.json"),
            "{\n  \"provider\": \"mock-sms\",\n  \"request_id\": \"{{ vars.request_id }}\"\n}\n",
        )
        .expect("write response fixture");

        let mut then_values = IndexMap::new();
        then_values.insert(
            "request_id".to_string(),
            Value::String("vip-request".to_string()),
        );
        let mut else_values = IndexMap::new();
        else_values.insert(
            "request_id".to_string(),
            Value::String("standard-request".to_string()),
        );

        let route = MockRoute {
            method: Method::POST,
            path: "/sms/send".to_string(),
            priority: 0,
            when: Vec::new(),
            extract: IndexMap::new(),
            steps: vec![Step::Conditional(ConditionalStep {
                condition: "${request.json.phone == '13800000000'}".to_string(),
                then_steps: vec![Step::Set {
                    values: then_values,
                }],
                else_steps: vec![Step::Set {
                    values: else_values,
                }],
            })],
            response: MockResponseDefinition {
                status: Some(json!(201)),
                headers: IndexMap::new(),
                body: None,
                body_file: Some("dynamic-response.json".to_string()),
            },
        };
        let state = test_state(vec![route], temp.path().to_path_buf());
        let response = resolve_request(
            &state,
            Method::POST,
            &"/sms/send".parse::<Uri>().expect("uri"),
            &HeaderMap::new(),
            br#"{"phone":"13800000000"}"#,
        )
        .expect("response")
        .expect("matched route");

        assert_eq!(response.status, StatusCode::CREATED);
        assert_eq!(
            response.body,
            "{\n  \"provider\": \"mock-sms\",\n  \"request_id\": \"vip-request\"\n}\n"
        );
    }

    #[tokio::test]
    async fn dynamic_mock_route_can_schedule_callbacks() {
        use axum::{Json, Router, routing::post};
        use std::sync::Mutex;

        let temp = tempdir().expect("tempdir");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind callback target");
        let addr = listener.local_addr().expect("local addr");
        let received = Arc::new(Mutex::new(Vec::<Value>::new()));
        let state = received.clone();
        let server = tokio::spawn(async move {
            let app = Router::new().route(
                "/callbacks/payment",
                post(move |Json(payload): Json<Value>| {
                    let state = state.clone();
                    async move {
                        state.lock().expect("received lock").push(payload);
                        StatusCode::NO_CONTENT
                    }
                }),
            );
            axum::serve(listener, app).await.expect("serve callback target");
        });

        let callback_runtime = CallbackRuntime::new(reqwest::Client::new());
        let request_context = RequestPreparationContext {
            apis: Arc::new(IndexMap::from([(
                "callback/payment/status".to_string(),
                crate::config::LoadedApi {
                    id: "callback/payment/status".to_string(),
                    relative_path: PathBuf::from("apis/callback/payment/status.yaml"),
                    definition: crate::config::ApiDefinition {
                        name: "Payment callback".to_string(),
                        method: "POST".to_string(),
                        path: "/callbacks/payment".to_string(),
                        base_url: Some(format!("http://{addr}")),
                        headers: IndexMap::from([(
                            "content-type".to_string(),
                            "application/json".to_string(),
                        )]),
                        query: IndexMap::new(),
                        body: None,
                        timeout_ms: None,
                    },
                },
            )])),
            environment_base_url: format!("http://{addr}"),
            environment_headers: IndexMap::new(),
        };
        let route = MockRoute {
            method: Method::POST,
            path: "/payments/create".to_string(),
            priority: 0,
            when: Vec::new(),
            extract: IndexMap::from([("order_no".to_string(), "request.json.order_no".to_string())]),
            steps: vec![Step::Callback(CallbackStep {
                after_ms: 10,
                request: crate::dsl::RequestSpec {
                    api: Some("callback/payment/status".to_string()),
                    base_url: None,
                    path_params: IndexMap::new(),
                    query: IndexMap::new(),
                    headers: IndexMap::new(),
                    body: Some(json!({
                        "order_no": "{{ vars.order_no }}",
                        "status": "SUCCESS"
                    })),
                },
            })],
            response: MockResponseDefinition {
                status: Some(json!(202)),
                headers: IndexMap::new(),
                body: Some(json!({ "accepted": true })),
                body_file: None,
            },
        };
        let state = MockState {
            routes: Arc::new(vec![route]),
            base_root: Arc::new(Map::new()),
            runner_root: Arc::new(temp.path().to_path_buf()),
            request_context,
            callback_runtime: callback_runtime.clone(),
        };

        let response = resolve_request(
            &state,
            Method::POST,
            &"/payments/create".parse::<Uri>().expect("uri"),
            &HeaderMap::new(),
            br#"{"order_no":"order-42"}"#,
        )
        .expect("response")
        .expect("matched route");

        assert_eq!(response.status, StatusCode::ACCEPTED);
        let reports = callback_runtime.flush().await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].status, "passed");
        assert_eq!(
            received.lock().expect("received lock").as_slice(),
            &[json!({
                "order_no": "order-42",
                "status": "SUCCESS"
            })]
        );

        server.abort();
    }
}
