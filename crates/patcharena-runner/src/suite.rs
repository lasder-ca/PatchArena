//! Checkpointed multi-task, multi-agent benchmark suite orchestration.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use patcharena_core::{
    SuiteDefinition, SuiteExecution, SuiteExecutionStatus, SuiteTaskSnapshot, TaskDefinition,
    suite_checkpoint_path, suite_run_directory,
};
use patcharena_git::Repository;

use crate::orchestration::{create_private_directory, ensure_private_contained_directory};
use crate::{
    AgentRunner, ArenaRunner, MAX_REPEAT, RunnerError, RunnerSettings, benchmark_identity,
};

/// Maximum task-agent invocations accepted in one suite plan.
pub const MAX_SUITE_INVOCATIONS: u64 = 1_000;

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
    pub definition: SuiteDefinition,
    /// Task documents loaded once in suite order.
    pub tasks: Vec<TaskDefinition>,
    /// Per-task expected benchmark identities pinned during preflight.
    pub task_snapshots: Vec<SuiteTaskSnapshot>,
    /// Explicit agent IDs in execution order.
    pub agents: Vec<String>,
    /// Repetitions requested for every task-agent cell.
    pub repeat: u32,
    /// Whether repository instruction files remain visible.
    pub instructions_enabled: bool,
    /// Shared repository commit pinned before agent execution.
    pub repository_commit: String,
    /// Fingerprint of the canonical suite definition.
    pub suite_fingerprint: String,
    /// Total planned agent invocations across all cells.
    pub invocation_count: u64,
}

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
        let mut ids = HashSet::with_capacity(agents.len());
        for agent in &agents {
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
        })
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
        let task_count = u64::try_from(tasks.len()).unwrap_or(u64::MAX);
        let agent_count = u64::try_from(self.agents.len()).unwrap_or(u64::MAX);
        let invocation_count = task_count
            .checked_mul(agent_count)
            .and_then(|count| count.checked_mul(u64::from(repeat)))
            .ok_or_else(|| RunnerError::Agent("suite invocation count overflowed".to_owned()))?;
        if invocation_count > MAX_SUITE_INVOCATIONS {
            return Err(RunnerError::Agent(format!(
                "suite plans at most 1,000 agent invocations; requested {invocation_count}"
            )));
        }
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
        self.validate_plan_agents(&plan)?;
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
        let checkpoint = self.checkpoint_path(&execution)?;
        if !checkpoint.is_file() {
            return Err(RunnerError::Agent(format!(
                "suite checkpoint `{}` is missing",
                checkpoint.display()
            )));
        }
        self.execute_pending(execution, &plan).await
    }

    fn create_checkpoint(&self, plan: &SuitePlan) -> Result<SuiteExecution, RunnerError> {
        self.validate_plan_agents(plan)?;
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
        let pending = execution.pending_cells().cloned().collect::<Vec<_>>();
        for cell in pending {
            if let Err(error) = self.validate_shared_basis(plan, &cell.task_id) {
                execution.mark_aborted(Utc::now())?;
                execution.save_replace(self.checkpoint_path(&execution)?)?;
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
                    self.validate_group(&group.group, plan, task, &agent.id)?;
                    execution.complete_cell(
                        task.id.as_str(),
                        &agent.id,
                        group.group.group_id,
                        Utc::now(),
                    )?;
                }
                Err(error) => {
                    let diagnostic = bounded_error(&error.to_string());
                    execution.error_cell(task.id.as_str(), &agent.id, &diagnostic, Utc::now())?;
                }
            }
            execution.save_replace(self.checkpoint_path(&execution)?)?;
        }
        execution.mark_finished(Utc::now())?;
        let checkpoint_path = self.checkpoint_path(&execution)?;
        execution.save_replace(&checkpoint_path)?;
        Ok(SuiteExecutionOutcome {
            execution,
            checkpoint_path,
        })
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
    use std::sync::Arc;
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
}
