use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::{CoreError, Result};

/// Validates a portable, non-empty relative path without `.` or `..` components.
///
/// Backslashes and colons are rejected even on Unix so a task created on Linux cannot become a
/// traversal or drive-qualified path when later opened on Windows.
pub fn ensure_safe_relative_path(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let value = path.to_str().ok_or_else(|| CoreError::UnsafePath {
        path: path.to_path_buf(),
        reason: "path must be valid UTF-8",
    })?;

    if value.is_empty() {
        return Err(CoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path must not be empty",
        });
    }
    if value.contains('\\') {
        return Err(CoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "backslash separators are not portable",
        });
    }
    if value.contains(':') {
        return Err(CoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "colon may introduce a Windows drive or alternate data stream",
        });
    }
    if value
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(CoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "empty, current-directory, and parent-directory components are forbidden",
        });
    }

    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(CoreError::UnsafePath {
                path: path.to_path_buf(),
                reason: "path must contain only normal relative components",
            });
        }
    }
    Ok(())
}

/// Joins a validated relative path to `base`.
///
/// This is a lexical check. Use [`safe_join_no_symlink_escape`] before writing through paths in
/// a potentially untrusted worktree.
pub fn safe_join(base: impl AsRef<Path>, relative: impl AsRef<Path>) -> Result<PathBuf> {
    ensure_safe_relative_path(relative.as_ref())?;
    Ok(base.as_ref().join(relative.as_ref()))
}

/// Joins `relative` below `base` and rejects existing symbolic-link escapes.
///
/// `base` must exist. Each existing component is canonicalized and checked against the canonical
/// base. Non-existing tail components are returned lexically beneath the last safe component.
/// Callers must still avoid check-to-use races when an untrusted process can mutate the same tree.
pub fn safe_join_no_symlink_escape(
    base: impl AsRef<Path>,
    relative: impl AsRef<Path>,
) -> Result<PathBuf> {
    let base = base.as_ref();
    let relative = relative.as_ref();
    ensure_safe_relative_path(relative)?;

    let canonical_base =
        fs::canonicalize(base).map_err(|error| CoreError::io("canonicalize", base, error))?;
    let mut current = canonical_base.clone();

    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(CoreError::UnsafePath {
                path: relative.to_path_buf(),
                reason: "path must contain only normal relative components",
            });
        };
        let candidate = current.join(part);
        match fs::symlink_metadata(&candidate) {
            Ok(_) => {
                let resolved = fs::canonicalize(&candidate)
                    .map_err(|error| CoreError::io("canonicalize", &candidate, error))?;
                if !resolved.starts_with(&canonical_base) {
                    return Err(CoreError::UnsafePath {
                        path: relative.to_path_buf(),
                        reason: "symbolic link resolves outside the allowed base directory",
                    });
                }
                current = resolved;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                current = candidate;
            }
            Err(error) => return Err(CoreError::io("inspect", &candidate, error)),
        }
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn safe_relative_paths_are_portable() {
        for path in [".git", ".env", "nested/output.log", "tasks/task-one.yaml"] {
            ensure_safe_relative_path(path).expect(path);
        }
    }

    #[test]
    fn traversal_and_platform_specific_paths_are_rejected() {
        for path in [
            "",
            ".",
            "..",
            "../secret",
            "nested/../../secret",
            "/absolute",
            "nested//file",
            "nested/./file",
            r"..\secret",
            r"C:\secret",
            "stream:secret",
        ] {
            assert!(
                ensure_safe_relative_path(Path::new(path)).is_err(),
                "accepted unsafe path {path:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let base = tempdir().expect("base");
        let outside = tempdir().expect("outside");
        symlink(outside.path(), base.path().join("escape")).expect("symlink");

        let error = safe_join_no_symlink_escape(base.path(), "escape/result.json")
            .expect_err("escape must fail");
        assert!(matches!(error, CoreError::UnsafePath { .. }));
    }
}
