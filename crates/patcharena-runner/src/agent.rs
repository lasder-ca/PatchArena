use std::{
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tokio::fs;

use crate::{ProcessRequest, RunnerError, execute_process};

/// Inputs made available to one agent invocation.
#[derive(Debug, Clone)]
pub struct AgentContext {
    /// Isolated worktree in which the agent may work.
    pub working_dir: PathBuf,
    /// Task instructions.
    pub prompt: String,
    /// Wall-clock limit for the agent.
    pub timeout: Duration,
    /// Maximum combined retained stdout and stderr bytes.
    pub max_output_bytes: usize,
    /// Parent environment variable names copied to the child.
    pub env_allowlist: Vec<String>,
}

/// Observable result of one agent invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentExecution {
    /// Exit status, or `None` after a timeout.
    pub exit_code: Option<i32>,
    /// Whether the invocation timed out.
    pub timed_out: bool,
    /// Elapsed wall-clock time.
    pub duration: Duration,
    /// Retained stdout bytes.
    pub stdout: Vec<u8>,
    /// Retained stderr bytes.
    pub stderr: Vec<u8>,
    /// Total standard output bytes observed before capture limiting.
    pub stdout_bytes: u64,
    /// Total standard error bytes observed before capture limiting.
    pub stderr_bytes: u64,
    /// Whether captured output exceeded its limit.
    pub output_truncated: bool,
}

impl AgentExecution {
    /// Whether the agent exited successfully before its deadline.
    #[must_use]
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// Pluggable coding-agent interface used by production and test runners.
#[async_trait]
pub trait AgentRunner: Send + Sync {
    /// Stable agent name stored in benchmark results.
    fn name(&self) -> &str;

    /// Render the fixed invocation shape for the command audit without including the prompt.
    fn audit_command(&self, _context: &AgentContext) -> String {
        format!("{} agent", self.name())
    }

    /// Run an agent once inside an isolated worktree.
    async fn run(&self, context: &AgentContext) -> Result<AgentExecution, RunnerError>;
}

/// Non-interactive Codex CLI runner.
#[derive(Debug, Clone)]
pub struct CodexRunner {
    executable: PathBuf,
}

impl Default for CodexRunner {
    fn default() -> Self {
        Self::new("codex")
    }
}

impl CodexRunner {
    /// Construct a runner using `executable` from the configured `PATH` or path.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
}

#[async_trait]
impl AgentRunner for CodexRunner {
    fn name(&self) -> &str {
        "codex"
    }

    fn audit_command(&self, context: &AgentContext) -> String {
        shell_words::join([
            self.executable.to_string_lossy().into_owned(),
            "--ask-for-approval".to_owned(),
            "never".to_owned(),
            "exec".to_owned(),
            "--ephemeral".to_owned(),
            "--color".to_owned(),
            "never".to_owned(),
            "--json".to_owned(),
            "--sandbox".to_owned(),
            "workspace-write".to_owned(),
            "--cd".to_owned(),
            context.working_dir.to_string_lossy().into_owned(),
            "-".to_owned(),
        ])
    }

    async fn run(&self, context: &AgentContext) -> Result<AgentExecution, RunnerError> {
        let request = ProcessRequest {
            program: self.executable.clone().into_os_string(),
            args: vec![
                "--ask-for-approval".into(),
                "never".into(),
                "exec".into(),
                "--ephemeral".into(),
                "--color".into(),
                "never".into(),
                "--json".into(),
                "--sandbox".into(),
                "workspace-write".into(),
                "--cd".into(),
                context.working_dir.clone().into_os_string(),
                "-".into(),
            ],
            current_dir: context.working_dir.clone(),
            stdin: Some(context.prompt.as_bytes().to_vec()),
            timeout: context.timeout,
            max_output_bytes: context.max_output_bytes,
            env_allowlist: context.env_allowlist.clone(),
        };
        let result = execute_process(request).await?;
        Ok(AgentExecution {
            exit_code: result.exit_code,
            timed_out: result.timed_out,
            duration: result.duration,
            stdout: result.stdout,
            stderr: result.stderr,
            stdout_bytes: result.stdout_bytes,
            stderr_bytes: result.stderr_bytes,
            output_truncated: result.output_truncated,
        })
    }
}

/// Deterministic fake-agent scenarios used by tests and downstream integrations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeBehavior {
    /// Exit successfully without modifying files.
    Success,
    /// Exit with a specified non-zero code and diagnostic.
    Failure {
        /// Process-like exit code.
        exit_code: i32,
        /// Error bytes captured on stderr.
        stderr: Vec<u8>,
    },
    /// Report a deterministic timeout without sleeping for the full deadline.
    Timeout,
    /// Write one relative file and exit successfully.
    WriteFile {
        /// Safe relative path below the worktree.
        path: PathBuf,
        /// File bytes.
        contents: Vec<u8>,
    },
    /// Write a configured number of bytes to stdout.
    LargeOutput {
        /// Bytes generated before applying the capture limit.
        bytes: usize,
    },
}

/// Configurable fake implementation of [`AgentRunner`].
#[derive(Debug, Clone)]
pub struct FakeAgentRunner {
    behavior: FakeBehavior,
}

impl FakeAgentRunner {
    /// Create a deterministic fake runner.
    #[must_use]
    pub fn new(behavior: FakeBehavior) -> Self {
        Self { behavior }
    }
}

#[async_trait]
impl AgentRunner for FakeAgentRunner {
    fn name(&self) -> &str {
        "fake"
    }

    async fn run(&self, context: &AgentContext) -> Result<AgentExecution, RunnerError> {
        let started = Instant::now();
        let mut execution = AgentExecution {
            exit_code: Some(0),
            timed_out: false,
            duration: Duration::ZERO,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_bytes: 0,
            stderr_bytes: 0,
            output_truncated: false,
        };
        match &self.behavior {
            FakeBehavior::Success => {}
            FakeBehavior::Failure { exit_code, stderr } => {
                execution.exit_code = Some(*exit_code);
                execution.stderr.clone_from(stderr);
                execution.stderr_bytes = u64::try_from(stderr.len()).unwrap_or(u64::MAX);
            }
            FakeBehavior::Timeout => {
                execution.exit_code = None;
                execution.timed_out = true;
            }
            FakeBehavior::WriteFile { path, contents } => {
                let destination = safe_write_path(&context.working_dir, path)?;
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)
                        .await
                        .map_err(|error| RunnerError::Agent(error.to_string()))?;
                }
                fs::write(destination, contents)
                    .await
                    .map_err(|error| RunnerError::Agent(error.to_string()))?;
            }
            FakeBehavior::LargeOutput { bytes } => {
                let retained = (*bytes).min(context.max_output_bytes);
                execution.stdout = vec![b'x'; retained];
                execution.stdout_bytes = u64::try_from(*bytes).unwrap_or(u64::MAX);
                execution.output_truncated = *bytes > retained;
            }
        }
        execution.duration = started.elapsed();
        Ok(execution)
    }
}

fn safe_write_path(root: &Path, relative: &Path) -> Result<PathBuf, RunnerError> {
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RunnerError::UnsafePath(relative.display().to_string()));
    }

    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            return Err(RunnerError::UnsafePath(relative.display().to_string()));
        };
        cursor.push(segment);
        if cursor.exists()
            && cursor
                .symlink_metadata()
                .map_err(|error| RunnerError::Agent(error.to_string()))?
                .file_type()
                .is_symlink()
        {
            return Err(RunnerError::UnsafePath(relative.display().to_string()));
        }
    }
    Ok(cursor)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use tempfile::tempdir;

    use super::{AgentContext, AgentRunner, FakeAgentRunner, FakeBehavior};

    fn context(path: PathBuf) -> AgentContext {
        AgentContext {
            working_dir: path,
            prompt: "make a change".to_owned(),
            timeout: Duration::from_secs(1),
            max_output_bytes: 8,
            env_allowlist: vec!["PATH".to_owned()],
        }
    }

    #[tokio::test]
    async fn fake_agent_changes_a_file() {
        let directory = tempdir().expect("temp dir");
        let runner = FakeAgentRunner::new(FakeBehavior::WriteFile {
            path: "src/lib.rs".into(),
            contents: b"pub fn answer() -> u8 { 42 }".to_vec(),
        });
        let result = runner
            .run(&context(directory.path().to_path_buf()))
            .await
            .expect("run fake");
        assert!(result.success());
        assert!(directory.path().join("src/lib.rs").is_file());
    }

    #[tokio::test]
    async fn fake_agent_rejects_parent_traversal() {
        let directory = tempdir().expect("temp dir");
        let runner = FakeAgentRunner::new(FakeBehavior::WriteFile {
            path: "../escaped".into(),
            contents: Vec::new(),
        });
        assert!(
            runner
                .run(&context(directory.path().to_path_buf()))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn fake_agent_reproduces_failure_timeout_and_large_output() {
        let directory = tempdir().expect("temp dir");
        let base = context(directory.path().to_path_buf());
        let failed = FakeAgentRunner::new(FakeBehavior::Failure {
            exit_code: 9,
            stderr: b"failed".to_vec(),
        })
        .run(&base)
        .await
        .expect("failure scenario");
        assert_eq!(failed.exit_code, Some(9));

        let timed_out = FakeAgentRunner::new(FakeBehavior::Timeout)
            .run(&base)
            .await
            .expect("timeout scenario");
        assert!(timed_out.timed_out);

        let large = FakeAgentRunner::new(FakeBehavior::LargeOutput { bytes: 64 })
            .run(&base)
            .await
            .expect("large-output scenario");
        assert_eq!(large.stdout.len(), 8);
        assert!(large.output_truncated);
    }
}
