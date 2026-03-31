pub mod callback;
pub mod cli;
pub mod config;
pub mod dsl;
pub mod environment;
pub mod init;
pub mod mock;
pub mod runner;
pub mod runtime;
pub mod schema;
pub mod url_rewrite;
pub mod web;
pub mod workflow;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Commands};

pub async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init(args) => init::run(args).await,
        Commands::Schema(args) => schema::run(args),
        Commands::Test { target } => runner::run(target).await,
        Commands::Web(args) => web::run(args).await,
    }
}
