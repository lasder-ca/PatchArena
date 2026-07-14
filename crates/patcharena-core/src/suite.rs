use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::fs::{read_utf8_limited, serialization_path, with_trailing_newline};
use crate::task::validate_portable_id;
use crate::{
    BenchmarkIdentity, CURRENT_RESULT_SCHEMA_VERSION, CoreError, Result, TaskId, ValidationError,
    atomic_write_new, atomic_write_replace,
};

/// The suite-definition schema version supported by this release.
pub const CURRENT_SUITE_SCHEMA_VERSION: u32 = 1;
/// Maximum task-agent invocations represented by one suite execution.
pub const MAX_SUITE_INVOCATIONS: u64 = 1_000;

const MAX_SUITE_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SUITE_EXECUTION_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SUITE_DESCRIPTION_BYTES: usize = 1024;
const MAX_SUITE_TASKS: usize = 100;
const MAX_SUITE_ERROR_BYTES: usize = 4096;

/// A validated suite identifier safe to embed in a portable filename.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SuiteId(String);

impl SuiteId {
    /// Parse and validate a suite ID.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_portable_id(&value).map_err(|reason| CoreError::InvalidSuiteId {
            value: value.clone(),
            reason,
        })?;
        Ok(Self(value))
    }

    /// Return the suite ID as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume this ID and return its owned string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for SuiteId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for SuiteId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SuiteId {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for SuiteId {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl Serialize for SuiteId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SuiteId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// A versioned, reviewable ordered set of benchmark tasks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteDefinition {
    /// Required suite-definition schema version.
    pub schema_version: u32,
    /// Stable suite ID, also used as the YAML filename.
    pub id: SuiteId,
    /// Optional human-readable purpose of this suite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Ordered unique task IDs included in the suite.
    pub tasks: Vec<TaskId>,
}

impl SuiteDefinition {
    /// Create and validate a suite definition.
    pub fn new(id: SuiteId, description: Option<String>, tasks: Vec<TaskId>) -> Result<Self> {
        let suite = Self {
            schema_version: CURRENT_SUITE_SCHEMA_VERSION,
            id,
            description,
            tasks,
        };
        suite.validate()?;
        Ok(suite)
    }

    /// Check schema, description, task count, and uniqueness invariants.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CURRENT_SUITE_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedSchema {
                document: "suite definition",
                found: self.schema_version,
                supported: CURRENT_SUITE_SCHEMA_VERSION,
            });
        }
        validate_portable_id(self.id.as_str()).map_err(|reason| CoreError::InvalidSuiteId {
            value: self.id.to_string(),
            reason,
        })?;
        if let Some(description) = &self.description {
            if description.trim().is_empty() {
                return Err(
                    ValidationError::new("description", "must not be blank when present").into(),
                );
            }
            if description.len() > MAX_SUITE_DESCRIPTION_BYTES {
                return Err(
                    ValidationError::new("description", "must be at most 1024 bytes").into(),
                );
            }
            if description.contains('\0') {
                return Err(
                    ValidationError::new("description", "must not contain a NUL byte").into(),
                );
            }
        }
        if self.tasks.is_empty() {
            return Err(ValidationError::new("tasks", "must contain at least one task").into());
        }
        if self.tasks.len() > MAX_SUITE_TASKS {
            return Err(ValidationError::new("tasks", "must contain at most 100 tasks").into());
        }
        let mut seen = HashSet::with_capacity(self.tasks.len());
        for task in &self.tasks {
            if !seen.insert(task) {
                return Err(
                    ValidationError::new("tasks", format!("duplicate task ID `{task}`")).into(),
                );
            }
        }
        Ok(())
    }

    /// Parse and validate a suite from YAML text.
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let suite: Self = serde_yaml::from_str(yaml).map_err(|source| CoreError::Yaml {
            path: serialization_path("suite YAML"),
            source,
        })?;
        suite.validate()?;
        Ok(suite)
    }

    /// Serialize a validated suite to YAML with a trailing newline.
    pub fn to_yaml(&self) -> Result<String> {
        self.validate()?;
        let mut yaml = serde_yaml::to_string(self).map_err(|source| CoreError::Yaml {
            path: serialization_path("suite YAML"),
            source,
        })?;
        if !yaml.ends_with('\n') {
            yaml.push('\n');
        }
        Ok(yaml)
    }

    /// Return the deterministic SHA-256 fingerprint of the validated definition.
    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|source| CoreError::Json {
            path: serialization_path("suite fingerprint JSON"),
            source,
        })?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    /// Read and validate a bounded regular suite YAML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let yaml = read_utf8_limited(path, MAX_SUITE_FILE_BYTES)?;
        let suite: Self = serde_yaml::from_str(&yaml).map_err(|source| CoreError::Yaml {
            path: path.to_path_buf(),
            source,
        })?;
        suite.validate()?;
        Ok(suite)
    }

    /// Atomically create a suite YAML file without overwriting existing content.
    pub fn save_new(&self, path: impl AsRef<Path>) -> Result<()> {
        atomic_write_new(path, self.to_yaml()?.as_bytes())
    }

    /// Atomically replace a regular suite YAML file after validation.
    pub fn save_replace(&self, path: impl AsRef<Path>) -> Result<()> {
        atomic_write_replace(path, self.to_yaml()?.as_bytes())
    }
}

/// Return the canonical suite YAML path for `id` below `suites_directory`.
#[must_use]
pub fn suite_file_path(suites_directory: impl AsRef<Path>, id: &SuiteId) -> PathBuf {
    suites_directory
        .as_ref()
        .join(format!("{}.yaml", id.as_str()))
}

/// Load all regular suite YAML files in lexical filename order.
pub fn load_suites(suites_directory: impl AsRef<Path>) -> Result<Vec<SuiteDefinition>> {
    let suites_directory = suites_directory.as_ref();
    let mut paths = Vec::new();
    for entry in fs::read_dir(suites_directory)
        .map_err(|error| CoreError::io("list", suites_directory, error))?
    {
        let entry = entry
            .map_err(|error| CoreError::io("read directory entry in", suites_directory, error))?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| CoreError::io("inspect", &path, error))?;
        let is_yaml = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension, "yaml" | "yml"));
        if is_yaml && metadata.file_type().is_symlink() {
            return Err(CoreError::UnsafePath {
                path,
                reason: "suite files must not be symbolic links",
            });
        }
        if is_yaml && metadata.is_file() {
            paths.push(path);
        }
    }
    paths.sort();

    let mut suites = Vec::with_capacity(paths.len());
    let mut ids = HashSet::new();
    for path in paths {
        let suite = SuiteDefinition::load(&path)?;
        if !ids.insert(suite.id.clone()) {
            return Err(ValidationError::new(
                "suites",
                format!("duplicate suite ID `{}`", suite.id),
            )
            .into());
        }
        let yaml_name = format!("{}.yaml", suite.id);
        let yml_name = format!("{}.yml", suite.id);
        let file_name = path.file_name().and_then(|value| value.to_str());
        if !matches!(file_name, Some(name) if name == yaml_name || name == yml_name) {
            return Err(ValidationError::new(
                "suite.id",
                format!(
                    "suite ID `{}` does not match filename `{}`",
                    suite.id,
                    path.display()
                ),
            )
            .into());
        }
        suites.push(suite);
    }
    Ok(suites)
}

/// Lifecycle state of a persisted suite execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteExecutionStatus {
    /// A legacy record that predates explicit lifecycle tracking.
    #[default]
    LegacyUnknown,
    /// Some planned cells have not reached a terminal state.
    Running,
    /// Every planned cell completed with a persisted run group.
    Completed,
    /// Every cell was attempted, but at least one orchestration error was recorded.
    CompletedWithErrors,
    /// Execution stopped because the shared comparison basis became invalid.
    Aborted,
}

/// Lifecycle state of one task-and-agent suite cell.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteCellStatus {
    /// The cell has not been attempted yet.
    #[default]
    Pending,
    /// The cell produced an immutable run group, regardless of benchmark success.
    Completed,
    /// The cell could not produce a complete group because orchestration failed.
    Error,
}

/// A task and its preflight benchmark identity captured for a suite execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteTaskSnapshot {
    /// Task ID selected by the suite definition.
    pub task_id: TaskId,
    /// Repository commit and effective task/policy fingerprint expected for every agent.
    pub benchmark_identity: BenchmarkIdentity,
}

impl SuiteTaskSnapshot {
    /// Create a validated task snapshot.
    pub fn new(task_id: TaskId, benchmark_identity: BenchmarkIdentity) -> Result<Self> {
        let snapshot = Self {
            task_id,
            benchmark_identity,
        };
        snapshot.validate("task")?;
        Ok(snapshot)
    }

    fn validate(&self, field: &str) -> Result<()> {
        self.benchmark_identity
            .validate(&format!("{field}.benchmark_identity"))
    }
}

/// One planned task-and-agent cell in a suite execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteCell {
    /// Task ID for this cell.
    pub task_id: TaskId,
    /// Explicit stable agent ID for this cell.
    pub agent_id: String,
    /// Current cell lifecycle state.
    #[serde(default)]
    pub status: SuiteCellStatus,
    /// Immutable group UUID produced by a completed cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Bounded sanitized diagnostic produced by an orchestration error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SuiteCell {
    fn pending(task_id: TaskId, agent_id: String) -> Self {
        Self {
            task_id,
            agent_id,
            status: SuiteCellStatus::Pending,
            group_id: None,
            error: None,
        }
    }

    fn validate(&self, field: &str) -> Result<()> {
        validate_agent_id(&format!("{field}.agent_id"), &self.agent_id)?;
        match self.status {
            SuiteCellStatus::Pending => {
                if self.group_id.is_some() || self.error.is_some() {
                    return Err(ValidationError::new(
                        field,
                        "a pending cell cannot contain a group or error",
                    )
                    .into());
                }
            }
            SuiteCellStatus::Completed => {
                let group_id = self.group_id.as_deref().ok_or_else(|| {
                    ValidationError::new(field, "a completed cell requires a group ID")
                })?;
                validate_uuid(&format!("{field}.group_id"), group_id)?;
                if self.error.is_some() {
                    return Err(ValidationError::new(
                        field,
                        "a completed cell cannot contain an orchestration error",
                    )
                    .into());
                }
            }
            SuiteCellStatus::Error => {
                if self.group_id.is_some() {
                    return Err(ValidationError::new(
                        field,
                        "an error cell cannot contain a group ID",
                    )
                    .into());
                }
                let error = self.error.as_deref().ok_or_else(|| {
                    ValidationError::new(field, "an error cell requires a diagnostic")
                })?;
                validate_stored_error(&format!("{field}.error"), error)?;
            }
        }
        Ok(())
    }
}

/// A versioned, atomically checkpointed execution of one benchmark suite.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteExecution {
    /// Required result-record schema version.
    pub schema_version: u32,
    /// PatchArena application version that created this execution.
    pub patcharena_version: String,
    /// UUID identifying this suite execution.
    pub suite_run_id: String,
    /// Suite definition selected for this execution.
    pub suite_id: SuiteId,
    /// SHA-256 fingerprint of the canonical suite definition.
    pub suite_fingerprint: String,
    /// Shared full Git commit used by every task and agent.
    pub repository_commit: String,
    /// Ordered task IDs and expected benchmark identities.
    pub tasks: Vec<SuiteTaskSnapshot>,
    /// Ordered explicit stable agent IDs.
    pub agents: Vec<String>,
    /// Requested independent repetitions for every cell.
    pub repeat: u32,
    /// Whether repository instruction files are visible to every cell.
    pub instructions_enabled: bool,
    /// UTC creation time.
    pub created_at: DateTime<Utc>,
    /// UTC time of the most recent checkpoint mutation.
    pub updated_at: DateTime<Utc>,
    /// UTC terminal time, absent while running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Current suite execution lifecycle state.
    #[serde(default)]
    pub status: SuiteExecutionStatus,
    /// Stable task-major, agent-minor Cartesian execution cells.
    pub cells: Vec<SuiteCell>,
}

impl SuiteExecution {
    /// Create a running execution containing the exact task-and-agent Cartesian product.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        patcharena_version: impl Into<String>,
        suite_id: SuiteId,
        suite_fingerprint: impl Into<String>,
        repository_commit: impl Into<String>,
        tasks: Vec<SuiteTaskSnapshot>,
        agents: Vec<String>,
        repeat: u32,
        instructions_enabled: bool,
        created_at: DateTime<Utc>,
    ) -> Result<Self> {
        let cells = tasks
            .iter()
            .flat_map(|task| {
                agents
                    .iter()
                    .map(|agent| SuiteCell::pending(task.task_id.clone(), agent.clone()))
            })
            .collect();
        let execution = Self {
            schema_version: CURRENT_RESULT_SCHEMA_VERSION,
            patcharena_version: patcharena_version.into(),
            suite_run_id: Uuid::new_v4().to_string(),
            suite_id,
            suite_fingerprint: suite_fingerprint.into(),
            repository_commit: repository_commit.into(),
            tasks,
            agents,
            repeat,
            instructions_enabled,
            created_at,
            updated_at: created_at,
            completed_at: None,
            status: SuiteExecutionStatus::Running,
            cells,
        };
        execution.validate()?;
        Ok(execution)
    }

    /// Validate schema, plan shape, identities, timestamps, and lifecycle invariants.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CURRENT_RESULT_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedSchema {
                document: "suite execution",
                found: self.schema_version,
                supported: CURRENT_RESULT_SCHEMA_VERSION,
            });
        }
        if self.patcharena_version.trim().is_empty()
            || self.patcharena_version.len() > 128
            || self.patcharena_version.contains('\0')
        {
            return Err(ValidationError::new(
                "patcharena_version",
                "must be a nonblank value of at most 128 bytes without NUL",
            )
            .into());
        }
        validate_uuid("suite_run_id", &self.suite_run_id)?;
        validate_hex_digest("suite_fingerprint", &self.suite_fingerprint, &[64])?;
        validate_hex_digest("repository_commit", &self.repository_commit, &[40, 64])?;
        if self.repeat == 0 || self.repeat > 1_000 {
            return Err(ValidationError::new("repeat", "must be between 1 and 1000").into());
        }
        if self.tasks.is_empty() || self.tasks.len() > MAX_SUITE_TASKS {
            return Err(
                ValidationError::new("tasks", "must contain between 1 and 100 tasks").into(),
            );
        }
        let mut task_ids = HashSet::with_capacity(self.tasks.len());
        for (index, task) in self.tasks.iter().enumerate() {
            task.validate(&format!("tasks[{index}]"))?;
            if task.benchmark_identity.repository_commit != self.repository_commit {
                return Err(ValidationError::new(
                    format!("tasks[{index}].benchmark_identity.repository_commit"),
                    "must match the suite repository commit",
                )
                .into());
            }
            if !task_ids.insert(&task.task_id) {
                return Err(ValidationError::new("tasks", "task IDs must be unique").into());
            }
        }
        if self.agents.is_empty() || self.agents.len() > 100 {
            return Err(
                ValidationError::new("agents", "must contain between 1 and 100 agents").into(),
            );
        }
        let mut agent_ids = HashSet::with_capacity(self.agents.len());
        for (index, agent) in self.agents.iter().enumerate() {
            validate_agent_id(&format!("agents[{index}]"), agent)?;
            if !agent_ids.insert(agent) {
                return Err(ValidationError::new("agents", "agent IDs must be unique").into());
            }
        }
        let expected_cells = self
            .tasks
            .len()
            .checked_mul(self.agents.len())
            .ok_or_else(|| ValidationError::new("cells", "cell count overflowed"))?;
        let invocation_count = u64::try_from(expected_cells)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.repeat));
        if invocation_count > MAX_SUITE_INVOCATIONS {
            return Err(ValidationError::new(
                "cells",
                "a suite execution must contain at most 1,000 agent invocations",
            )
            .into());
        }
        if self.cells.len() != expected_cells {
            return Err(ValidationError::new(
                "cells",
                format!("expected {expected_cells} task-and-agent cells"),
            )
            .into());
        }
        for (index, ((task, agent), cell)) in self
            .tasks
            .iter()
            .flat_map(|task| self.agents.iter().map(move |agent| (task, agent)))
            .zip(&self.cells)
            .enumerate()
        {
            if cell.task_id != task.task_id || &cell.agent_id != agent {
                return Err(ValidationError::new(
                    format!("cells[{index}]"),
                    "must follow stable task-major, agent-minor plan order",
                )
                .into());
            }
            cell.validate(&format!("cells[{index}]"))?;
        }
        if self.updated_at < self.created_at {
            return Err(ValidationError::new("updated_at", "must not precede created_at").into());
        }
        if self
            .completed_at
            .is_some_and(|completed_at| completed_at < self.created_at)
        {
            return Err(ValidationError::new("completed_at", "must not precede created_at").into());
        }
        let pending = self
            .cells
            .iter()
            .filter(|cell| cell.status == SuiteCellStatus::Pending)
            .count();
        let errors = self
            .cells
            .iter()
            .filter(|cell| cell.status == SuiteCellStatus::Error)
            .count();
        match self.status {
            SuiteExecutionStatus::LegacyUnknown => {}
            SuiteExecutionStatus::Running => {
                if self.completed_at.is_some() {
                    return Err(ValidationError::new(
                        "completed_at",
                        "must be absent while a suite is running",
                    )
                    .into());
                }
            }
            SuiteExecutionStatus::Completed => {
                if pending != 0 || errors != 0 || self.completed_at.is_none() {
                    return Err(ValidationError::new(
                        "status",
                        "completed requires terminal group-backed cells and a completion time",
                    )
                    .into());
                }
            }
            SuiteExecutionStatus::CompletedWithErrors => {
                if pending != 0 || errors == 0 || self.completed_at.is_none() {
                    return Err(ValidationError::new(
                        "status",
                        "completed_with_errors requires no pending cells, at least one error, and a completion time",
                    )
                    .into());
                }
            }
            SuiteExecutionStatus::Aborted => {
                if self.completed_at.is_none() {
                    return Err(ValidationError::new(
                        "status",
                        "aborted requires a completion time",
                    )
                    .into());
                }
            }
        }
        Ok(())
    }

    /// Mark one pending cell completed with an immutable group UUID.
    pub fn complete_cell(
        &mut self,
        task_id: &str,
        agent_id: &str,
        group_id: impl Into<String>,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        self.validate()?;
        ensure_running(self.status)?;
        let group_id = group_id.into();
        validate_uuid("group_id", &group_id)?;
        let index = self.cell_index(task_id, agent_id)?;
        if self.cells[index].status != SuiteCellStatus::Pending {
            return Err(
                ValidationError::new("cell", "only a pending cell can be completed").into(),
            );
        }
        let previous_cell = self.cells[index].clone();
        let previous_updated_at = self.updated_at;
        self.cells[index].status = SuiteCellStatus::Completed;
        self.cells[index].group_id = Some(group_id);
        self.updated_at = updated_at;
        if let Err(error) = self.validate() {
            self.cells[index] = previous_cell;
            self.updated_at = previous_updated_at;
            return Err(error);
        }
        Ok(())
    }

    /// Mark one pending cell failed with a bounded sanitized orchestration diagnostic.
    pub fn error_cell(
        &mut self,
        task_id: &str,
        agent_id: &str,
        error: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        self.validate()?;
        ensure_running(self.status)?;
        let error = sanitize_error(error)?;
        let index = self.cell_index(task_id, agent_id)?;
        if self.cells[index].status != SuiteCellStatus::Pending {
            return Err(
                ValidationError::new("cell", "only a pending cell can record an error").into(),
            );
        }
        let previous_cell = self.cells[index].clone();
        let previous_updated_at = self.updated_at;
        self.cells[index].status = SuiteCellStatus::Error;
        self.cells[index].error = Some(error);
        self.updated_at = updated_at;
        if let Err(error) = self.validate() {
            self.cells[index] = previous_cell;
            self.updated_at = previous_updated_at;
            return Err(error);
        }
        Ok(())
    }

    /// Mark a fully attempted running execution completed, preserving cell errors.
    pub fn mark_finished(&mut self, completed_at: DateTime<Utc>) -> Result<()> {
        self.validate()?;
        ensure_running(self.status)?;
        if self.pending_cells().next().is_some() {
            return Err(ValidationError::new(
                "cells",
                "all cells must be terminal before completion",
            )
            .into());
        }
        self.status = if self
            .cells
            .iter()
            .any(|cell| cell.status == SuiteCellStatus::Error)
        {
            SuiteExecutionStatus::CompletedWithErrors
        } else {
            SuiteExecutionStatus::Completed
        };
        self.updated_at = completed_at;
        self.completed_at = Some(completed_at);
        self.validate()
    }

    /// Mark a running execution aborted while retaining terminal and pending cells.
    pub fn mark_aborted(&mut self, completed_at: DateTime<Utc>) -> Result<()> {
        self.validate()?;
        match self.status {
            SuiteExecutionStatus::Aborted => return Ok(()),
            SuiteExecutionStatus::Running => {}
            _ => {
                return Err(ValidationError::new(
                    "status",
                    "only a running suite execution can be aborted",
                )
                .into());
            }
        }
        self.status = SuiteExecutionStatus::Aborted;
        self.updated_at = completed_at;
        self.completed_at = Some(completed_at);
        self.validate()
    }

    /// Iterate over pending cells in stable plan order.
    pub fn pending_cells(&self) -> impl Iterator<Item = &SuiteCell> {
        self.cells
            .iter()
            .filter(|cell| cell.status == SuiteCellStatus::Pending)
    }

    /// Read and validate a bounded suite execution JSON checkpoint.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let json = read_utf8_limited(path, MAX_SUITE_EXECUTION_FILE_BYTES)?;
        let execution: Self = serde_json::from_str(&json).map_err(|source| CoreError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        execution.validate()?;
        Ok(execution)
    }

    /// Serialize a validated suite execution as stable pretty JSON.
    pub fn to_json_pretty(&self) -> Result<String> {
        self.validate()?;
        let json = serde_json::to_string_pretty(self).map_err(|source| CoreError::Json {
            path: serialization_path("suite execution JSON"),
            source,
        })?;
        String::from_utf8(with_trailing_newline(json))
            .map_err(|error| ValidationError::new("suite execution", error.to_string()).into())
    }

    /// Atomically create a new suite execution checkpoint.
    pub fn save_new(&self, path: impl AsRef<Path>) -> Result<()> {
        atomic_write_new(path, self.to_json_pretty()?.as_bytes())
    }

    /// Atomically replace an existing regular suite execution checkpoint.
    pub fn save_replace(&self, path: impl AsRef<Path>) -> Result<()> {
        atomic_write_replace(path, self.to_json_pretty()?.as_bytes())
    }

    fn cell_index(&self, task_id: &str, agent_id: &str) -> Result<usize> {
        self.cells
            .iter()
            .position(|cell| cell.task_id.as_str() == task_id && cell.agent_id == agent_id)
            .ok_or_else(|| {
                ValidationError::new(
                    "cell",
                    format!("unknown task-and-agent cell `{task_id}` / `{agent_id}`"),
                )
                .into()
            })
    }
}

/// Return the generated directory for a validated suite-run UUID.
pub fn suite_run_directory(
    suite_runs_root: impl AsRef<Path>,
    suite_run_id: &str,
) -> Result<PathBuf> {
    validate_uuid("suite_run_id", suite_run_id)?;
    Ok(suite_runs_root.as_ref().join(suite_run_id))
}

/// Return the `suite.json` checkpoint path for a validated suite-run UUID.
pub fn suite_checkpoint_path(
    suite_runs_root: impl AsRef<Path>,
    suite_run_id: &str,
) -> Result<PathBuf> {
    Ok(suite_run_directory(suite_runs_root, suite_run_id)?.join("suite.json"))
}

fn ensure_running(status: SuiteExecutionStatus) -> Result<()> {
    if status == SuiteExecutionStatus::Running {
        Ok(())
    } else {
        Err(ValidationError::new("status", "cell updates require a running suite execution").into())
    }
}

fn validate_uuid(field: &str, value: &str) -> Result<()> {
    Uuid::parse_str(value)
        .map(|_| ())
        .map_err(|error| ValidationError::new(field, format!("must be a UUID: {error}")).into())
}

fn validate_hex_digest(field: &str, value: &str, lengths: &[usize]) -> Result<()> {
    if !lengths.contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ValidationError::new(
            field,
            format!("must be hexadecimal with length in {lengths:?}"),
        )
        .into());
    }
    Ok(())
}

fn validate_agent_id(field: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 128 || value.contains('\0') {
        return Err(ValidationError::new(
            field,
            "must be a nonempty value of at most 128 bytes without NUL",
        )
        .into());
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(ValidationError::new(
            field,
            "must use lowercase ASCII letters, digits, or hyphens",
        )
        .into());
    }
    Ok(())
}

fn sanitize_error(value: &str) -> Result<String> {
    if value.len() > MAX_SUITE_ERROR_BYTES {
        return Err(ValidationError::new("error", "must be at most 4096 bytes").into());
    }
    let replaced = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let sanitized = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    validate_stored_error("error", &sanitized)?;
    Ok(sanitized)
}

fn validate_stored_error(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(ValidationError::new(field, "must not be blank").into());
    }
    if value.len() > MAX_SUITE_ERROR_BYTES {
        return Err(ValidationError::new(field, "must be at most 4096 bytes").into());
    }
    if value.chars().any(char::is_control) {
        return Err(ValidationError::new(field, "must not contain control characters").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::*;
    use crate::BenchmarkIdentity;

    fn identity(character: char) -> BenchmarkIdentity {
        BenchmarkIdentity {
            repository_commit: "a".repeat(40),
            task_fingerprint: character.to_string().repeat(64),
        }
    }

    fn execution_fixture() -> SuiteExecution {
        SuiteExecution::new(
            "0.3.0",
            SuiteId::new("core").unwrap(),
            "b".repeat(64),
            "a".repeat(40),
            vec![
                SuiteTaskSnapshot::new(TaskId::new("one").unwrap(), identity('1')).unwrap(),
                SuiteTaskSnapshot::new(TaskId::new("two").unwrap(), identity('2')).unwrap(),
            ],
            vec!["alpha".to_owned(), "beta".to_owned()],
            2,
            true,
            Utc::now(),
        )
        .unwrap()
    }

    #[test]
    fn maximum_error_matrix_round_trips_through_the_checkpoint_limit() {
        let now = Utc::now();
        let tasks = (0..100)
            .map(|index| {
                let suffix = format!("{index:03}");
                SuiteTaskSnapshot::new(
                    TaskId::new(format!("t{suffix}{}", "t".repeat(128 - 1 - suffix.len())))
                        .unwrap(),
                    BenchmarkIdentity {
                        repository_commit: "a".repeat(40),
                        task_fingerprint: format!("{index:064x}"),
                    },
                )
                .unwrap()
            })
            .collect();
        let agents = (0..10)
            .map(|index| {
                let suffix = index.to_string();
                format!("a{suffix}{}", "a".repeat(128 - 1 - suffix.len()))
            })
            .collect();
        let mut execution = SuiteExecution::new(
            "\"".repeat(128),
            SuiteId::new("s".repeat(128)).unwrap(),
            "b".repeat(64),
            "a".repeat(40),
            tasks,
            agents,
            1,
            true,
            now,
        )
        .unwrap();
        for cell in &mut execution.cells {
            cell.status = SuiteCellStatus::Error;
            cell.error = Some("\"".repeat(MAX_SUITE_ERROR_BYTES));
        }
        execution.status = SuiteExecutionStatus::CompletedWithErrors;
        execution.completed_at = Some(now);
        execution.validate().unwrap();
        let directory = tempdir().unwrap();
        let path = directory.path().join("suite.json");

        execution.save_new(&path).unwrap();
        assert_eq!(SuiteExecution::load(path).unwrap(), execution);
    }

    #[test]
    fn execution_rejects_more_than_one_thousand_agent_invocations() {
        let tasks = (0..11)
            .map(|index| {
                SuiteTaskSnapshot::new(
                    TaskId::new(format!("task-{index}")).unwrap(),
                    BenchmarkIdentity {
                        repository_commit: "a".repeat(40),
                        task_fingerprint: format!("{index:064x}"),
                    },
                )
                .unwrap()
            })
            .collect();
        let agents = (0..10).map(|index| format!("agent-{index}")).collect();

        let result = SuiteExecution::new(
            "0.3.0",
            SuiteId::new("too-large").unwrap(),
            "b".repeat(64),
            "a".repeat(40),
            tasks,
            agents,
            10,
            true,
            Utc::now(),
        );

        assert!(
            matches!(result, Err(CoreError::Validation(error)) if error.to_string().contains("1,000"))
        );
    }

    #[test]
    fn execution_builds_exact_cartesian_product_and_checkpoints() {
        let directory = tempdir().expect("temporary directory");
        let mut execution = execution_fixture();
        assert_eq!(execution.cells.len(), 4);
        assert_eq!(execution.cells[0].task_id.as_str(), "one");
        assert_eq!(execution.cells[0].agent_id, "alpha");
        assert_eq!(execution.cells[3].task_id.as_str(), "two");
        assert_eq!(execution.cells[3].agent_id, "beta");

        execution
            .complete_cell("one", "alpha", Uuid::new_v4().to_string(), Utc::now())
            .unwrap();
        execution
            .error_cell("one", "beta", "agent\nunavailable", Utc::now())
            .unwrap();
        assert_eq!(
            execution.cells[1].error.as_deref(),
            Some("agent unavailable")
        );
        assert!(execution.mark_finished(Utc::now()).is_err());
        assert_eq!(execution.pending_cells().count(), 2);

        for (task, agent) in [("two", "alpha"), ("two", "beta")] {
            execution
                .complete_cell(task, agent, Uuid::new_v4().to_string(), Utc::now())
                .unwrap();
        }
        execution.mark_finished(Utc::now()).unwrap();
        assert_eq!(execution.status, SuiteExecutionStatus::CompletedWithErrors);

        let run_directory = suite_run_directory(directory.path(), &execution.suite_run_id).unwrap();
        std::fs::create_dir(&run_directory).expect("suite run directory");
        let checkpoint = suite_checkpoint_path(directory.path(), &execution.suite_run_id).unwrap();
        execution.save_new(&checkpoint).expect("save execution");
        assert_eq!(SuiteExecution::load(checkpoint).unwrap(), execution);
    }

    #[test]
    fn execution_rejects_illegal_cell_shapes_and_transitions() {
        let mut execution = execution_fixture();
        execution
            .complete_cell("one", "alpha", Uuid::new_v4().to_string(), Utc::now())
            .unwrap();
        assert!(
            execution
                .complete_cell("one", "alpha", Uuid::new_v4().to_string(), Utc::now(),)
                .is_err()
        );
        assert!(
            execution
                .error_cell("missing", "alpha", "error", Utc::now())
                .is_err()
        );
        assert!(
            execution
                .error_cell("one", "beta", &"x".repeat(4097), Utc::now())
                .is_err()
        );
        execution.mark_aborted(Utc::now()).unwrap();
        assert_eq!(execution.status, SuiteExecutionStatus::Aborted);
        assert!(
            execution
                .error_cell("one", "beta", "late", Utc::now())
                .is_err()
        );
    }

    #[test]
    fn suite_run_paths_require_a_uuid() {
        let directory = tempdir().expect("temporary directory");
        assert!(suite_run_directory(directory.path(), "../outside").is_err());
        assert!(suite_checkpoint_path(directory.path(), "not-a-uuid").is_err());
    }
}
