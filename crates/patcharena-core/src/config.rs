use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs::{read_utf8_limited, with_trailing_newline};
use crate::{
    CoreError, Result, ValidationError, atomic_write_new, atomic_write_replace,
    ensure_safe_relative_path, safe_join,
};

/// The repository-root configuration filename.
pub const CONFIG_FILE_NAME: &str = "patcharena.toml";

/// The only project-configuration schema version supported by this release.
pub const CURRENT_CONFIG_SCHEMA_VERSION: u32 = 1;

const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;

fn default_timeout_seconds() -> u64 {
    600
}

fn default_max_output_bytes() -> u64 {
    10 * 1024 * 1024
}

fn default_max_changed_files() -> u64 {
    8
}

fn default_max_diff_lines() -> u64 {
    500
}

fn default_environment_allowlist() -> Vec<String> {
    [
        "PATH",
        "HOME",
        "USER",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "RUSTUP_HOME",
        "CARGO_HOME",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Portable paths used for PatchArena repository metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectPaths {
    /// Root directory for all PatchArena-generated repository metadata.
    pub state_dir: PathBuf,
    /// Directory containing task YAML files.
    pub tasks_dir: PathBuf,
    /// Directory containing immutable per-run artifacts.
    pub runs_dir: PathBuf,
    /// Directory containing repeat-run group metadata.
    pub groups_dir: PathBuf,
}

impl Default for ProjectPaths {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::from(".patcharena"),
            tasks_dir: PathBuf::from(".patcharena/tasks"),
            runs_dir: PathBuf::from(".patcharena/runs"),
            groups_dir: PathBuf::from(".patcharena/groups"),
        }
    }
}

impl ProjectPaths {
    fn validate(&self) -> Result<()> {
        for (field, path) in [
            ("paths.state_dir", &self.state_dir),
            ("paths.tasks_dir", &self.tasks_dir),
            ("paths.runs_dir", &self.runs_dir),
            ("paths.groups_dir", &self.groups_dir),
        ] {
            ensure_safe_relative_path(path)
                .map_err(|error| ValidationError::new(field, format!("{error}")))?;
        }
        let mut paths = HashSet::new();
        for (field, path) in [
            ("paths.state_dir", &self.state_dir),
            ("paths.tasks_dir", &self.tasks_dir),
            ("paths.runs_dir", &self.runs_dir),
            ("paths.groups_dir", &self.groups_dir),
        ] {
            if !paths.insert(path) {
                return Err(
                    ValidationError::new(field, "configured paths must be distinct").into(),
                );
            }
        }
        for (field, path) in [
            ("paths.tasks_dir", &self.tasks_dir),
            ("paths.runs_dir", &self.runs_dir),
            ("paths.groups_dir", &self.groups_dir),
        ] {
            if !path.starts_with(&self.state_dir) {
                return Err(
                    ValidationError::new(field, "must be contained by paths.state_dir").into(),
                );
            }
        }
        Ok(())
    }
}

/// Project safety ceilings and environment policy used to seed and cap tasks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunnerDefaults {
    /// Project-wide upper bound for the agent timeout, in seconds.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Project-wide upper bound for captured bytes from each command.
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: u64,
    /// Project-wide upper bound for the changed-file count.
    #[serde(default = "default_max_changed_files")]
    pub max_changed_files: u64,
    /// Project-wide upper bound for added plus deleted diff lines.
    #[serde(default = "default_max_diff_lines")]
    pub max_diff_lines: u64,
    /// Names of host environment variables copied into child processes.
    #[serde(default = "default_environment_allowlist")]
    pub environment_allowlist: Vec<String>,
}

impl Default for RunnerDefaults {
    fn default() -> Self {
        Self {
            timeout_seconds: default_timeout_seconds(),
            max_output_bytes: default_max_output_bytes(),
            max_changed_files: default_max_changed_files(),
            max_diff_lines: default_max_diff_lines(),
            environment_allowlist: default_environment_allowlist(),
        }
    }
}

impl RunnerDefaults {
    fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("defaults.timeout_seconds", self.timeout_seconds),
            ("defaults.max_output_bytes", self.max_output_bytes),
            ("defaults.max_changed_files", self.max_changed_files),
            ("defaults.max_diff_lines", self.max_diff_lines),
        ] {
            if value == 0 {
                return Err(ValidationError::new(field, "must be greater than zero").into());
            }
        }
        let mut names = HashSet::new();
        for (index, name) in self.environment_allowlist.iter().enumerate() {
            if !is_environment_name(name) {
                return Err(ValidationError::new(
                    format!("defaults.environment_allowlist[{index}]"),
                    "must be a portable environment-variable name",
                )
                .into());
            }
            if !names.insert(name) {
                return Err(ValidationError::new(
                    format!("defaults.environment_allowlist[{index}]"),
                    "duplicates an earlier variable name",
                )
                .into());
            }
        }
        Ok(())
    }
}

/// Project-wide defense-in-depth patterns added to each task's rules.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityDefaults {
    /// Dangerous command text patterns monitored by the runner.
    pub forbidden_commands: Vec<String>,
    /// Repository-relative paths monitored by the runner.
    pub forbidden_paths: Vec<PathBuf>,
}

impl Default for SecurityDefaults {
    fn default() -> Self {
        Self {
            forbidden_commands: vec!["git push".to_owned(), "cargo publish".to_owned()],
            forbidden_paths: vec![PathBuf::from(".git"), PathBuf::from(".env")],
        }
    }
}

impl SecurityDefaults {
    fn validate(&self) -> Result<()> {
        let mut commands = HashSet::new();
        for (index, command) in self.forbidden_commands.iter().enumerate() {
            if command.trim().is_empty() || command.contains('\0') {
                return Err(ValidationError::new(
                    format!("security.forbidden_commands[{index}]"),
                    "must not be blank or contain NUL",
                )
                .into());
            }
            if !commands.insert(command) {
                return Err(ValidationError::new(
                    format!("security.forbidden_commands[{index}]"),
                    "duplicates an earlier command pattern",
                )
                .into());
            }
        }
        let mut paths = HashSet::new();
        for (index, path) in self.forbidden_paths.iter().enumerate() {
            ensure_safe_relative_path(path).map_err(|error| {
                ValidationError::new(
                    format!("security.forbidden_paths[{index}]"),
                    format!("{error}"),
                )
            })?;
            if !paths.insert(path) {
                return Err(ValidationError::new(
                    format!("security.forbidden_paths[{index}]"),
                    "duplicates an earlier forbidden path",
                )
                .into());
            }
        }
        Ok(())
    }
}

/// The complete `patcharena.toml` project configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Required configuration schema version.
    pub schema_version: u32,
    /// Repository-relative metadata paths.
    #[serde(default)]
    pub paths: ProjectPaths,
    /// Default limits and environment allowlist.
    #[serde(default)]
    pub defaults: RunnerDefaults,
    /// Project-wide forbidden operations.
    #[serde(default)]
    pub security: SecurityDefaults,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_CONFIG_SCHEMA_VERSION,
            paths: ProjectPaths::default(),
            defaults: RunnerDefaults::default(),
            security: SecurityDefaults::default(),
        }
    }
}

impl ProjectConfig {
    /// Parses and validates project configuration from TOML text.
    pub fn from_toml(toml_text: &str) -> Result<Self> {
        let config: Self = toml::from_str(toml_text).map_err(|source| CoreError::TomlDecode {
            path: PathBuf::from("<project configuration TOML>"),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Reads, parses, schema-checks, and validates `patcharena.toml`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = read_utf8_limited(path, MAX_CONFIG_FILE_BYTES)?;
        let config: Self = toml::from_str(&text).map_err(|source| CoreError::TomlDecode {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Serializes a validated project configuration as pretty TOML with a trailing newline.
    pub fn to_toml_pretty(&self) -> Result<String> {
        self.validate()?;
        let text =
            toml::to_string_pretty(self).map_err(|source| CoreError::TomlEncode { source })?;
        String::from_utf8(with_trailing_newline(text))
            .map_err(|error| ValidationError::new("configuration", error.to_string()).into())
    }

    /// Atomically creates a configuration file and refuses to overwrite one that exists.
    pub fn save_new(&self, path: impl AsRef<Path>) -> Result<()> {
        let text = self.to_toml_pretty()?;
        atomic_write_new(path, text.as_bytes())
    }

    /// Atomically replaces a regular configuration file after validation.
    pub fn save_replace(&self, path: impl AsRef<Path>) -> Result<()> {
        let text = self.to_toml_pretty()?;
        atomic_write_replace(path, text.as_bytes())
    }

    /// Resolves the configured metadata paths below a repository root.
    pub fn resolve_paths(&self, repository_root: impl AsRef<Path>) -> Result<ResolvedProjectPaths> {
        self.validate()?;
        let repository_root = repository_root.as_ref().to_path_buf();
        Ok(ResolvedProjectPaths {
            state_dir: safe_join(&repository_root, &self.paths.state_dir)?,
            tasks_dir: safe_join(&repository_root, &self.paths.tasks_dir)?,
            runs_dir: safe_join(&repository_root, &self.paths.runs_dir)?,
            groups_dir: safe_join(&repository_root, &self.paths.groups_dir)?,
            config_file: repository_root.join(CONFIG_FILE_NAME),
            repository_root,
        })
    }

    /// Checks schema, paths, limits, environment names, and forbidden-operation patterns.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CURRENT_CONFIG_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedSchema {
                document: "project configuration",
                found: self.schema_version,
                supported: CURRENT_CONFIG_SCHEMA_VERSION,
            });
        }
        self.paths.validate()?;
        self.defaults.validate()?;
        self.security.validate()?;
        Ok(())
    }
}

/// Absolute paths derived from validated project configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedProjectPaths {
    /// The repository root supplied by the caller.
    pub repository_root: PathBuf,
    /// Absolute `patcharena.toml` path.
    pub config_file: PathBuf,
    /// Absolute PatchArena metadata root.
    pub state_dir: PathBuf,
    /// Absolute task directory.
    pub tasks_dir: PathBuf,
    /// Absolute run directory.
    pub runs_dir: PathBuf,
    /// Absolute run-group directory.
    pub groups_dir: PathBuf,
}

fn is_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_configuration_round_trips() {
        let config = ProjectConfig::default();
        let text = config.to_toml_pretty().expect("serialize");
        let reparsed = ProjectConfig::from_toml(&text).expect("parse");
        assert_eq!(reparsed, config);
        assert!(text.contains("schema_version = 1"));
        assert!(text.contains("[paths]"));
    }

    #[test]
    fn traversal_and_absolute_metadata_paths_are_rejected() {
        for path in ["../tasks", "/tmp/tasks", r"C:\tasks"] {
            let mut config = ProjectConfig::default();
            config.paths.tasks_dir = PathBuf::from(path);
            assert!(config.validate().is_err(), "accepted {path:?}");
        }
    }

    #[test]
    fn metadata_directories_must_remain_under_state_directory() {
        let mut config = ProjectConfig::default();
        config.paths.tasks_dir = PathBuf::from("tasks");
        assert!(config.validate().is_err());
    }

    #[test]
    fn environment_allowlist_is_validated() {
        let mut config = ProjectConfig::default();
        config
            .defaults
            .environment_allowlist
            .push("BAD=VALUE".to_owned());
        assert!(config.validate().is_err());
    }
}
