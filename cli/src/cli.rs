use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "test-runner",
    version,
    about = "A Rust CLI for HTTP-centric integration testing"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Initialize the .testrunner scaffold inside a target project
    Init(InitArgs),
    /// Run test cases for one API, one directory, or the whole project
    Test {
        #[command(subcommand)]
        target: TestCommand,
    },
}

#[derive(Debug, Clone, Args)]
pub struct InitArgs {
    /// Project root where .testrunner will be created
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    /// Overwrite existing generated files
    #[arg(long)]
    pub force: bool,
    /// Pick the default environment profile for the generated project
    #[arg(long, value_enum, default_value_t = EnvTemplate::Local)]
    pub env_template: EnvTemplate,
    /// Generate mock server templates
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub with_mock: bool,
}

#[derive(Debug, Clone, ValueEnum)]
pub enum EnvTemplate {
    Local,
    Ci,
    Minimal,
}

#[derive(Debug, Clone, Subcommand)]
pub enum TestCommand {
    /// Run all cases that reference a specific API id
    Api(TestApiArgs),
    /// Run all cases beneath a specific directory prefix
    Dir(TestDirArgs),
    /// Run every discovered test case
    All(TestAllArgs),
    /// Run one workflow definition from .testrunner/workflows
    Workflow(TestWorkflowArgs),
}

#[derive(Debug, Clone, Args)]
pub struct TestApiArgs {
    pub api_id: String,
    #[command(flatten)]
    pub common: CommonTestArgs,
}

#[derive(Debug, Clone, Args)]
pub struct TestDirArgs {
    pub dir: String,
    #[command(flatten)]
    pub common: CommonTestArgs,
}

#[derive(Debug, Clone, Args)]
pub struct TestAllArgs {
    #[command(flatten)]
    pub common: CommonTestArgs,
}

#[derive(Debug, Clone, Args)]
pub struct TestWorkflowArgs {
    #[arg(required_unless_present = "all")]
    pub workflow_id: Option<String>,
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "workflow_id")]
    pub all: bool,
    #[command(flatten)]
    pub common: CommonTestArgs,
}

#[derive(Debug, Clone, Args)]
pub struct CommonTestArgs {
    /// Project root that contains .testrunner
    #[arg(long, default_value = ".")]
    pub root: PathBuf,
    /// Environment profile name from .testrunner/env
    #[arg(long)]
    pub env: Option<String>,
    /// Filter cases by tag (can be repeated)
    #[arg(long)]
    pub tag: Vec<String>,
    /// Filter by case id or case name substring
    #[arg(long = "case")]
    pub case_pattern: Option<String>,
    /// Stop scheduling new cases after the first failure
    #[arg(long)]
    pub fail_fast: bool,
    /// Enable parallel execution when the selected runtime supports slot isolation
    #[arg(long, action = ArgAction::SetTrue)]
    pub parallel: bool,
    /// Override the number of parallel jobs / slots to use
    #[arg(long, value_name = "N")]
    pub jobs: Option<usize>,
    /// Only show the execution plan without running cases
    #[arg(long)]
    pub dry_run: bool,
    /// Force-enable the embedded mock server
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "no_mock")]
    pub mock: bool,
    /// Force-disable the embedded mock server
    #[arg(long = "no-mock", action = ArgAction::SetTrue, conflicts_with = "mock")]
    pub no_mock: bool,
    /// Stream configured environment service logs to stderr while the run is in progress
    #[arg(long, action = ArgAction::SetTrue)]
    pub follow_env_logs: bool,
    /// Control how run results are emitted to stdout
    #[arg(long, value_enum, default_value_t = ReportFormat::Summary)]
    pub report_format: ReportFormat,
}

impl CommonTestArgs {
    pub fn mock_override(&self) -> Option<bool> {
        if self.mock {
            Some(true)
        } else if self.no_mock {
            Some(false)
        } else {
            None
        }
    }

    pub fn parallel_requested(&self) -> bool {
        self.parallel || self.jobs.is_some()
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum ReportFormat {
    Summary,
    Json,
    Junit,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn common_test_args_parse_follow_env_logs_flag() {
        let cli = Cli::parse_from(["test-runner", "test", "all", "--follow-env-logs"]);
        match cli.command {
            Commands::Test {
                target: TestCommand::All(args),
            } => assert!(args.common.follow_env_logs),
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
