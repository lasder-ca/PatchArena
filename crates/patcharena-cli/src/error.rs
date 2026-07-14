/// Errors surfaced by PatchArena command handlers.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Configuration, task, or filesystem input was invalid.
    #[error(transparent)]
    Core(#[from] patcharena_core::CoreError),
    /// Git prerequisites or worktree operations failed.
    #[error(transparent)]
    Git(#[from] patcharena_git::GitError),
    /// Benchmark execution failed before a result could be recorded.
    #[error(transparent)]
    Runner(#[from] patcharena_runner::RunnerError),
    /// Report or comparison generation failed.
    #[error(transparent)]
    Report(#[from] patcharena_report::ReportError),
    /// A local I/O operation failed.
    #[error("{operation} `{path}`: {source}")]
    Io {
        /// Human-readable operation.
        operation: &'static str,
        /// Affected path.
        path: std::path::PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A required external executable was unavailable or unhealthy.
    #[error("prerequisite check failed: {0}")]
    Prerequisite(String),
}

impl CliError {
    /// Stable process exit code by error category.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Core(_) | Self::Io { .. } => 3,
            Self::Git(_) | Self::Prerequisite(_) => 4,
            Self::Runner(_) => 5,
            Self::Report(_) => 7,
        }
    }
}
