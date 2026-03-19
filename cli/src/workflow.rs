use anyhow::{Result, bail};
use indexmap::{IndexMap, IndexSet};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use serde_yaml::{Mapping, Value as YamlValue};

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowFile {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub vars: IndexMap<String, Value>,
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
}

#[derive(Debug, Clone)]
pub enum WorkflowStep {
    RunCase(RunCaseStep),
    Conditional(WorkflowConditionalStep),
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunCaseStep {
    pub id: String,
    #[serde(rename = "case")]
    pub case_id: String,
    #[serde(default)]
    pub inputs: IndexMap<String, Value>,
    #[serde(default)]
    pub exports: IndexMap<String, String>,
    #[serde(default)]
    pub cleanup: CleanupPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupPolicy {
    #[default]
    Immediate,
    Defer,
    Skip,
}

#[derive(Debug, Clone)]
pub struct WorkflowConditionalStep {
    pub condition: String,
    pub then_steps: Vec<WorkflowStep>,
    pub else_steps: Vec<WorkflowStep>,
}

impl<'de> Deserialize<'de> for WorkflowStep {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = YamlValue::deserialize(deserializer)?;
        parse_workflow_step(value).map_err(D::Error::custom)
    }
}

pub fn validate_workflow_definition(definition: &WorkflowFile) -> Result<()> {
    let mut ids = IndexSet::new();
    validate_steps(&definition.steps, &mut ids)
}

fn validate_steps(steps: &[WorkflowStep], ids: &mut IndexSet<String>) -> Result<()> {
    for step in steps {
        match step {
            WorkflowStep::RunCase(step) => {
                if step.id.trim().is_empty() {
                    bail!("workflow run_case id cannot be empty");
                }
                if step.case_id.trim().is_empty() {
                    bail!("workflow run_case `{}` must reference a case id", step.id);
                }
                if !ids.insert(step.id.clone()) {
                    bail!("duplicate workflow step id `{}`", step.id);
                }
            }
            WorkflowStep::Conditional(step) => {
                validate_steps(&step.then_steps, ids)?;
                validate_steps(&step.else_steps, ids)?;
            }
        }
    }
    Ok(())
}

 
fn parse_workflow_step(value: YamlValue) -> Result<WorkflowStep> {
    let mapping = value
        .as_mapping()
        .ok_or_else(|| anyhow::anyhow!("workflow step entries must be YAML mappings"))?;

    if let Some(raw) = get_value(mapping, "run_case") {
        let step: RunCaseStep = serde_yaml::from_value(raw.clone())?;
        return Ok(WorkflowStep::RunCase(step));
    }

    // Workflow 已经实现了,这个问题最小,但是下面两个不好实现的:

    // 其中一种作法就是自己创建一个 .case_verisons 的目录
    // 每次运行都记录 case  的 hash
    // 如果 hash 不一样,就在 .case_versions 下创建一个新的版本,版本号递增
    // 但是这样会带到版本冲突的问题
    // 多人合作中,你不知道两个不同的 case 的修改时间, 就不知道哪一个 case 版本更早
    
    // 关于探针的问题, 在 Rust 这一侧是好做的,但是 PHP 呢?
    // 比如数据库中的变化, Event 如记录?
    // Database 可以在 ORM 的 Hook 中加入日志
    // Redis 可以呢? 并没有好的办法,只能自行在每一个 RedisLogic 中加入日志, 或修改框架的源码
    // 其它的语言和框架, 想来更不容易

    if let Some(condition) = get_value(mapping, "if") {
        let then_steps = get_value(mapping, "then")
            .ok_or_else(|| anyhow::anyhow!("workflow conditional steps require a then block"))?;
        let else_steps = get_value(mapping, "else");
        return Ok(WorkflowStep::Conditional(WorkflowConditionalStep {
            condition: serde_yaml::from_value(condition.clone())?,
            then_steps: serde_yaml::from_value(then_steps.clone())?,
            else_steps: else_steps
                .map(|raw| serde_yaml::from_value(raw.clone()))
                .transpose()?
                .unwrap_or_default(),
        }));
    }

    bail!("unsupported workflow step shape: {value:?}")
}

fn get_value<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a YamlValue> {
    mapping.get(YamlValue::String(key.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_parser_supports_run_case_and_conditionals() {
        let workflow: WorkflowFile = serde_yaml::from_str(
            r#"
name: auth flow
vars:
  phone: "13800000000"
steps:
  - run_case:
      id: send-sms
      case: user/send-sms-code/happy-path
      cleanup: defer
      exports:
        sms_code: vars.sms_code
  - if: "${workflow.steps.send-sms.passed}"
    then:
      - run_case:
          id: login
          case: user/login/happy-path
          inputs:
            sms_code: "{{ workflow.steps.send-sms.exports.sms_code }}"
    else:
      - run_case:
          id: fallback
          case: system/health/smoke
"#,
        )
        .expect("workflow should deserialize");

        validate_workflow_definition(&workflow).expect("workflow should validate");
        assert_eq!(workflow.steps.len(), 2);
        match &workflow.steps[0] {
            WorkflowStep::RunCase(step) => {
                assert_eq!(step.id, "send-sms");
                assert_eq!(step.cleanup, CleanupPolicy::Defer);
            }
            _ => panic!("expected run_case step"),
        }
    }

    #[test]
    fn workflow_validation_rejects_duplicate_step_ids() {
        let workflow: WorkflowFile = serde_yaml::from_str(
            r#"
name: duplicate ids
steps:
  - run_case:
      id: step-1
      case: system/health/smoke
  - if: "${true}"
    then:
      - run_case:
          id: step-1
          case: system/health/smoke
"#,
        )
        .expect("workflow should deserialize");

        let error = validate_workflow_definition(&workflow).expect_err("duplicate ids must fail");
        assert!(
            error
                .to_string()
                .contains("duplicate workflow step id `step-1`")
        );
    }
}
