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
    /// Discover and diagnose coding-agent adapters.
    Agent {
        /// Agent registry operation.
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Define, execute, resume, and report a multi-task benchmark suite.
    Suite {
        /// Suite operation.
        #[command(subcommand)]
        command: SuiteCommand,
    },
    /// Execute an isolated benchmark group.
    Run(RunArgs),
    /// Run several agents sequentially against the same task and base commit.
    Battle(BattleArgs),
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

/// Agent registry operations.
#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// List built-in and configured agents with availability and versions.
    List,
    /// Diagnose one agent without printing credentials or prompts.
    Doctor {
        /// Stable agent ID.
        agent: String,
    },
}

/// Benchmark suite operations.
#[derive(Debug, Subcommand)]
pub enum SuiteCommand {
    /// Create a reviewable suite definition.
    Add(SuiteAddArgs),
    /// List configured suite definitions.
    List,
    /// Preflight and execute a suite against explicit agents.
    Run(SuiteRunArgs),
    /// Resume the pending cells of a checkpointed suite run.
    Resume(SuiteResumeArgs),
    /// Re-render one suite run from persisted evidence.
    Report(SuiteReportArgs),
}

/// Arguments for `suite add`.
#[derive(Debug, clap::Args)]
pub struct SuiteAddArgs {
    /// Stable filesystem-safe suite identifier.
    #[arg(long)]
    pub id: String,
    /// Optional human-readable suite purpose.
    #[arg(long)]
    pub description: Option<String>,
    /// Task ID in suite order; repeat this option for multiple tasks.
    #[arg(long, required = true, action = ArgAction::Append)]
    pub task: Vec<String>,
}

/// Arguments for `suite run`.
#[derive(Debug, clap::Args)]
pub struct SuiteRunArgs {
    /// Suite definition ID.
    #[arg(long)]
    pub suite: String,
    /// Comma-separated stable agent IDs in execution order.
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    pub agents: Vec<String>,
    /// Independent repetitions for every task-and-agent cell.
    #[arg(long, default_value_t = NonZeroU32::MIN)]
    pub repeat: NonZeroU32,
    /// Temporarily hide regular repository instruction files.
    #[arg(long)]
    pub without_instructions: bool,
    /// Validate and print the immutable plan without creating artifacts.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for `suite resume`.
#[derive(Debug, clap::Args)]
pub struct SuiteResumeArgs {
    /// Suite-run UUID to resume.
    #[arg(long)]
    pub run: String,
}

/// Arguments for `suite report`.
#[derive(Debug, clap::Args)]
pub struct SuiteReportArgs {
    /// Suite-run UUID to load.
    #[arg(long)]
    pub run: String,
    /// Report serialization format.
    #[arg(long, value_enum)]
    pub format: ReportFormat,
    /// Write to a file instead of standard output.
    #[arg(long)]
    pub output: Option<PathBuf>,
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
    #[arg(long, default_value = "codex")]
    pub agent: String,
    /// Number of independent worktrees and invocations (maximum 1000).
    #[arg(long, default_value_t = NonZeroU32::MIN)]
    pub repeat: NonZeroU32,
    /// Temporarily hide regular AGENTS.md files discovered after setup during each run.
    #[arg(long)]
    pub without_instructions: bool,
}

/// Arguments for `battle`.
#[derive(Debug, clap::Args)]
pub struct BattleArgs {
    /// Task ID to execute.
    #[arg(long)]
    pub task: String,
    /// Comma-separated stable agent IDs.
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    pub agents: Vec<String>,
    /// Independent repetitions per agent.
    #[arg(long, default_value_t = NonZeroU32::MIN)]
    pub repeat: NonZeroU32,
    /// Temporarily hide repository instructions.
    #[arg(long)]
    pub without_instructions: bool,
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

    use super::{AgentCommand, Cli, Command, ReportFormat, SuiteCommand, TaskCommand};

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

    #[test]
    fn parses_custom_agent_and_battle_agent_list() {
        let run = Cli::try_parse_from([
            "patcharena",
            "run",
            "--task",
            "example",
            "--agent",
            "my-agent",
        ])
        .expect("run");
        let Command::Run(run) = run.command else {
            panic!("run")
        };
        assert_eq!(run.agent, "my-agent");
        let battle = Cli::try_parse_from([
            "patcharena",
            "battle",
            "--task",
            "example",
            "--agents",
            "codex,claude,gemini",
        ])
        .expect("battle");
        let Command::Battle(battle) = battle.command else {
            panic!("battle")
        };
        assert_eq!(battle.agents, ["codex", "claude", "gemini"]);
        let doctor =
            Cli::try_parse_from(["patcharena", "agent", "doctor", "codex"]).expect("doctor");
        assert!(
            matches!(doctor.command,Command::Agent{command:AgentCommand::Doctor{agent}} if agent=="codex")
        );
    }

    #[test]
    fn parses_suite_add_run_resume_and_report() {
        let add = Cli::try_parse_from([
            "patcharena",
            "suite",
            "add",
            "--id",
            "core",
            "--task",
            "one",
            "--task",
            "two",
        ])
        .expect("suite add");
        assert!(matches!(
            add.command,
            Command::Suite {
                command: SuiteCommand::Add(_)
            }
        ));

        let run = Cli::try_parse_from([
            "patcharena",
            "suite",
            "run",
            "--suite",
            "core",
            "--agents",
            "codex,claude",
            "--repeat",
            "3",
            "--dry-run",
        ])
        .expect("suite run");
        let Command::Suite {
            command: SuiteCommand::Run(run),
        } = run.command
        else {
            panic!("expected suite run");
        };
        assert_eq!(run.agents, ["codex", "claude"]);
        assert_eq!(run.repeat.get(), 3);
        assert!(run.dry_run);

        let id = "00000000-0000-0000-0000-000000000000";
        assert!(Cli::try_parse_from(["patcharena", "suite", "resume", "--run", id]).is_ok());
        assert!(
            Cli::try_parse_from([
                "patcharena",
                "suite",
                "report",
                "--run",
                id,
                "--format",
                "html",
                "--output",
                "report.html",
            ])
            .is_ok()
        );
    }
}
