use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use patcharena_core::{
    ArtifactPaths, AuditEvent, BenchmarkIdentity, CURRENT_RESULT_SCHEMA_VERSION, CommandOutcome,
    RunGroup, RunPhase, RunResult, TaskCommand, TaskDefinition, Violation, ViolationKind,
};
use patcharena_git::Repository;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    AgentContext, AgentExecution, AgentRunner, InstructionMask, ProcessOutput, ProcessRequest,
    RunnerError, command_contains_forbidden, execute_process, extract_codex_commands,
    path_is_forbidden,
};

/// Maximum repetitions accepted in one group to bound allocation and accidental cost.
pub const MAX_REPEAT: u32 = 1_000;

/// Project-wide execution and security settings supplied by `patcharena.toml`.
#[derive(Clone, Debug)]
pub struct RunnerSettings {
    /// Project-wide upper bound for the agent and each task command.
    pub timeout_seconds: u64,
    /// Project-wide upper bound for retained process output.
    pub max_output_bytes: u64,
    /// Project-wide upper bound for changed files.
    pub max_changed_files: u64,
    /// Project-wide upper bound for added plus deleted lines.
    pub max_diff_lines: u64,
    /// Names of host environment variables copied into subprocesses.
    pub environment_allowlist: Vec<String>,
    /// Project-wide forbidden command token sequences.
    pub forbidden_commands: Vec<String>,
    /// Project-wide forbidden repository-relative paths.
    pub forbidden_paths: Vec<PathBuf>,
}

/// A completed repeated-run group and each immutable result.
#[derive(Clone, Debug)]
pub struct GroupExecution {
    /// Persisted group metadata.
    pub group: RunGroup,
    /// Results in invocation order.
    pub results: Vec<RunResult>,
}

/// Coordinates safe worktree creation, agent execution, verification, and evidence persistence.
pub struct ArenaRunner {
    repository: Repository,
    runs_directory: PathBuf,
    groups_directory: PathBuf,
    agent: Arc<dyn AgentRunner>,
    settings: RunnerSettings,
}

impl std::fmt::Debug for ArenaRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ArenaRunner")
            .field("repository", &self.repository)
            .field("runs_directory", &self.runs_directory)
            .field("groups_directory", &self.groups_directory)
            .field("agent", &self.agent.name())
            .field("settings", &self.settings)
            .finish()
    }
}

impl ArenaRunner {
    /// Create an orchestrator after checking that artifact directories stay inside the repository.
    pub fn new(
        repository: Repository,
        runs_directory: impl Into<PathBuf>,
        groups_directory: impl Into<PathBuf>,
        agent: Arc<dyn AgentRunner>,
        settings: RunnerSettings,
    ) -> Result<Self, RunnerError> {
        let runs_directory = runs_directory.into();
        let groups_directory = groups_directory.into();
        ensure_private_contained_directory(repository.root(), &runs_directory)?;
        ensure_private_contained_directory(repository.root(), &groups_directory)?;
        Ok(Self {
            repository,
            runs_directory,
            groups_directory,
            agent,
            settings,
        })
    }

    /// Run a task independently `repeat` times and persist a versioned run group.
    pub async fn run_group(
        &self,
        task: &TaskDefinition,
        repeat: u32,
        instructions_enabled: bool,
    ) -> Result<GroupExecution, RunnerError> {
        if repeat == 0 || repeat > MAX_REPEAT {
            return Err(RunnerError::Agent(format!(
                "repeat count must be between 1 and {MAX_REPEAT}"
            )));
        }
        task.validate()?;
        self.repository.ensure_tracked_clean()?;
        if !self.repository.status_porcelain()?.is_empty() {
            tracing::warn!(
                "untracked files are not copied to benchmark worktrees; only committed HEAD is evaluated"
            );
        }

        let mut group = RunGroup::new(task.id.clone(), self.agent.name(), Utc::now(), repeat)?;
        group.instructions_enabled = instructions_enabled;
        let benchmark_identity = self.benchmark_identity(task)?;
        group.benchmark_identity = Some(benchmark_identity.clone());
        let group_path = self
            .groups_directory
            .join(format!("{}.json", group.group_id));
        group.save_new(&group_path)?;
        let mut results = Vec::with_capacity(usize::try_from(repeat).unwrap_or(0));
        for iteration in 1..=repeat {
            tracing::info!(
                task = %task.id,
                group_id = %group.group_id,
                iteration,
                repeat,
                "starting benchmark run"
            );
            let result = match self
                .run_once(
                    task,
                    &group.group_id,
                    instructions_enabled,
                    &benchmark_identity,
                )
                .await
            {
                Ok(result) => result,
                Err(source) => {
                    return Err(abort_group(&mut group, &group_path, source));
                }
            };
            if let Err(source) = group.push_run_id(result.run_id.clone()) {
                return Err(abort_group(&mut group, &group_path, source.into()));
            }
            results.push(result);
            if let Err(source) = group.save_replace(&group_path) {
                return Err(abort_group(&mut group, &group_path, source.into()));
            }
        }
        if let Err(source) = group
            .mark_completed()
            .and_then(|()| group.save_replace(&group_path))
        {
            return Err(abort_group(&mut group, &group_path, source.into()));
        }
        Ok(GroupExecution { group, results })
    }

    async fn run_once(
        &self,
        task: &TaskDefinition,
        group_id: &str,
        instructions_enabled: bool,
        benchmark_identity: &BenchmarkIdentity,
    ) -> Result<RunResult, RunnerError> {
        let run_id = Uuid::new_v4().to_string();
        let run_directory = self.runs_directory.join(&run_id);
        create_private_directory(&run_directory)?;
        let started_at = Utc::now();
        let wall_clock = Instant::now();
        let worktree_started_at = Utc::now();
        let worktree_timer = Instant::now();

        let temporary_parent = tempfile::Builder::new()
            .prefix("patcharena-worktree-")
            .tempdir()
            .map_err(|source| RunnerError::Io {
                operation: "create temporary worktree parent",
                path: std::env::temp_dir(),
                source,
            })?;
        let worktree_path = temporary_parent.path().join("worktree");
        let worktree = match self
            .repository
            .create_detached_worktree(&worktree_path, Some(&benchmark_identity.repository_commit))
        {
            Ok(worktree) => worktree,
            Err(error) => {
                remove_empty_directory(&run_directory)?;
                return Err(error.into());
            }
        };
        if worktree.commit() != benchmark_identity.repository_commit {
            let observed = worktree.commit().to_owned();
            worktree.close()?;
            remove_empty_directory(&run_directory)?;
            return Err(RunnerError::Agent(format!(
                "worktree resolved commit `{observed}` instead of pinned benchmark commit `{}`",
                benchmark_identity.repository_commit
            )));
        }

        let effective_timeout = task
            .limits
            .timeout_seconds
            .min(self.settings.timeout_seconds);
        let effective_output_bytes = task
            .limits
            .max_output_bytes
            .min(self.settings.max_output_bytes);
        let effective_max_changed_files = task
            .limits
            .max_changed_files
            .min(self.settings.max_changed_files);
        let effective_max_diff_lines = task.limits.max_diff_lines.min(self.settings.max_diff_lines);
        let timeout = Duration::from_secs(effective_timeout);
        let output_limit = usize::try_from(effective_output_bytes).unwrap_or(usize::MAX);
        let forbidden_commands =
            merge_strings(&self.settings.forbidden_commands, &task.forbidden.commands);
        let forbidden_paths = merge_paths(&self.settings.forbidden_paths, &task.forbidden.paths);

        let mut setup = Vec::new();
        let mut verification = Vec::new();
        let mut audit = vec![AuditEvent {
            phase: RunPhase::Setup,
            started_at: worktree_started_at,
            outcome: CommandOutcome::exited(
                "git worktree add --detach <temporary-worktree> HEAD",
                0,
                duration_ms(worktree_timer.elapsed()),
            ),
        }];
        let mut violations = Vec::new();
        let mut errors = Vec::new();
        let mut agent_stdout = Vec::new();
        let mut agent_stderr = Vec::new();
        let mut agent_exit_code = None;
        let mut agent_outcome = None;
        let forbidden_before = match snapshot_forbidden_paths(worktree.path(), &forbidden_paths) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                errors.push(format!(
                    "could not inventory forbidden paths before execution: {error}"
                ));
                violations.push(Violation::new(
                    ViolationKind::Other,
                    "pre-run forbidden-path inventory failed",
                ));
                None
            }
        };
        let git_before = match git_safety_snapshot(worktree.repository()) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                errors.push(format!(
                    "could not inventory Git metadata before execution: {error}"
                ));
                violations.push(Violation::new(
                    ViolationKind::Other,
                    "pre-run Git metadata inventory failed",
                ));
                None
            }
        };

        for command in &task.setup.commands {
            let execution = self
                .run_task_command(
                    command,
                    worktree.path(),
                    timeout,
                    output_limit,
                    &forbidden_commands,
                    RunPhase::Setup,
                    &mut violations,
                )
                .await?;
            let succeeded = execution.success;
            audit.push(execution.audit);
            setup.push(execution.outcome);
            if !succeeded {
                errors.push("setup command failed".to_owned());
                break;
            }
        }

        if setup.iter().all(|outcome| outcome.success) {
            let agent_started_at = Utc::now();
            let context = AgentContext {
                working_dir: worktree.path().to_path_buf(),
                prompt: task.prompt.clone(),
                timeout,
                max_output_bytes: output_limit,
                env_allowlist: self.settings.environment_allowlist.clone(),
            };
            let agent_audit_command = self.agent.audit_command(&context);
            let mask = if instructions_enabled {
                Ok(None)
            } else {
                instruction_paths(worktree.path())
                    .and_then(|paths| InstructionMask::hide(worktree.path(), paths).map(Some))
            };
            let agent_result = match mask {
                Ok(mask) => {
                    let agent_result = self.agent.run(&context).await;
                    if let Some(mask) = mask {
                        if let Err(error) = mask.restore() {
                            errors.push(format!(
                                "could not restore repository instructions: {error}"
                            ));
                            violations.push(Violation::new(
                                ViolationKind::SymlinkEscape,
                                "repository instruction restoration failed a containment check",
                            ));
                        }
                    }
                    agent_result
                }
                Err(error) => Err(error),
            };
            match agent_result {
                Ok(execution) => {
                    agent_exit_code = execution.exit_code;
                    agent_stdout.clone_from(&execution.stdout);
                    agent_stderr.clone_from(&execution.stderr);
                    let outcome = agent_command_outcome(agent_audit_command, &execution);
                    if execution.output_truncated {
                        violations.push(Violation::new(
                            ViolationKind::OutputLimit,
                            "agent output exceeded the configured capture limit",
                        ));
                    }
                    for observed in extract_codex_commands(&execution.stdout) {
                        for forbidden in &forbidden_commands {
                            if command_contains_forbidden(&observed, forbidden) {
                                violations.push(
                                    Violation::new(
                                        ViolationKind::ForbiddenCommand,
                                        format!(
                                            "agent command matched forbidden pattern `{forbidden}`"
                                        ),
                                    )
                                    .with_command(observed.clone()),
                                );
                            }
                        }
                    }
                    if !outcome.success {
                        errors.push(if outcome.timed_out {
                            "agent timed out".to_owned()
                        } else {
                            "agent exited unsuccessfully".to_owned()
                        });
                    }
                    audit.push(AuditEvent {
                        phase: RunPhase::Agent,
                        started_at: agent_started_at,
                        outcome: outcome.clone(),
                    });
                    agent_outcome = Some(outcome);
                }
                Err(error) => {
                    let message = error.to_string();
                    errors.push(message.clone());
                    let outcome = CommandOutcome::failed(agent_audit_command, 0, message);
                    audit.push(AuditEvent {
                        phase: RunPhase::Agent,
                        started_at: agent_started_at,
                        outcome: outcome.clone(),
                    });
                    agent_outcome = Some(outcome);
                }
            }

            for command in &task.verify.commands {
                let execution = self
                    .run_task_command(
                        command,
                        worktree.path(),
                        timeout,
                        output_limit,
                        &forbidden_commands,
                        RunPhase::Verification,
                        &mut violations,
                    )
                    .await?;
                let succeeded = execution.success;
                audit.push(execution.audit);
                verification.push(execution.outcome);
                if !succeeded {
                    errors.push("verification command failed".to_owned());
                }
            }
        }

        if let Some(before) = forbidden_before {
            match snapshot_forbidden_paths(worktree.path(), &forbidden_paths) {
                Ok(after) => {
                    for (path, prior) in before {
                        if after.get(&path) != Some(&prior) {
                            push_forbidden_path_violation(
                                &mut violations,
                                &path,
                                "forbidden path changed, including ignored filesystem state",
                            );
                        }
                    }
                }
                Err(error) => {
                    errors.push(format!(
                        "could not inventory forbidden paths after execution: {error}"
                    ));
                    violations.push(Violation::new(
                        ViolationKind::Other,
                        "post-run forbidden-path inventory failed",
                    ));
                }
            }
        }
        if let Some(before) = git_before {
            match git_safety_snapshot(worktree.repository()) {
                Ok(after) if after != before => push_forbidden_path_violation(
                    &mut violations,
                    Path::new(".git"),
                    "Git HEAD, refs, index, or local configuration changed during the run",
                ),
                Ok(_) => {}
                Err(error) => {
                    errors.push(format!(
                        "could not inventory Git metadata after execution: {error}"
                    ));
                    push_forbidden_path_violation(
                        &mut violations,
                        Path::new(".git"),
                        "post-run Git metadata inventory failed",
                    );
                }
            }
        }

        let git_started_at = Utc::now();
        let git_timer = Instant::now();
        let (patch, stats, changed_paths, git_outcome) = match worktree.repository().capture_diff()
        {
            Ok(capture) => (
                capture.patch,
                Some(capture.stats),
                capture.changed_paths,
                CommandOutcome::exited(
                    "git capture diff with temporary index",
                    0,
                    duration_ms(git_timer.elapsed()),
                ),
            ),
            Err(error) => {
                let message = format!("could not capture Git diff: {error}");
                errors.push(message.clone());
                (
                    Vec::new(),
                    None,
                    Vec::new(),
                    CommandOutcome::failed(
                        "git capture diff with temporary index",
                        duration_ms(git_timer.elapsed()),
                        message,
                    ),
                )
            }
        };
        audit.push(AuditEvent {
            phase: RunPhase::Git,
            started_at: git_started_at,
            outcome: git_outcome,
        });
        let changed_files = stats.map_or(0, |stats| {
            u64::try_from(stats.changed_files).unwrap_or(u64::MAX)
        });
        let added_lines = stats.map_or(0, |stats| stats.added_lines);
        let deleted_lines = stats.map_or(0, |stats| stats.deleted_lines);
        if changed_files > effective_max_changed_files {
            violations.push(Violation::new(
                ViolationKind::LimitExceeded,
                format!(
                    "changed {changed_files} files; limit is {}",
                    effective_max_changed_files
                ),
            ));
        }
        let diff_lines = added_lines.saturating_add(deleted_lines);
        if diff_lines > effective_max_diff_lines {
            violations.push(Violation::new(
                ViolationKind::LimitExceeded,
                format!(
                    "diff contains {diff_lines} added/deleted lines; limit is {}",
                    effective_max_diff_lines
                ),
            ));
        }
        for path in &changed_paths {
            for forbidden in &forbidden_paths {
                if path_is_forbidden(path, forbidden) {
                    push_forbidden_path_violation(
                        &mut violations,
                        path,
                        &format!("changed forbidden path `{}`", path.display()),
                    );
                }
            }
            match changed_symlink_escapes(worktree.path(), path) {
                Ok(true) => violations.push(
                    Violation::new(
                        ViolationKind::SymlinkEscape,
                        format!(
                            "symbolic link `{}` resolves outside the worktree",
                            path.display()
                        ),
                    )
                    .with_path(path.clone()),
                ),
                Ok(false) => {}
                Err(error) => {
                    errors.push(format!(
                        "could not inspect changed path `{}`: {error}",
                        path.display()
                    ));
                    violations.push(
                        Violation::new(
                            ViolationKind::Other,
                            "changed path could not be checked for a symbolic-link escape",
                        )
                        .with_path(path.clone()),
                    );
                }
            }
        }

        let cleanup_started_at = Utc::now();
        let cleanup_timer = Instant::now();
        let cleanup_outcome = match worktree.close() {
            Ok(()) => CommandOutcome::exited(
                "git worktree remove and prune <temporary-worktree>",
                0,
                duration_ms(cleanup_timer.elapsed()),
            ),
            Err(error) => {
                let message = format!("worktree cleanup failed: {error}");
                errors.push(message.clone());
                violations.push(Violation::new(
                    ViolationKind::Other,
                    "Git worktree cleanup failed; manual inspection may be required",
                ));
                CommandOutcome::failed(
                    "git worktree remove and prune <temporary-worktree>",
                    duration_ms(cleanup_timer.elapsed()),
                    message,
                )
            }
        };
        audit.push(AuditEvent {
            phase: RunPhase::Cleanup,
            started_at: cleanup_started_at,
            outcome: cleanup_outcome,
        });

        let artifacts = ArtifactPaths::default();
        patcharena_core::atomic_write_new(run_directory.join(&artifacts.stdout), &agent_stdout)?;
        patcharena_core::atomic_write_new(run_directory.join(&artifacts.stderr), &agent_stderr)?;
        patcharena_core::atomic_write_new(run_directory.join(&artifacts.patch), &patch)?;
        if let Some(audit_path) = &artifacts.audit {
            let mut audit_jsonl = Vec::new();
            for event in &audit {
                serde_json::to_writer(&mut audit_jsonl, event)?;
                audit_jsonl.push(b'\n');
            }
            patcharena_core::atomic_write_new(run_directory.join(audit_path), &audit_jsonl)?;
        }

        let finished_at = Utc::now();
        let success = setup.iter().all(|outcome| outcome.success)
            && agent_outcome
                .as_ref()
                .is_some_and(|outcome| outcome.success)
            && verification.iter().all(|outcome| outcome.success)
            && verification.len() == task.verify.commands.len()
            && violations.is_empty()
            && errors.is_empty();
        let result = RunResult {
            schema_version: CURRENT_RESULT_SCHEMA_VERSION,
            run_id,
            group_id: Some(group_id.to_owned()),
            task_id: task.id.clone(),
            agent: self.agent.name().to_owned(),
            instructions_enabled,
            benchmark_identity: Some(benchmark_identity.clone()),
            started_at,
            finished_at,
            duration_ms: duration_ms(wall_clock.elapsed()),
            success,
            exit_code: agent_exit_code,
            changed_files,
            added_lines,
            deleted_lines,
            setup,
            agent_outcome,
            verification,
            audit,
            violations,
            artifacts,
            error: (!errors.is_empty()).then(|| deduplicate_errors(errors).join("; ")),
        };
        result.save_new(run_directory.join("result.json"))?;
        Ok(result)
    }

    fn benchmark_identity(&self, task: &TaskDefinition) -> Result<BenchmarkIdentity, RunnerError> {
        let repository_commit = self.repository.resolve_commit("HEAD")?;
        let mut hasher = Sha256::new();
        hash_field(&mut hasher, task.to_yaml()?.as_bytes());
        for value in [
            task.limits
                .timeout_seconds
                .min(self.settings.timeout_seconds),
            task.limits
                .max_output_bytes
                .min(self.settings.max_output_bytes),
            task.limits
                .max_changed_files
                .min(self.settings.max_changed_files),
            task.limits.max_diff_lines.min(self.settings.max_diff_lines),
        ] {
            hash_field(&mut hasher, &value.to_le_bytes());
        }
        for variable in &self.settings.environment_allowlist {
            hash_field(&mut hasher, variable.as_bytes());
        }
        for command in merge_strings(&self.settings.forbidden_commands, &task.forbidden.commands) {
            hash_field(&mut hasher, command.as_bytes());
        }
        for path in merge_paths(&self.settings.forbidden_paths, &task.forbidden.paths) {
            hash_field(&mut hasher, path.as_os_str().as_encoded_bytes());
        }
        Ok(BenchmarkIdentity {
            repository_commit,
            task_fingerprint: format!("{:x}", hasher.finalize()),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_task_command(
        &self,
        command: &TaskCommand,
        working_directory: &Path,
        timeout: Duration,
        output_limit: usize,
        forbidden_commands: &[String],
        phase: RunPhase,
        violations: &mut Vec<Violation>,
    ) -> Result<TaskCommandExecution, RunnerError> {
        let audit_string = command.audit_string();
        let started_at = Utc::now();
        if let Some(forbidden) = forbidden_commands
            .iter()
            .find(|forbidden| command_contains_forbidden(&audit_string, forbidden))
        {
            violations.push(
                Violation::new(
                    ViolationKind::ForbiddenCommand,
                    format!("blocked task command matching `{forbidden}`"),
                )
                .with_command(audit_string.clone()),
            );
            let outcome = CommandOutcome::failed(
                audit_string,
                0,
                "command blocked by forbidden-command policy",
            );
            return Ok(TaskCommandExecution {
                success: false,
                audit: AuditEvent {
                    phase,
                    started_at,
                    outcome: outcome.clone(),
                },
                outcome,
            });
        }

        let (program, args) = command.to_argv()?;
        let request = ProcessRequest {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            current_dir: working_directory.to_path_buf(),
            stdin: None,
            timeout,
            max_output_bytes: output_limit,
            env_allowlist: self.settings.environment_allowlist.clone(),
        };
        let outcome = match execute_process(request).await {
            Ok(process) => process_command_outcome(audit_string, &process),
            Err(error) => CommandOutcome::failed(audit_string, 0, error.to_string()),
        };
        if outcome.output_truncated {
            violations.push(
                Violation::new(
                    ViolationKind::OutputLimit,
                    "task command output exceeded the configured capture limit",
                )
                .with_command(outcome.command.clone()),
            );
        }
        Ok(TaskCommandExecution {
            success: outcome.success,
            audit: AuditEvent {
                phase,
                started_at,
                outcome: outcome.clone(),
            },
            outcome,
        })
    }
}

fn abort_group(group: &mut RunGroup, group_path: &Path, source: RunnerError) -> RunnerError {
    let group_id = group.group_id.clone();
    if let Err(persistence_error) = group
        .mark_aborted()
        .and_then(|()| group.save_replace(group_path))
    {
        tracing::error!(
            group_id,
            %persistence_error,
            "could not persist aborted run-group status"
        );
    }
    RunnerError::GroupAborted {
        group_id,
        source: Box::new(source),
    }
}

struct TaskCommandExecution {
    success: bool,
    outcome: CommandOutcome,
    audit: AuditEvent,
}

fn process_command_outcome(command: String, process: &ProcessOutput) -> CommandOutcome {
    CommandOutcome {
        command,
        success: process.success(),
        exit_code: process.exit_code,
        duration_ms: duration_ms(process.duration),
        timed_out: process.timed_out,
        stdout_bytes: process.stdout_bytes,
        stderr_bytes: process.stderr_bytes,
        output_truncated: process.output_truncated,
        error: process.timed_out.then(|| "command timed out".to_owned()),
    }
}

fn agent_command_outcome(command: String, execution: &AgentExecution) -> CommandOutcome {
    CommandOutcome {
        command,
        success: execution.success(),
        exit_code: execution.exit_code,
        duration_ms: duration_ms(execution.duration),
        timed_out: execution.timed_out,
        stdout_bytes: execution.stdout_bytes,
        stderr_bytes: execution.stderr_bytes,
        output_truncated: execution.output_truncated,
        error: execution.timed_out.then(|| "agent timed out".to_owned()),
    }
}

fn ensure_private_contained_directory(root: &Path, directory: &Path) -> Result<(), RunnerError> {
    let canonical_root = fs::canonicalize(root).map_err(|source| RunnerError::Io {
        operation: "canonicalize repository root",
        path: root.to_path_buf(),
        source,
    })?;
    let relative = directory
        .strip_prefix(root)
        .map_err(|_| RunnerError::UnsafePath(directory.display().to_string()))?;
    let mut current = canonical_root.clone();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(segment) => current.push(segment),
            std::path::Component::CurDir => continue,
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(RunnerError::UnsafePath(directory.display().to_string()));
            }
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(RunnerError::UnsafePath(current.display().to_string()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|source| RunnerError::Io {
                    operation: "create private directory",
                    path: current.clone(),
                    source,
                })?;
            }
            Err(source) => {
                return Err(RunnerError::Io {
                    operation: "inspect private directory",
                    path: current,
                    source,
                });
            }
        }
    }
    let canonical_directory = fs::canonicalize(directory).map_err(|source| RunnerError::Io {
        operation: "canonicalize artifact directory",
        path: directory.to_path_buf(),
        source,
    })?;
    if !canonical_directory.starts_with(canonical_root) {
        return Err(RunnerError::UnsafePath(directory.display().to_string()));
    }
    set_private_permissions(directory)?;
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), RunnerError> {
    fs::create_dir(path).map_err(|source| RunnerError::Io {
        operation: "create private directory",
        path: path.to_path_buf(),
        source,
    })?;
    set_private_permissions(path)
}

fn set_private_permissions(path: &Path) -> Result<(), RunnerError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            RunnerError::Io {
                operation: "set private directory permissions on",
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn remove_empty_directory(path: &Path) -> Result<(), RunnerError> {
    fs::remove_dir(path).map_err(|source| RunnerError::Io {
        operation: "remove empty failed-run directory",
        path: path.to_path_buf(),
        source,
    })
}

fn merge_strings(left: &[String], right: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    left.iter()
        .chain(right)
        .filter(|value| seen.insert(value.as_str()))
        .cloned()
        .collect()
}

fn merge_paths(left: &[PathBuf], right: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    left.iter()
        .chain(right)
        .filter(|value| seen.insert(value.as_path()))
        .cloned()
        .collect()
}

const INVENTORY_MAX_ENTRIES: usize = 10_000;
const INVENTORY_MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct PathFingerprint {
    entries: Vec<EntryFingerprint>,
    truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntryFingerprint {
    relative: PathBuf,
    kind: EntryKind,
    length: u64,
    content_hash: u64,
    link_target: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    Missing,
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Default)]
struct InventoryBudget {
    entries: usize,
    bytes: u64,
    truncated: bool,
}

fn snapshot_forbidden_paths(
    root: &Path,
    paths: &[PathBuf],
) -> Result<BTreeMap<PathBuf, PathFingerprint>, RunnerError> {
    paths
        .iter()
        .map(|path| fingerprint_forbidden_path(root, path).map(|value| (path.clone(), value)))
        .collect()
}

fn fingerprint_forbidden_path(
    root: &Path,
    relative: &Path,
) -> Result<PathFingerprint, RunnerError> {
    let mut cursor = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(segment) = component else {
            return Err(RunnerError::UnsafePath(relative.display().to_string()));
        };
        cursor.push(segment);
        if index + 1 < components.len() {
            match fs::symlink_metadata(&cursor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Ok(PathFingerprint {
                        entries: vec![EntryFingerprint {
                            relative: components[..=index].iter().collect(),
                            kind: EntryKind::Symlink,
                            length: metadata.len(),
                            content_hash: 0,
                            link_target: Some(fs::read_link(&cursor).map_err(|source| {
                                RunnerError::Io {
                                    operation: "read forbidden-path ancestor symlink",
                                    path: cursor.clone(),
                                    source,
                                }
                            })?),
                        }],
                        truncated: false,
                    });
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Ok(PathFingerprint {
                        entries: vec![EntryFingerprint {
                            relative: components[..=index].iter().collect(),
                            kind: EntryKind::Other,
                            length: metadata.len(),
                            content_hash: 0,
                            link_target: None,
                        }],
                        truncated: false,
                    });
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(PathFingerprint {
                        entries: vec![EntryFingerprint {
                            relative: relative.to_path_buf(),
                            kind: EntryKind::Missing,
                            length: 0,
                            content_hash: 0,
                            link_target: None,
                        }],
                        truncated: false,
                    });
                }
                Err(source) => {
                    return Err(RunnerError::Io {
                        operation: "inspect forbidden-path ancestor",
                        path: cursor,
                        source,
                    });
                }
            }
        }
    }
    let mut entries = Vec::new();
    let mut budget = InventoryBudget::default();
    collect_fingerprint(&cursor, Path::new(""), &mut entries, &mut budget)?;
    entries.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(PathFingerprint {
        entries,
        truncated: budget.truncated,
    })
}

fn collect_fingerprint(
    path: &Path,
    relative: &Path,
    entries: &mut Vec<EntryFingerprint>,
    budget: &mut InventoryBudget,
) -> Result<(), RunnerError> {
    if budget.entries >= INVENTORY_MAX_ENTRIES {
        budget.truncated = true;
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            entries.push(EntryFingerprint {
                relative: relative.to_path_buf(),
                kind: EntryKind::Missing,
                length: 0,
                content_hash: 0,
                link_target: None,
            });
            budget.entries += 1;
            return Ok(());
        }
        Err(source) => {
            return Err(RunnerError::Io {
                operation: "inspect forbidden path",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    budget.entries += 1;
    if metadata.file_type().is_symlink() {
        entries.push(EntryFingerprint {
            relative: relative.to_path_buf(),
            kind: EntryKind::Symlink,
            length: metadata.len(),
            content_hash: 0,
            link_target: Some(fs::read_link(path).map_err(|source| RunnerError::Io {
                operation: "read forbidden-path symlink",
                path: path.to_path_buf(),
                source,
            })?),
        });
    } else if metadata.is_file() {
        let content_hash = hash_file_bounded(path, budget)?;
        entries.push(EntryFingerprint {
            relative: relative.to_path_buf(),
            kind: EntryKind::File,
            length: metadata.len(),
            content_hash,
            link_target: None,
        });
    } else if metadata.is_dir() {
        entries.push(EntryFingerprint {
            relative: relative.to_path_buf(),
            kind: EntryKind::Directory,
            length: metadata.len(),
            content_hash: 0,
            link_target: None,
        });
        let mut children = fs::read_dir(path)
            .map_err(|source| RunnerError::Io {
                operation: "list forbidden directory",
                path: path.to_path_buf(),
                source,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| RunnerError::Io {
                operation: "read forbidden directory entry",
                path: path.to_path_buf(),
                source,
            })?;
        children.sort_by_key(fs::DirEntry::file_name);
        for child in children {
            let child_relative = relative.join(child.file_name());
            collect_fingerprint(&child.path(), &child_relative, entries, budget)?;
            if budget.truncated {
                break;
            }
        }
    } else {
        entries.push(EntryFingerprint {
            relative: relative.to_path_buf(),
            kind: EntryKind::Other,
            length: metadata.len(),
            content_hash: 0,
            link_target: None,
        });
    }
    Ok(())
}

fn hash_file_bounded(path: &Path, budget: &mut InventoryBudget) -> Result<u64, RunnerError> {
    let mut file = File::open(path).map_err(|source| RunnerError::Io {
        operation: "open forbidden file for inventory",
        path: path.to_path_buf(),
        source,
    })?;
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut buffer = [0_u8; 8192];
    while budget.bytes < INVENTORY_MAX_BYTES {
        let remaining = usize::try_from(INVENTORY_MAX_BYTES - budget.bytes)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = file
            .read(&mut buffer[..remaining])
            .map_err(|source| RunnerError::Io {
                operation: "read forbidden file for inventory",
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            return Ok(hash);
        }
        budget.bytes = budget
            .bytes
            .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        for byte in &buffer[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    budget.truncated = true;
    Ok(hash)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitSafetySnapshot {
    commit: String,
    refs: Vec<u8>,
    local_config: Vec<u8>,
    staged_diff: Vec<u8>,
}

fn git_safety_snapshot(repository: &Repository) -> Result<GitSafetySnapshot, RunnerError> {
    Ok(GitSafetySnapshot {
        commit: repository.resolve_commit("HEAD")?,
        refs: repository
            .run_git(["show-ref", "--head", "-d"])?
            .into_stdout(),
        local_config: repository
            .run_git(["config", "--local", "--null", "--list"])?
            .into_stdout(),
        staged_diff: repository
            .run_git(["diff", "--cached", "--binary", "--no-ext-diff", "--"])?
            .into_stdout(),
    })
}

fn push_forbidden_path_violation(violations: &mut Vec<Violation>, path: &Path, message: &str) {
    if violations.iter().any(|violation| {
        violation.kind == ViolationKind::ForbiddenPath && violation.path.as_deref() == Some(path)
    }) {
        return;
    }
    violations
        .push(Violation::new(ViolationKind::ForbiddenPath, message).with_path(path.to_path_buf()));
}

const INSTRUCTION_SCAN_MAX_ENTRIES: usize = 100_000;

fn instruction_paths(root: &Path) -> Result<Vec<PathBuf>, RunnerError> {
    let mut pending = vec![PathBuf::new()];
    let mut instructions = Vec::new();
    let mut inspected = 0_usize;
    while let Some(relative_directory) = pending.pop() {
        let directory = root.join(&relative_directory);
        let entries = fs::read_dir(&directory).map_err(|source| RunnerError::Io {
            operation: "scan repository instructions in",
            path: directory.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| RunnerError::Io {
                operation: "read repository instruction directory entry",
                path: directory.clone(),
                source,
            })?;
            inspected = inspected.saturating_add(1);
            if inspected > INSTRUCTION_SCAN_MAX_ENTRIES {
                return Err(RunnerError::Agent(format!(
                    "repository instruction scan exceeded {INSTRUCTION_SCAN_MAX_ENTRIES} entries"
                )));
            }
            let relative = relative_directory.join(entry.file_name());
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| RunnerError::Io {
                operation: "inspect repository instruction candidate",
                path: path.clone(),
                source,
            })?;
            if metadata.file_type().is_symlink() {
                if relative.file_name().and_then(|name| name.to_str()) == Some("AGENTS.md") {
                    return Err(RunnerError::UnsafePath(relative.display().to_string()));
                }
                continue;
            }
            if metadata.is_dir() {
                pending.push(relative);
            } else if metadata.is_file()
                && relative.file_name().and_then(|name| name.to_str()) == Some("AGENTS.md")
            {
                instructions.push(relative);
            }
        }
    }
    instructions.sort();
    Ok(instructions)
}

fn changed_symlink_escapes(root: &Path, relative: &Path) -> Result<bool, RunnerError> {
    let path = root.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(RunnerError::Io {
                operation: "inspect changed path",
                path,
                source,
            });
        }
    };
    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }
    match fs::canonicalize(&path) {
        Ok(resolved) => {
            let canonical_root = fs::canonicalize(root).map_err(|source| RunnerError::Io {
                operation: "canonicalize worktree for symbolic-link check",
                path: root.to_path_buf(),
                source,
            })?;
            return Ok(!resolved.starts_with(canonical_root));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(RunnerError::Io {
                operation: "resolve changed symbolic link",
                path,
                source,
            });
        }
    }
    let target = fs::read_link(&path).map_err(|source| RunnerError::Io {
        operation: "read changed symbolic link",
        path: path.clone(),
        source,
    })?;
    if target.is_absolute() {
        return Ok(true);
    }
    let Some(parent) = relative.parent() else {
        return Ok(target
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir)));
    };
    let mut depth = parent
        .components()
        .filter(|component| matches!(component, std::path::Component::Normal(_)))
        .count();
    for component in target.components() {
        match component {
            std::path::Component::Normal(_) => depth = depth.saturating_add(1),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if depth == 0 => return Ok(true),
            std::path::Component::ParentDir => depth -= 1,
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return Ok(true),
        }
    }
    Ok(false)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn deduplicate_errors(errors: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    errors
        .into_iter()
        .filter(|error| seen.insert(error.clone()))
        .collect()
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use async_trait::async_trait;
    use chrono::Utc;
    use patcharena_core::{
        RunGroup, RunGroupStatus, TaskCommand, TaskDefinition, TaskId, ViolationKind,
    };
    use patcharena_git::Repository;
    use tempfile::TempDir;

    use crate::{
        AgentContext, AgentExecution, AgentRunner, ArenaRunner, FakeAgentRunner, FakeBehavior,
        MAX_REPEAT, RunnerError, RunnerSettings,
    };

    use super::abort_group;

    fn git(directory: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> (TempDir, Repository) {
        let directory = tempfile::tempdir().expect("temp repository");
        git(directory.path(), &["init", "--quiet"]);
        fs::write(directory.path().join("file.txt"), "before\n").expect("write source");
        fs::write(directory.path().join("AGENTS.md"), "test instructions\n")
            .expect("write instructions");
        fs::write(directory.path().join(".gitignore"), ".env\n").expect("write ignore file");
        git(
            directory.path(),
            &["add", "file.txt", "AGENTS.md", ".gitignore"],
        );
        git(
            directory.path(),
            &[
                "-c",
                "user.name=PatchArena Tests",
                "-c",
                "user.email=tests@patcharena.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let repository = Repository::discover(directory.path()).expect("discover repository");
        (directory, repository)
    }

    fn task() -> TaskDefinition {
        TaskDefinition::new(
            TaskId::new("change-file").expect("task id"),
            "Change file.txt",
            [TaskCommand::new("git", ["status", "--porcelain"])],
        )
        .expect("task")
    }

    fn settings() -> RunnerSettings {
        RunnerSettings {
            timeout_seconds: 600,
            max_output_bytes: 10 * 1024 * 1024,
            max_changed_files: 8,
            max_diff_lines: 500,
            environment_allowlist: vec!["PATH".to_owned(), "HOME".to_owned()],
            forbidden_commands: vec!["git push".to_owned(), "cargo publish".to_owned()],
            forbidden_paths: vec![".git".into(), ".env".into()],
        }
    }

    fn runner(repository: Repository, root: &Path, agent: Arc<dyn AgentRunner>) -> ArenaRunner {
        ArenaRunner::new(
            repository,
            root.join(".patcharena/runs"),
            root.join(".patcharena/groups"),
            agent,
            settings(),
        )
        .expect("create runner")
    }

    #[tokio::test]
    async fn runs_repeated_isolated_worktrees_and_persists_evidence() {
        let (directory, repo) = repository();
        let agent = Arc::new(FakeAgentRunner::new(FakeBehavior::WriteFile {
            path: "file.txt".into(),
            contents: b"after\n".to_vec(),
        }));
        let execution = runner(repo, directory.path(), agent)
            .run_group(&task(), 2, true)
            .await
            .expect("run group");
        assert_eq!(execution.results.len(), 2);
        assert!(execution.results.iter().all(|result| result.success));
        assert_eq!(execution.group.requested_runs, Some(2));
        assert_eq!(execution.group.status, RunGroupStatus::Completed);
        assert!(execution.group.benchmark_identity.is_some());
        assert!(
            execution
                .results
                .iter()
                .all(|result| { result.benchmark_identity == execution.group.benchmark_identity })
        );
        assert!(
            execution
                .results
                .iter()
                .all(|result| result.changed_files == 1)
        );
        for result in &execution.results {
            let run_directory = directory
                .path()
                .join(".patcharena/runs")
                .join(&result.run_id);
            assert!(run_directory.join("result.json").is_file());
            let patch = fs::read_to_string(run_directory.join("changes.diff")).expect("read patch");
            assert!(patch.contains("+after"));
        }
        assert!(
            directory
                .path()
                .join(".patcharena/groups")
                .join(format!("{}.json", execution.group.group_id))
                .is_file()
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("file.txt")).expect("read source"),
            "before\n"
        );
    }

    #[tokio::test]
    async fn detects_forbidden_path_changes_and_large_output() {
        let (directory, repo) = repository();
        let forbidden_agent = Arc::new(FakeAgentRunner::new(FakeBehavior::WriteFile {
            path: ".env".into(),
            contents: b"not-a-real-secret\n".to_vec(),
        }));
        let execution = runner(repo, directory.path(), forbidden_agent)
            .run_group(&task(), 1, true)
            .await
            .expect("forbidden run");
        let result = &execution.results[0];
        assert!(!result.success);
        assert!(
            result
                .violations
                .iter()
                .any(|violation| violation.kind == ViolationKind::ForbiddenPath)
        );
        assert_eq!(
            result.changed_files, 0,
            "ignored .env must not enter the Git diff"
        );

        let (directory, repo) = repository();
        let output_agent = Arc::new(FakeAgentRunner::new(FakeBehavior::LargeOutput {
            bytes: 11 * 1024 * 1024,
        }));
        let execution = runner(repo, directory.path(), output_agent)
            .run_group(&task(), 1, true)
            .await
            .expect("large-output run");
        assert!(
            execution.results[0]
                .violations
                .iter()
                .any(|violation| violation.kind == ViolationKind::OutputLimit)
        );
    }

    #[derive(Debug)]
    struct InstructionProbe;

    #[async_trait]
    impl AgentRunner for InstructionProbe {
        fn name(&self) -> &str {
            "instruction-probe"
        }

        async fn run(&self, context: &AgentContext) -> Result<AgentExecution, RunnerError> {
            let started = Instant::now();
            let hidden = !context.working_dir.join("AGENTS.md").exists()
                && !context.working_dir.join("generated/AGENTS.md").exists();
            Ok(AgentExecution {
                exit_code: Some(if hidden { 0 } else { 9 }),
                timed_out: false,
                duration: started.elapsed(),
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_bytes: 0,
                stderr_bytes: 0,
                output_truncated: false,
            })
        }
    }

    #[tokio::test]
    async fn hides_and_restores_agents_in_disabled_mode() {
        let (directory, repository) = repository();
        let execution = runner(repository, directory.path(), Arc::new(InstructionProbe))
            .run_group(&task(), 1, false)
            .await
            .expect("instruction-disabled run");
        assert!(execution.results[0].success);
        assert!(!execution.results[0].instructions_enabled);
        assert_eq!(
            fs::read_to_string(directory.path().join("AGENTS.md")).expect("source instructions"),
            "test instructions\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hides_untracked_instructions_created_by_setup() {
        let (directory, repository) = repository();
        let mut configured_task = task();
        configured_task.setup.commands.push(TaskCommand::new(
            "sh",
            [
                "-c",
                "mkdir -p generated && printf 'generated instructions\\n' > generated/AGENTS.md",
            ],
        ));
        let execution = runner(repository, directory.path(), Arc::new(InstructionProbe))
            .run_group(&configured_task, 1, false)
            .await
            .expect("instruction-disabled run");
        assert!(execution.results[0].success);
        assert_eq!(execution.results[0].changed_files, 1);
    }

    #[derive(Debug)]
    struct GitConfigMutator;

    #[async_trait]
    impl AgentRunner for GitConfigMutator {
        fn name(&self) -> &str {
            "git-config-mutator"
        }

        async fn run(&self, context: &AgentContext) -> Result<AgentExecution, RunnerError> {
            let started = Instant::now();
            let status = Command::new("git")
                .args(["config", "--local", "patcharena.test", "changed"])
                .current_dir(&context.working_dir)
                .status()
                .map_err(|error| RunnerError::Agent(error.to_string()))?;
            Ok(AgentExecution {
                exit_code: status.code(),
                timed_out: false,
                duration: started.elapsed(),
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_bytes: 0,
                stderr_bytes: 0,
                output_truncated: false,
            })
        }
    }

    #[tokio::test]
    async fn detects_git_metadata_mutation_outside_the_diff() {
        let (directory, repository) = repository();
        let execution = runner(repository, directory.path(), Arc::new(GitConfigMutator))
            .run_group(&task(), 1, true)
            .await
            .expect("metadata mutation run");
        let result = &execution.results[0];
        assert!(!result.success);
        assert!(result.violations.iter().any(|violation| {
            violation.kind == ViolationKind::ForbiddenPath
                && violation.path.as_deref() == Some(Path::new(".git"))
        }));
    }

    #[tokio::test]
    async fn rejects_excessive_repeat_before_creating_runs() {
        let (directory, repository) = repository();
        let arena = runner(
            repository,
            directory.path(),
            Arc::new(FakeAgentRunner::new(FakeBehavior::Success)),
        );
        let error = arena
            .run_group(&task(), MAX_REPEAT + 1, true)
            .await
            .expect_err("repeat must be capped");
        assert!(error.to_string().contains("repeat count"));
        assert_eq!(
            fs::read_dir(directory.path().join(".patcharena/runs"))
                .expect("runs directory")
                .count(),
            0
        );
    }

    #[test]
    fn abort_checkpoint_marks_a_group_incomplete() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let group_path = directory.path().join("group.json");
        let mut group = RunGroup::new(
            TaskId::new("aborted").expect("task ID"),
            "fake",
            Utc::now(),
            3,
        )
        .expect("group");
        group.save_new(&group_path).expect("initial checkpoint");
        let error = abort_group(
            &mut group,
            &group_path,
            RunnerError::Agent("intentional hard failure".to_owned()),
        );
        assert!(matches!(error, RunnerError::GroupAborted { .. }));
        let group = RunGroup::load(&group_path).expect("load aborted group");
        assert_eq!(group.requested_runs, Some(3));
        assert_eq!(group.status, RunGroupStatus::Aborted);
        assert!(group.run_ids.is_empty());
    }

    #[derive(Debug)]
    struct MovingHeadProbe {
        source: PathBuf,
        groups_directory: PathBuf,
        moved: AtomicBool,
        observed_contents: Mutex<Vec<String>>,
        observed_group_members: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl AgentRunner for MovingHeadProbe {
        fn name(&self) -> &str {
            "moving-head-probe"
        }

        async fn run(&self, context: &AgentContext) -> Result<AgentExecution, RunnerError> {
            let started = Instant::now();
            let contents = fs::read_to_string(context.working_dir.join("file.txt"))
                .map_err(|error| RunnerError::Agent(error.to_string()))?;
            self.observed_contents
                .lock()
                .map_err(|error| RunnerError::Agent(error.to_string()))?
                .push(contents);

            let group_paths = fs::read_dir(&self.groups_directory)
                .map_err(|error| RunnerError::Agent(error.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| RunnerError::Agent(error.to_string()))?;
            if group_paths.len() != 1 {
                return Err(RunnerError::Agent(format!(
                    "expected one incrementally persisted group, found {}",
                    group_paths.len()
                )));
            }
            let group = RunGroup::load(group_paths[0].path())?;
            self.observed_group_members
                .lock()
                .map_err(|error| RunnerError::Agent(error.to_string()))?
                .push(group.run_ids.len());

            if !self.moved.swap(true, Ordering::SeqCst) {
                fs::write(self.source.join("file.txt"), "new head\n")
                    .map_err(|error| RunnerError::Agent(error.to_string()))?;
                git(&self.source, &["add", "file.txt"]);
                git(
                    &self.source,
                    &[
                        "-c",
                        "user.name=PatchArena Tests",
                        "-c",
                        "user.email=tests@patcharena.invalid",
                        "commit",
                        "--quiet",
                        "-m",
                        "move head during benchmark",
                    ],
                );
            }

            Ok(AgentExecution {
                exit_code: Some(0),
                timed_out: false,
                duration: started.elapsed(),
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_bytes: 0,
                stderr_bytes: 0,
                output_truncated: false,
            })
        }
    }

    #[tokio::test]
    async fn pins_commit_and_persists_group_between_repeats() {
        let (directory, repository) = repository();
        let pinned_commit = repository.resolve_commit("HEAD").expect("initial HEAD");
        let probe = Arc::new(MovingHeadProbe {
            source: directory.path().to_path_buf(),
            groups_directory: directory.path().join(".patcharena/groups"),
            moved: AtomicBool::new(false),
            observed_contents: Mutex::new(Vec::new()),
            observed_group_members: Mutex::new(Vec::new()),
        });
        let execution = runner(repository.clone(), directory.path(), probe.clone())
            .run_group(&task(), 2, true)
            .await
            .expect("pinned repeated run");
        assert_eq!(
            probe
                .observed_contents
                .lock()
                .expect("observations")
                .as_slice(),
            ["before\n", "before\n"]
        );
        assert_eq!(
            probe
                .observed_group_members
                .lock()
                .expect("group observations")
                .as_slice(),
            [0, 1]
        );
        assert_eq!(
            execution
                .group
                .benchmark_identity
                .as_ref()
                .expect("benchmark identity")
                .repository_commit,
            pinned_commit
        );
        assert_ne!(
            repository.resolve_commit("HEAD").expect("moved HEAD"),
            pinned_commit
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_artifact_directory_symlink_before_creating_outside() {
        use std::os::unix::fs::symlink;

        let (directory, repository) = repository();
        let outside = tempfile::tempdir().expect("outside directory");
        symlink(outside.path(), directory.path().join(".patcharena")).expect("state symlink");
        let result = ArenaRunner::new(
            repository,
            directory.path().join(".patcharena/runs"),
            directory.path().join(".patcharena/groups"),
            Arc::new(FakeAgentRunner::new(FakeBehavior::Success)),
            settings(),
        );
        assert!(result.is_err());
        assert!(!outside.path().join("runs").exists());
    }

    #[test]
    fn duration_conversion_saturates() {
        assert_eq!(super::duration_ms(Duration::from_millis(7)), 7);
    }
}
