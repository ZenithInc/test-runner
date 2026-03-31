use anyhow::{Result, bail};
use indexmap::IndexMap;
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use serde_yaml::{Mapping, Value as YamlValue};

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct CaseFile {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub api: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub vars: IndexMap<String, Value>,
    #[serde(default)]
    pub setup: Vec<Step>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub teardown: Vec<Step>,
}

#[derive(Debug, Clone)]
pub enum Step {
    UseData { path: String },
    Set { values: IndexMap<String, Value> },
    Sql(SqlExecStep),
    Redis(RedisCommandStep),
    Request(RequestStep),
    Callback(CallbackStep),
    Sleep(SleepStep),
    QueryDb(QueryDbStep),
    QueryRedis(QueryRedisStep),
    Conditional(ConditionalStep),
    Foreach(ForeachStep),
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SqlExecStep {
    pub datasource: String,
    #[serde(default)]
    pub sql: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RedisCommandStep {
    pub datasource: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RequestSpec {
    #[serde(default)]
    pub api: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub path_params: IndexMap<String, Value>,
    #[serde(default)]
    pub query: IndexMap<String, Value>,
    #[serde(default)]
    pub headers: IndexMap<String, Value>,
    #[serde(default)]
    pub body: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct RequestStep {
    pub request: RequestSpec,
    pub extract: IndexMap<String, String>,
    pub assertions: Vec<Assertion>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct CallbackStep {
    #[serde(default)]
    pub after_ms: u64,
    pub request: RequestSpec,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SleepStep {
    pub ms: u64,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct QueryDbSpec {
    pub datasource: String,
    #[serde(default)]
    pub sql: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueryDbStep {
    pub query: QueryDbSpec,
    pub extract: IndexMap<String, String>,
    pub assertions: Vec<Assertion>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct QueryRedisSpec {
    pub datasource: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct QueryRedisStep {
    pub query: QueryRedisSpec,
    pub extract: IndexMap<String, String>,
    pub assertions: Vec<Assertion>,
}

#[derive(Debug, Clone)]
pub struct ConditionalStep {
    pub condition: String,
    pub then_steps: Vec<Step>,
    pub else_steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub struct ForeachStep {
    pub expression: String,
    pub binding: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone)]
pub struct Assertion {
    pub kind: AssertionKind,
    pub args: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertionKind {
    Eq,
    Ne,
    Contains,
    NotEmpty,
    Exists,
    Gt,
    Ge,
    Lt,
    Le,
}

const STEP_PRIMARY_KEYS: &[&str] = &[
    "use_data",
    "set",
    "sql",
    "redis",
    "request",
    "callback",
    "sleep",
    "query_db",
    "query_redis",
    "if",
    "foreach",
];

const ASSERTION_OPERATORS: &[&str] = &[
    "eq",
    "ne",
    "contains",
    "not_empty",
    "exists",
    "gt",
    "ge",
    "lt",
    "le",
];

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = YamlValue::deserialize(deserializer)?;
        parse_step(value).map_err(D::Error::custom)
    }
}

impl<'de> Deserialize<'de> for Assertion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = YamlValue::deserialize(deserializer)?;
        parse_assertion(value).map_err(D::Error::custom)
    }
}

fn parse_step(value: YamlValue) -> Result<Step> {
    let mapping = value
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("step entries must be YAML mappings"))?;

    let primary_keys = present_primary_step_keys(mapping);
    if primary_keys.len() > 1 {
        bail!(
            "step entries must contain exactly one primary step key, found [{}]; supported keys: [{}]",
            primary_keys.join(", "),
            STEP_PRIMARY_KEYS.join(", ")
        );
    }

    if let Some(raw) = get_value(mapping, "use_data") {
        return Ok(Step::UseData {
            path: serde_yaml::from_value(raw.clone())?,
        });
    }

    if let Some(raw) = get_value(mapping, "set") {
        return Ok(Step::Set {
            values: serde_yaml::from_value(raw.clone())?,
        });
    }

    if let Some(raw) = get_value(mapping, "sql") {
        let step: SqlExecStep = serde_yaml::from_value(raw.clone())?;
        validate_sqlish(step.sql.as_ref(), step.file.as_ref(), "sql")?;
        return Ok(Step::Sql(step));
    }

    if let Some(raw) = get_value(mapping, "redis") {
        let step: RedisCommandStep = serde_yaml::from_value(raw.clone())?;
        return Ok(Step::Redis(step));
    }

    if let Some(raw) = get_value(mapping, "request") {
        let request = serde_yaml::from_value(raw.clone())?;
        return Ok(Step::Request(RequestStep {
            request,
            extract: extract_map(mapping)?,
            assertions: assertions(mapping)?,
        }));
    }

    if let Some(raw) = get_value(mapping, "callback") {
        return Ok(Step::Callback(serde_yaml::from_value(raw.clone())?));
    }

    if let Some(raw) = get_value(mapping, "sleep") {
        return Ok(Step::Sleep(serde_yaml::from_value(raw.clone())?));
    }

    if let Some(raw) = get_value(mapping, "query_db") {
        let query: QueryDbSpec = serde_yaml::from_value(raw.clone())?;
        validate_sqlish(query.sql.as_ref(), query.file.as_ref(), "query_db")?;
        return Ok(Step::QueryDb(QueryDbStep {
            query,
            extract: extract_map(mapping)?,
            assertions: assertions(mapping)?,
        }));
    }

    if let Some(raw) = get_value(mapping, "query_redis") {
        let query = serde_yaml::from_value(raw.clone())?;
        return Ok(Step::QueryRedis(QueryRedisStep {
            query,
            extract: extract_map(mapping)?,
            assertions: assertions(mapping)?,
        }));
    }

    if let Some(condition) = get_value(mapping, "if") {
        let then_steps = get_value(mapping, "then")
            .ok_or_else(|| anyhow::anyhow!("conditional steps require a then block"))?;
        let else_steps = get_value(mapping, "else");
        return Ok(Step::Conditional(ConditionalStep {
            condition: serde_yaml::from_value(condition.clone())?,
            then_steps: serde_yaml::from_value(then_steps.clone())?,
            else_steps: else_steps
                .map(|raw| serde_yaml::from_value(raw.clone()))
                .transpose()?
                .unwrap_or_default(),
        }));
    }

    if let Some(expression) = get_value(mapping, "foreach") {
        let steps = get_value(mapping, "steps")
            .ok_or_else(|| anyhow::anyhow!("foreach steps require a steps block"))?;
        let binding = get_value(mapping, "as")
            .ok_or_else(|| anyhow::anyhow!("foreach steps require an `as` binding"))?;
        return Ok(Step::Foreach(ForeachStep {
            expression: serde_yaml::from_value(expression.clone())?,
            binding: serde_yaml::from_value(binding.clone())?,
            steps: serde_yaml::from_value(steps.clone())?,
        }));
    }

    let keys = mapping_keys(mapping);
    let key_summary = if keys.is_empty() {
        "no keys found".to_string()
    } else {
        format!("found keys [{}]", keys.join(", "))
    };
    bail!(
        "unsupported step shape ({key_summary}); expected exactly one of [{}]. `request`, `query_db`, and `query_redis` may also include `extract` / `assert`; `if` uses `then` / `else`; `foreach` uses `as` / `steps`",
        STEP_PRIMARY_KEYS.join(", ")
    )
}

fn parse_assertion(value: YamlValue) -> Result<Assertion> {
    let mapping = value
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("assert entries must be YAML mappings"))?;
    if mapping.len() != 1 {
        let keys = mapping_keys(mapping);
        bail!(
            "assert entries must contain exactly one operator; found [{}]. Supported operators: [{}]",
            keys.join(", "),
            ASSERTION_OPERATORS.join(", ")
        );
    }

    let (raw_kind, raw_args) = mapping.iter().next().expect("mapping has one entry");
    let kind = match raw_kind.as_str().unwrap_or_default() {
        "eq" => AssertionKind::Eq,
        "ne" => AssertionKind::Ne,
        "contains" => AssertionKind::Contains,
        "not_empty" => AssertionKind::NotEmpty,
        "exists" => AssertionKind::Exists,
        "gt" => AssertionKind::Gt,
        "ge" => AssertionKind::Ge,
        "lt" => AssertionKind::Lt,
        "le" => AssertionKind::Le,
        other => bail!(
            "unsupported assertion operator `{other}`; supported operators: [{}]",
            ASSERTION_OPERATORS.join(", ")
        ),
    };

    let args = match raw_args {
        YamlValue::Sequence(sequence) => sequence
            .iter()
            .map(|item| serde_yaml::from_value(item.clone()))
            .collect::<Result<Vec<Value>, _>>()?,
        _ => vec![serde_yaml::from_value(raw_args.clone())?],
    };

    validate_assertion_arity(kind, args.len())?;

    Ok(Assertion { kind, args })
}

fn assertions(mapping: &Mapping) -> Result<Vec<Assertion>> {
    if let Some(raw) = get_value(mapping, "assert") {
        Ok(serde_yaml::from_value(raw.clone())?)
    } else {
        Ok(Vec::new())
    }
}

fn extract_map(mapping: &Mapping) -> Result<IndexMap<String, String>> {
    if let Some(raw) = get_value(mapping, "extract") {
        let extract: IndexMap<String, String> = serde_yaml::from_value(raw.clone())?;
        validate_extract_map(&extract)?;
        Ok(extract)
    } else {
        Ok(IndexMap::new())
    }
}

fn get_value<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a YamlValue> {
    mapping.get(YamlValue::String(key.to_string()))
}

fn present_primary_step_keys(mapping: &Mapping) -> Vec<String> {
    STEP_PRIMARY_KEYS
        .iter()
        .filter(|key| get_value(mapping, key).is_some())
        .map(|key| (*key).to_string())
        .collect()
}

fn mapping_keys(mapping: &Mapping) -> Vec<String> {
    mapping
        .keys()
        .filter_map(|key| key.as_str().map(ToOwned::to_owned))
        .collect()
}

fn validate_assertion_arity(kind: AssertionKind, arg_count: usize) -> Result<()> {
    let (expected, name) = match kind {
        AssertionKind::Eq => (2, "eq"),
        AssertionKind::Ne => (2, "ne"),
        AssertionKind::Contains => (2, "contains"),
        AssertionKind::NotEmpty => (1, "not_empty"),
        AssertionKind::Exists => (1, "exists"),
        AssertionKind::Gt => (2, "gt"),
        AssertionKind::Ge => (2, "ge"),
        AssertionKind::Lt => (2, "lt"),
        AssertionKind::Le => (2, "le"),
    };

    if arg_count == expected {
        Ok(())
    } else {
        bail!("assert `{name}` expects exactly {expected} argument(s), got {arg_count}");
    }
}

fn validate_extract_map(extract: &IndexMap<String, String>) -> Result<()> {
    for (name, expression) in extract {
        let trimmed = expression.trim();
        if trimmed.is_empty() {
            bail!("extract `{name}` cannot be empty");
        }
        if trimmed.starts_with("${") && trimmed.ends_with('}') {
            bail!(
                "extract `{name}` must use a raw expression like `response.status`, not `{trimmed}`"
            );
        }
        if trimmed.contains("{{") && trimmed.contains("}}") {
            bail!(
                "extract `{name}` must use a raw expression like `response.status`, not the template form `{trimmed}`"
            );
        }
    }
    Ok(())
}

fn validate_sqlish(sql: Option<&String>, file: Option<&String>, step_name: &str) -> Result<()> {
    if sql.is_some() || file.is_some() {
        Ok(())
    } else {
        bail!("{step_name} requires either `sql` or `file`")
    }
}

pub(crate) fn step_kind_name(step: &Step) -> &'static str {
    match step {
        Step::UseData { .. } => "use_data",
        Step::Set { .. } => "set",
        Step::Sql(_) => "sql",
        Step::Redis(_) => "redis",
        Step::Request(_) => "request",
        Step::Callback(_) => "callback",
        Step::Sleep(_) => "sleep",
        Step::QueryDb(_) => "query_db",
        Step::QueryRedis(_) => "query_redis",
        Step::Conditional(_) => "if",
        Step::Foreach(_) => "foreach",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_supports_callback_and_sleep_steps() {
        let case: CaseFile = serde_yaml::from_str(
            r#"
name: callback case
api: system/health
steps:
  - callback:
      after_ms: 250
      request:
        api: callback/payment/status
        body:
          order_no: "order-1"
          status: SUCCESS
  - sleep:
      ms: 400
"#,
        )
        .expect("case should deserialize");

        assert_eq!(case.steps.len(), 2);
        match &case.steps[0] {
            Step::Callback(step) => {
                assert_eq!(step.after_ms, 250);
                assert_eq!(step.request.api.as_deref(), Some("callback/payment/status"));
            }
            other => panic!("expected callback step, got {other:?}"),
        }
        match &case.steps[1] {
            Step::Sleep(step) => assert_eq!(step.ms, 400),
            other => panic!("expected sleep step, got {other:?}"),
        }
    }

    #[test]
    fn parser_reports_supported_step_keys_for_unknown_shapes() {
        let error = serde_yaml::from_str::<CaseFile>(
            r#"
name: invalid
api: system/health
steps:
  - nope:
      value: 1
"#,
        )
        .expect_err("invalid step must fail");

        assert!(
            error
                .to_string()
                .contains("expected exactly one of [use_data, set, sql, redis, request, callback, sleep, query_db, query_redis, if, foreach]")
        );
    }

    #[test]
    fn parser_rejects_wrapped_extract_expressions() {
        let error = serde_yaml::from_str::<CaseFile>(
            r#"
name: invalid extract
api: system/health
steps:
  - request:
      api: system/health
    extract:
      status_code: "${response.status}"
"#,
        )
        .expect_err("wrapped extract must fail");

        assert!(
            error
                .to_string()
                .contains("extract `status_code` must use a raw expression")
        );
    }
}
