#![allow(dead_code)]

use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use schemars::{JsonSchema, Schema, SchemaGenerator, generate::SchemaSettings, json_schema};
use serde::Deserialize;
use serde_json::Value;
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::{SchemaArgs, SchemaKind};
use crate::config::{
    ApiDefinition, DatasourceCatalog, EnvironmentConfig, MockRouteDefinition, ProjectConfig,
};
use crate::dsl::{
    Assertion, CallbackStep, CaseFile, QueryDbSpec, QueryRedisSpec, RedisCommandStep, RequestSpec,
    SleepStep, SqlExecStep, Step,
};
use crate::workflow::{RunCaseStep, WorkflowFile, WorkflowStep};

pub fn run(args: SchemaArgs) -> Result<()> {
    if args.kind == SchemaKind::All {
        let documents = all_schema_documents()?;
        if let Some(output) = args.output {
            write_schema_directory(&output, &documents)?;
            println!(
                "Wrote {} schema file(s) to {}",
                documents.len(),
                output.display()
            );
        } else {
            println!("{}", serde_json::to_string_pretty(&documents)?);
        }
        return Ok(());
    }

    let document = schema_document(args.kind)?;
    if let Some(output) = args.output {
        let output = single_output_path(args.kind, &output);
        write_json_file(&output, &document)?;
        println!("Wrote {} schema to {}", args.kind.slug(), output.display());
    } else {
        println!("{}", serde_json::to_string_pretty(&document)?);
    }
    Ok(())
}

impl SchemaKind {
    fn slug(self) -> &'static str {
        match self {
            SchemaKind::All => "all",
            SchemaKind::Project => "project",
            SchemaKind::Environment => "environment",
            SchemaKind::Datasources => "datasources",
            SchemaKind::Api => "api",
            SchemaKind::Case => "case",
            SchemaKind::Workflow => "workflow",
            SchemaKind::MockRoute => "mock-route",
        }
    }

    fn file_name(self) -> String {
        format!("{}.schema.json", self.slug())
    }
}

fn single_output_path(kind: SchemaKind, output: &Path) -> PathBuf {
    if output.exists() && output.is_dir() {
        output.join(kind.file_name())
    } else {
        output.to_path_buf()
    }
}

fn write_schema_directory(output: &Path, documents: &IndexMap<String, Value>) -> Result<()> {
    if output.exists() && !output.is_dir() {
        bail!(
            "schema output for `all` must be a directory, but `{}` is a file",
            output.display()
        );
    }
    fs::create_dir_all(output).with_context(|| {
        format!(
            "failed to create schema output directory {}",
            output.display()
        )
    })?;
    for (name, document) in documents {
        write_json_file(&output.join(format!("{name}.schema.json")), document)?;
    }
    Ok(())
}

fn write_json_file(output: &Path, document: &Value) -> Result<()> {
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(document).context("failed to serialize schema JSON")?;
    fs::write(output, format!("{raw}\n"))
        .with_context(|| format!("failed to write schema file {}", output.display()))
}

fn all_schema_documents() -> Result<IndexMap<String, Value>> {
    let mut documents = IndexMap::new();
    for kind in [
        SchemaKind::Project,
        SchemaKind::Environment,
        SchemaKind::Datasources,
        SchemaKind::Api,
        SchemaKind::Case,
        SchemaKind::Workflow,
        SchemaKind::MockRoute,
    ] {
        documents.insert(kind.slug().to_string(), schema_document(kind)?);
    }
    Ok(documents)
}

fn schema_document(kind: SchemaKind) -> Result<Value> {
    match kind {
        SchemaKind::All => bail!("`all` does not correspond to a single JSON Schema document"),
        SchemaKind::Project => schema_document_for::<ProjectConfig>(),
        SchemaKind::Environment => schema_document_for::<EnvironmentConfig>(),
        SchemaKind::Datasources => schema_document_for::<DatasourceCatalog>(),
        SchemaKind::Api => schema_document_for::<ApiDefinition>(),
        SchemaKind::Case => schema_document_for::<CaseFile>(),
        SchemaKind::Workflow => schema_document_for::<WorkflowFile>(),
        SchemaKind::MockRoute => schema_document_for::<MockRouteDefinition>(),
    }
}

fn schema_document_for<T: JsonSchema>() -> Result<Value> {
    let generator = SchemaSettings::draft2020_12()
        .for_deserialize()
        .into_generator();
    let schema = generator.into_root_schema_for::<T>();
    serde_json::to_value(schema).context("failed to serialize schema document")
}

impl JsonSchema for Step {
    fn schema_name() -> Cow<'static, str> {
        "Step".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::Step").into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "title": "Step",
            "oneOf": [
                generator.subschema_for::<UseDataStepSchema>(),
                generator.subschema_for::<SetStepSchema>(),
                generator.subschema_for::<SqlStepSchema>(),
                generator.subschema_for::<RedisStepSchema>(),
                generator.subschema_for::<RequestStepSchema>(),
                generator.subschema_for::<CallbackStepSchema>(),
                generator.subschema_for::<SleepStepSchema>(),
                generator.subschema_for::<QueryDbStepSchema>(),
                generator.subschema_for::<QueryRedisStepSchema>(),
                generator.subschema_for::<ConditionalStepSchema>(),
                generator.subschema_for::<ForeachStepSchema>()
            ]
        })
    }
}

impl JsonSchema for Assertion {
    fn schema_name() -> Cow<'static, str> {
        "Assertion".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::Assertion").into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "title": "Assertion",
            "oneOf": [
                generator.subschema_for::<EqAssertionSchema>(),
                generator.subschema_for::<NeAssertionSchema>(),
                generator.subschema_for::<ContainsAssertionSchema>(),
                generator.subschema_for::<NotEmptyAssertionSchema>(),
                generator.subschema_for::<ExistsAssertionSchema>(),
                generator.subschema_for::<GtAssertionSchema>(),
                generator.subschema_for::<GeAssertionSchema>(),
                generator.subschema_for::<LtAssertionSchema>(),
                generator.subschema_for::<LeAssertionSchema>()
            ]
        })
    }
}

impl JsonSchema for WorkflowStep {
    fn schema_name() -> Cow<'static, str> {
        "WorkflowStep".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::WorkflowStep").into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "title": "WorkflowStep",
            "oneOf": [
                generator.subschema_for::<RunCaseWorkflowStepSchema>(),
                generator.subschema_for::<ConditionalWorkflowStepSchema>()
            ]
        })
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UseDataStepSchema {
    use_data: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetStepSchema {
    set: IndexMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SqlStepSchema {
    sql: SqlExecStep,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RedisStepSchema {
    redis: RedisCommandStep,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequestStepSchema {
    request: RequestSpec,
    #[serde(default)]
    extract: IndexMap<String, String>,
    #[serde(default, rename = "assert")]
    assertions: Vec<Assertion>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CallbackStepSchema {
    callback: CallbackStep,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SleepStepSchema {
    sleep: SleepStep,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct QueryDbStepSchema {
    query_db: QueryDbSpec,
    #[serde(default)]
    extract: IndexMap<String, String>,
    #[serde(default, rename = "assert")]
    assertions: Vec<Assertion>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct QueryRedisStepSchema {
    query_redis: QueryRedisSpec,
    #[serde(default)]
    extract: IndexMap<String, String>,
    #[serde(default, rename = "assert")]
    assertions: Vec<Assertion>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ConditionalStepSchema {
    #[serde(rename = "if")]
    condition: String,
    then: Vec<Step>,
    #[serde(default, rename = "else")]
    else_steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ForeachStepSchema {
    #[serde(rename = "foreach")]
    expression: String,
    #[serde(rename = "as")]
    binding: String,
    steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EqAssertionSchema {
    eq: [Value; 2],
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NeAssertionSchema {
    ne: [Value; 2],
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ContainsAssertionSchema {
    contains: [Value; 2],
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NotEmptyAssertionSchema {
    not_empty: [Value; 1],
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExistsAssertionSchema {
    exists: [Value; 1],
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GtAssertionSchema {
    gt: [Value; 2],
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GeAssertionSchema {
    ge: [Value; 2],
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LtAssertionSchema {
    lt: [Value; 2],
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LeAssertionSchema {
    le: [Value; 2],
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunCaseWorkflowStepSchema {
    run_case: RunCaseStep,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ConditionalWorkflowStepSchema {
    #[serde(rename = "if")]
    condition: String,
    then: Vec<WorkflowStep>,
    #[serde(default, rename = "else")]
    else_steps: Vec<WorkflowStep>,
}
