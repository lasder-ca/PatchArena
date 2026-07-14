//! Agent execution and benchmark orchestration for PatchArena.

#![forbid(unsafe_code)]

mod agent;
mod agents;
mod audit;
mod instructions;
mod orchestration;
mod process;
mod registry;

pub use agent::{
    AdapterRunner, AgentAdapter, AgentContext, AgentDescriptor, AgentExecution, AgentInvocation,
    AgentRunner, FakeAgentRunner, FakeBehavior, detect_version,
};
pub use agents::{ClaudeAdapter, CodexAdapter, CodexRunner, CustomAdapter, GeminiAdapter};
pub use audit::{command_contains_forbidden, extract_codex_commands, path_is_forbidden};
pub use instructions::InstructionMask;
pub use orchestration::{ArenaRunner, GroupExecution, MAX_REPEAT, RunnerSettings};
pub use process::{ProcessOutput, ProcessRequest, execute_process, parse_command};
pub use registry::AgentRegistry;

/// Errors produced while running an agent or benchmark command.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// Shared model, validation, or persistence operation failed.
    #[error(transparent)]
    Core(#[from] patcharena_core::CoreError),
    /// Git repository or worktree operation failed.
    #[error(transparent)]
    Git(#[from] patcharena_git::GitError),
    /// A configured command was empty or could not be tokenized.
    #[error("invalid command `{command}`: {reason}")]
    InvalidCommand {
        /// Original command string.
        command: String,
        /// Parser error.
        reason: String,
    },
    /// A subprocess could not be started or observed.
    #[error("failed to execute `{command}`: {source}")]
    Process {
        /// Rendered command for diagnostics.
        command: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A subprocess output task failed.
    #[error("failed to collect {stream} for `{command}`: {source}")]
    Output {
        /// Stream name.
        stream: &'static str,
        /// Rendered command for diagnostics.
        command: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A background task failed unexpectedly.
    #[error("background task for `{command}` failed: {source}")]
    Join {
        /// Rendered command for diagnostics.
        command: String,
        /// Tokio join failure.
        #[source]
        source: tokio::task::JoinError,
    },
    /// A fake runner was asked to write an unsafe path.
    #[error("unsafe fake-agent path `{0}`")]
    UnsafePath(String),
    /// An agent-specific error occurred.
    #[error("agent runner failed: {0}")]
    Agent(String),
    /// A repeated benchmark stopped after its group record had been created.
    #[error("benchmark group `{group_id}` aborted after partial persistence: {source}")]
    GroupAborted {
        /// Persisted group UUID that can be used to inspect completed repeats.
        group_id: String,
        /// Failure that prevented the next repeat or group update from completing.
        #[source]
        source: Box<RunnerError>,
    },
    /// A runner-owned filesystem operation failed.
    #[error("failed to {operation} `{path}`: {source}")]
    Io {
        /// Operation being attempted.
        operation: &'static str,
        /// Affected path.
        path: std::path::PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Audit serialization failed.
    #[error("failed to serialize command audit: {0}")]
    Json(#[from] serde_json::Error),
    /// Repository instructions could not be temporarily hidden or restored.
    #[error("could not {operation} repository instruction `{path}`: {source}")]
    Instructions {
        /// Operation being attempted.
        operation: &'static str,
        /// Instruction path.
        path: std::path::PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}
