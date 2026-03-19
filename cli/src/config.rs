use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::dsl::{Assertion, CaseFile, ConditionalStep, Step};
use crate::workflow::{WorkflowFile, validate_workflow_definition};

pub const TESTRUNNER_DIR: &str = ".testrunner";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    pub project: ProjectMetadata,
    #[serde(default)]
    pub defaults: ProjectDefaults,
    #[serde(default)]
    pub mock: MockServerConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectMetadata {
    #[serde(default = "default_project_name")]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDefaults {
    #[serde(default = "default_env_name")]
    pub env: String,
    #[serde(default = "default_execution_mode")]
    pub execution_mode: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for ProjectDefaults {
    fn default() -> Self {
        Self {
            env: default_env_name(),
            execution_mode: default_execution_mode(),
            timeout_ms: default_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockServerConfig {
    #[serde(default = "default_mock_enabled")]
    pub enabled: bool,
    #[serde(default = "default_mock_host")]
    pub host: String,
    #[serde(default = "default_mock_port")]
    pub port: u16,
}

impl Default for MockServerConfig {
    fn default() -> Self {
        Self {
            enabled: default_mock_enabled(),
            host: default_mock_host(),
            port: default_mock_port(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    #[serde(default)]
    pub variables: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasourceCatalog {
    #[serde(default)]
    pub datasources: IndexMap<String, DatasourceDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DatasourceDefinition {
    Mysql(SqlDatasource),
    Postgres(SqlDatasource),
    Redis(RedisDatasource),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlDatasource {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisDatasource {
    pub url: String,
    #[serde(default)]
    pub key_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiDefinition {
    pub name: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    #[serde(default)]
    pub query: IndexMap<String, Value>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MockRouteDefinition {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub when: Vec<Assertion>,
    #[serde(default)]
    pub extract: IndexMap<String, String>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub respond: Option<MockResponseDefinition>,
    #[serde(default = "default_http_status")]
    pub status: u16,
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub body_file: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MockResponseDefinition {
    #[serde(default)]
    pub status: Option<Value>,
    #[serde(default)]
    pub headers: IndexMap<String, Value>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub body_file: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedApi {
    pub id: String,
    pub relative_path: PathBuf,
    pub definition: ApiDefinition,
}

#[derive(Debug, Clone)]
pub struct LoadedCase {
    pub id: String,
    pub relative_path: PathBuf,
    pub definition: CaseFile,
}

#[derive(Debug, Clone)]
pub struct LoadedWorkflow {
    pub id: String,
    pub relative_path: PathBuf,
    pub definition: WorkflowFile,
}

#[derive(Debug, Clone)]
pub struct LoadedProject {
    pub root: PathBuf,
    pub runner_root: PathBuf,
    pub project: ProjectConfig,
    pub environment_name: String,
    pub environment: EnvironmentConfig,
    pub datasources: IndexMap<String, DatasourceDefinition>,
    pub apis: IndexMap<String, LoadedApi>,
    pub cases: Vec<LoadedCase>,
    pub workflows: IndexMap<String, LoadedWorkflow>,
    pub mock_routes: Vec<MockRouteDefinition>,
}

pub fn load_project(root: &Path, env_override: Option<&str>) -> Result<LoadedProject> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let runner_root = root.join(TESTRUNNER_DIR);

    if !runner_root.exists() {
        bail!("{} does not exist under {}", TESTRUNNER_DIR, root.display());
    }

    let project: ProjectConfig = read_yaml(runner_root.join("project.yaml"))?;
    let environment_name = env_override
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| project.defaults.env.clone());
    let environment_path = runner_root
        .join("env")
        .join(format!("{environment_name}.yaml"));
    let mut environment: EnvironmentConfig = read_yaml(&environment_path).with_context(|| {
        format!(
            "failed to load environment file {}",
            environment_path.display()
        )
    })?;
    environment.name = Some(environment_name.clone());

    let datasources = load_datasources(&runner_root.join("datasources"))?;
    let apis = load_apis(&runner_root.join("apis"))?;
    let cases = load_cases(&runner_root.join("cases"))?;
    let workflows = load_workflows(&runner_root.join("workflows"))?;
    let mock_routes = load_mock_routes(&runner_root.join("mocks").join("routes"))?;

    if apis.is_empty() {
        bail!(
            "no API definitions were found under {}",
            runner_root.join("apis").display()
        );
    }
    if cases.is_empty() {
        bail!(
            "no test cases were found under {}",
            runner_root.join("cases").display()
        );
    }

    Ok(LoadedProject {
        root,
        runner_root,
        project,
        environment_name,
        environment,
        datasources,
        apis,
        cases,
        workflows,
        mock_routes,
    })
}

pub fn project_root(root: &Path) -> PathBuf {
    root.join(TESTRUNNER_DIR)
}

pub fn load_data_tree(data_root: &Path) -> Result<Value> {
    let mut root = serde_json::Map::new();

    if !data_root.exists() {
        return Ok(Value::Object(root));
    }

    for entry in WalkDir::new(data_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
    {
        let extension = entry
            .path()
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        if !matches!(extension, "json" | "yaml" | "yml") {
            continue;
        }

        let relative = entry.path().strip_prefix(data_root)?;
        let mut segments = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>();

        if let Some(last) = segments.last_mut()
            && let Some((stem, _)) = last.rsplit_once('.')
        {
            *last = stem.to_string();
        }

        let parsed = match extension {
            "json" => serde_json::from_str::<Value>(&fs::read_to_string(entry.path())?)?,
            _ => serde_yaml::from_str::<Value>(&fs::read_to_string(entry.path())?)?,
        };
        insert_nested_value(&mut root, &segments, parsed)?;
    }

    Ok(Value::Object(root))
}

fn load_datasources(root: &Path) -> Result<IndexMap<String, DatasourceDefinition>> {
    let mut merged = IndexMap::new();
    if !root.exists() {
        return Ok(merged);
    }

    for path in discover_yaml_files(root)? {
        let catalog: DatasourceCatalog = read_yaml(&path)?;
        for (name, definition) in catalog.datasources {
            merged.insert(name, definition);
        }
    }

    Ok(merged)
}

fn load_apis(root: &Path) -> Result<IndexMap<String, LoadedApi>> {
    let mut apis = IndexMap::new();
    for path in discover_yaml_files(root)? {
        let definition: ApiDefinition = read_yaml(&path)?;
        let relative = path.strip_prefix(root)?.to_path_buf();
        let id = file_id_from_relative(&relative);
        apis.insert(
            id.clone(),
            LoadedApi {
                id,
                relative_path: relative,
                definition,
            },
        );
    }
    Ok(apis)
}

fn load_cases(root: &Path) -> Result<Vec<LoadedCase>> {
    let mut cases = Vec::new();
    for path in discover_yaml_files(root)? {
        let definition: CaseFile = read_yaml(&path)?;
        let relative = path.strip_prefix(root)?.to_path_buf();
        let id = file_id_from_relative(&relative);
        cases.push(LoadedCase {
            id,
            relative_path: relative,
            definition,
        });
    }
    Ok(cases)
}

fn load_workflows(root: &Path) -> Result<IndexMap<String, LoadedWorkflow>> {
    let mut workflows = IndexMap::new();
    for path in discover_yaml_files(root)? {
        let definition: WorkflowFile = read_yaml(&path)?;
        validate_workflow_definition(&definition)
            .with_context(|| format!("invalid workflow definition in {}", path.display()))?;
        let relative = path.strip_prefix(root)?.to_path_buf();
        let id = file_id_from_relative(&relative);
        workflows.insert(
            id.clone(),
            LoadedWorkflow {
                id,
                relative_path: relative,
                definition,
            },
        );
    }
    Ok(workflows)
}

fn load_mock_routes(root: &Path) -> Result<Vec<MockRouteDefinition>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut routes = Vec::new();
    for path in discover_yaml_files(root)? {
        let route = read_yaml::<MockRouteDefinition>(&path)?;
        validate_mock_route(&route)
            .with_context(|| format!("invalid mock route definition in {}", path.display()))?;
        routes.push(route);
    }
    routes.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.method.cmp(&right.method))
    });
    Ok(routes)
}

fn validate_mock_route(route: &MockRouteDefinition) -> Result<()> {
    if let Some(respond) = &route.respond
        && respond.body.is_some()
        && respond.body_file.is_some()
    {
        bail!("mock `respond` cannot define both `body` and `body_file`");
    }

    if route.respond.is_none() && route.body.is_some() && route.body_file.is_some() {
        bail!("mock route cannot define both `body` and `body_file`");
    }

    validate_mock_steps(&route.steps)
}

fn validate_mock_steps(steps: &[Step]) -> Result<()> {
    for step in steps {
        match step {
            Step::Set { .. } => {}
            Step::Conditional(ConditionalStep {
                then_steps,
                else_steps,
                ..
            }) => {
                validate_mock_steps(then_steps)?;
                validate_mock_steps(else_steps)?;
            }
            _ => bail!("mock route steps currently support only `set` and `if`"),
        }
    }
    Ok(())
}

fn discover_yaml_files(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut files = WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            matches!(
                entry.path().extension().and_then(OsStr::to_str),
                Some("yaml") | Some("yml")
            )
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn file_id_from_relative(path: &Path) -> String {
    path.with_extension("")
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn insert_nested_value(
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
            bail!("{segment} is not an object node in the data tree");
        };
        current = object;
    }

    current.insert(path[path.len() - 1].clone(), value);
    Ok(())
}

fn read_yaml<T: for<'de> Deserialize<'de>>(path: impl AsRef<Path>) -> Result<T> {
    let path = path.as_ref();
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn default_version() -> u32 {
    1
}

fn default_project_name() -> String {
    "sample-http-service".to_string()
}

fn default_env_name() -> String {
    "local".to_string()
}

fn default_execution_mode() -> String {
    "serial".to_string()
}

fn default_timeout_ms() -> u64 {
    30_000
}

fn default_mock_enabled() -> bool {
    true
}

fn default_mock_host() -> String {
    "127.0.0.1".to_string()
}

fn default_mock_port() -> u16 {
    18_080
}

fn default_base_url() -> String {
    "http://127.0.0.1:3000".to_string()
}

fn default_http_status() -> u16 {
    200
}
