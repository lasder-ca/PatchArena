# Benchmark Suites Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a first-class, checkpointed benchmark suite that runs several repository tasks against explicitly selected agents and produces auditable JSON, Markdown, and HTML matrix reports.

**Architecture:** Add suite definitions and lifecycle records to `patcharena-core`, compose existing `ArenaRunner` groups in a focused `patcharena-runner::suite` module, and derive a separate suite report strictly from persisted evidence. Keep `run`, `battle`, existing result schemas, and existing report fields unchanged; the CLI adds a `suite` command family and delegates to focused modules.

**Tech Stack:** Rust 2024, Rust 1.85 MSRV, clap, serde/serde_yaml/serde_json, sha2, chrono, uuid, tokio, tempfile, assert_cmd.

## Global Constraints

- Preserve Rust 2024, MSRV 1.85, `unsafe_code = "forbid"`, and warning-free Clippy.
- Do not add shell interpolation, broaden the environment allowlist, or describe worktrees as a sandbox.
- Suite definitions contain at most 100 unique task IDs; a plan contains at most 1,000 agent invocations.
- Every task/agent cell reuses the existing independent `ArenaRunner::run_group` evidence path.
- Pin one repository commit and one expected benchmark identity per task before the first task agent invocation.
- Generated records and reports must use bounded reads, atomic writes, path containment, and symlink refusal.
- Existing `run`, `battle`, `compare`, `report`, schema-1 config, and schema-1 result behavior remain compatible.
- Tests never require network access or a real coding-agent installation.
- Use WSL Git for index and commit operations in this workspace; native Windows Git cannot create `.git/index.lock` here.

---

## File structure

### New files

- `crates/patcharena-core/src/suite.rs`: suite IDs, definitions, fingerprints, execution records, cell lifecycle, persistence, and path helpers.
- `crates/patcharena-runner/src/suite.rs`: suite preflight, checkpointed sequential execution, and resume.
- `crates/patcharena-report/src/suite.rs`: evidence validation, matrix aggregation, JSON/Markdown/HTML rendering.
- `crates/patcharena-cli/src/suite.rs`: suite command handlers, selected-agent resolution, progress, and report export.
- `examples/rust-basic/suite.yaml`: checked-in suite-definition example.

### Modified files

- `crates/patcharena-core/src/error.rs`: add a suite-ID error variant.
- `crates/patcharena-core/src/task.rs`: share the private portable-ID validator.
- `crates/patcharena-core/src/config.rs`: defaulted suites and suite-run paths.
- `crates/patcharena-core/src/lib.rs`: export suite APIs.
- `crates/patcharena-core/Cargo.toml`: add `sha2`.
- `crates/patcharena-core/tests/public_api.rs`: public suite surface and old-config compatibility.
- `crates/patcharena-runner/src/orchestration.rs`: expose pure benchmark-identity calculation and crate-local safe-directory helper.
- `crates/patcharena-runner/src/lib.rs`: export suite runner APIs.
- `crates/patcharena-report/src/lib.rs`: declare/export the focused suite module.
- `crates/patcharena-cli/src/args.rs`: parse the `suite` command family.
- `crates/patcharena-cli/src/commands.rs`: dispatch suite commands and expose small crate-local project helpers.
- `crates/patcharena-cli/src/lib.rs`: export new argument types.
- `crates/patcharena-cli/tests/cli.rs`: end-to-end suite flow.
- `.gitignore`, `patcharena.toml.example`, `README.md`, `README.ja.md`, `CHANGELOG.md`, `docs/architecture.md`, and `docs/threat-model.md`: generated paths and user/security contracts.
- `Cargo.toml` and Cargo-generated `Cargo.lock`: release version 0.3.0 and dependency lock.

---

### Task 1: Suite definitions and configured paths

**Files:**

- Create: `crates/patcharena-core/src/suite.rs`
- Modify: `crates/patcharena-core/src/error.rs`
- Modify: `crates/patcharena-core/src/task.rs`
- Modify: `crates/patcharena-core/src/config.rs`
- Modify: `crates/patcharena-core/src/lib.rs`
- Modify: `crates/patcharena-core/Cargo.toml`
- Test: `crates/patcharena-core/src/suite.rs`
- Test: `crates/patcharena-core/tests/public_api.rs`

**Interfaces:**

- Consumes: `TaskId`, `CoreError`, `Result`, `ValidationError`, `atomic_write_new`, `atomic_write_replace`, `read_utf8_limited`.
- Produces: `SuiteId`, `SuiteDefinition`, `CURRENT_SUITE_SCHEMA_VERSION`, `suite_file_path`, `load_suites`, `ProjectPaths::{suites_dir,suite_runs_dir}`, and `ResolvedProjectPaths::{suites_dir,suite_runs_dir}`.

- [ ] **Step 1: Add failing definition and compatibility tests**

Add tests that establish strict parsing, stable order, deterministic fingerprinting, safe IDs, and defaulted old configuration:

```rust
#[test]
fn suite_definition_round_trips_and_fingerprints_stably() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let suite = SuiteDefinition::new(
        SuiteId::new("core").expect("suite id"),
        Some("Core maintenance tasks".to_owned()),
        vec![
            TaskId::new("csv-newline").expect("task id"),
            TaskId::new("config-validation").expect("task id"),
        ],
    )
    .expect("suite");
    let path = suite_file_path(directory.path(), &suite.id);
    suite.save_new(&path).expect("save suite");
    let loaded = SuiteDefinition::load(&path).expect("load suite");
    assert_eq!(loaded, suite);
    assert_eq!(loaded.fingerprint().expect("fingerprint").len(), 64);
    assert_eq!(loaded.fingerprint().unwrap(), suite.fingerprint().unwrap());
}

#[test]
fn suite_definition_rejects_empty_duplicate_and_unknown_fields() {
    assert!(SuiteDefinition::new(SuiteId::new("empty").unwrap(), None, vec![]).is_err());
    let repeated = TaskId::new("same").unwrap();
    assert!(SuiteDefinition::new(
        SuiteId::new("duplicate").unwrap(),
        None,
        vec![repeated.clone(), repeated],
    )
    .is_err());
    assert!(SuiteDefinition::from_yaml(
        "schema_version: 1\nid: core\ntasks: [one]\nunknown: true\n"
    )
    .is_err());
}

#[test]
fn schema_one_config_without_suite_paths_uses_safe_defaults() {
    let config = ProjectConfig::from_toml(
        "schema_version = 1\n[paths]\nstate_dir = '.patcharena'\ntasks_dir = '.patcharena/tasks'\nruns_dir = '.patcharena/runs'\ngroups_dir = '.patcharena/groups'\nbattles_dir = '.patcharena/battles'\n"
    )
    .expect("old config");
    assert_eq!(config.paths.suites_dir, PathBuf::from(".patcharena/suites"));
    assert_eq!(config.paths.suite_runs_dir, PathBuf::from(".patcharena/suite-runs"));
}
```

- [ ] **Step 2: Run the focused tests and confirm RED**

Run:

```text
cargo test -p patcharena-core suite --all-features
cargo test -p patcharena-core --test public_api schema_one_config_without_suite_paths_uses_safe_defaults
```

Expected: compilation fails because suite APIs and path fields do not exist.

- [ ] **Step 3: Implement portable IDs and suite definitions**

Extract the current task-ID character/device-name logic into this private contract:

```rust
pub(crate) fn validate_portable_id(value: &str) -> std::result::Result<(), &'static str>;
```

Map its reason to `CoreError::InvalidTaskId` for `TaskId` and to this new variant for `SuiteId`:

```rust
#[error("invalid suite ID `{value}`: {reason}")]
InvalidSuiteId {
    value: String,
    reason: &'static str,
},
```

Implement and export this public suite surface:

```rust
pub const CURRENT_SUITE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SuiteId(String);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteDefinition {
    pub schema_version: u32,
    pub id: SuiteId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub tasks: Vec<TaskId>,
}

impl SuiteDefinition {
    pub fn new(id: SuiteId, description: Option<String>, tasks: Vec<TaskId>) -> Result<Self>;
    pub fn validate(&self) -> Result<()>;
    pub fn from_yaml(yaml: &str) -> Result<Self>;
    pub fn to_yaml(&self) -> Result<String>;
    pub fn fingerprint(&self) -> Result<String>;
    pub fn load(path: impl AsRef<Path>) -> Result<Self>;
    pub fn save_new(&self, path: impl AsRef<Path>) -> Result<()>;
    pub fn save_replace(&self, path: impl AsRef<Path>) -> Result<()>;
}

pub fn suite_file_path(suites_directory: impl AsRef<Path>, id: &SuiteId) -> PathBuf;
pub fn load_suites(suites_directory: impl AsRef<Path>) -> Result<Vec<SuiteDefinition>>;
```

Use a 1 MiB input limit, 100-task limit, nonblank 1,024-byte description limit, unique ordered tasks, SHA-256 over `serde_json::to_vec(self)`, and the existing atomic persistence helpers.

- [ ] **Step 4: Add compatible configured paths**

Add defaulted fields in `ProjectPaths` and resolved fields in `ResolvedProjectPaths`:

```rust
pub suites_dir: PathBuf,
pub suite_runs_dir: PathBuf,
```

Defaults are `.patcharena/suites` and `.patcharena/suite-runs`. Include both in safe-relative, distinct-path, state-containment, and `resolve_paths` logic. Add `sha2.workspace = true` to core dependencies and export the suite module from `lib.rs`.

- [ ] **Step 5: Run focused tests and confirm GREEN**

Run:

```text
cargo test -p patcharena-core --all-features
cargo clippy -p patcharena-core --all-targets --all-features -- -D warnings
```

Expected: all core tests pass and Clippy emits no warning.

- [ ] **Step 6: Commit Task 1**

```text
git add crates/patcharena-core
git commit -m "Add versioned benchmark suite definitions"
```

---

### Task 2: Suite execution lifecycle and checkpoint model

**Files:**

- Modify: `crates/patcharena-core/src/suite.rs`
- Modify: `crates/patcharena-core/src/lib.rs`
- Test: `crates/patcharena-core/src/suite.rs`
- Test: `crates/patcharena-core/tests/public_api.rs`

**Interfaces:**

- Consumes: `SuiteId`, `TaskId`, `BenchmarkIdentity`, `CURRENT_RESULT_SCHEMA_VERSION`.
- Produces: `SuiteExecution`, `SuiteExecutionStatus`, `SuiteTaskSnapshot`, `SuiteCell`, `SuiteCellStatus`, `suite_run_directory`, and `suite_checkpoint_path`.

- [ ] **Step 1: Add failing lifecycle tests**

```rust
fn identity(task_byte: char) -> BenchmarkIdentity {
    BenchmarkIdentity {
        repository_commit: "a".repeat(40),
        task_fingerprint: task_byte.to_string().repeat(64),
    }
}

#[test]
fn execution_builds_exact_cartesian_product_and_checkpoints() {
    let tasks = vec![
        SuiteTaskSnapshot::new(TaskId::new("one").unwrap(), identity('1')).unwrap(),
        SuiteTaskSnapshot::new(TaskId::new("two").unwrap(), identity('2')).unwrap(),
    ];
    let mut execution = SuiteExecution::new(
        "0.3.0",
        SuiteId::new("core").unwrap(),
        "b".repeat(64),
        "a".repeat(40),
        tasks,
        vec!["alpha".to_owned(), "beta".to_owned()],
        2,
        true,
        Utc::now(),
    )
    .unwrap();
    assert_eq!(execution.cells.len(), 4);
    execution
        .complete_cell("one", "alpha", Uuid::new_v4().to_string(), Utc::now())
        .unwrap();
    execution
        .error_cell("one", "beta", "agent unavailable", Utc::now())
        .unwrap();
    assert!(execution.mark_finished(Utc::now()).is_err());
    assert_eq!(execution.pending_cells().count(), 2);
}

#[test]
fn execution_rejects_illegal_cell_shapes_and_transitions() {
    let mut execution = execution_fixture();
    let group = Uuid::new_v4().to_string();
    execution.complete_cell("one", "alpha", group, Utc::now()).unwrap();
    assert!(execution
        .complete_cell("one", "alpha", Uuid::new_v4().to_string(), Utc::now())
        .is_err());
    assert!(execution.error_cell("missing", "alpha", "error", Utc::now()).is_err());
}
```

- [ ] **Step 2: Run focused tests and confirm RED**

Run: `cargo test -p patcharena-core suite::tests::execution --all-features`

Expected: compilation fails because lifecycle types do not exist.

- [ ] **Step 3: Implement strict lifecycle records**

Add these serialized contracts:

```rust
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteExecutionStatus {
    #[default]
    LegacyUnknown,
    Running,
    Completed,
    CompletedWithErrors,
    Aborted,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteCellStatus {
    #[default]
    Pending,
    Completed,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteTaskSnapshot {
    pub task_id: TaskId,
    pub benchmark_identity: BenchmarkIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteCell {
    pub task_id: TaskId,
    pub agent_id: String,
    pub status: SuiteCellStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteExecution {
    pub schema_version: u32,
    pub patcharena_version: String,
    pub suite_run_id: String,
    pub suite_id: SuiteId,
    pub suite_fingerprint: String,
    pub repository_commit: String,
    pub tasks: Vec<SuiteTaskSnapshot>,
    pub agents: Vec<String>,
    pub repeat: u32,
    pub instructions_enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub status: SuiteExecutionStatus,
    pub cells: Vec<SuiteCell>,
}
```

Implement `new`, `validate`, `complete_cell`, `error_cell`, `mark_finished`, `mark_aborted`, `pending_cells`, `load`, `to_json_pretty`, `save_new`, and `save_replace`. Validate exact task-major/agent-minor Cartesian order; unique task and agent IDs; UUIDs; positive repeat; 40- or 64-hex commit; 64-hex fingerprints; cell-state field shapes; 4 KiB sanitized errors; and terminal completeness. `mark_finished` requires no pending cells and chooses `completed_with_errors` when any cell is `error`.

- [ ] **Step 4: Add safe path helpers and public API tests**

```rust
pub fn suite_run_directory(root: impl AsRef<Path>, suite_run_id: &str) -> Result<PathBuf>;
pub fn suite_checkpoint_path(root: impl AsRef<Path>, suite_run_id: &str) -> Result<PathBuf>;
```

Both validate the UUID before joining. The checkpoint path is `<root>/<uuid>/suite.json`. Add JSON round-trip, future-schema, path traversal, terminal-state, and public-import tests.

- [ ] **Step 5: Run focused tests and confirm GREEN**

Run: `cargo test -p patcharena-core --all-features`

Expected: all core unit, integration, and doc tests pass.

- [ ] **Step 6: Commit Task 2**

```text
git add crates/patcharena-core
git commit -m "Add checkpointed suite execution records"
```

---

### Task 3: Suite preflight, execution, and resume

**Files:**

- Create: `crates/patcharena-runner/src/suite.rs`
- Modify: `crates/patcharena-runner/src/orchestration.rs`
- Modify: `crates/patcharena-runner/src/lib.rs`
- Test: `crates/patcharena-runner/src/suite.rs`

**Interfaces:**

- Consumes: `Repository`, `RunnerSettings`, `ArenaRunner`, `AgentRunner`, suite core models, task definitions.
- Produces: `benchmark_identity`, `SelectedSuiteAgent`, `SuitePlan`, `SuiteRunner`, `SuiteExecutionOutcome`, and `MAX_SUITE_INVOCATIONS`.

- [ ] **Step 1: Add failing pure preflight tests**

```rust
#[test]
fn preflight_builds_stable_plan_and_caps_invocations() {
    let fixture = SuiteFixture::new();
    let runner = fixture.runner(vec![named_success("alpha"), named_success("beta")]);
    let plan = runner
        .preflight(&fixture.suite, fixture.tasks(), 3, true)
        .expect("preflight");
    assert_eq!(plan.invocation_count, 12);
    assert_eq!(plan.repository_commit, fixture.head());
    assert_eq!(plan.task_snapshots.len(), 2);
    assert!(plan.task_snapshots.iter().all(|task| {
        task.benchmark_identity.repository_commit == plan.repository_commit
    }));

    let error = runner
        .preflight(&fixture.suite, fixture.tasks(), 251, true)
        .expect_err("1,004 invocations must be rejected");
    assert!(error.to_string().contains("1,000"));
}

#[test]
fn dry_preflight_creates_no_group_or_suite_artifact() {
    let fixture = SuiteFixture::new();
    let runner = fixture.runner(vec![named_success("alpha")]);
    runner.preflight(&fixture.suite, fixture.tasks(), 1, true).unwrap();
    assert_eq!(std::fs::read_dir(fixture.groups()).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(fixture.suite_runs()).unwrap().count(), 0);
}
```

The fixture initializes and commits a tiny Git repository, writes two valid tasks with `true` verification, and uses a small test-only `NamedFakeAgent` implementing `AgentRunner`.

- [ ] **Step 2: Run preflight tests and confirm RED**

Run: `cargo test -p patcharena-runner suite::tests::preflight --all-features`

Expected: compilation fails because the suite runner does not exist.

- [ ] **Step 3: Extract pure benchmark identity calculation**

Move the current `ArenaRunner::benchmark_identity` body behind this public function and call it from `run_group`:

```rust
pub fn benchmark_identity(
    repository: &Repository,
    settings: &RunnerSettings,
    task: &TaskDefinition,
) -> Result<BenchmarkIdentity, RunnerError>;
```

The hash inputs and order must remain byte-for-byte compatible with v0.2.0 so existing comparisons are unaffected.

- [ ] **Step 4: Implement selected agents and immutable preflight plan**

```rust
pub const MAX_SUITE_INVOCATIONS: u64 = 1_000;

#[derive(Clone)]
pub struct SelectedSuiteAgent {
    pub id: String,
    pub runner: Arc<dyn AgentRunner>,
}

#[derive(Clone, Debug)]
pub struct SuitePlan {
    pub definition: SuiteDefinition,
    pub tasks: Vec<TaskDefinition>,
    pub task_snapshots: Vec<SuiteTaskSnapshot>,
    pub agents: Vec<String>,
    pub repeat: u32,
    pub instructions_enabled: bool,
    pub repository_commit: String,
    pub suite_fingerprint: String,
    pub invocation_count: u64,
}

pub struct SuiteRunner {
    repository: Repository,
    runs_directory: PathBuf,
    groups_directory: PathBuf,
    suite_runs_directory: PathBuf,
    agents: Vec<SelectedSuiteAgent>,
    settings: RunnerSettings,
    patcharena_version: String,
}
```

`SuiteRunner::new` validates nonempty unique agent IDs, `agent.id == agent.runner.name()`, and contained directories. `preflight` validates suite/task order, tracked-clean Git, one resolved `HEAD`, task identities, positive repeat, checked multiplication, and the 1,000-invocation cap.

- [ ] **Step 5: Add failing checkpoint/continuation tests**

```rust
#[tokio::test]
async fn execute_checkpoints_every_cell_and_continues_after_local_error() {
    let fixture = SuiteFixture::new();
    let runner = fixture.runner(vec![named_success("alpha"), named_failure("beta")]);
    let plan = runner.preflight(&fixture.suite, fixture.tasks(), 1, true).unwrap();
    let outcome = runner.execute(plan).await.expect("suite execution");
    assert_eq!(outcome.execution.status, SuiteExecutionStatus::Completed);
    assert_eq!(outcome.execution.cells.len(), 4);
    assert!(outcome.execution.cells.iter().all(|cell| cell.group_id.is_some()));
    assert_eq!(SuiteExecution::load(outcome.checkpoint_path).unwrap(), outcome.execution);
}

#[tokio::test]
async fn resume_runs_pending_cells_without_repeating_completed_cells() {
    let fixture = SuiteFixture::new();
    let runner = fixture.runner(vec![named_success("alpha"), named_success("beta")]);
    let plan = runner.preflight(&fixture.suite, fixture.tasks(), 1, true).unwrap();
    let mut execution = runner.create_checkpoint(&plan).expect("checkpoint");
    let existing_group = fixture.completed_group("one", "alpha");
    execution.complete_cell("one", "alpha", existing_group, Utc::now()).unwrap();
    execution.save_replace(runner.checkpoint_path(&execution).unwrap()).unwrap();
    let outcome = runner.resume(execution, &fixture.suite, fixture.tasks()).await.unwrap();
    assert_eq!(outcome.execution.cells.len(), 4);
    assert_eq!(fixture.group_count(), 4);
}
```

- [ ] **Step 6: Implement checkpointed execution and resume**

Expose:

```rust
#[derive(Clone, Debug)]
pub struct SuiteExecutionOutcome {
    pub execution: SuiteExecution,
    pub checkpoint_path: PathBuf,
}

impl SuiteRunner {
    pub fn new(
        repository: Repository,
        runs_directory: impl Into<PathBuf>,
        groups_directory: impl Into<PathBuf>,
        suite_runs_directory: impl Into<PathBuf>,
        agents: Vec<SelectedSuiteAgent>,
        settings: RunnerSettings,
        patcharena_version: impl Into<String>,
    ) -> Result<Self, RunnerError>;
    pub fn preflight(
        &self,
        suite: &SuiteDefinition,
        tasks: Vec<TaskDefinition>,
        repeat: u32,
        instructions_enabled: bool,
    ) -> Result<SuitePlan, RunnerError>;
    pub async fn execute(&self, plan: SuitePlan) -> Result<SuiteExecutionOutcome, RunnerError>;
    pub async fn resume(
        &self,
        execution: SuiteExecution,
        suite: &SuiteDefinition,
        tasks: Vec<TaskDefinition>,
    ) -> Result<SuiteExecutionOutcome, RunnerError>;
}
```

Create the suite-run directory with owner-only permissions where supported, save the initial checkpoint before the first cell, and replace it after each attempted cell. A completed group remains a completed cell even when its runs fail. Record bounded cell errors and continue. Recheck `HEAD` and expected identity before each cell; on drift, mark the execution aborted, checkpoint it, and return an error. Resume only `running` executions after exact suite fingerprint, agent order, repeat, instructions, commit, and task-identity validation.

- [ ] **Step 7: Run runner tests and confirm GREEN**

Run:

```text
cargo test -p patcharena-runner --all-features
cargo clippy -p patcharena-runner --all-targets --all-features -- -D warnings
```

Expected: existing 29+ tests and all new suite tests pass without warnings.

- [ ] **Step 8: Commit Task 3**

```text
git add crates/patcharena-runner
git commit -m "Run checkpointed benchmark suites"
```

---

### Task 4: Evidence-backed suite reports

**Files:**

- Create: `crates/patcharena-report/src/suite.rs`
- Modify: `crates/patcharena-report/src/lib.rs`
- Test: `crates/patcharena-report/src/suite.rs`

**Interfaces:**

- Consumes: `SuiteExecution`, `SuiteCellStatus`, `GroupReport`, and existing persisted selection loading.
- Produces: `SuiteMatrixCell`, `SuiteAgentSummary`, `SuiteReport`, and `load_suite_report`.

- [ ] **Step 1: Add failing aggregation and incompatibility tests**

```rust
#[test]
fn suite_report_builds_matrix_and_task_macro_average() {
    let (execution, groups) = report_fixture(vec![
        ("one", "alpha", 1.0),
        ("two", "alpha", 0.0),
        ("one", "beta", 0.5),
        ("two", "beta", 1.0),
    ]);
    let report = SuiteReport::new(execution, groups).expect("suite report");
    assert_eq!(report.cells.len(), 4);
    assert_eq!(report.agent("alpha").unwrap().macro_success_rate, Some(0.5));
    assert_eq!(report.agent("beta").unwrap().macro_success_rate, Some(0.75));
    assert!(!report.to_markdown().contains("winner"));
}

#[test]
fn suite_report_rejects_wrong_group_identity() {
    let (execution, mut groups) = report_fixture(vec![("one", "alpha", 1.0)]);
    groups[0].agent = "different".to_owned();
    assert!(SuiteReport::new(execution, groups).is_err());
}

#[test]
fn suite_html_escapes_untrusted_labels() {
    let (execution, groups) = report_fixture(vec![("one", "alpha", 1.0)]);
    let mut report = SuiteReport::new(execution, groups).unwrap();
    report.description = Some("<script>alert(1)</script>".to_owned());
    let html = report.to_html();
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(html.contains("&lt;script&gt;"));
}
```

- [ ] **Step 2: Run report tests and confirm RED**

Run: `cargo test -p patcharena-report suite --all-features`

Expected: compilation fails because suite report types do not exist.

- [ ] **Step 3: Implement validated matrix aggregation**

Use these public contracts:

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuiteMatrixCell {
    pub task_id: String,
    pub agent_id: String,
    pub status: SuiteCellStatus,
    pub group_id: Option<String>,
    pub successful_runs: usize,
    pub requested_runs: usize,
    pub success_rate: Option<f64>,
    pub median_duration_ms: Option<f64>,
    pub median_changed_files: Option<f64>,
    pub median_diff_lines: Option<f64>,
    pub verification_failures: usize,
    pub violation_count: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuiteAgentSummary {
    pub agent_id: String,
    pub completed_tasks: usize,
    pub error_tasks: usize,
    pub pending_tasks: usize,
    pub successful_runs: usize,
    pub total_runs: usize,
    pub macro_success_rate: Option<f64>,
    pub verification_failures: usize,
    pub violation_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuiteReport {
    pub schema_version: u32,
    pub suite_run_id: String,
    pub suite_id: String,
    pub description: Option<String>,
    pub status: SuiteExecutionStatus,
    pub repository_commit: String,
    pub suite_fingerprint: String,
    pub instructions_enabled: bool,
    pub repeat: u32,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub tasks: Vec<String>,
    pub agents: Vec<SuiteAgentSummary>,
    pub cells: Vec<SuiteMatrixCell>,
}
```

`SuiteReport::new` accepts one execution and completed group reports, rejects unreferenced or duplicate groups, and validates task, agent, identity, instructions, and requested/observed counts. Macro success is the arithmetic mean of complete task-cell success rates. Pending/error cells have `None` metrics rather than fabricated zeroes.

- [ ] **Step 4: Implement persistence-only loading and renderers**

```rust
pub fn load_suite_report(
    execution: SuiteExecution,
    description: Option<String>,
    runs_directory: impl AsRef<Path>,
    groups_directory: impl AsRef<Path>,
) -> Result<SuiteReport, ReportError>;
```

Load each completed group with existing `load_selection`. Add `to_json`, `to_markdown`, `to_html`, `agent`, and `all_benchmarks_succeeded`. Markdown and HTML show provenance, coverage, per-agent summaries, and a task-by-agent matrix. Reuse existing control sanitization and escaping. HTML remains a single document with embedded CSS and no network assets.

- [ ] **Step 5: Run report tests and confirm GREEN**

Run:

```text
cargo test -p patcharena-report --all-features
cargo clippy -p patcharena-report --all-targets --all-features -- -D warnings
```

Expected: all report tests pass and hostile labels are escaped.

- [ ] **Step 6: Commit Task 4**

```text
git add crates/patcharena-report
git commit -m "Render auditable benchmark suite reports"
```

---

### Task 5: Suite CLI, dry run, resume, and export

**Files:**

- Create: `crates/patcharena-cli/src/suite.rs`
- Modify: `crates/patcharena-cli/src/args.rs`
- Modify: `crates/patcharena-cli/src/commands.rs`
- Modify: `crates/patcharena-cli/src/lib.rs`
- Test: `crates/patcharena-cli/src/args.rs`
- Test: `crates/patcharena-cli/tests/cli.rs`

**Interfaces:**

- Consumes: all prior task interfaces plus existing `Project`, `AgentRegistry`, exit codes, and generated-file writer.
- Produces: `SuiteCommand`, `SuiteAddArgs`, `SuiteRunArgs`, `SuiteResumeArgs`, `SuiteReportArgs`, and `suite::run`.

- [ ] **Step 1: Add failing argument parser tests**

```rust
#[test]
fn parses_suite_add_run_resume_and_report() {
    let add = Cli::try_parse_from([
        "patcharena", "suite", "add", "--id", "core",
        "--task", "one", "--task", "two",
    ])
    .expect("suite add");
    assert!(matches!(add.command, Command::Suite { command: SuiteCommand::Add(_) }));

    let run = Cli::try_parse_from([
        "patcharena", "suite", "run", "--suite", "core",
        "--agents", "codex,claude", "--repeat", "3", "--dry-run",
    ])
    .expect("suite run");
    let Command::Suite { command: SuiteCommand::Run(run) } = run.command else {
        panic!("expected suite run");
    };
    assert_eq!(run.agents, ["codex", "claude"]);
    assert_eq!(run.repeat.get(), 3);
    assert!(run.dry_run);

    let id = "00000000-0000-0000-0000-000000000000";
    assert!(Cli::try_parse_from(["patcharena", "suite", "resume", "--run", id]).is_ok());
    assert!(Cli::try_parse_from([
        "patcharena", "suite", "report", "--run", id,
        "--format", "html", "--output", "report.html",
    ])
    .is_ok());
}
```

- [ ] **Step 2: Run parser tests and confirm RED**

Run: `cargo test -p patcharena-cli args::tests::parses_suite --all-features`

Expected: compilation fails because suite command variants do not exist.

- [ ] **Step 3: Add exact clap contracts and dispatch**

```rust
#[derive(Debug, Subcommand)]
pub enum SuiteCommand {
    Add(SuiteAddArgs),
    List,
    Run(SuiteRunArgs),
    Resume(SuiteResumeArgs),
    Report(SuiteReportArgs),
}

#[derive(Debug, clap::Args)]
pub struct SuiteAddArgs {
    #[arg(long)]
    pub id: String,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long, required = true, action = ArgAction::Append)]
    pub task: Vec<String>,
}

#[derive(Debug, clap::Args)]
pub struct SuiteRunArgs {
    #[arg(long)]
    pub suite: String,
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    pub agents: Vec<String>,
    #[arg(long, default_value_t = NonZeroU32::MIN)]
    pub repeat: NonZeroU32,
    #[arg(long)]
    pub without_instructions: bool,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, clap::Args)]
pub struct SuiteResumeArgs {
    #[arg(long)]
    pub run: String,
}

#[derive(Debug, clap::Args)]
pub struct SuiteReportArgs {
    #[arg(long)]
    pub run: String,
    #[arg(long, value_enum)]
    pub format: ReportFormat,
    #[arg(long)]
    pub output: Option<PathBuf>,
}
```

Add `Command::Suite { command: SuiteCommand }`, export argument types, and dispatch to `crate::suite::run(command).await`.

- [ ] **Step 4: Implement definition/list and selected-agent preflight handlers**

Make `Project`, `load_project`, `runner_settings`, `create_contained_directory`, and `write_generated_file` `pub(crate)` without changing behavior. In `suite.rs`, implement:

```rust
pub async fn run(command: SuiteCommand) -> Result<u8, CliError>;

fn load_suite_tasks(project: &Project, suite: &SuiteDefinition) -> Result<Vec<TaskDefinition>, CliError>;

fn selected_agents(
    registry: &AgentRegistry,
    ids: &[String],
) -> Result<Vec<SelectedSuiteAgent>, CliError>;
```

Reject duplicate agents and unavailable version probes before constructing `SuiteRunner`. `suite add` refuses overwrite. `suite list` prints ID, task count, and description. Dry run prints base commit, task/agent/repeat counts, instructions condition, and invocation total, then returns success without creating artifacts.

- [ ] **Step 5: Add failing end-to-end CLI suite test**

On Unix, create two executable custom agents that answer `--version` and otherwise exit zero. Initialize a fixture repository, add two task YAML files and one suite YAML file, commit them, and run:

```rust
Command::cargo_bin("patcharena")
    .unwrap()
    .current_dir(directory.path())
    .args([
        "suite", "run", "--suite", "core",
        "--agents", "fake-a,fake-b", "--repeat", "1",
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains("suite run:"))
    .stdout(predicate::str::contains("HTML:"));
```

Assert one suite-run directory, four group records, four run directories, valid final `suite.json`, and parseable `report.json`; assert `report.html` contains a four-cell matrix and no remote asset URL.

- [ ] **Step 6: Implement real run, resume, reports, and exit policy**

For real run, call `preflight` then `execute`, print one line per cell, build `SuiteReport` from persisted evidence, and atomically write `report.json`, `report.md`, and `report.html` beside `suite.json`. Resume loads the UUID-validated checkpoint, current suite definition, current tasks, and the recorded agents; it invokes pending cells only and regenerates reports. Report export loads persisted evidence and writes or prints exactly one requested format.

Return exit code 0 only when `SuiteReport::all_benchmarks_succeeded()` is true. Return existing benchmark-failed code 6 for completed evidence containing benchmark failures or cell errors. Preflight, runner, report, and filesystem failures retain their existing typed error exit classes.

- [ ] **Step 7: Include suite directories in initialization and doctor checks**

Add `suites_dir` and `suite_runs_dir` to `create_project_directories` and `check_state_writable`. Ensure `suite add` leaves definitions trackable while generated suite-run contents remain ignored.

- [ ] **Step 8: Run CLI tests and confirm GREEN**

Run:

```text
cargo test -p patcharena-cli --all-features
cargo clippy -p patcharena-cli --all-targets --all-features -- -D warnings
```

Expected: parser and two-task/two-agent end-to-end tests pass; no real agent or network is used.

- [ ] **Step 9: Commit Task 5**

```text
git add crates/patcharena-cli
git commit -m "Add one-command benchmark suite workflow"
```

---

### Task 6: Version, documentation, examples, and security contract

**Files:**

- Modify: `Cargo.toml`
- Modify through Cargo: `Cargo.lock`
- Modify: `.gitignore`
- Modify: `patcharena.toml.example`
- Modify: `README.md`
- Modify: `README.ja.md`
- Modify: `CHANGELOG.md`
- Modify: `docs/architecture.md`
- Modify: `docs/threat-model.md`
- Create: `examples/rust-basic/suite.yaml`

**Interfaces:**

- Consumes: final CLI and persistence contracts.
- Produces: complete English/Japanese user guidance and v0.3.0 release metadata.

- [ ] **Step 1: Add generated-path protection and example config**

Add these ignore rules while keeping `.patcharena/suites/` trackable:

```gitignore
/.patcharena/battles/
/.patcharena/suite-runs/
```

Add `suites_dir = ".patcharena/suites"` and `suite_runs_dir = ".patcharena/suite-runs"` to `patcharena.toml.example`. Add `examples/rust-basic/suite.yaml`:

```yaml
schema_version: 1
id: rust-basic
description: Basic Rust maintenance benchmark
tasks:
  - fix-add
```

- [ ] **Step 2: Bump the workspace release through Cargo**

Change workspace version from `0.2.0` to `0.3.0`, then run `cargo check --workspace --all-features` so Cargo updates every lockfile package version. Do not hand-edit dependency checksums.

- [ ] **Step 3: Document the complete user journey in both languages**

Add matching English and Japanese sections covering:

```text
patcharena suite add --id core --task task-a --task task-b
git add .patcharena/tasks .patcharena/suites patcharena.toml
git commit -m "Add PatchArena benchmark suite"
patcharena suite run --suite core --agents codex,claude --repeat 3 --dry-run
patcharena suite run --suite core --agents codex,claude --repeat 3
patcharena suite resume --run <suite-run-id>
patcharena suite report --run <suite-run-id> --format html --output report.html
```

Explain the 1,000-invocation cap, cost visibility, explicit agents, equal task weighting, no winner/significance claim, checkpoint/resume compatibility checks, task-quality responsibility, generated artifact locations, and the unchanged non-sandbox warning.

- [ ] **Step 4: Update architecture, threat model, and changelog**

Architecture must show suite definitions above existing run groups and reports derived only from persisted records. Threat model must cover multiplication of agent cost, preflight limits, suite metadata prompt non-disclosure, checkpoint integrity, and resume refusal on identity drift. Changelog 0.3.0 must list suites, dry-run, resume, matrix reports, compatibility, and security boundaries.

- [ ] **Step 5: Verify documentation paths and command help**

Run:

```text
cargo run -p patcharena-cli -- suite --help
cargo run -p patcharena-cli -- suite run --help
rg -n "suite-runs|suite run|not a sandbox|サンドボックス" README.md README.ja.md docs patcharena.toml.example .gitignore
```

Expected: help lists add/list/run/resume/report; both READMEs and security docs contain the new contracts; generated paths are ignored.

- [ ] **Step 6: Commit Task 6**

```text
git add Cargo.toml Cargo.lock .gitignore patcharena.toml.example README.md README.ja.md CHANGELOG.md docs examples/rust-basic/suite.yaml
git commit -m "Document benchmark suites release"
```

---

### Task 7: Compatibility, adversarial review, and full verification

**Files:**

- Modify as failures require: only files already in Tasks 1-6
- Test: entire workspace

**Interfaces:**

- Consumes: complete v0.3.0 implementation.
- Produces: a clean, verified branch with no accidental artifacts or secrets.

- [ ] **Step 1: Run formatting and diff hygiene**

Run:

```text
cargo fmt --all
cargo fmt --all -- --check
git diff --check
```

Expected: formatter makes no second-pass change and diff check reports nothing.

- [ ] **Step 2: Run focused compatibility checks**

Run:

```text
cargo test -p patcharena-core --test public_api
cargo test -p patcharena-cli --test cli
cargo test -p patcharena-report suite
cargo test -p patcharena-runner suite
```

Expected: old schema-1 fixture tests and every new end-to-end suite test pass.

- [ ] **Step 3: Run the repository-required full verification sequence**

Run:

```text
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked --workspace --release
```

Expected: all four commands exit 0.

- [ ] **Step 4: Perform the security and artifact audit**

Check the final diff for credentials, machine-local paths, `.patcharena` run data, generated reports, executable shell interpolation, unbounded stored errors, report escaping regressions, and accidental tracked `target` or `work` files. Confirm only suite definitions/examples—not suite-run artifacts—are trackable.

- [ ] **Step 5: Review every acceptance criterion against evidence**

Map each design acceptance criterion to a passing test or documentation section. Confirm the two-task/two-agent CLI test proves four independent groups, pre-execution checkpoint creation, generated three-format reports, and no external agent/network dependency. Confirm resume tests prove completed cells are not rerun.

- [ ] **Step 6: Commit verification-only corrections if present**

```text
git add -u
git commit -m "Harden benchmark suite verification"
```

Skip the commit when Step 1-5 require no correction.

- [ ] **Step 7: Record final repository state**

Run:

```text
git status --short --branch
git log -8 --oneline
```

Expected: no unintended worktree changes; local branch contains focused commits for definitions, lifecycle, runner, reports, CLI, and documentation.
