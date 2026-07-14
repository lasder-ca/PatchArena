use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use tempfile::Builder;

use crate::{CoreError, Result};

/// Read a regular UTF-8 file with a hard byte limit, refusing symbolic links.
///
/// The file is inspected before opening and the reader itself is limited, so a growing input
/// cannot force an unbounded allocation. Callers should still avoid concurrent mutation of
/// security-sensitive inputs because this is not a descriptor-relative filesystem sandbox.
pub fn read_utf8_limited(path: &Path, limit_bytes: u64) -> Result<String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| CoreError::io("inspect", path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(CoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "refusing to read a symbolic link",
        });
    }
    if !metadata.is_file() {
        return Err(CoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "input must be a regular file",
        });
    }
    if metadata.len() > limit_bytes {
        return Err(CoreError::FileTooLarge {
            path: path.to_path_buf(),
            actual_bytes: metadata.len(),
            limit_bytes,
        });
    }

    let file = File::open(path).map_err(|error| CoreError::io("open", path, error))?;
    let capacity = usize::try_from(metadata.len()).unwrap_or(0).min(64 * 1024);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(limit_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| CoreError::io("read", path, error))?;

    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit_bytes {
        return Err(CoreError::FileTooLarge {
            path: path.to_path_buf(),
            actual_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            limit_bytes,
        });
    }

    String::from_utf8(bytes).map_err(|error| {
        CoreError::io(
            "decode as UTF-8",
            path,
            io::Error::new(io::ErrorKind::InvalidData, error),
        )
    })
}

/// Atomically creates `path` with `contents`, refusing to replace any existing entry.
///
/// The temporary file is created in the destination directory, flushed to stable storage, and
/// installed using a no-clobber operation. The parent directory must already exist. Files are
/// requests owner-only permissions on Unix. Some mounted filesystems (notably Windows drives in
/// WSL without metadata support) may ignore Unix mode bits, so applications should surface that
/// limitation in security diagnostics rather than treating file modes as a sandbox boundary.
pub fn atomic_write_new(path: impl AsRef<Path>, contents: &[u8]) -> Result<()> {
    atomic_write(path.as_ref(), contents, WriteMode::CreateNew)
}

/// Atomically replaces a regular file at `path` with `contents`.
///
/// This function refuses to replace a symbolic link. Use [`atomic_write_new`] for task and run
/// creation when overwriting would destroy evidence.
pub fn atomic_write_replace(path: impl AsRef<Path>, contents: &[u8]) -> Result<()> {
    atomic_write(path.as_ref(), contents, WriteMode::Replace)
}

#[derive(Copy, Clone)]
enum WriteMode {
    CreateNew,
    Replace,
}

fn atomic_write(path: &Path, contents: &[u8], mode: WriteMode) -> Result<()> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .ok_or_else(|| CoreError::UnsafePath {
            path: path.to_path_buf(),
            reason: "destination must have a parent directory",
        })?;

    if matches!(mode, WriteMode::CreateNew) && fs::symlink_metadata(path).is_ok() {
        return Err(CoreError::AlreadyExists {
            path: path.to_path_buf(),
        });
    }
    if matches!(mode, WriteMode::Replace) {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CoreError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: "refusing to replace a symbolic link",
                });
            }
            Ok(metadata) if metadata.is_dir() => {
                return Err(CoreError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: "destination is a directory",
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(CoreError::io("inspect", path, error)),
        }
    }

    let mut temporary = Builder::new()
        .prefix(".patcharena-write-")
        .tempfile_in(parent)
        .map_err(|error| CoreError::io("create temporary file in", parent, error))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| CoreError::io("set permissions on", temporary.path(), error))?;
    }

    temporary
        .write_all(contents)
        .map_err(|error| CoreError::io("write temporary file for", path, error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| CoreError::io("flush temporary file for", path, error))?;

    match mode {
        WriteMode::CreateNew => temporary.persist_noclobber(path).map_err(|error| {
            if error.error.kind() == io::ErrorKind::AlreadyExists {
                CoreError::AlreadyExists {
                    path: path.to_path_buf(),
                }
            } else {
                CoreError::io("install", path, error.error)
            }
        })?,
        WriteMode::Replace => temporary
            .persist(path)
            .map_err(|error| CoreError::io("replace", path, error.error))?,
    };

    sync_parent(parent)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CoreError::io("flush directory", path, error))
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}

pub(crate) fn with_trailing_newline(mut text: String) -> Vec<u8> {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.into_bytes()
}

pub(crate) fn serialization_path(label: &'static str) -> PathBuf {
    PathBuf::from(format!("<{label}>"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn new_write_does_not_overwrite() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("value.json");
        atomic_write_new(&path, b"first").expect("first write");

        let error = atomic_write_new(&path, b"second").expect_err("overwrite must fail");
        assert!(matches!(error, CoreError::AlreadyExists { .. }));
        assert_eq!(fs::read(&path).expect("read original"), b"first");
    }

    #[test]
    fn replace_write_updates_regular_file() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("value.json");
        atomic_write_new(&path, b"first").expect("first write");
        atomic_write_replace(&path, b"second").expect("replace");
        assert_eq!(fs::read(&path).expect("read replacement"), b"second");
    }

    #[cfg(unix)]
    #[test]
    fn replace_refuses_symbolic_link() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("temporary directory");
        let target = directory.path().join("target");
        let link = directory.path().join("link");
        fs::write(&target, "secret").expect("target");
        symlink(&target, &link).expect("symlink");

        let error = atomic_write_replace(&link, b"replacement").expect_err("symlink must fail");
        assert!(matches!(error, CoreError::UnsafePath { .. }));
        assert_eq!(
            fs::read_to_string(target).expect("target unchanged"),
            "secret"
        );
    }

    #[cfg(unix)]
    #[test]
    fn limited_read_refuses_symbolic_link() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("temporary directory");
        let target = directory.path().join("target.yaml");
        let link = directory.path().join("task.yaml");
        fs::write(&target, "id: outside").expect("target");
        symlink(&target, &link).expect("symlink");
        assert!(read_utf8_limited(&link, 1024).is_err());
    }

    #[test]
    fn limited_read_rejects_oversized_input() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("prompt.md");
        fs::write(&path, b"12345").expect("write prompt");
        let error = read_utf8_limited(&path, 4).expect_err("input must be bounded");
        assert!(matches!(error, CoreError::FileTooLarge { .. }));
    }
}
