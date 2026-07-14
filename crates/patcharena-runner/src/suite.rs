//! Checkpointed multi-task, multi-agent benchmark suite orchestration.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
pub use patcharena_core::MAX_SUITE_INVOCATIONS;
use patcharena_core::{
    SuiteCellStatus, SuiteDefinition, SuiteExecution, SuiteExecutionStatus, SuiteTaskSnapshot,
    TaskDefinition, suite_checkpoint_path, suite_run_directory,
};
use patcharena_git::Repository;

use crate::orchestration::{create_private_directory, ensure_private_contained_directory};
use crate::{
    AgentRunner, ArenaRunner, MAX_REPEAT, RunnerError, RunnerSettings, benchmark_identity,
};

const MAX_SUITE_AGENTS: usize = 100;

/// One explicit agent selected for a suite in stable CLI order.
#[derive(Clone)]
pub struct SelectedSuiteAgent {
    /// Stable agent ID stored in execution and group records.
    pub id: String,
    /// Resolved executable agent implementation.
    pub runner: Arc<dyn AgentRunner>,
}

impl std::fmt::Debug for SelectedSuiteAgent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SelectedSuiteAgent")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// An immutable, fully validated suite execution plan.
#[derive(Clone, Debug)]
pub struct SuitePlan {
    /// Suite definition loaded for this plan.
    definition: SuiteDefinition,
    /// Task documents loaded once in suite order.
    tasks: Vec<TaskDefinition>,
    /// Per-task expected benchmark identities pinned during preflight.
    task_snapshots: Vec<SuiteTaskSnapshot>,
    /// Explicit agent IDs in execution order.
    agents: Vec<String>,
    /// Repetitions requested for every task-agent cell.
    repeat: u32,
    /// Whether repository instruction files remain visible.
    instructions_enabled: bool,
    /// Shared repository commit pinned before agent execution.
    repository_commit: String,
    /// Fingerprint of the canonical suite definition.
    suite_fingerprint: String,
    /// Total planned agent invocations across all cells.
    invocation_count: u64,
}

impl SuitePlan {
    /// Validated suite definition.
    #[must_use]
    pub fn definition(&self) -> &SuiteDefinition {
        &self.definition
    }

    /// Validated task documents in execution order.
    #[must_use]
    pub fn tasks(&self) -> &[TaskDefinition] {
        &self.tasks
    }

    /// Pinned task identities in execution order.
    #[must_use]
    pub fn task_snapshots(&self) -> &[SuiteTaskSnapshot] {
        &self.task_snapshots
    }

    /// Explicit agent IDs in execution order.
    #[must_use]
    pub fn agents(&self) -> &[String] {
        &self.agents
    }

    /// Independent invocations requested for every cell.
    #[must_use]
    pub const fn repeat(&self) -> u32 {
        self.repeat
    }

    /// Whether repository instruction files remain visible.
    #[must_use]
    pub const fn instructions_enabled(&self) -> bool {
        self.instructions_enabled
    }

    /// Full repository commit pinned during preflight.
    #[must_use]
    pub fn repository_commit(&self) -> &str {
        &self.repository_commit
    }

    /// Canonical suite-definition fingerprint.
    #[must_use]
    pub fn suite_fingerprint(&self) -> &str {
        &self.suite_fingerprint
    }

    /// Checked total invocation count.
    #[must_use]
    pub const fn invocation_count(&self) -> u64 {
        self.invocation_count
    }
}

/// One durably checkpointed cell-completion event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuiteCellProgress {
    /// Number of completed or error cells now stored in the checkpoint.
    pub finished_cells: usize,
    /// Total cells in the immutable execution matrix.
    pub total_cells: usize,
    /// Stable task ID.
    pub task_id: String,
    /// Stable agent ID.
    pub agent_id: String,
    /// Durable terminal cell status.
    pub status: SuiteCellStatus,
    /// Immutable group UUID for a completed cell.
    pub group_id: Option<String>,
    /// Bounded diagnostic for an orchestration-error cell.
    pub error: Option<String>,
}

type ProgressCallback = dyn Fn(&SuiteCellProgress) + Send + Sync;

/// Final execution state plus its durable checkpoint location.
#[derive(Clone, Debug)]
pub struct SuiteExecutionOutcome {
    /// Terminal or still-inspectable suite execution model.
    pub execution: SuiteExecution,
    /// Absolute path to the execution's `suite.json` checkpoint.
    pub checkpoint_path: PathBuf,
}

/// Coordinates preflight, sequential group execution, checkpointing, and resume.
pub struct SuiteRunner {
    repository: Repository,
    runs_directory: PathBuf,
    groups_directory: PathBuf,
    suite_runs_directory: PathBuf,
    agents: Vec<SelectedSuiteAgent>,
    settings: RunnerSettings,
    patcharena_version: String,
    progress: Option<Arc<ProgressCallback>>,
}

impl std::fmt::Debug for SuiteRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SuiteRunner")
            .field("repository", &self.repository)
            .field("runs_directory", &self.runs_directory)
            .field("groups_directory", &self.groups_directory)
            .field("suite_runs_directory", &self.suite_runs_directory)
            .field("agents", &self.agents)
            .field("settings", &self.settings)
            .field("patcharena_version", &self.patcharena_version)
            .field("progress_enabled", &self.progress.is_some())
            .finish()
    }
}

impl SuiteRunner {
    /// Create a suite orchestrator after validating paths and selected agent identities.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repository: Repository,
        runs_directory: impl Into<PathBuf>,
        groups_directory: impl Into<PathBuf>,
        suite_runs_directory: impl Into<PathBuf>,
        agents: Vec<SelectedSuiteAgent>,
        settings: RunnerSettings,
        patcharena_version: impl Into<String>,
    ) -> Result<Self, RunnerError> {
        let runs_directory = runs_directory.into();
        let groups_directory = groups_directory.into();
        let suite_runs_directory = suite_runs_directory.into();
        ensure_private_contained_directory(repository.root(), &runs_directory)?;
        ensure_private_contained_directory(repository.root(), &groups_directory)?;
        ensure_private_contained_directory(repository.root(), &suite_runs_directory)?;
        if agents.is_empty() {
            return Err(RunnerError::Agent(
                "a suite requires at least one explicit agent".to_owned(),
            ));
        }
        if agents.len() > MAX_SUITE_AGENTS {
            return Err(RunnerError::Agent(format!(
                "a suite supports at most {MAX_SUITE_AGENTS} explicit agents"
            )));
        }
        let mut ids = HashSet::with_capacity(agents.len());
        for agent in &agents {
            if !portable_agent_id(&agent.id) {
                return Err(RunnerError::Agent(format!(
                    "suite agent ID `{}` must use 1 to 128 lowercase ASCII letters, digits, or hyphens",
                    agent.id
                )));
            }
            if agent.id != agent.runner.name() {
                return Err(RunnerError::Agent(format!(
                    "selected agent ID `{}` does not match runner name `{}`",
                    agent.id,
                    agent.runner.name()
                )));
            }
            if !ids.insert(agent.id.as_str()) {
                return Err(RunnerError::Agent(format!(
                    "duplicate suite agent `{}`",
                    agent.id
                )));
            }
        }
        let patcharena_version = patcharena_version.into();
        if patcharena_version.trim().is_empty() {
            return Err(RunnerError::Agent(
                "PatchArena version must not be blank".to_owned(),
            ));
        }
        Ok(Self {
            repository,
            runs_directory,
            groups_directory,
            suite_runs_directory,
            agents,
            settings,
            patcharena_version,
            progress: None,
        })
    }

    /// Report each terminal cell immediately after its checkpoint replacement succeeds.
    #[must_use]
    pub fn with_progress<F>(mut self, callback: F) -> Self
    where
        F: Fn(&SuiteCellProgress) + Send + Sync + 'static,
    {
        self.progress = Some(Arc::new(callback));
        self
    }

    /// Validate a complete suite plan without creating run, group, or suite-run records.
    pub fn preflight(
        &self,
        suite: &SuiteDefinition,
        tasks: Vec<TaskDefinition>,
        repeat: u32,
        instructions_enabled: bool,
    ) -> Result<SuitePlan, RunnerError> {
        suite.validate()?;
        if repeat == 0 || repeat > MAX_REPEAT {
            return Err(RunnerError::Agent(format!(
                "repeat count must be between 1 and {MAX_REPEAT}"
            )));
        }
        if tasks.len() != suite.tasks.len()
            || tasks
                .iter()
                .zip(&suite.tasks)
                .any(|(task, expected)| &task.id != expected)
        {
            return Err(RunnerError::Agent(
                "loaded tasks must exactly match suite task order".to_owned(),
            ));
        }
        for task in &tasks {
            task.validate()?;
        }
        self.repository.ensure_tracked_clean()?;
        if !self.repository.status_porcelain()?.is_empty() {
            tracing::warn!(
                "untracked files are not copied to benchmark worktrees; only committed HEAD is evaluated"
            );
        }
        let repository_commit = self.repository.resolve_commit("HEAD")?;
        let invocation_count = checked_invocation_count(tasks.len(), self.agents.len(), repeat)?;
        let task_snapshots = tasks
            .iter()
            .map(|task| {
                let identity = benchmark_identity(&self.repository, &self.settings, task)?;
                if identity.repository_commit != repository_commit {
                    return Err(RunnerError::Agent(format!(
                        "task `{}` resolved a different repository commit during preflight",
                        task.id
                    )));
                }
                SuiteTaskSnapshot::new(task.id.clone(), identity).map_err(Into::into)
            })
            .collect::<Result<Vec<_>, RunnerError>>()?;
        Ok(SuitePlan {
            definition: suite.clone(),
            tasks,
            task_snapshots,
            agents: self.agents.iter().map(|agent| agent.id.clone()).collect(),
            repeat,
            instructions_enabled,
            repository_commit,
            suite_fingerprint: suite.fingerprint()?,
            invocation_count,
        })
    }

    /// Execute a preflighted suite plan and return its durable terminal checkpoint.
    pub async fn execute(&self, plan: SuitePlan) -> Result<SuiteExecutionOutcome, RunnerError> {
        let execution = self.create_checkpoint(&plan)?;
        self.execute_pending(execution, &plan).await
    }

    /// Resume only pending cells after revalidating every recorded comparison input.
    pub async fn resume(
        &self,
        execution: SuiteExecution,
        suite: &SuiteDefinition,
        tasks: Vec<TaskDefinition>,
    ) -> Result<SuiteExecutionOutcome, RunnerError> {
        execution.validate()?;
        if execution.status != SuiteExecutionStatus::Running {
            return Err(RunnerError::Agent(
                "only a running suite execution can be resumed".to_owned(),
            ));
        }
        if execution.suite_id != suite.id {
            return Err(RunnerError::Agent(
                "suite ID changed since the execution checkpoint".to_owned(),
            ));
        }
        let plan = self.preflight(
            suite,
            tasks,
            execution.repeat,
            execution.instructions_enabled,
        )?;
        if execution.suite_fingerprint != plan.suite_fingerprint
            || execution.repository_commit != plan.repository_commit
            || execution.tasks != plan.task_snapshots
            || execution.agents != plan.agents
        {
            return Err(RunnerError::Agent(
                "suite definition, task policy, agents, or repository commit changed since checkpoint"
                    .to_owned(),
            ));
        }
        let checkpoint = self.validate_checkpoint_directory(&execution)?;
        validate_regular_checkpoint(&checkpoint)?;
        self.execute_pending(execution, &plan).await
    }

    fn create_checkpoint(&self, plan: &SuitePlan) -> Result<SuiteExecution, RunnerError> {
        self.validate_plan(plan)?;
        let execution = SuiteExecution::new(
            self.patcharena_version.clone(),
            plan.definition.id.clone(),
            plan.suite_fingerprint.clone(),
            plan.repository_commit.clone(),
            plan.task_snapshots.clone(),
            plan.agents.clone(),
            plan.repeat,
            plan.instructions_enabled,
            Utc::now(),
        )?;
        let directory = suite_run_directory(&self.suite_runs_directory, &execution.suite_run_id)?;
        create_private_directory(&directory)?;
        execution.save_new(self.checkpoint_path(&execution)?)?;
        Ok(execution)
    }

    fn checkpoint_path(&self, execution: &SuiteExecution) -> Result<PathBuf, RunnerError> {
        suite_checkpoint_path(&self.suite_runs_directory, &execution.suite_run_id)
            .map_err(Into::into)
    }

    async fn execute_pending(
        &self,
        mut execution: SuiteExecution,
        plan: &SuitePlan,
    ) -> Result<SuiteExecutionOutcome, RunnerError> {
        let checkpoint_path = self.validate_checkpoint_directory(&execution)?;
        validate_regular_checkpoint(&checkpoint_path)?;
        let pending = execution.pending_cells().cloned().collect::<Vec<_>>();
        for cell in pending {
            if let Err(error) = self.validate_shared_basis(plan, &cell.task_id) {
                self.abort_checkpoint(&mut execution)?;
                return Err(error);
            }
            let task = plan
                .tasks
                .iter()
                .find(|task| task.id == cell.task_id)
                .ok_or_else(|| {
                    RunnerError::Agent(format!("planned task `{}` is missing", cell.task_id))
                })?;
            let agent = self
                .agents
                .iter()
                .find(|agent| agent.id == cell.agent_id)
                .ok_or_else(|| {
                    RunnerError::Agent(format!("planned agent `{}` is missing", cell.agent_id))
                })?;
            let arena = ArenaRunner::new(
                self.repository.clone(),
                &self.runs_directory,
                &self.groups_directory,
                Arc::clone(&agent.runner),
                self.settings.clone(),
            )?;
            match arena
                .run_group(task, plan.repeat, plan.instructions_enabled)
                .await
            {
                Ok(group) => {
                    if let Err(error) = self.validate_shared_basis(plan, &cell.task_id) {
                        self.abort_checkpoint(&mut execution)?;
                        return Err(error);
                    }
                    if let Err(error) = self.validate_group(&group.group, plan, task, &agent.id) {
                        self.abort_checkpoint(&mut execution)?;
                        return Err(error);
                    }
                    if let Err(error) = execution.complete_cell(
                        task.id.as_str(),
                        &agent.id,
                        group.group.group_id,
                        Utc::now(),
                    ) {
                        self.abort_checkpoint(&mut execution)?;
                        return Err(error.into());
                    }
                }
                Err(error) => {
                    let diagnostic = bounded_error(&error.to_string());
                    execution.error_cell(task.id.as_str(), &agent.id, &diagnostic, Utc::now())?;
                }
            }
            self.replace_checkpoint(&execution)?;
            self.emit_progress(&execution, cell.task_id.as_str(), &cell.agent_id);
        }
        execution.mark_finished(Utc::now())?;
        self.replace_checkpoint(&execution)?;
        Ok(SuiteExecutionOutcome {
            execution,
            checkpoint_path,
        })
    }

    fn abort_checkpoint(&self, execution: &mut SuiteExecution) -> Result<(), RunnerError> {
        execution.mark_aborted(Utc::now())?;
        self.replace_checkpoint(execution)?;
        Ok(())
    }

    fn replace_checkpoint(&self, execution: &SuiteExecution) -> Result<(), RunnerError> {
        let checkpoint = self.validate_checkpoint_directory(execution)?;
        validate_regular_checkpoint(&checkpoint)?;
        execution.save_replace(checkpoint)?;
        Ok(())
    }

    fn emit_progress(&self, execution: &SuiteExecution, task_id: &str, agent_id: &str) {
        let Some(callback) = &self.progress else {
            return;
        };
        let Some(cell) = execution
            .cells
            .iter()
            .find(|cell| cell.task_id.as_str() == task_id && cell.agent_id == agent_id)
        else {
            return;
        };
        let progress = SuiteCellProgress {
            finished_cells: execution
                .cells
                .iter()
                .filter(|cell| cell.status != SuiteCellStatus::Pending)
                .count(),
            total_cells: execution.cells.len(),
            task_id: cell.task_id.to_string(),
            agent_id: cell.agent_id.clone(),
            status: cell.status,
            group_id: cell.group_id.clone(),
            error: cell.error.clone(),
        };
        callback(&progress);
    }

    fn validate_plan_agents(&self, plan: &SuitePlan) -> Result<(), RunnerError> {
        let agents = self
            .agents
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>();
        let planned = plan.agents.iter().map(String::as_str).collect::<Vec<_>>();
        if agents != planned {
            return Err(RunnerError::Agent(
                "selected agents do not match the preflight plan".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_plan(&self, plan: &SuitePlan) -> Result<(), RunnerError> {
        plan.definition.validate()?;
        if plan.repeat == 0 || plan.repeat > MAX_REPEAT {
            return Err(RunnerError::Agent(format!(
                "repeat count must be between 1 and {MAX_REPEAT}"
            )));
        }
        if plan.tasks.len() != plan.definition.tasks.len()
            || plan
                .tasks
                .iter()
                .zip(&plan.definition.tasks)
                .any(|(task, expected)| &task.id != expected)
        {
            return Err(RunnerError::Agent(
                "planned tasks do not match the suite definition".to_owned(),
            ));
        }
        for task in &plan.tasks {
            task.validate()?;
        }
        if plan.task_snapshots.len() != plan.tasks.len()
            || plan
                .task_snapshots
                .iter()
                .zip(&plan.tasks)
                .any(|(snapshot, task)| snapshot.task_id != task.id)
        {
            return Err(RunnerError::Agent(
                "planned task snapshots do not match task order".to_owned(),
            ));
        }
        self.validate_plan_agents(plan)?;
        let fingerprint = plan.definition.fingerprint()?;
        if plan.suite_fingerprint != fingerprint {
            return Err(RunnerError::Agent(
                "suite fingerprint changed after preflight".to_owned(),
            ));
        }
        let invocation_count =
            checked_invocation_count(plan.tasks.len(), plan.agents.len(), plan.repeat)?;
        if invocation_count != plan.invocation_count {
            return Err(RunnerError::Agent(
                "suite invocation count changed after preflight".to_owned(),
            ));
        }
        for task in &plan.tasks {
            self.validate_shared_basis(plan, &task.id)?;
        }
        Ok(())
    }

    fn validate_checkpoint_directory(
        &self,
        execution: &SuiteExecution,
    ) -> Result<PathBuf, RunnerError> {
        let directory = suite_run_directory(&self.suite_runs_directory, &execution.suite_run_id)?;
        let metadata = fs::symlink_metadata(&directory).map_err(|source| RunnerError::Io {
            operation: "inspect suite-run directory",
            path: directory.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(RunnerError::UnsafePath(directory.display().to_string()));
        }
        ensure_private_contained_directory(self.repository.root(), &directory)?;
        self.checkpoint_path(execution)
    }

    fn validate_shared_basis(
        &self,
        plan: &SuitePlan,
        task_id: &patcharena_core::TaskId,
    ) -> Result<(), RunnerError> {
        let current_commit = self.repository.resolve_commit("HEAD")?;
        if current_commit != plan.repository_commit {
            return Err(RunnerError::Agent(format!(
                "repository HEAD changed from `{}` to `{current_commit}` during suite execution",
                plan.repository_commit
            )));
        }
        let task = plan
            .tasks
            .iter()
            .find(|task| &task.id == task_id)
            .ok_or_else(|| RunnerError::Agent(format!("task `{task_id}` is missing")))?;
        let expected = plan
            .task_snapshots
            .iter()
            .find(|snapshot| &snapshot.task_id == task_id)
            .ok_or_else(|| RunnerError::Agent(format!("task snapshot `{task_id}` is missing")))?;
        if benchmark_identity(&self.repository, &self.settings, task)?
            != expected.benchmark_identity
        {
            return Err(RunnerError::Agent(format!(
                "task `{task_id}` benchmark identity changed during suite execution"
            )));
        }
        Ok(())
    }

    fn validate_group(
        &self,
        group: &patcharena_core::RunGroup,
        plan: &SuitePlan,
        task: &TaskDefinition,
        agent_id: &str,
    ) -> Result<(), RunnerError> {
        let expected = plan
            .task_snapshots
            .iter()
            .find(|snapshot| snapshot.task_id == task.id)
            .ok_or_else(|| RunnerError::Agent(format!("task snapshot `{}` is missing", task.id)))?;
        if group.task_id != task.id
            || group.agent != agent_id
            || group.instructions_enabled != plan.instructions_enabled
            || group.requested_runs != Some(plan.repeat)
            || group.benchmark_identity.as_ref() != Some(&expected.benchmark_identity)
        {
            return Err(RunnerError::Agent(format!(
                "group `{}` does not match its suite cell plan",
                group.group_id
            )));
        }
        Ok(())
    }
}

fn checked_invocation_count(
    task_count: usize,
    agent_count: usize,
    repeat: u32,
) -> Result<u64, RunnerError> {
    let invocation_count = u64::try_from(task_count)
        .unwrap_or(u64::MAX)
        .checked_mul(u64::try_from(agent_count).unwrap_or(u64::MAX))
        .and_then(|count| count.checked_mul(u64::from(repeat)))
        .ok_or_else(|| RunnerError::Agent("suite invocation count overflowed".to_owned()))?;
    if invocation_count > MAX_SUITE_INVOCATIONS {
        return Err(RunnerError::Agent(format!(
            "suite plans at most 1,000 agent invocations; requested {invocation_count}"
        )));
    }
    Ok(invocation_count)
}

fn validate_regular_checkpoint(path: &std::path::Path) -> Result<(), RunnerError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| RunnerError::Io {
        operation: "inspect suite checkpoint",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RunnerError::UnsafePath(path.display().to_string()));
    }
    Ok(())
}

fn portable_agent_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn bounded_error(value: &str) -> String {
    const LIMIT: usize = 4096;
    if value.len() <= LIMIT {
        return value.to_owned();
    }
    let mut end = LIMIT;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::Utc;
    use patcharena_core::{
        SuiteDefinition, SuiteExecution, SuiteExecutionStatus, SuiteId, TaskCommand,
        TaskDefinition, TaskId,
    };
    use patcharena_git::Repository;
    use tempfile::TempDir;

    use crate::{
        AgentContext, AgentExecution, AgentRunner, ArenaRunner, RunnerError, RunnerSettings,
    };

    use super::{SelectedSuiteAgent, SuiteRunner};

    #[derive(Debug)]
    struct NamedAgent {
        name: String,
        exit_code: i32,
    }

    #[derive(Debug)]
    struct FlippingNameAgent {
        name_calls: AtomicUsize,
    }

    #[derive(Debug)]
    struct HeadChangingAgent {
        repository_root: PathBuf,
    }

    #[async_trait]
    impl AgentRunner for FlippingNameAgent {
        fn name(&self) -> &str {
            if self.name_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                "alpha"
            } else {
                "different"
            }
        }

        async fn run(&self, _context: &AgentContext) -> Result<AgentExecution, RunnerError> {
            Ok(AgentExecution {
                exit_code: Some(0),
                timed_out: false,
                duration: Duration::from_millis(1),
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_bytes: 0,
                stderr_bytes: 0,
                output_truncated: false,
            })
        }
    }

    #[async_trait]
    impl AgentRunner for HeadChangingAgent {
        fn name(&self) -> &str {
            "alpha"
        }

        async fn run(&self, _context: &AgentContext) -> Result<AgentExecution, RunnerError> {
            git(
                &self.repository_root,
                &["commit", "--quiet", "--allow-empty", "-m", "drift"],
            );
            Ok(AgentExecution {
                exit_code: Some(0),
                timed_out: false,
                duration: Duration::from_millis(1),
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_bytes: 0,
                stderr_bytes: 0,
                output_truncated: false,
            })
        }
    }

    #[async_trait]
    impl AgentRunner for NamedAgent {
        fn name(&self) -> &str {
            &self.name
        }

        async fn run(&self, _context: &AgentContext) -> Result<AgentExecution, RunnerError> {
            Ok(AgentExecution {
                exit_code: Some(self.exit_code),
                timed_out: false,
                duration: Duration::from_millis(1),
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_bytes: 0,
                stderr_bytes: 0,
                output_truncated: false,
            })
        }
    }

    struct SuiteFixture {
        _directory: TempDir,
        repository: Repository,
        runs: PathBuf,
        groups: PathBuf,
        suite_runs: PathBuf,
        suite: SuiteDefinition,
        tasks: Vec<TaskDefinition>,
    }

    impl SuiteFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("temporary repository");
            git(directory.path(), &["init", "--quiet"]);
            git(
                directory.path(),
                &["config", "user.email", "test@example.com"],
            );
            git(
                directory.path(),
                &["config", "user.name", "PatchArena Test"],
            );
            fs::write(directory.path().join("file.txt"), "baseline\n").expect("source file");
            git(directory.path(), &["add", "file.txt"]);
            git(directory.path(), &["commit", "--quiet", "-m", "initial"]);
            let repository = Repository::discover(directory.path()).expect("repository");
            let state = directory.path().join(".patcharena");
            let runs = state.join("runs");
            let groups = state.join("groups");
            let suite_runs = state.join("suite-runs");
            for path in [&runs, &groups, &suite_runs] {
                fs::create_dir_all(path).expect("artifact directory");
            }
            let tasks = vec![task("one"), task("two")];
            let suite = SuiteDefinition::new(
                SuiteId::new("core").unwrap(),
                Some("Core suite".to_owned()),
                tasks.iter().map(|task| task.id.clone()).collect(),
            )
            .unwrap();
            Self {
                _directory: directory,
                repository,
                runs,
                groups,
                suite_runs,
                suite,
                tasks,
            }
        }

        fn runner(&self, agents: Vec<SelectedSuiteAgent>) -> SuiteRunner {
            SuiteRunner::new(
                self.repository.clone(),
                &self.runs,
                &self.groups,
                &self.suite_runs,
                agents,
                settings(),
                "0.3.0",
            )
            .expect("suite runner")
        }

        fn group_count(&self) -> usize {
            fs::read_dir(&self.groups).expect("groups").count()
        }
    }

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

    fn task(id: &str) -> TaskDefinition {
        TaskDefinition::new(
            TaskId::new(id).unwrap(),
            "Leave the fixture valid.",
            [TaskCommand::new("true", std::iter::empty::<&str>())],
        )
        .unwrap()
    }

    fn settings() -> RunnerSettings {
        RunnerSettings {
            timeout_seconds: 10,
            max_output_bytes: 1024,
            max_changed_files: 8,
            max_diff_lines: 500,
            environment_allowlist: vec!["PATH".to_owned()],
            forbidden_commands: vec!["git push".to_owned()],
            forbidden_paths: vec![PathBuf::from(".git")],
        }
    }

    fn named_agent(name: &str, exit_code: i32) -> SelectedSuiteAgent {
        SelectedSuiteAgent {
            id: name.to_owned(),
            runner: Arc::new(NamedAgent {
                name: name.to_owned(),
                exit_code,
            }),
        }
    }

    #[test]
    fn preflight_builds_stable_plan_and_caps_invocations() {
        let fixture = SuiteFixture::new();
        let runner = fixture.runner(vec![named_agent("alpha", 0), named_agent("beta", 0)]);
        let plan = runner
            .preflight(&fixture.suite, fixture.tasks.clone(), 3, true)
            .expect("preflight");
        assert_eq!(plan.invocation_count, 12);
        assert_eq!(
            plan.repository_commit,
            fixture.repository.resolve_commit("HEAD").unwrap()
        );
        assert_eq!(plan.task_snapshots.len(), 2);
        assert!(
            plan.task_snapshots.iter().all(|task| {
                task.benchmark_identity.repository_commit == plan.repository_commit
            })
        );

        let error = runner
            .preflight(&fixture.suite, fixture.tasks.clone(), 251, true)
            .expect_err("1004 invocations must be rejected");
        assert!(error.to_string().contains("1,000"));
    }

    #[test]
    fn suite_runner_rejects_more_agents_than_the_execution_schema() {
        let fixture = SuiteFixture::new();
        let agents = (0..101)
            .map(|index| named_agent(&format!("agent-{index}"), 0))
            .collect();

        let result = SuiteRunner::new(
            fixture.repository.clone(),
            &fixture.runs,
            &fixture.groups,
            &fixture.suite_runs,
            agents,
            settings(),
            "0.3.0",
        );

        assert!(
            matches!(result, Err(RunnerError::Agent(message)) if message.contains("at most 100"))
        );
    }

    #[test]
    fn suite_runner_rejects_nonportable_agent_ids_during_construction() {
        let fixture = SuiteFixture::new();
        for id in ["Upper Case".to_owned(), "a".repeat(129)] {
            let result = SuiteRunner::new(
                fixture.repository.clone(),
                &fixture.runs,
                &fixture.groups,
                &fixture.suite_runs,
                vec![named_agent(&id, 0)],
                settings(),
                "0.3.0",
            );

            assert!(
                matches!(result, Err(RunnerError::Agent(message)) if message.contains("1 to 128"))
            );
        }
    }

    #[tokio::test]
    async fn group_plan_mismatch_aborts_the_persisted_suite_checkpoint() {
        let fixture = SuiteFixture::new();
        let runner = fixture.runner(vec![SelectedSuiteAgent {
            id: "alpha".to_owned(),
            runner: Arc::new(FlippingNameAgent {
                name_calls: AtomicUsize::new(0),
            }),
        }]);
        let plan = runner
            .preflight(&fixture.suite, fixture.tasks.clone(), 1, true)
            .unwrap();

        assert!(runner.execute(plan).await.is_err());
        let checkpoint = fs::read_dir(&fixture.suite_runs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("suite.json");
        let execution = SuiteExecution::load(checkpoint).unwrap();
        assert_eq!(execution.status, SuiteExecutionStatus::Aborted);
    }

    #[tokio::test]
    async fn execute_rejects_a_plan_mutated_after_preflight_before_creating_artifacts() {
        let fixture = SuiteFixture::new();
        let runner = fixture.runner(vec![named_agent("alpha", 0)]);
        let mut plan = runner
            .preflight(&fixture.suite, fixture.tasks.clone(), 1, true)
            .unwrap();
        plan.repeat = 2;

        let error = runner
            .execute(plan)
            .await
            .expect_err("a changed plan must be revalidated");

        assert!(error.to_string().contains("invocation count"));
        assert_eq!(fixture.group_count(), 0);
        assert_eq!(fs::read_dir(&fixture.suite_runs).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn head_change_during_the_last_cell_aborts_its_checkpoint() {
        let fixture = SuiteFixture::new();
        let task = fixture.tasks[0].clone();
        let suite =
            SuiteDefinition::new(SuiteId::new("single").unwrap(), None, vec![task.id.clone()])
                .unwrap();
        let runner = fixture.runner(vec![SelectedSuiteAgent {
            id: "alpha".to_owned(),
            runner: Arc::new(HeadChangingAgent {
                repository_root: fixture.repository.root().to_path_buf(),
            }),
        }]);
        let plan = runner.preflight(&suite, vec![task], 1, true).unwrap();

        let error = runner
            .execute(plan)
            .await
            .expect_err("HEAD drift during the final cell must abort the suite");

        assert!(error.to_string().contains("HEAD changed"));
        let checkpoint = fs::read_dir(&fixture.suite_runs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("suite.json");
        let execution = SuiteExecution::load(checkpoint).unwrap();
        assert_eq!(execution.status, SuiteExecutionStatus::Aborted);
        assert_eq!(
            execution.cells[0].status,
            patcharena_core::SuiteCellStatus::Pending
        );
    }

    #[test]
    fn dry_preflight_creates_no_group_or_suite_artifact() {
        let fixture = SuiteFixture::new();
        let runner = fixture.runner(vec![named_agent("alpha", 0)]);
        runner
            .preflight(&fixture.suite, fixture.tasks.clone(), 1, true)
            .unwrap();
        assert_eq!(fixture.group_count(), 0);
        assert_eq!(fs::read_dir(&fixture.suite_runs).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn execute_checkpoints_every_cell_and_keeps_benchmark_failures_as_groups() {
        let fixture = SuiteFixture::new();
        let runner = fixture.runner(vec![named_agent("alpha", 0), named_agent("beta", 1)]);
        let plan = runner
            .preflight(&fixture.suite, fixture.tasks.clone(), 1, true)
            .unwrap();
        let outcome = runner.execute(plan).await.expect("suite execution");
        assert_eq!(outcome.execution.status, SuiteExecutionStatus::Completed);
        assert_eq!(outcome.execution.cells.len(), 4);
        assert!(
            outcome
                .execution
                .cells
                .iter()
                .all(|cell| cell.group_id.is_some())
        );
        assert_eq!(fixture.group_count(), 4);
        assert_eq!(
            SuiteExecution::load(outcome.checkpoint_path).unwrap(),
            outcome.execution
        );
    }

    #[tokio::test]
    async fn progress_events_are_emitted_after_the_matching_checkpoint_is_durable() {
        let fixture = SuiteFixture::new();
        let observations = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&observations);
        let suite_runs = fixture.suite_runs.clone();
        let runner = fixture
            .runner(vec![named_agent("alpha", 0)])
            .with_progress(move |progress| {
                let checkpoint = fs::read_dir(&suite_runs)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path()
                    .join("suite.json");
                let persisted = SuiteExecution::load(checkpoint).unwrap();
                let persisted_finished = persisted
                    .cells
                    .iter()
                    .filter(|cell| cell.status != patcharena_core::SuiteCellStatus::Pending)
                    .count();
                assert_eq!(persisted_finished, progress.finished_cells);
                observed.lock().unwrap().push(progress.finished_cells);
            });
        let plan = runner
            .preflight(&fixture.suite, fixture.tasks.clone(), 1, true)
            .unwrap();

        runner.execute(plan).await.unwrap();

        assert_eq!(*observations.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn resume_runs_pending_cells_without_repeating_completed_cells() {
        let fixture = SuiteFixture::new();
        let alpha = named_agent("alpha", 0);
        let runner = fixture.runner(vec![alpha.clone(), named_agent("beta", 0)]);
        let plan = runner
            .preflight(&fixture.suite, fixture.tasks.clone(), 1, true)
            .unwrap();
        let mut execution = runner.create_checkpoint(&plan).expect("checkpoint");
        let arena = ArenaRunner::new(
            fixture.repository.clone(),
            &fixture.runs,
            &fixture.groups,
            alpha.runner,
            settings(),
        )
        .unwrap();
        let existing = arena
            .run_group(&fixture.tasks[0], 1, true)
            .await
            .expect("existing group");
        execution
            .complete_cell("one", "alpha", existing.group.group_id, Utc::now())
            .unwrap();
        execution
            .save_replace(runner.checkpoint_path(&execution).unwrap())
            .unwrap();

        let outcome = runner
            .resume(execution, &fixture.suite, fixture.tasks.clone())
            .await
            .expect("resume");
        assert_eq!(outcome.execution.status, SuiteExecutionStatus::Completed);
        assert_eq!(fixture.group_count(), 4);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resume_refuses_a_symlinked_suite_run_directory() {
        use std::os::unix::fs::symlink;

        let fixture = SuiteFixture::new();
        let runner = fixture.runner(vec![named_agent("alpha", 0)]);
        let plan = runner
            .preflight(&fixture.suite, fixture.tasks.clone(), 1, true)
            .unwrap();
        let execution = runner.create_checkpoint(&plan).unwrap();
        let checkpoint = runner.checkpoint_path(&execution).unwrap();
        let suite_run_directory = checkpoint.parent().unwrap().to_path_buf();
        let relocated = fixture.repository.root().join("relocated-suite-run");
        fs::rename(&suite_run_directory, &relocated).unwrap();
        symlink(&relocated, &suite_run_directory).unwrap();

        let error = runner
            .resume(execution, &fixture.suite, fixture.tasks.clone())
            .await
            .expect_err("resume must reject a linked checkpoint directory");

        assert!(matches!(error, RunnerError::UnsafePath(_)));
    }
}
