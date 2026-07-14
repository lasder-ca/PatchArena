//! Shared, versioned data types and safe persistence primitives for PatchArena.
//!
//! This crate intentionally contains no process execution or Git integration.  It owns the
//! formats that cross crate and on-disk boundaries, together with their validation rules.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod config;
mod error;
mod fs;
mod path;
mod result;
mod task;

pub use config::{
    CONFIG_FILE_NAME, CURRENT_CONFIG_SCHEMA_VERSION, ProjectConfig, ProjectPaths,
    ResolvedProjectPaths, RunnerDefaults, SecurityDefaults,
};
pub use error::{CoreError, Result, ValidationError};
pub use fs::{atomic_write_new, atomic_write_replace, read_utf8_limited};
pub use path::{ensure_safe_relative_path, safe_join, safe_join_no_symlink_escape};
pub use result::{
    ArtifactPaths, AuditEvent, BenchmarkIdentity, CURRENT_RESULT_SCHEMA_VERSION, CommandOutcome,
    RunGroup, RunGroupStatus, RunPhase, RunResult, RunSummary, VerificationResult, Violation,
    ViolationKind,
};
pub use task::{
    CommandList, ForbiddenRules, StructuredCommand, TaskCommand, TaskDefinition, TaskId,
    TaskLimits, load_tasks, task_file_path,
};
