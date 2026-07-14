use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// A result returned by PatchArena core APIs.
pub type Result<T> = std::result::Result<T, CoreError>;

/// A field-level validation failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid `{field}`: {message}")]
pub struct ValidationError {
    /// The logical field or object that failed validation.
    pub field: String,
    /// A human-readable explanation suitable for a CLI error.
    pub message: String,
}

impl ValidationError {
    /// Creates a validation error for `field`.
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Errors produced while parsing, validating, or persisting PatchArena data.
#[derive(Debug, Error)]
pub enum CoreError {
    /// A filesystem operation failed.
    #[error("failed to {operation} `{path}`: {source}")]
    Io {
        /// The operation being attempted.
        operation: &'static str,
        /// The affected filesystem path.
        path: PathBuf,
        /// The underlying operating-system error.
        #[source]
        source: io::Error,
    },

    /// YAML could not be parsed or serialized.
    #[error("invalid YAML for `{path}`: {source}")]
    Yaml {
        /// The source path, or a descriptive virtual path while serializing.
        path: PathBuf,
        /// The YAML codec error.
        #[source]
        source: serde_yaml::Error,
    },

    /// JSON could not be parsed or serialized.
    #[error("invalid JSON for `{path}`: {source}")]
    Json {
        /// The source path, or a descriptive virtual path while serializing.
        path: PathBuf,
        /// The JSON codec error.
        #[source]
        source: serde_json::Error,
    },

    /// TOML could not be parsed.
    #[error("invalid TOML in `{path}`: {source}")]
    TomlDecode {
        /// The TOML source path.
        path: PathBuf,
        /// The TOML decoding error.
        #[source]
        source: toml::de::Error,
    },

    /// TOML could not be serialized.
    #[error("could not serialize TOML: {source}")]
    TomlEncode {
        /// The TOML encoding error.
        #[source]
        source: toml::ser::Error,
    },

    /// A parsed object violated a semantic invariant.
    #[error(transparent)]
    Validation(#[from] ValidationError),

    /// A task ID was not safe to use as a filename.
    #[error("invalid task ID `{value}`: {reason}")]
    InvalidTaskId {
        /// The rejected task ID.
        value: String,
        /// The rule that the ID violated.
        reason: &'static str,
    },

    /// A suite ID was not safe to use as a filename.
    #[error("invalid suite ID `{value}`: {reason}")]
    InvalidSuiteId {
        /// The rejected suite ID.
        value: String,
        /// The rule that the ID violated.
        reason: &'static str,
    },

    /// A path was absolute, traversed a parent, or escaped through a symbolic link.
    #[error("unsafe path `{path}`: {reason}")]
    UnsafePath {
        /// The rejected path.
        path: PathBuf,
        /// The safety rule that the path violated.
        reason: &'static str,
    },

    /// A create-only write refused to overwrite an existing path.
    #[error("refusing to overwrite existing path `{path}`")]
    AlreadyExists {
        /// The path that already existed.
        path: PathBuf,
    },

    /// An input file exceeded its defensive size limit.
    #[error("`{path}` is {actual_bytes} bytes; maximum accepted size is {limit_bytes} bytes")]
    FileTooLarge {
        /// The oversized input path.
        path: PathBuf,
        /// The observed file size.
        actual_bytes: u64,
        /// The configured maximum size.
        limit_bytes: u64,
    },

    /// An on-disk object uses a schema version this crate cannot safely interpret.
    #[error("unsupported {document} schema version {found}; supported version is {supported}")]
    UnsupportedSchema {
        /// A short document kind such as `run result`.
        document: &'static str,
        /// The version found on disk.
        found: u32,
        /// The only version currently supported.
        supported: u32,
    },
}

impl CoreError {
    pub(crate) fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}
