use anyhow::{Result, bail};
use indexmap::IndexMap;
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use serde_yaml::{Mapping, Value as YamlValue};

#[derive(Debug, Clone, Deserialize)]
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
    QueryDb(QueryDbStep),
    QueryRedis(QueryRedisStep),
    Conditional(ConditionalStep),
    Foreach(ForeachStep),
}

#[derive(Debug, Clone, Deserialize)]
pub struct SqlExecStep {
    pub datasource: String,
    #[serde(default)]
    pub sql: Option<String>,
    #[serde(default)]
    pub file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RedisCommandStep {
    pub datasource: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

    bail!("unsupported step shape: {value:?}")
}

fn parse_assertion(value: YamlValue) -> Result<Assertion> {
    let mapping = value
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("assert entries must be YAML mappings"))?;
    if mapping.len() != 1 {
        bail!("assert entries must contain exactly one operation");
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
        other => bail!("unsupported assertion operator `{other}`"),
    };

    let args = match raw_args {
        YamlValue::Sequence(sequence) => sequence
            .iter()
            .map(|item| serde_yaml::from_value(item.clone()))
            .collect::<Result<Vec<Value>, _>>()?,
        _ => vec![serde_yaml::from_value(raw_args.clone())?],
    };

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
        Ok(serde_yaml::from_value(raw.clone())?)
    } else {
        Ok(IndexMap::new())
    }
}

fn get_value<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a YamlValue> {
    mapping.get(YamlValue::String(key.to_string()))
}

fn validate_sqlish(sql: Option<&String>, file: Option<&String>, step_name: &str) -> Result<()> {
    if sql.is_some() || file.is_some() {
        Ok(())
    } else {
        bail!("{step_name} requires either `sql` or `file`")
    }
}
