use std::{
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

use crate::RunnerError;

const POST_KILL_WAIT: Duration = Duration::from_secs(5);
const PIPE_DRAIN_WAIT: Duration = Duration::from_secs(2);

/// A subprocess request expressed as a program plus an argument array.
#[derive(Debug, Clone)]
pub struct ProcessRequest {
    /// Program to execute without an intermediary shell.
    pub program: OsString,
    /// Arguments passed verbatim to the program.
    pub args: Vec<OsString>,
    /// Working directory for the child process.
    pub current_dir: PathBuf,
    /// Optional bytes written to the child's standard input.
    pub stdin: Option<Vec<u8>>,
    /// Wall-clock timeout.
    pub timeout: Duration,
    /// Maximum combined bytes retained across stdout and stderr.
    pub max_output_bytes: usize,
    /// Environment variable names copied from the parent process.
    pub env_allowlist: Vec<String>,
}

impl ProcessRequest {
    /// Render this request for an audit log. This is never passed to a shell.
    #[must_use]
    pub fn display_command(&self) -> String {
        let mut words = Vec::with_capacity(self.args.len() + 1);
        words.push(self.program.to_string_lossy().into_owned());
        words.extend(
            self.args
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned()),
        );
        shell_words::join(words)
    }
}

/// Captured output and status for one subprocess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    /// Exit status, or `None` when unavailable or timed out.
    pub exit_code: Option<i32>,
    /// Whether the wall-clock deadline was reached.
    pub timed_out: bool,
    /// Elapsed wall-clock time.
    pub duration: Duration,
    /// Retained standard output bytes.
    pub stdout: Vec<u8>,
    /// Retained standard error bytes.
    pub stderr: Vec<u8>,
    /// Total standard output bytes observed before retention limiting.
    pub stdout_bytes: u64,
    /// Total standard error bytes observed before retention limiting.
    pub stderr_bytes: u64,
    /// Whether output was discarded after reaching the configured limit.
    pub output_truncated: bool,
}

impl ProcessOutput {
    /// Whether the process exited successfully before its deadline.
    #[must_use]
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// Parse a human-authored command with POSIX shell quoting, without invoking a shell.
///
/// Operators such as `|`, `>`, `&&`, and variable expansion have no special meaning.
pub fn parse_command(command: &str) -> Result<(OsString, Vec<OsString>), RunnerError> {
    let words = shell_words::split(command).map_err(|error| RunnerError::InvalidCommand {
        command: command.to_owned(),
        reason: error.to_string(),
    })?;
    let (program, arguments) = words
        .split_first()
        .ok_or_else(|| RunnerError::InvalidCommand {
            command: command.to_owned(),
            reason: "command is empty".to_owned(),
        })?;
    Ok((
        OsString::from(program),
        arguments.iter().map(OsString::from).collect(),
    ))
}

/// Execute a child process with a deadline, an environment allowlist, and bounded capture.
pub async fn execute_process(request: ProcessRequest) -> Result<ProcessOutput, RunnerError> {
    let rendered = request.display_command();
    let mut command = Command::new(&request.program);
    command
        .args(&request.args)
        .current_dir(&request.current_dir)
        .env_clear()
        .kill_on_drop(true)
        .stdin(if request.stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    for name in &request.env_allowlist {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    #[cfg(unix)]
    command.process_group(0);

    tracing::debug!(command = %rendered, cwd = %request.current_dir.display(), "starting process");
    let started = Instant::now();
    let mut child = command.spawn().map_err(|source| RunnerError::Process {
        command: rendered.clone(),
        source,
    })?;

    let stdin_task = if let (Some(input), Some(mut stdin)) = (request.stdin, child.stdin.take()) {
        Some(tokio::spawn(async move {
            stdin.write_all(&input).await?;
            stdin.shutdown().await
        }))
    } else {
        None
    };

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RunnerError::Agent("child stdout was not piped as requested".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RunnerError::Agent("child stderr was not piped as requested".to_owned()))?;
    let budget = Arc::new(AtomicUsize::new(0));
    let stdout_task = tokio::spawn(read_bounded(
        stdout,
        Arc::clone(&budget),
        request.max_output_bytes,
    ));
    let stderr_task = tokio::spawn(read_bounded(stderr, budget, request.max_output_bytes));

    let child_id = child.id();
    let (exit_code, timed_out, post_exit_error) = match timeout(request.timeout, child.wait()).await
    {
        Ok(status) => {
            let status = status.map_err(|source| RunnerError::Process {
                command: rendered.clone(),
                source,
            })?;
            let post_exit_error = terminate_remaining_process_group(child_id, &rendered).err();
            (status.code(), false, post_exit_error)
        }
        Err(_) => {
            tracing::warn!(command = %rendered, "process timed out");
            terminate_process_tree(&mut child, child_id, &rendered).await?;
            timeout(POST_KILL_WAIT, child.wait())
                .await
                .map_err(|_| RunnerError::Process {
                    command: rendered.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "child did not exit after process-tree termination",
                    ),
                })?
                .map_err(|source| RunnerError::Process {
                    command: rendered.clone(),
                    source,
                })?;
            (None, true, None)
        }
    };

    let (stdout, stdout_truncated, stdout_bytes) =
        collect_task_bounded(stdout_task, "stdout", &rendered).await?;
    let (stderr, stderr_truncated, stderr_bytes) =
        collect_task_bounded(stderr_task, "stderr", &rendered).await?;
    if let Some(task) = stdin_task {
        collect_input_task_bounded(task, &rendered).await?;
    }
    if let Some(error) = post_exit_error {
        return Err(error);
    }
    Ok(ProcessOutput {
        exit_code,
        timed_out,
        duration: started.elapsed(),
        stdout,
        stderr,
        stdout_bytes,
        stderr_bytes,
        output_truncated: stdout_truncated || stderr_truncated,
    })
}

fn terminate_remaining_process_group(
    child_id: Option<u32>,
    command: &str,
) -> Result<(), RunnerError> {
    #[cfg(unix)]
    if let Some(id) = child_id.and_then(|id| i32::try_from(id).ok()) {
        return match nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(id),
            nix::sys::signal::Signal::SIGKILL,
        ) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => Err(RunnerError::Process {
                command: command.to_owned(),
                source: std::io::Error::from(error),
            }),
        };
    }
    let _ = (child_id, command);
    Ok(())
}

async fn terminate_process_tree(
    child: &mut tokio::process::Child,
    child_id: Option<u32>,
    command: &str,
) -> Result<(), RunnerError> {
    #[cfg(unix)]
    if let Some(id) = child_id.and_then(|id| i32::try_from(id).ok()) {
        match nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(id),
            nix::sys::signal::Signal::SIGKILL,
        ) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => return Ok(()),
            Err(error) => {
                tracing::warn!(%error, "could not kill process group; falling back to direct child")
            }
        }
    }
    child.kill().await.map_err(|source| RunnerError::Process {
        command: command.to_owned(),
        source,
    })
}

async fn collect_input_task_bounded(
    mut task: tokio::task::JoinHandle<std::io::Result<()>>,
    command: &str,
) -> Result<(), RunnerError> {
    match timeout(PIPE_DRAIN_WAIT, &mut task).await {
        Ok(result) => result
            .map_err(|source| RunnerError::Join {
                command: command.to_owned(),
                source,
            })?
            .map_err(|source| RunnerError::Output {
                stream: "stdin",
                command: command.to_owned(),
                source,
            }),
        Err(_) => {
            task.abort();
            Err(RunnerError::Agent(format!(
                "stdin writer for `{command}` did not finish after process exit"
            )))
        }
    }
}

async fn collect_task_bounded(
    mut task: tokio::task::JoinHandle<std::io::Result<(Vec<u8>, bool, u64)>>,
    stream: &'static str,
    command: &str,
) -> Result<(Vec<u8>, bool, u64), RunnerError> {
    match timeout(PIPE_DRAIN_WAIT, &mut task).await {
        Ok(result) => result
            .map_err(|source| RunnerError::Join {
                command: command.to_owned(),
                source,
            })?
            .map_err(|source| RunnerError::Output {
                stream,
                command: command.to_owned(),
                source,
            }),
        Err(_) => {
            tracing::warn!(
                stream,
                command,
                "pipe remained open after child exit; capture discarded"
            );
            task.abort();
            Ok((Vec::new(), true, 0))
        }
    }
}

async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    consumed: Arc<AtomicUsize>,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool, u64)> {
    let mut retained = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    let mut observed = 0_u64;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        observed = observed.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        let previous = consumed
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                Some(used.saturating_add(read).min(limit))
            })
            .unwrap_or_else(|used| used);
        let available = limit.saturating_sub(previous);
        let keep = available.min(read);
        retained.extend_from_slice(&chunk[..keep]);
        truncated |= keep < read;
    }
    Ok((retained, truncated, observed))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{ProcessRequest, execute_process, parse_command};

    fn request(program: &str, args: &[&str]) -> ProcessRequest {
        ProcessRequest {
            program: program.into(),
            args: args.iter().map(Into::into).collect(),
            current_dir: std::env::current_dir().expect("current dir"),
            stdin: None,
            timeout: Duration::from_secs(2),
            max_output_bytes: 1_024,
            env_allowlist: vec!["PATH".to_owned()],
        }
    }

    #[test]
    fn parses_quoted_arguments_without_a_shell() {
        let (program, arguments) = parse_command("cargo test 'one test'").expect("parse command");
        assert_eq!(program, "cargo");
        assert_eq!(arguments, ["test", "one test"]);
    }

    #[test]
    fn rejects_an_empty_command() {
        assert!(parse_command("  ").is_err());
    }

    #[tokio::test]
    async fn applies_timeout() {
        let mut request = request("sleep", &["2"]);
        request.timeout = Duration::from_millis(25);
        let result = execute_process(request).await.expect("run sleep");
        assert!(result.timed_out);
        assert!(!result.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_descendants_holding_output_pipes() {
        let mut request = request("sh", &["-c", "sleep 30 & wait"]);
        request.timeout = Duration::from_millis(25);
        let started = Instant::now();
        let result = execute_process(request)
            .await
            .expect("terminate process group");
        assert!(result.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "descendant kept process pipes open"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_parent_does_not_leave_background_descendants() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut request = request(
            "sh",
            &[
                "-c",
                "(sleep 0.15; : > descendant-marker) >/dev/null 2>&1 &",
            ],
        );
        request.current_dir = directory.path().to_path_buf();
        let result = execute_process(request)
            .await
            .expect("run background-process producer");
        assert!(result.success());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!directory.path().join("descendant-marker").exists());
    }

    #[tokio::test]
    async fn bounds_combined_output() {
        let mut request = request("sh", &["-c", "printf 1234567890; printf abcdefghij >&2"]);
        request.max_output_bytes = 12;
        let result = execute_process(request).await.expect("run output producer");
        assert_eq!(result.stdout.len() + result.stderr.len(), 12);
        assert!(result.output_truncated);
    }
}
