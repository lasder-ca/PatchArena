use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fs::{read_utf8_limited, serialization_path, with_trailing_newline};
use crate::{
    CoreError, Result, TaskId, ValidationError, atomic_write_new, atomic_write_replace,
    ensure_safe_relative_path,
};

/// The only run-result and run-group schema version supported by this release.
pub const CURRENT_RESULT_SCHEMA_VERSION: u32 = 1;

const MAX_RESULT_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_GROUP_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_BATTLE_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Agent identity and redacted invocation details recorded by schema-aware producers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMetadata {
    /// Stable registry ID.
    pub id: String,
    /// Human-readable agent name.
    pub display_name: String,
    /// Detected CLI version, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    /// Adapter implementation version.
    pub adapter_version: String,
    /// Redacted command audit string. Prompts and credentials must never appear here.
    pub command: String,
}

/// Reproducibility context added to v0.2.0 run results.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionMetadata {
    /// Host operating-system identifier.
    pub os: String,
    /// Host CPU architecture identifier.
    pub arch: String,
    /// One-based repeat index within a run group.
    pub repeat_index: u32,
    /// SHA-256 of the selected agent configuration.
    pub agent_config_hash: String,
}

/// Immutable inputs used to decide whether benchmark groups are comparable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkIdentity {
    /// Full Git commit object ID used for the detached worktree.
    pub repository_commit: String,
    /// SHA-256 of the canonical task and effective execution policy.
    pub task_fingerprint: String,
}

impl BenchmarkIdentity {
    fn validate(&self, field: &str) -> Result<()> {
        if !matches!(self.repository_commit.len(), 40 | 64)
            || !self
                .repository_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ValidationError::new(
                format!("{field}.repository_commit"),
                "must be a full SHA-1 or SHA-256 Git object ID",
            )
            .into());
        }
        if self.task_fingerprint.len() != 64
            || !self
                .task_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(ValidationError::new(
                format!("{field}.task_fingerprint"),
                "must be a 64-character SHA-256 digest",
            )
            .into());
        }
        Ok(())
    }
}

/// The captured outcome of one setup, agent, or verification command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandOutcome {
    /// A stable audit rendering of the executable and arguments.
    pub command: String,
    /// Whether the process completed successfully.
    pub success: bool,
    /// The process exit code, or `None` when no code was produced.
    pub exit_code: Option<i32>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Whether PatchArena terminated the command after its deadline.
    #[serde(default, skip_serializing_if = "is_false")]
    pub timed_out: bool,
    /// Number of stdout bytes observed before output limiting.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stdout_bytes: u64,
    /// Number of stderr bytes observed before output limiting.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stderr_bytes: u64,
    /// Whether stdout or stderr was truncated by an output limit.
    #[serde(default, skip_serializing_if = "is_false")]
    pub output_truncated: bool,
    /// A concise launch, wait, timeout, or capture error when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CommandOutcome {
    /// Creates an outcome from a process exit code and duration.
    #[must_use]
    pub fn exited(command: impl Into<String>, exit_code: i32, duration_ms: u64) -> Self {
        Self {
            command: command.into(),
            success: exit_code == 0,
            exit_code: Some(exit_code),
            duration_ms,
            timed_out: false,
            stdout_bytes: 0,
            stderr_bytes: 0,
            output_truncated: false,
            error: None,
        }
    }

    /// Creates an outcome for a command terminated after its deadline.
    #[must_use]
    pub fn timeout(command: impl Into<String>, duration_ms: u64) -> Self {
        Self {
            command: command.into(),
            success: false,
            exit_code: None,
            duration_ms,
            timed_out: true,
            stdout_bytes: 0,
            stderr_bytes: 0,
            output_truncated: false,
            error: Some("command timed out".to_owned()),
        }
    }

    /// Creates an outcome for a command that could not be launched or awaited.
    #[must_use]
    pub fn failed(command: impl Into<String>, duration_ms: u64, error: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            success: false,
            exit_code: None,
            duration_ms,
            timed_out: false,
            stdout_bytes: 0,
            stderr_bytes: 0,
            output_truncated: false,
            error: Some(error.into()),
        }
    }

    /// Checks the internal consistency of this outcome.
    pub fn validate(&self) -> Result<()> {
        if self.command.trim().is_empty() {
            return Err(ValidationError::new("command", "must not be empty").into());
        }
        if self.command.contains('\0') {
            return Err(ValidationError::new("command", "must not contain a NUL byte").into());
        }
        if self.success && self.exit_code != Some(0) {
            return Err(ValidationError::new(
                "success",
                "a successful command must have exit code 0",
            )
            .into());
        }
        if self.success && self.timed_out {
            return Err(ValidationError::new(
                "timed_out",
                "a timed-out command cannot be successful",
            )
            .into());
        }
        if self.timed_out && self.exit_code.is_some() {
            return Err(ValidationError::new(
                "exit_code",
                "a timed-out command must not claim an exit code",
            )
            .into());
        }
        if self
            .error
            .as_ref()
            .is_some_and(|error| error.trim().is_empty())
        {
            return Err(ValidationError::new("error", "must not be blank when present").into());
        }
        if self.success && self.error.is_some() {
            return Err(ValidationError::new(
                "error",
                "a successful command cannot contain an error summary",
            )
            .into());
        }
        Ok(())
    }
}

/// A command outcome produced during task verification.
pub type VerificationResult = CommandOutcome;

/// The execution phase associated with an audit event.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    /// Repository or worktree preparation.
    Setup,
    /// Coding-agent execution.
    Agent,
    /// Task verification.
    Verification,
    /// Git inspection or diff capture.
    Git,
    /// Worktree cleanup.
    Cleanup,
}

/// One timestamped command entry in a run's audit log.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEvent {
    /// The run phase in which the command was executed.
    pub phase: RunPhase,
    /// The instant at which the command was started.
    pub started_at: DateTime<Utc>,
    /// The command's captured outcome.
    pub outcome: CommandOutcome,
}

impl AuditEvent {
    fn validate(&self, field: &str) -> Result<()> {
        self.outcome
            .validate()
            .map_err(|error| prefix_validation(field, error))
    }
}

/// A category of policy or resource-limit violation detected during a run.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    /// A configured dangerous command pattern was observed.
    ForbiddenCommand,
    /// A configured forbidden path was changed.
    ForbiddenPath,
    /// A task resource or patch-size limit was exceeded.
    LimitExceeded,
    /// A path attempted to escape its allowed root lexically.
    PathTraversal,
    /// A symbolic link resolved outside an allowed root.
    SymlinkEscape,
    /// Output was truncated because it exceeded the configured maximum.
    OutputLimit,
    /// A runner-specific policy violation not covered by another stable category.
    Other,
}

/// A policy violation detected while executing or inspecting a run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Violation {
    /// The stable violation category.
    pub kind: ViolationKind,
    /// A concise human-readable explanation.
    pub message: String,
    /// The implicated command, when the violation concerns command execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The implicated repository-relative path, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

impl Violation {
    /// Creates a violation without optional command or path context.
    #[must_use]
    pub fn new(kind: ViolationKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            command: None,
            path: None,
        }
    }

    /// Adds command context to a violation.
    #[must_use]
    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = Some(command.into());
        self
    }

    /// Adds repository-relative path context to a violation.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = Some(path.into());
        self
    }

    fn validate(&self) -> Result<()> {
        if self.message.trim().is_empty() {
            return Err(ValidationError::new("message", "must not be empty").into());
        }
        if self
            .command
            .as_ref()
            .is_some_and(|command| command.trim().is_empty() || command.contains('\0'))
        {
            return Err(ValidationError::new(
                "command",
                "must not be blank or contain NUL when present",
            )
            .into());
        }
        if let Some(path) = &self.path {
            ensure_safe_relative_path(path)?;
        }
        Ok(())
    }
}

/// Repository-relative files captured for a run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPaths {
    /// Captured agent stdout.
    pub stdout: PathBuf,
    /// Captured agent stderr.
    pub stderr: PathBuf,
    /// Unified diff of the worktree changes.
    pub patch: PathBuf,
    /// Optional JSON Lines command audit log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<PathBuf>,
}

impl Default for ArtifactPaths {
    fn default() -> Self {
        Self {
            stdout: PathBuf::from("stdout.log"),
            stderr: PathBuf::from("stderr.log"),
            patch: PathBuf::from("changes.diff"),
            audit: Some(PathBuf::from("audit.jsonl")),
        }
    }
}

impl ArtifactPaths {
    /// Validates that artifact paths are safe, relative, and distinct.
    pub fn validate(&self) -> Result<()> {
        let mut paths = HashSet::new();
        for (field, path) in [
            ("artifacts.stdout", &self.stdout),
            ("artifacts.stderr", &self.stderr),
            ("artifacts.patch", &self.patch),
        ] {
            ensure_safe_relative_path(path)
                .map_err(|error| ValidationError::new(field, format!("{error}")))?;
            if !paths.insert(path) {
                return Err(ValidationError::new(field, "artifact paths must be distinct").into());
            }
        }
        if let Some(path) = &self.audit {
            ensure_safe_relative_path(path)
                .map_err(|error| ValidationError::new("artifacts.audit", format!("{error}")))?;
            if !paths.insert(path) {
                return Err(ValidationError::new(
                    "artifacts.audit",
                    "artifact paths must be distinct",
                )
                .into());
            }
        }
        Ok(())
    }
}

/// A versioned record of one isolated agent attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResult {
    /// Required on-disk schema version.
    pub schema_version: u32,
    /// PatchArena application version that created this result. Absent in v0.1.x evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patcharena_version: Option<String>,
    /// UUID identifying this attempt.
    pub run_id: String,
    /// Optional UUID of the repeat-run group containing this attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Task that was attempted.
    pub task_id: TaskId,
    /// Stable agent backend name, initially `codex`.
    pub agent: String,
    /// Additive structured agent identity retained alongside the legacy string field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_metadata: Option<AgentMetadata>,
    /// Additive host and repeat metadata retained without changing schema version 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_metadata: Option<ExecutionMetadata>,
    /// Whether repository instructions such as `AGENTS.md` were available to the agent.
    #[serde(default = "default_instructions_enabled")]
    pub instructions_enabled: bool,
    /// Repository revision and task/policy digest for comparison safety.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_identity: Option<BenchmarkIdentity>,
    /// UTC start time.
    pub started_at: DateTime<Utc>,
    /// UTC finish time.
    pub finished_at: DateTime<Utc>,
    /// Measured wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Whether the agent ran, verification passed, and no policy violation occurred.
    pub success: bool,
    /// Agent process exit code, or `None` if it never produced one.
    pub exit_code: Option<i32>,
    /// Number of changed repository files.
    pub changed_files: u64,
    /// Number of added diff lines.
    pub added_lines: u64,
    /// Number of deleted diff lines.
    pub deleted_lines: u64,
    /// Outcomes of task setup commands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<CommandOutcome>,
    /// Detailed agent command outcome, when captured by this schema producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_outcome: Option<CommandOutcome>,
    /// Outcomes of task verification commands.
    pub verification: Vec<VerificationResult>,
    /// Timestamped command audit events across all phases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audit: Vec<AuditEvent>,
    /// Detected policy and resource-limit violations.
    pub violations: Vec<Violation>,
    /// Relative paths to captured run artifacts.
    pub artifacts: ArtifactPaths,
    /// Concise top-level failure summary, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RunResult {
    /// Reads, parses, schema-checks, and validates a result JSON file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let json = read_utf8_limited(path, MAX_RESULT_FILE_BYTES)?;
        let result: Self = serde_json::from_str(&json).map_err(|source| CoreError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        result.validate()?;
        Ok(result)
    }

    /// Parses, schema-checks, and validates a result from JSON text.
    pub fn from_json(json: &str) -> Result<Self> {
        let result: Self = serde_json::from_str(json).map_err(|source| CoreError::Json {
            path: serialization_path("run result JSON"),
            source,
        })?;
        result.validate()?;
        Ok(result)
    }

    /// Serializes a validated result as pretty JSON with a trailing newline.
    pub fn to_json_pretty(&self) -> Result<String> {
        self.validate()?;
        let json = serde_json::to_string_pretty(self).map_err(|source| CoreError::Json {
            path: serialization_path("run result JSON"),
            source,
        })?;
        String::from_utf8(with_trailing_newline(json)).map_err(|error| {
            CoreError::Validation(ValidationError::new("result", error.to_string()))
        })
    }

    /// Atomically creates a result JSON file without overwriting existing evidence.
    pub fn save_new(&self, path: impl AsRef<Path>) -> Result<()> {
        let json = self.to_json_pretty()?;
        atomic_write_new(path, json.as_bytes())
    }

    /// Atomically replaces a regular result file after validation.
    ///
    /// Benchmark runners should normally prefer [`RunResult::save_new`]. Replacement exists for
    /// explicit schema migration and repair tools.
    pub fn save_replace(&self, path: impl AsRef<Path>) -> Result<()> {
        let json = self.to_json_pretty()?;
        atomic_write_replace(path, json.as_bytes())
    }

    /// Returns added plus deleted lines using saturating arithmetic.
    #[must_use]
    pub fn diff_lines(&self) -> u64 {
        self.added_lines.saturating_add(self.deleted_lines)
    }

    /// Checks schema, identifier, timestamp, command, and artifact invariants.
    pub fn validate(&self) -> Result<()> {
        validate_schema("run result", self.schema_version)?;
        validate_uuid("run_id", &self.run_id)?;
        if let Some(group_id) = &self.group_id {
            validate_uuid("group_id", group_id)?;
        }
        validate_agent(&self.agent)?;
        if self
            .patcharena_version
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(ValidationError::new(
                "patcharena_version",
                "must not be blank when present",
            )
            .into());
        }
        if let Some(metadata) = &self.agent_metadata {
            validate_agent(&metadata.id)?;
            if metadata.id != self.agent {
                return Err(ValidationError::new(
                    "agent_metadata.id",
                    "must match the legacy agent field",
                )
                .into());
            }
            for (field, value) in [
                ("agent_metadata.display_name", &metadata.display_name),
                ("agent_metadata.adapter_version", &metadata.adapter_version),
                ("agent_metadata.command", &metadata.command),
            ] {
                if value.trim().is_empty() || value.contains('\0') {
                    return Err(
                        ValidationError::new(field, "must not be blank or contain NUL").into(),
                    );
                }
            }
        }
        if let Some(metadata) = &self.execution_metadata {
            if metadata.os.trim().is_empty()
                || metadata.arch.trim().is_empty()
                || metadata.repeat_index == 0
            {
                return Err(ValidationError::new(
                    "execution_metadata",
                    "requires non-empty OS/arch and a positive repeat index",
                )
                .into());
            }
            if metadata.agent_config_hash.len() != 64
                || !metadata
                    .agent_config_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(ValidationError::new(
                    "execution_metadata.agent_config_hash",
                    "must be a 64-character SHA-256 digest",
                )
                .into());
            }
        }
        if let Some(identity) = &self.benchmark_identity {
            identity.validate("benchmark_identity")?;
        }
        if self.finished_at < self.started_at {
            return Err(ValidationError::new("finished_at", "must not precede started_at").into());
        }
        if self.success && self.exit_code != Some(0) {
            return Err(ValidationError::new(
                "success",
                "a successful run must have agent exit code 0",
            )
            .into());
        }
        if self.success && self.verification.is_empty() {
            return Err(ValidationError::new(
                "verification",
                "a successful run must contain verification evidence",
            )
            .into());
        }

        validate_outcomes("setup", &self.setup)?;
        if let Some(outcome) = &self.agent_outcome {
            outcome
                .validate()
                .map_err(|error| prefix_validation("agent_outcome", error))?;
        }
        validate_outcomes("verification", &self.verification)?;
        for (index, event) in self.audit.iter().enumerate() {
            event.validate(&format!("audit[{index}]"))?;
        }
        for (index, violation) in self.violations.iter().enumerate() {
            violation
                .validate()
                .map_err(|error| prefix_validation(&format!("violations[{index}]"), error))?;
        }
        self.artifacts.validate()?;
        if self
            .error
            .as_ref()
            .is_some_and(|error| error.trim().is_empty())
        {
            return Err(ValidationError::new("error", "must not be blank when present").into());
        }
        if self.success && self.error.is_some() {
            return Err(ValidationError::new(
                "error",
                "a successful run cannot contain an error summary",
            )
            .into());
        }
        if self.success
            && (self.setup.iter().any(|outcome| !outcome.success)
                || self
                    .agent_outcome
                    .as_ref()
                    .is_some_and(|outcome| !outcome.success)
                || self.verification.iter().any(|outcome| !outcome.success)
                || !self.violations.is_empty())
        {
            return Err(ValidationError::new(
                "success",
                "a successful run cannot contain failed commands or violations",
            )
            .into());
        }
        Ok(())
    }
}

/// Per-agent entry in a sequential battle summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BattleAgentResult {
    /// Stable agent ID.
    pub agent_id: String,
    /// Run-group UUID when startup reached persistence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Ordered run UUIDs produced for this agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub run_ids: Vec<String>,
    /// Error summary when this agent could not complete its group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Versioned summary linking normal run results from a sequential multi-agent battle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BattleResult {
    /// Battle document schema version, independent of the application version.
    pub schema_version: u32,
    /// PatchArena application version that created this document.
    pub patcharena_version: String,
    /// Battle UUID.
    pub battle_id: String,
    /// Shared task ID.
    pub task_id: TaskId,
    /// Shared pinned Git commit.
    pub base_commit: String,
    /// Requested repeats per agent.
    pub repeat: u32,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Agent entries in invocation order.
    pub agents: Vec<BattleAgentResult>,
}

impl BattleResult {
    /// Validate battle identity, shared base, and linked run identifiers.
    pub fn validate(&self) -> Result<()> {
        validate_schema("battle result", self.schema_version)?;
        validate_uuid("battle_id", &self.battle_id)?;
        if self.patcharena_version.trim().is_empty() || self.repeat == 0 {
            return Err(ValidationError::new(
                "battle",
                "requires a version and positive repeat count",
            )
            .into());
        }
        BenchmarkIdentity {
            repository_commit: self.base_commit.clone(),
            task_fingerprint: "0".repeat(64),
        }
        .validate("battle")?;
        if self.agents.is_empty() {
            return Err(ValidationError::new("agents", "must not be empty").into());
        }
        let mut ids = HashSet::new();
        for entry in &self.agents {
            validate_agent(&entry.agent_id)?;
            if !ids.insert(&entry.agent_id) {
                return Err(ValidationError::new("agents", "agent IDs must be unique").into());
            }
            if let Some(group_id) = &entry.group_id {
                validate_uuid("agents.group_id", group_id)?;
            }
            for run_id in &entry.run_ids {
                validate_uuid("agents.run_ids", run_id)?;
            }
            if entry
                .error
                .as_ref()
                .is_some_and(|error| error.trim().is_empty())
            {
                return Err(
                    ValidationError::new("agents.error", "must not be blank when present").into(),
                );
            }
        }
        Ok(())
    }

    /// Read and validate a battle JSON document.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let json = read_utf8_limited(path, MAX_BATTLE_FILE_BYTES)?;
        let value: Self = serde_json::from_str(&json).map_err(|source| CoreError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        value.validate()?;
        Ok(value)
    }

    /// Serialize a battle document as stable pretty JSON.
    pub fn to_json_pretty(&self) -> Result<String> {
        self.validate()?;
        let json = serde_json::to_string_pretty(self).map_err(|source| CoreError::Json {
            path: serialization_path("battle result JSON"),
            source,
        })?;
        String::from_utf8(with_trailing_newline(json))
            .map_err(|error| ValidationError::new("battle", error.to_string()).into())
    }

    /// Persist a new immutable battle document.
    pub fn save_new(&self, path: impl AsRef<Path>) -> Result<()> {
        atomic_write_new(path, self.to_json_pretty()?.as_bytes())
    }
}

/// Lifecycle state of a persisted repeated-run group.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunGroupStatus {
    /// Legacy group metadata that predates explicit lifecycle tracking.
    #[default]
    Unknown,
    /// The requested runs are still being attempted.
    Running,
    /// Every requested run was persisted successfully.
    Completed,
    /// Execution stopped before the group could complete.
    Aborted,
}

/// A versioned group of repeated runs for one task and agent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunGroup {
    /// Required on-disk schema version.
    pub schema_version: u32,
    /// UUID identifying the group.
    pub group_id: String,
    /// Task shared by all runs in the group.
    pub task_id: TaskId,
    /// Agent shared by all runs in the group.
    pub agent: String,
    /// Whether repository instructions such as `AGENTS.md` are enabled for this group.
    #[serde(default = "default_instructions_enabled")]
    pub instructions_enabled: bool,
    /// Repository revision and task/policy digest shared by member runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_identity: Option<BenchmarkIdentity>,
    /// UTC group creation time.
    pub created_at: DateTime<Utc>,
    /// Number of runs requested when this group was created.
    ///
    /// This is absent only on legacy group records created before lifecycle tracking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_runs: Option<u32>,
    /// Current lifecycle state. Missing values on legacy records default to [`RunGroupStatus::Unknown`].
    #[serde(default)]
    pub status: RunGroupStatus,
    /// Ordered UUIDs of member runs.
    pub run_ids: Vec<String>,
}

impl RunGroup {
    /// Creates an empty group with a randomly generated UUID.
    pub fn new(
        task_id: TaskId,
        agent: impl Into<String>,
        created_at: DateTime<Utc>,
        requested_runs: u32,
    ) -> Result<Self> {
        let group = Self {
            schema_version: CURRENT_RESULT_SCHEMA_VERSION,
            group_id: Uuid::new_v4().to_string(),
            task_id,
            agent: agent.into(),
            instructions_enabled: true,
            benchmark_identity: None,
            created_at,
            requested_runs: Some(requested_runs),
            status: RunGroupStatus::Running,
            run_ids: Vec::new(),
        };
        group.validate()?;
        Ok(group)
    }

    /// Adds a validated, non-duplicate run UUID while this group is running.
    pub fn push_run_id(&mut self, run_id: impl Into<String>) -> Result<()> {
        self.validate()?;
        let run_id = run_id.into();
        validate_uuid("run_ids", &run_id)?;
        if self.status != RunGroupStatus::Running {
            return Err(ValidationError::new(
                "status",
                "run IDs can only be added while a group is running",
            )
            .into());
        }
        if self.run_ids.iter().any(|existing| existing == &run_id) {
            return Err(ValidationError::new("run_ids", "run IDs must be unique").into());
        }
        let Some(requested_runs) = self.requested_runs else {
            return Err(ValidationError::new(
                "requested_runs",
                "a running group must record its requested run count",
            )
            .into());
        };
        if self.run_ids.len() as u64 >= u64::from(requested_runs) {
            return Err(ValidationError::new(
                "run_ids",
                "cannot add more runs than requested_runs",
            )
            .into());
        }
        self.run_ids.push(run_id);
        Ok(())
    }

    /// Marks a running group complete once every requested run has been recorded.
    pub fn mark_completed(&mut self) -> Result<()> {
        self.validate()?;
        match self.status {
            RunGroupStatus::Completed => Ok(()),
            RunGroupStatus::Running => {
                let previous = self.status;
                self.status = RunGroupStatus::Completed;
                if let Err(error) = self.validate() {
                    self.status = previous;
                    return Err(error);
                }
                Ok(())
            }
            RunGroupStatus::Unknown | RunGroupStatus::Aborted => Err(ValidationError::new(
                "status",
                "only a running group can be marked completed",
            )
            .into()),
        }
    }

    /// Marks a running or not-yet-persisted completed group aborted.
    ///
    /// Allowing the completed-to-aborted transition lets a caller record that persisting the final
    /// completed state failed without discarding member run IDs.
    pub fn mark_aborted(&mut self) -> Result<()> {
        self.validate()?;
        match self.status {
            RunGroupStatus::Aborted => Ok(()),
            RunGroupStatus::Running | RunGroupStatus::Completed => {
                self.status = RunGroupStatus::Aborted;
                Ok(())
            }
            RunGroupStatus::Unknown => Err(ValidationError::new(
                "status",
                "a legacy group cannot be marked aborted without requested_runs",
            )
            .into()),
        }
    }

    /// Reads, parses, schema-checks, and validates a run-group JSON file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let json = read_utf8_limited(path, MAX_GROUP_FILE_BYTES)?;
        let group: Self = serde_json::from_str(&json).map_err(|source| CoreError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        group.validate()?;
        Ok(group)
    }

    /// Serializes a validated group as pretty JSON with a trailing newline.
    pub fn to_json_pretty(&self) -> Result<String> {
        self.validate()?;
        let json = serde_json::to_string_pretty(self).map_err(|source| CoreError::Json {
            path: serialization_path("run group JSON"),
            source,
        })?;
        String::from_utf8(with_trailing_newline(json)).map_err(|error| {
            CoreError::Validation(ValidationError::new("run group", error.to_string()))
        })
    }

    /// Atomically creates a run-group JSON file without overwriting an existing group.
    pub fn save_new(&self, path: impl AsRef<Path>) -> Result<()> {
        let json = self.to_json_pretty()?;
        atomic_write_new(path, json.as_bytes())
    }

    /// Atomically replaces a regular run-group file.
    pub fn save_replace(&self, path: impl AsRef<Path>) -> Result<()> {
        let json = self.to_json_pretty()?;
        atomic_write_replace(path, json.as_bytes())
    }

    /// Computes aggregate metrics after checking exact group membership.
    pub fn summarize(&self, results: &[RunResult]) -> Result<RunSummary> {
        self.validate()?;
        let mut by_id = HashMap::with_capacity(results.len());
        for result in results {
            result.validate()?;
            if result.task_id != self.task_id {
                return Err(ValidationError::new(
                    "results.task_id",
                    format!(
                        "run `{}` belongs to task `{}`",
                        result.run_id, result.task_id
                    ),
                )
                .into());
            }
            if result.agent != self.agent {
                return Err(ValidationError::new(
                    "results.agent",
                    format!(
                        "run `{}` belongs to agent `{}`",
                        result.run_id, result.agent
                    ),
                )
                .into());
            }
            if result.instructions_enabled != self.instructions_enabled {
                return Err(ValidationError::new(
                    "results.instructions_enabled",
                    format!(
                        "run `{}` has instructions_enabled={} but the group has {}",
                        result.run_id, result.instructions_enabled, self.instructions_enabled
                    ),
                )
                .into());
            }
            if result.benchmark_identity != self.benchmark_identity {
                return Err(ValidationError::new(
                    "results.benchmark_identity",
                    format!("run `{}` has a different benchmark identity", result.run_id),
                )
                .into());
            }
            if result
                .group_id
                .as_ref()
                .is_some_and(|group_id| group_id != &self.group_id)
            {
                return Err(ValidationError::new(
                    "results.group_id",
                    format!("run `{}` belongs to a different group", result.run_id),
                )
                .into());
            }
            if by_id.insert(result.run_id.as_str(), result).is_some() {
                return Err(ValidationError::new(
                    "results.run_id",
                    format!("duplicate result `{}`", result.run_id),
                )
                .into());
            }
        }
        if by_id.len() != self.run_ids.len()
            || self
                .run_ids
                .iter()
                .any(|run_id| !by_id.contains_key(run_id.as_str()))
        {
            return Err(ValidationError::new(
                "results",
                "results must exactly match the group's run_ids",
            )
            .into());
        }

        let ordered = self
            .run_ids
            .iter()
            .filter_map(|run_id| by_id.get(run_id.as_str()).copied())
            .collect::<Vec<_>>();
        Ok(RunSummary::from_validated(self, &ordered))
    }

    /// Checks schema, UUID, agent, lifecycle, and membership invariants.
    pub fn validate(&self) -> Result<()> {
        validate_schema("run group", self.schema_version)?;
        validate_uuid("group_id", &self.group_id)?;
        validate_agent(&self.agent)?;
        if let Some(identity) = &self.benchmark_identity {
            identity.validate("benchmark_identity")?;
        }
        let mut run_ids = HashSet::new();
        for (index, run_id) in self.run_ids.iter().enumerate() {
            validate_uuid(&format!("run_ids[{index}]"), run_id)?;
            if !run_ids.insert(run_id) {
                return Err(ValidationError::new(
                    format!("run_ids[{index}]"),
                    "run IDs must be unique",
                )
                .into());
            }
        }
        match (self.requested_runs, self.status) {
            (None, RunGroupStatus::Unknown) => {}
            (None, _) => {
                return Err(ValidationError::new(
                    "requested_runs",
                    "non-legacy group status requires a requested run count",
                )
                .into());
            }
            (Some(_), RunGroupStatus::Unknown) => {
                return Err(ValidationError::new(
                    "status",
                    "unknown status is reserved for legacy groups without requested_runs",
                )
                .into());
            }
            (Some(0), _) => {
                return Err(
                    ValidationError::new("requested_runs", "must be greater than zero").into(),
                );
            }
            (Some(requested_runs), status) => {
                let run_count = self.run_ids.len() as u64;
                let requested_runs = u64::from(requested_runs);
                if run_count > requested_runs {
                    return Err(ValidationError::new(
                        "run_ids",
                        "must not contain more entries than requested_runs",
                    )
                    .into());
                }
                if status == RunGroupStatus::Completed && run_count != requested_runs {
                    return Err(ValidationError::new(
                        "run_ids",
                        "a completed group must contain exactly requested_runs entries",
                    )
                    .into());
                }
            }
        }
        Ok(())
    }
}

/// Aggregate metrics for a validated run group.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSummary {
    /// Schema version of the summarized group.
    pub schema_version: u32,
    /// Group UUID.
    pub group_id: String,
    /// Summarized task ID.
    pub task_id: TaskId,
    /// Summarized agent backend.
    pub agent: String,
    /// Whether repository instructions were enabled for the summarized group.
    pub instructions_enabled: bool,
    /// Total number of runs.
    pub run_count: usize,
    /// Number of successful runs.
    pub successful_runs: usize,
    /// Successful runs divided by total runs, or zero for an empty group.
    pub success_rate: f64,
    /// Median wall-clock duration in milliseconds.
    pub median_duration_ms: Option<f64>,
    /// Population standard deviation of duration in milliseconds.
    pub duration_stddev_ms: Option<f64>,
    /// Median changed-file count.
    pub median_changed_files: Option<f64>,
    /// Median added-plus-deleted line count.
    pub median_diff_lines: Option<f64>,
    /// Number of failed verification commands across all runs.
    pub verification_failures: usize,
    /// Number of detected violations across all runs.
    pub violation_count: usize,
}

impl RunSummary {
    fn from_validated(group: &RunGroup, results: &[&RunResult]) -> Self {
        let successful_runs = results.iter().filter(|result| result.success).count();
        let durations = results
            .iter()
            .map(|result| result.duration_ms)
            .collect::<Vec<_>>();
        let changed_files = results
            .iter()
            .map(|result| result.changed_files)
            .collect::<Vec<_>>();
        let diff_lines = results
            .iter()
            .map(|result| result.diff_lines())
            .collect::<Vec<_>>();
        let verification_failures = results
            .iter()
            .flat_map(|result| &result.verification)
            .filter(|verification| !verification.success)
            .count();
        let violation_count = results.iter().map(|result| result.violations.len()).sum();
        let run_count = results.len();

        Self {
            schema_version: group.schema_version,
            group_id: group.group_id.clone(),
            task_id: group.task_id.clone(),
            agent: group.agent.clone(),
            instructions_enabled: group.instructions_enabled,
            run_count,
            successful_runs,
            success_rate: if run_count == 0 {
                0.0
            } else {
                successful_runs as f64 / run_count as f64
            },
            median_duration_ms: median(&durations),
            duration_stddev_ms: population_stddev(&durations),
            median_changed_files: median(&changed_files),
            median_diff_lines: median(&diff_lines),
            verification_failures,
            violation_count,
        }
    }
}

fn validate_schema(document: &'static str, schema_version: u32) -> Result<()> {
    if schema_version != CURRENT_RESULT_SCHEMA_VERSION {
        return Err(CoreError::UnsupportedSchema {
            document,
            found: schema_version,
            supported: CURRENT_RESULT_SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn validate_uuid(field: &str, value: &str) -> Result<()> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|error| ValidationError::new(field, format!("must be a UUID: {error}")).into())
}

fn validate_agent(agent: &str) -> Result<()> {
    if agent.trim().is_empty() {
        return Err(ValidationError::new("agent", "must not be empty").into());
    }
    if agent.len() > 128 {
        return Err(ValidationError::new("agent", "must be at most 128 bytes").into());
    }
    if agent.contains('\0') {
        return Err(ValidationError::new("agent", "must not contain a NUL byte").into());
    }
    Ok(())
}

fn validate_outcomes(field: &str, outcomes: &[CommandOutcome]) -> Result<()> {
    for (index, outcome) in outcomes.iter().enumerate() {
        outcome
            .validate()
            .map_err(|error| prefix_validation(&format!("{field}[{index}]"), error))?;
    }
    Ok(())
}

fn prefix_validation(prefix: &str, error: CoreError) -> CoreError {
    match error {
        CoreError::Validation(inner) => {
            ValidationError::new(format!("{prefix}.{}", inner.field), inner.message).into()
        }
        other => other,
    }
}

fn median(values: &[u64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        Some((sorted[middle - 1] as f64 + sorted[middle] as f64) / 2.0)
    } else {
        Some(sorted[middle] as f64)
    }
}

fn population_stddev(values: &[u64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mean = values.iter().map(|value| *value as f64).sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| {
            let delta = *value as f64 - mean;
            delta * delta
        })
        .sum::<f64>()
        / values.len() as f64;
    Some(variance.sqrt())
}

const fn is_false(value: &bool) -> bool {
    !*value
}

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

const fn default_instructions_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn result(run_id: &str, group_id: &str, success: bool, duration_ms: u64) -> RunResult {
        let exit_code = if success { 0 } else { 1 };
        RunResult {
            schema_version: 1,
            patcharena_version: None,
            run_id: run_id.to_owned(),
            group_id: Some(group_id.to_owned()),
            task_id: TaskId::new("example").expect("task ID"),
            agent: "codex".to_owned(),
            agent_metadata: None,
            execution_metadata: None,
            instructions_enabled: true,
            benchmark_identity: None,
            started_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            finished_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 1).unwrap(),
            duration_ms,
            success,
            exit_code: Some(exit_code),
            changed_files: if success { 2 } else { 4 },
            added_lines: 10,
            deleted_lines: 2,
            setup: vec![CommandOutcome::exited("cargo build", 0, 10)],
            agent_outcome: Some(CommandOutcome::exited("codex exec", exit_code, duration_ms)),
            verification: vec![CommandOutcome::exited("cargo test", exit_code, 20)],
            audit: Vec::new(),
            violations: if success {
                Vec::new()
            } else {
                vec![Violation::new(ViolationKind::ForbiddenPath, "changed .env").with_path(".env")]
            },
            artifacts: ArtifactPaths::default(),
            error: (!success).then(|| "verification failed".to_owned()),
        }
    }

    #[test]
    fn schema_version_is_required() {
        let json = r#"{
          "run_id":"d3b07384-d9a8-4c18-8f63-4f627c301097",
          "task_id":"example",
          "agent":"codex",
          "started_at":"2026-01-01T00:00:00Z",
          "finished_at":"2026-01-01T00:00:01Z",
          "duration_ms":1000,
          "success":true,
          "exit_code":0,
          "changed_files":1,
          "added_lines":1,
          "deleted_lines":0,
          "verification":[],
          "violations":[],
          "artifacts":{"stdout":"stdout.log","stderr":"stderr.log","patch":"changes.diff"}
        }"#;
        assert!(RunResult::from_json(json).is_err());
    }

    #[test]
    fn battle_json_round_trips_without_scoring_fields() {
        let battle = BattleResult {
            schema_version: 1,
            patcharena_version: "0.2.0".to_owned(),
            battle_id: "d3b07384-d9a8-4c18-8f63-4f627c301097".to_owned(),
            task_id: TaskId::new("example").expect("task"),
            base_commit: "a".repeat(40),
            repeat: 1,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            agents: vec![BattleAgentResult {
                agent_id: "codex".into(),
                group_id: None,
                run_ids: Vec::new(),
                error: Some("unavailable".into()),
            }],
        };
        let json = battle.to_json_pretty().expect("json");
        assert!(!json.contains("winner"));
        assert!(!json.contains("score"));
        let decoded: BattleResult = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, battle);
    }

    #[test]
    fn future_schema_is_rejected() {
        let group = RunGroup {
            schema_version: 2,
            group_id: Uuid::new_v4().to_string(),
            task_id: TaskId::new("example").expect("task ID"),
            agent: "codex".to_owned(),
            instructions_enabled: true,
            benchmark_identity: None,
            created_at: Utc::now(),
            requested_runs: None,
            status: RunGroupStatus::Unknown,
            run_ids: Vec::new(),
        };
        assert!(matches!(
            group.validate(),
            Err(CoreError::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn successful_result_requires_verification_and_no_error() {
        let group_id = Uuid::new_v4().to_string();
        let run_id = Uuid::new_v4().to_string();
        let mut successful = result(&run_id, &group_id, true, 100);
        successful.verification.clear();
        assert!(successful.validate().is_err());

        successful.verification = vec![CommandOutcome::exited("cargo test", 0, 20)];
        successful.error = Some("verification was skipped".to_owned());
        assert!(successful.validate().is_err());
    }

    #[test]
    fn summary_computes_success_and_variance() {
        let group_id = Uuid::new_v4().to_string();
        let run_one = Uuid::new_v4().to_string();
        let run_two = Uuid::new_v4().to_string();
        let group = RunGroup {
            schema_version: 1,
            group_id: group_id.clone(),
            task_id: TaskId::new("example").expect("task ID"),
            agent: "codex".to_owned(),
            instructions_enabled: true,
            benchmark_identity: None,
            created_at: Utc::now(),
            requested_runs: Some(2),
            status: RunGroupStatus::Completed,
            run_ids: vec![run_one.clone(), run_two.clone()],
        };
        let results = [
            result(&run_one, &group_id, true, 100),
            result(&run_two, &group_id, false, 300),
        ];
        let summary = group.summarize(&results).expect("summary");
        assert_eq!(summary.run_count, 2);
        assert_eq!(summary.success_rate, 0.5);
        assert_eq!(summary.median_duration_ms, Some(200.0));
        assert_eq!(summary.duration_stddev_ms, Some(100.0));
        assert_eq!(summary.verification_failures, 1);
        assert_eq!(summary.violation_count, 1);
    }

    #[test]
    fn run_group_lifecycle_enforces_requested_count_and_terminal_states() {
        let task_id = TaskId::new("example").expect("task ID");
        assert!(RunGroup::new(task_id.clone(), "codex", Utc::now(), 0).is_err());

        let mut group = RunGroup::new(task_id, "codex", Utc::now(), 2).expect("running group");
        assert_eq!(group.requested_runs, Some(2));
        assert_eq!(group.status, RunGroupStatus::Running);

        group
            .push_run_id(Uuid::new_v4().to_string())
            .expect("first run");
        assert!(group.mark_completed().is_err());
        assert_eq!(group.status, RunGroupStatus::Running);

        group
            .push_run_id(Uuid::new_v4().to_string())
            .expect("second run");
        assert!(group.push_run_id(Uuid::new_v4().to_string()).is_err());
        group.mark_completed().expect("complete group");
        assert_eq!(group.status, RunGroupStatus::Completed);
        assert!(group.push_run_id(Uuid::new_v4().to_string()).is_err());
        group.mark_completed().expect("completion is idempotent");
        group
            .mark_aborted()
            .expect("abort after persistence failure");
        assert_eq!(group.status, RunGroupStatus::Aborted);
        assert!(group.mark_completed().is_err());
    }

    #[test]
    fn running_group_can_be_aborted_but_not_completed_afterward() {
        let mut group = RunGroup::new(
            TaskId::new("example").expect("task ID"),
            "codex",
            Utc::now(),
            2,
        )
        .expect("running group");
        group
            .push_run_id(Uuid::new_v4().to_string())
            .expect("partial run");
        group.mark_aborted().expect("abort group");
        assert_eq!(group.status, RunGroupStatus::Aborted);
        assert!(group.push_run_id(Uuid::new_v4().to_string()).is_err());
        assert!(group.mark_completed().is_err());
        group.mark_aborted().expect("abort is idempotent");
    }

    #[test]
    fn run_group_validation_rejects_nonlegacy_unknown_and_invalid_counts() {
        let mut group = RunGroup::new(
            TaskId::new("example").expect("task ID"),
            "codex",
            Utc::now(),
            1,
        )
        .expect("running group");
        group.status = RunGroupStatus::Unknown;
        assert!(group.validate().is_err());

        group.requested_runs = None;
        group.status = RunGroupStatus::Running;
        assert!(group.validate().is_err());

        group.requested_runs = Some(0);
        assert!(group.validate().is_err());

        group.requested_runs = Some(1);
        group.status = RunGroupStatus::Completed;
        assert!(group.validate().is_err());
    }

    #[test]
    fn run_group_status_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&RunGroupStatus::Running).expect("serialize status"),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&RunGroupStatus::Completed).expect("serialize status"),
            "\"completed\""
        );
    }

    #[test]
    fn artifact_traversal_is_rejected() {
        let artifacts = ArtifactPaths {
            patch: PathBuf::from("../changes.diff"),
            ..ArtifactPaths::default()
        };
        assert!(artifacts.validate().is_err());
    }
}
