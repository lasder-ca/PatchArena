use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use sha2::{Digest, Sha256};

use crate::fs::{read_utf8_limited, serialization_path};
use crate::task::validate_portable_id;
use crate::{CoreError, Result, TaskId, ValidationError, atomic_write_new, atomic_write_replace};

/// The suite-definition schema version supported by this release.
pub const CURRENT_SUITE_SCHEMA_VERSION: u32 = 1;

const MAX_SUITE_FILE_BYTES: u64 = 1024 * 1024;
const MAX_SUITE_DESCRIPTION_BYTES: usize = 1024;
const MAX_SUITE_TASKS: usize = 100;

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
