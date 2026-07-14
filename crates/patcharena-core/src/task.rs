use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::fs::{read_utf8_limited, serialization_path};
use crate::{
    CoreError, Result, ValidationError, atomic_write_new, atomic_write_replace,
    ensure_safe_relative_path,
};

const MAX_TASK_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TASK_ID_BYTES: usize = 128;
const MAX_PROMPT_BYTES: usize = 1024 * 1024;

fn default_timeout_seconds() -> u64 {
    600
}

fn default_max_changed_files() -> u64 {
    8
}

fn default_max_diff_lines() -> u64 {
    500
}

fn default_max_output_bytes() -> u64 {
    10 * 1024 * 1024
}

/// A validated task identifier that is safe to embed in a portable filename.
///
/// IDs contain one to 128 ASCII letters, digits, hyphens, or underscores and must begin with a
/// letter or digit. Windows device names are rejected for portability.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskId(String);

impl TaskId {
    /// Parses and validates a task ID.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_task_id(&value)?;
        Ok(Self(value))
    }

    /// Returns the task ID as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes this ID and returns its owned string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for TaskId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for TaskId {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for TaskId {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl Serialize for TaskId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TaskId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

fn validate_task_id(value: &str) -> Result<()> {
    validate_portable_id(value).map_err(|reason| CoreError::InvalidTaskId {
        value: value.to_owned(),
        reason,
    })
}

pub(crate) fn validate_portable_id(value: &str) -> std::result::Result<(), &'static str> {
    if value.is_empty() {
        return Err("ID must not be empty");
    }
    if value.len() > MAX_TASK_ID_BYTES {
        return Err("ID must be at most 128 bytes");
    }

    let mut bytes = value.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
        return Err("ID must begin with an ASCII letter or digit");
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')) {
        return Err("ID may contain only ASCII letters, digits, hyphens, and underscores");
    }

    let upper = value.to_ascii_uppercase();
    let is_device = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'));
    if is_device {
        return Err("ID is a reserved Windows device name");
    }
    Ok(())
}

/// A command expressed as an executable and an argument array.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredCommand {
    /// The executable name or path, passed directly to the process API.
    pub program: String,
    /// Arguments passed without shell interpolation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

/// A setup or verification command in a task definition.
///
/// The string form matches the concise YAML format in PatchArena examples. It is tokenized with
/// POSIX quoting rules and then executed directly as an executable plus argument array; shell
/// operators, substitutions, redirects, and environment assignments are never interpreted.
/// The structured form is preferred because it preserves the boundary without tokenization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TaskCommand {
    /// A command line tokenized into an executable and arguments without invoking a shell.
    CommandLine(String),
    /// An executable and explicit arguments.
    Structured(StructuredCommand),
}

impl TaskCommand {
    /// Creates a structured command without invoking a shell.
    #[must_use]
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self::Structured(StructuredCommand {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        })
    }

    /// Creates a command line that will be tokenized and executed without a shell.
    #[must_use]
    pub fn command_line(command: impl Into<String>) -> Self {
        Self::CommandLine(command.into())
    }

    /// Returns whether this command already supplies an explicit argument array.
    #[must_use]
    pub fn is_structured(&self) -> bool {
        matches!(self, Self::Structured(_))
    }

    /// Resolves the command into an executable and argument array without invoking a shell.
    ///
    /// The concise string form accepts POSIX-style quoting for grouping only. Tokens such as `|`,
    /// `&&`, `$HOME`, and `>` remain ordinary arguments and have no shell behavior.
    pub fn to_argv(&self) -> Result<(String, Vec<String>)> {
        match self {
            Self::CommandLine(command) => {
                let mut words = shell_words::split(command).map_err(|error| {
                    ValidationError::new(
                        "command",
                        format!("cannot tokenize command line: {error}"),
                    )
                })?;
                if words.is_empty() {
                    return Err(ValidationError::new("command", "must not be empty").into());
                }
                let program = words.remove(0);
                Ok((program, words))
            }
            Self::Structured(command) => Ok((command.program.clone(), command.args.clone())),
        }
    }

    /// Produces a stable, human-readable command string for logs and reports.
    #[must_use]
    pub fn audit_string(&self) -> String {
        match self {
            Self::CommandLine(command) => command.clone(),
            Self::Structured(command) => std::iter::once(command.program.as_str())
                .chain(command.args.iter().map(String::as_str))
                .map(quote_for_audit)
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    /// Validates that the executable, shell text, and arguments are non-empty and NUL-free.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::CommandLine(command) => {
                validate_command_piece("command", command, false)?;
                self.to_argv().map(|_| ())
            }
            Self::Structured(command) => {
                validate_command_piece("program", &command.program, false)?;
                for argument in &command.args {
                    validate_command_piece("argument", argument, true)?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for TaskCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.audit_string())
    }
}

fn quote_for_audit(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_+-./:=@".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn validate_command_piece(field: &str, value: &str, allow_empty: bool) -> Result<()> {
    if !allow_empty && value.trim().is_empty() {
        return Err(ValidationError::new(field, "must not be empty").into());
    }
    if value.contains('\0') {
        return Err(ValidationError::new(field, "must not contain a NUL byte").into());
    }
    Ok(())
}

/// An ordered list of task commands.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CommandList {
    /// Commands run sequentially in declaration order.
    pub commands: Vec<TaskCommand>,
}

impl CommandList {
    /// Returns whether the command list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    fn validate(&self, field: &str) -> Result<()> {
        for (index, command) in self.commands.iter().enumerate() {
            command.validate().map_err(|error| match error {
                CoreError::Validation(inner) => ValidationError::new(
                    format!("{field}.commands[{index}].{}", inner.field),
                    inner.message,
                )
                .into(),
                other => other,
            })?;
        }
        Ok(())
    }
}

/// Resource and change limits for one task run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskLimits {
    /// Maximum wall-clock duration for the agent, in seconds.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Maximum number of files the agent may change.
    #[serde(default = "default_max_changed_files")]
    pub max_changed_files: u64,
    /// Maximum total number of added and deleted diff lines.
    #[serde(default = "default_max_diff_lines")]
    pub max_diff_lines: u64,
    /// Maximum captured stdout plus stderr bytes for each process.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: u64,
}

impl Default for TaskLimits {
    fn default() -> Self {
        Self {
            timeout_seconds: default_timeout_seconds(),
            max_changed_files: default_max_changed_files(),
            max_diff_lines: default_max_diff_lines(),
            max_output_bytes: default_max_output_bytes(),
        }
    }
}

impl TaskLimits {
    fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("limits.timeout_seconds", self.timeout_seconds),
            ("limits.max_changed_files", self.max_changed_files),
            ("limits.max_diff_lines", self.max_diff_lines),
            ("limits.max_output_bytes", self.max_output_bytes),
        ] {
            if value == 0 {
                return Err(ValidationError::new(field, "must be greater than zero").into());
            }
        }
        Ok(())
    }
}

/// Command patterns and repository-relative paths a task must not touch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForbiddenRules {
    /// Case-sensitive command prefixes or substrings monitored by the runner.
    pub commands: Vec<String>,
    /// Portable repository-relative paths monitored by the runner.
    pub paths: Vec<PathBuf>,
}

impl Default for ForbiddenRules {
    fn default() -> Self {
        Self {
            commands: vec!["git push".to_owned(), "cargo publish".to_owned()],
            paths: vec![PathBuf::from(".git"), PathBuf::from(".env")],
        }
    }
}

impl ForbiddenRules {
    fn validate(&self) -> Result<()> {
        let mut commands = HashSet::new();
        for (index, command) in self.commands.iter().enumerate() {
            validate_command_piece(&format!("forbidden.commands[{index}]"), command, false)?;
            if !commands.insert(command) {
                return Err(ValidationError::new(
                    format!("forbidden.commands[{index}]"),
                    "duplicates an earlier command pattern",
                )
                .into());
            }
        }

        let mut paths = HashSet::new();
        for (index, path) in self.paths.iter().enumerate() {
            ensure_safe_relative_path(path).map_err(|error| {
                ValidationError::new(format!("forbidden.paths[{index}]"), format!("{error}"))
            })?;
            if !paths.insert(path) {
                return Err(ValidationError::new(
                    format!("forbidden.paths[{index}]"),
                    "duplicates an earlier forbidden path",
                )
                .into());
            }
        }
        Ok(())
    }
}

/// A complete, version-independent PatchArena task definition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDefinition {
    /// The stable task ID, also used as the YAML filename.
    pub id: TaskId,
    /// The prompt passed to the coding agent.
    pub prompt: String,
    /// Commands that prepare the isolated worktree before agent execution.
    #[serde(default, skip_serializing_if = "CommandList::is_empty")]
    pub setup: CommandList,
    /// Commands that determine whether the resulting patch is correct.
    pub verify: CommandList,
    /// Time, output, and patch-size limits.
    #[serde(default)]
    pub limits: TaskLimits,
    /// Forbidden command patterns and paths.
    #[serde(default)]
    pub forbidden: ForbiddenRules,
}

impl TaskDefinition {
    /// Creates a task with safe default limits and forbidden-operation rules.
    pub fn new(
        id: TaskId,
        prompt: impl Into<String>,
        verify: impl IntoIterator<Item = TaskCommand>,
    ) -> Result<Self> {
        let task = Self {
            id,
            prompt: prompt.into(),
            setup: CommandList::default(),
            verify: CommandList {
                commands: verify.into_iter().collect(),
            },
            limits: TaskLimits::default(),
            forbidden: ForbiddenRules::default(),
        };
        task.validate()?;
        Ok(task)
    }

    /// Parses and validates a task from YAML text.
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let task: Self = serde_yaml::from_str(yaml).map_err(|source| CoreError::Yaml {
            path: serialization_path("task YAML"),
            source,
        })?;
        task.validate()?;
        Ok(task)
    }

    /// Serializes a validated task to YAML with a trailing newline.
    pub fn to_yaml(&self) -> Result<String> {
        self.validate()?;
        let mut yaml = serde_yaml::to_string(self).map_err(|source| CoreError::Yaml {
            path: serialization_path("task YAML"),
            source,
        })?;
        if !yaml.ends_with('\n') {
            yaml.push('\n');
        }
        Ok(yaml)
    }

    /// Reads, parses, and validates a task YAML file with a defensive size limit.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let yaml = read_utf8_limited(path, MAX_TASK_FILE_BYTES)?;
        let task: Self = serde_yaml::from_str(&yaml).map_err(|source| CoreError::Yaml {
            path: path.to_path_buf(),
            source,
        })?;
        task.validate()?;
        Ok(task)
    }

    /// Atomically creates a task YAML file and refuses to overwrite an existing task.
    pub fn save_new(&self, path: impl AsRef<Path>) -> Result<()> {
        let yaml = self.to_yaml()?;
        atomic_write_new(path, yaml.as_bytes())
    }

    /// Atomically replaces a regular task YAML file after validating the task.
    pub fn save_replace(&self, path: impl AsRef<Path>) -> Result<()> {
        let yaml = self.to_yaml()?;
        atomic_write_replace(path, yaml.as_bytes())
    }

    /// Checks semantic invariants not expressible through Serde alone.
    pub fn validate(&self) -> Result<()> {
        validate_task_id(self.id.as_str())?;
        if self.prompt.trim().is_empty() {
            return Err(ValidationError::new("prompt", "must not be empty").into());
        }
        if self.prompt.len() > MAX_PROMPT_BYTES {
            return Err(ValidationError::new("prompt", "must be at most 1 MiB").into());
        }
        if self.prompt.contains('\0') {
            return Err(ValidationError::new("prompt", "must not contain a NUL byte").into());
        }
        self.setup.validate("setup")?;
        if self.verify.commands.is_empty() {
            return Err(ValidationError::new(
                "verify.commands",
                "must contain at least one verification command",
            )
            .into());
        }
        self.verify.validate("verify")?;
        self.limits.validate()?;
        self.forbidden.validate()?;
        Ok(())
    }
}

/// Returns the canonical task YAML path for `id` below `tasks_directory`.
#[must_use]
pub fn task_file_path(tasks_directory: impl AsRef<Path>, id: &TaskId) -> PathBuf {
    tasks_directory
        .as_ref()
        .join(format!("{}.yaml", id.as_str()))
}

/// Loads all regular `.yaml` and `.yml` task files in lexical filename order.
///
/// Symbolic links are rejected so a task directory cannot silently load YAML from outside the
/// repository metadata directory.
pub fn load_tasks(tasks_directory: impl AsRef<Path>) -> Result<Vec<TaskDefinition>> {
    let tasks_directory = tasks_directory.as_ref();
    let mut paths = Vec::new();
    for entry in fs::read_dir(tasks_directory)
        .map_err(|error| CoreError::io("list", tasks_directory, error))?
    {
        let entry = entry
            .map_err(|error| CoreError::io("read directory entry in", tasks_directory, error))?;
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
                reason: "task files must not be symbolic links",
            });
        }
        if is_yaml && metadata.is_file() {
            paths.push(path);
        }
    }
    paths.sort();

    let mut tasks = Vec::with_capacity(paths.len());
    let mut ids = HashSet::new();
    for path in paths {
        let task = TaskDefinition::load(&path)?;
        if !ids.insert(task.id.clone()) {
            return Err(
                ValidationError::new("tasks", format!("duplicate task ID `{}`", task.id)).into(),
            );
        }
        let expected_name = format!("{}.yaml", task.id);
        let file_name = path.file_name().and_then(|value| value.to_str());
        let alternate_name = format!("{}.yml", task.id);
        if !matches!(file_name, Some(name) if name == expected_name || name == alternate_name) {
            return Err(ValidationError::new(
                "task.id",
                format!(
                    "task ID `{}` does not match filename `{}`",
                    task.id,
                    path.display()
                ),
            )
            .into());
        }
        tasks.push(task);
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_rejects_traversal_and_reserved_names() {
        for id in ["../escape", "a/b", "a\\b", ".hidden", "", "CON", "com1"] {
            assert!(TaskId::new(id).is_err(), "accepted {id:?}");
        }
        for id in ["csv-newline-regression", "Task_2", "0"] {
            TaskId::new(id).expect(id);
        }
    }

    #[test]
    fn audit_rendering_quotes_ambiguous_arguments() {
        let command = TaskCommand::new("cargo", ["test", "name with spaces", "it's"]);
        assert_eq!(
            command.audit_string(),
            "cargo test 'name with spaces' 'it'\\''s'"
        );
    }

    #[test]
    fn yaml_supports_string_and_structured_commands() {
        let yaml = r#"
id: example-task
prompt: Fix it.
setup:
  commands:
    - cargo build
verify:
  commands:
    - program: cargo
      args: [test, --all]
limits:
  timeout_seconds: 42
forbidden:
  commands: [git push]
  paths: [.git]
"#;
        let task = TaskDefinition::from_yaml(yaml).expect("valid YAML");
        assert!(!task.setup.commands[0].is_structured());
        assert_eq!(
            task.setup.commands[0].to_argv().expect("tokenize"),
            ("cargo".to_owned(), vec!["build".to_owned()])
        );
        assert_eq!(task.limits.timeout_seconds, 42);
        assert_eq!(task.limits.max_changed_files, 8);
        assert_eq!(
            task.to_yaml().expect("serialize").lines().next(),
            Some("id: example-task")
        );
    }

    #[test]
    fn yaml_rejects_unknown_fields_and_empty_verification() {
        let typo = "id: task\nprompt: Fix\nverify:\n  commands: [cargo test]\nverfy: true\n";
        assert!(TaskDefinition::from_yaml(typo).is_err());

        let empty = "id: task\nprompt: Fix\nverify:\n  commands: []\n";
        assert!(TaskDefinition::from_yaml(empty).is_err());
    }

    #[test]
    fn serializer_helper_adds_newline() {
        assert_eq!(
            crate::fs::with_trailing_newline("value".to_owned()),
            b"value\n"
        );
    }
}
