use std::{num::NonZeroU32, path::PathBuf};

use clap::{ArgAction, Parser, Subcommand, ValueEnum};

/// Reproducible benchmark runner for AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "patcharena", version, about)]
pub struct Cli {
    /// Increase diagnostic logging (`-v` for info, `-vv` for debug).
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Operation to perform.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level PatchArena operations.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize PatchArena metadata in the current Git repository.
    Init,
    /// Create and inspect task definitions.
    Task {
        /// Task operation.
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Execute an isolated benchmark group.
    Run(RunArgs),
    /// Compare two run groups or individual runs.
    Compare(CompareArgs),
    /// Render benchmark results.
    Report(ReportArgs),
    /// Check local prerequisites and worktree support.
    Doctor,
}

/// Task definition operations.
#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Add a task from a prompt file and verification commands.
    Add(Box<TaskAddArgs>),
    /// List configured tasks.
    List,
}

/// Arguments for `task add`.
#[derive(Debug, clap::Args)]
pub struct TaskAddArgs {
    /// Stable filesystem-safe task identifier.
    #[arg(long)]
    pub id: String,
    /// UTF-8 file containing the coding-agent prompt.
    #[arg(long)]
    pub prompt_file: PathBuf,
    /// Setup command, repeatable. Commands are tokenized and never sent through a shell.
    #[arg(long, action = ArgAction::Append)]
    pub setup: Vec<String>,
    /// Verification command, repeatable and required.
    #[arg(long, required = true, action = ArgAction::Append)]
    pub verify: Vec<String>,
    /// Agent timeout in seconds.
    #[arg(long)]
    pub timeout_seconds: Option<u64>,
    /// Maximum number of files the agent may change.
    #[arg(long)]
    pub max_changed_files: Option<u64>,
    /// Maximum number of added plus deleted lines.
    #[arg(long)]
    pub max_diff_lines: Option<u64>,
    /// Maximum combined retained stdout and stderr bytes per process.
    #[arg(long)]
    pub max_output_bytes: Option<u64>,
    /// Forbidden command token sequence, repeatable.
    #[arg(long = "forbidden-command", action = ArgAction::Append)]
    pub forbidden_commands: Vec<String>,
    /// Forbidden repository-relative path, repeatable.
    #[arg(long = "forbidden-path", action = ArgAction::Append)]
    pub forbidden_paths: Vec<PathBuf>,
}

/// Arguments for `run`.
#[derive(Debug, clap::Args)]
pub struct RunArgs {
    /// Task ID to execute.
    #[arg(long)]
    pub task: String,
    /// Coding agent implementation.
    #[arg(long, value_enum, default_value_t = Agent::Codex)]
    pub agent: Agent,
    /// Number of independent worktrees and invocations (maximum 1000).
    #[arg(long, default_value_t = NonZeroU32::MIN)]
    pub repeat: NonZeroU32,
    /// Temporarily hide regular AGENTS.md files discovered after setup during each run.
    #[arg(long)]
    pub without_instructions: bool,
}

/// Supported production agents.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Agent {
    /// OpenAI Codex CLI in non-interactive mode.
    Codex,
}

/// Arguments for `compare`.
#[derive(Debug, clap::Args)]
pub struct CompareArgs {
    /// Baseline group ID or run ID.
    #[arg(long)]
    pub baseline: String,
    /// Candidate group ID or run ID.
    #[arg(long)]
    pub candidate: String,
    /// JSON output path; defaults to `.patcharena/comparisons/`.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// Arguments for `report`.
#[derive(Debug, clap::Args)]
pub struct ReportArgs {
    /// Report serialization format.
    #[arg(long, value_enum)]
    pub format: ReportFormat,
    /// Optional group ID; by default all discovered results are included.
    #[arg(long)]
    pub group: Option<String>,
    /// Write to a file instead of standard output.
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// Supported report formats.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ReportFormat {
    /// Human-readable Markdown.
    Markdown,
    /// Machine-readable JSON.
    Json,
    /// Self-contained HTML with no external assets.
    Html,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command, ReportFormat, TaskCommand};

    #[test]
    fn parses_task_add_with_repeated_commands() {
        let cli = Cli::try_parse_from([
            "patcharena",
            "task",
            "add",
            "--id",
            "csv-newline",
            "--prompt-file",
            "prompt.md",
            "--setup",
            "cargo build",
            "--verify",
            "cargo test csv_export",
            "--verify",
            "cargo clippy",
        ])
        .expect("parse task add");
        let Command::Task {
            command: TaskCommand::Add(arguments),
        } = cli.command
        else {
            panic!("expected task add");
        };
        assert_eq!(arguments.verify.len(), 2);
        assert_eq!(arguments.setup, ["cargo build"]);
    }

    #[test]
    fn parses_html_report() {
        let cli = Cli::try_parse_from(["patcharena", "report", "--format", "html"])
            .expect("parse report");
        let Command::Report(arguments) = cli.command else {
            panic!("expected report");
        };
        assert_eq!(arguments.format, ReportFormat::Html);
    }

    #[test]
    fn run_repeat_must_be_nonzero() {
        assert!(
            Cli::try_parse_from(["patcharena", "run", "--task", "example", "--repeat", "0",])
                .is_err()
        );
    }
}
