//! Command handlers for the `patcharena` binary.

#![forbid(unsafe_code)]

mod args;
mod commands;
mod error;

pub use args::{
    AgentCommand, BattleArgs, Cli, Command, CompareArgs, ReportArgs, ReportFormat, RunArgs,
    TaskAddArgs, TaskCommand,
};
pub use error::CliError;

/// Execute one parsed top-level command and return its process exit code.
pub async fn run(command: Command) -> Result<u8, CliError> {
    commands::run(command).await
}
