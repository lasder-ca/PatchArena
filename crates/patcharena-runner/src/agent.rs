use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
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
    /// Stable task ID used by custom templates.
    pub task_id: String,
    /// Stable run UUID used by custom templates.
    pub run_id: String,
    /// Private directory containing this run's evidence.
    pub result_dir: PathBuf,
}

/// Static and detected identity for one registered agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDescriptor {
    /// Stable registry ID.
    pub id: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Executable name or configured path.
    pub executable: PathBuf,
    /// Detected version text.
    pub cli_version: Option<String>,
    /// Adapter implementation version.
    pub adapter_version: String,
    /// SHA-256 of non-secret adapter configuration.
    pub config_hash: String,
}

/// Direct, shell-free process invocation built by an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInvocation {
    /// Executable to launch.
    pub program: OsString,
    /// Exact argv values.
    pub args: Vec<OsString>,
    /// Optional stdin bytes.
    pub stdin: Option<Vec<u8>>,
    /// Redacted command suitable for durable evidence.
    pub audit_command: String,
    /// Optional prompt file created with private permissions for this invocation.
    pub prompt_file: Option<PathBuf>,
}

/// Adapter contract that isolates CLI-specific detection and invocation construction.
pub trait AgentAdapter: Send + Sync {
    /// Static/detected identity.
    fn descriptor(&self) -> &AgentDescriptor;
    /// Construct a direct process invocation.
    fn build_invocation(&self, context: &AgentContext) -> Result<AgentInvocation, RunnerError>;
    /// Optional adapter-specific timeout ceiling.
    fn timeout_seconds(&self) -> Option<u64> {
        None
    }
    /// Normalize or parse captured CLI output. Default adapters retain raw evidence unchanged.
    fn parse_output(&self, execution: AgentExecution) -> Result<AgentExecution, RunnerError> {
        Ok(execution)
    }
}

/// Detect an executable version without invoking a shell.
pub fn detect_version(executable: &Path, version_args: &[&str]) -> Result<String, String> {
    let output = Command::new(executable)
        .args(version_args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("exited with {:?}", output.status.code()));
    }
    let bytes = if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    };
    let value = String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned();
    (!value.is_empty())
        .then_some(value)
        .ok_or_else(|| "version command returned no text".to_owned())
}

/// Process-backed runner shared by all built-in and custom adapters.
#[derive(Clone)]
pub struct AdapterRunner {
    adapter: Arc<dyn AgentAdapter>,
}

impl std::fmt::Debug for AdapterRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdapterRunner")
            .field("agent", &self.adapter.descriptor().id)
            .finish()
    }
}

impl AdapterRunner {
    /// Wrap an adapter as an executable runner.
    #[must_use]
    pub fn new(adapter: Arc<dyn AgentAdapter>) -> Self {
        Self { adapter }
    }
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

    /// Agent identity and detected version used in result metadata.
    fn descriptor(&self) -> AgentDescriptor {
        AgentDescriptor {
            id: self.name().to_owned(),
            display_name: self.name().to_owned(),
            executable: PathBuf::from(self.name()),
            cli_version: None,
            adapter_version: env!("CARGO_PKG_VERSION").to_owned(),
            config_hash: "0".repeat(64),
        }
    }

    /// Render the fixed invocation shape for the command audit without including the prompt.
    fn audit_command(&self, _context: &AgentContext) -> String {
        format!("{} agent", self.name())
    }

    /// Run an agent once inside an isolated worktree.
    async fn run(&self, context: &AgentContext) -> Result<AgentExecution, RunnerError>;
}

#[async_trait]
impl AgentRunner for AdapterRunner {
    fn name(&self) -> &str {
        &self.adapter.descriptor().id
    }
    fn descriptor(&self) -> AgentDescriptor {
        self.adapter.descriptor().clone()
    }
    fn audit_command(&self, context: &AgentContext) -> String {
        self.adapter.build_invocation(context).map_or_else(
            |_| format!("{} <invalid invocation>", self.name()),
            |value| value.audit_command,
        )
    }
    async fn run(&self, context: &AgentContext) -> Result<AgentExecution, RunnerError> {
        let invocation = self.adapter.build_invocation(context)?;
        if let Some(path) = &invocation.prompt_file {
            fs::write(path, context.prompt.as_bytes())
                .await
                .map_err(|source| RunnerError::Io {
                    operation: "write private prompt file",
                    path: path.clone(),
                    source,
                })?;
        }
        let timeout = self
            .adapter
            .timeout_seconds()
            .map_or(context.timeout, |seconds| {
                context.timeout.min(Duration::from_secs(seconds))
            });
        let result = execute_process(ProcessRequest {
            program: invocation.program,
            args: invocation.args,
            current_dir: context.working_dir.clone(),
            stdin: invocation.stdin,
            timeout,
            max_output_bytes: context.max_output_bytes,
            env_allowlist: context.env_allowlist.clone(),
        })
        .await;
        if let Some(path) = invocation.prompt_file {
            let _ = fs::remove_file(path).await;
        }
        let result = result?;
        self.adapter.parse_output(AgentExecution {
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

    #[cfg(unix)]
    #[test]
    fn detects_version_from_a_test_executable() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempdir().expect("temp dir");
        let executable = directory.path().join("agent");
        std::fs::write(&executable, "#!/bin/sh\nprintf 'agent 1.2.3\\n'\n").expect("write");
        let mut permissions = std::fs::metadata(&executable)
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).expect("chmod");
        assert_eq!(
            super::detect_version(&executable, &["--version"]).expect("version"),
            "agent 1.2.3"
        );
    }

    fn context(path: PathBuf) -> AgentContext {
        AgentContext {
            working_dir: path.clone(),
            prompt: "make a change".to_owned(),
            timeout: Duration::from_secs(1),
            max_output_bytes: 8,
            env_allowlist: vec!["PATH".to_owned()],
            task_id: "example".to_owned(),
            run_id: "00000000-0000-0000-0000-000000000000".to_owned(),
            result_dir: path,
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
