use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use crate::RunnerError;

#[derive(Debug)]
struct HiddenInstruction {
    path: PathBuf,
    contents: Vec<u8>,
    permissions: fs::Permissions,
}

/// Guard that temporarily hides discovered `AGENTS.md` files in a benchmark worktree.
///
/// The caller supplies safely scanned paths. Files are restored explicitly with
/// [`InstructionMask::restore`]; dropping the guard also makes a best-effort attempt.
#[derive(Debug)]
pub struct InstructionMask {
    root: PathBuf,
    hidden: Vec<HiddenInstruction>,
    active: bool,
}

impl InstructionMask {
    /// Remove the supplied repository-relative instruction files after validating containment.
    pub fn hide(
        worktree_root: impl AsRef<Path>,
        relative_paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, RunnerError> {
        let root = fs::canonicalize(worktree_root.as_ref()).map_err(|source| {
            RunnerError::Instructions {
                operation: "canonicalize worktree for",
                path: worktree_root.as_ref().to_path_buf(),
                source,
            }
        })?;
        let mut mask = Self {
            root: root.clone(),
            hidden: Vec::new(),
            active: true,
        };

        for relative in relative_paths {
            if relative.file_name().and_then(|name| name.to_str()) != Some("AGENTS.md")
                || relative.is_absolute()
                || relative.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                let _ = mask.restore_inner();
                return Err(RunnerError::UnsafePath(relative.display().to_string()));
            }
            let path = root.join(&relative);
            let metadata =
                fs::symlink_metadata(&path).map_err(|source| RunnerError::Instructions {
                    operation: "inspect",
                    path: path.clone(),
                    source,
                })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                let _ = mask.restore_inner();
                return Err(RunnerError::UnsafePath(relative.display().to_string()));
            }
            let canonical =
                fs::canonicalize(&path).map_err(|source| RunnerError::Instructions {
                    operation: "canonicalize",
                    path: path.clone(),
                    source,
                })?;
            if !canonical.starts_with(&root) {
                let _ = mask.restore_inner();
                return Err(RunnerError::UnsafePath(relative.display().to_string()));
            }
            let contents = fs::read(&canonical).map_err(|source| RunnerError::Instructions {
                operation: "read",
                path: canonical.clone(),
                source,
            })?;
            fs::remove_file(&canonical).map_err(|source| RunnerError::Instructions {
                operation: "hide",
                path: canonical.clone(),
                source,
            })?;
            mask.hidden.push(HiddenInstruction {
                path: canonical,
                contents,
                permissions: metadata.permissions(),
            });
        }
        Ok(mask)
    }

    /// Restore every hidden file, replacing any file the agent created at the same path.
    pub fn restore(mut self) -> Result<(), RunnerError> {
        self.restore_inner()
    }

    fn restore_inner(&mut self) -> Result<(), RunnerError> {
        if !self.active {
            return Ok(());
        }
        for instruction in &self.hidden {
            validate_restore_ancestors(&self.root, &instruction.path)?;
            match fs::symlink_metadata(&instruction.path) {
                Ok(metadata) if metadata.is_dir() => {
                    return Err(RunnerError::UnsafePath(
                        instruction.path.display().to_string(),
                    ));
                }
                Ok(_) => fs::remove_file(&instruction.path).map_err(|source| {
                    RunnerError::Instructions {
                        operation: "remove agent replacement for",
                        path: instruction.path.clone(),
                        source,
                    }
                })?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(RunnerError::Instructions {
                        operation: "inspect replacement for",
                        path: instruction.path.clone(),
                        source,
                    });
                }
            }
            let mut restored = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&instruction.path)
                .map_err(|source| RunnerError::Instructions {
                    operation: "recreate",
                    path: instruction.path.clone(),
                    source,
                })?;
            restored
                .write_all(&instruction.contents)
                .map_err(|source| RunnerError::Instructions {
                    operation: "restore",
                    path: instruction.path.clone(),
                    source,
                })?;
            fs::set_permissions(&instruction.path, instruction.permissions.clone()).map_err(
                |source| RunnerError::Instructions {
                    operation: "restore permissions for",
                    path: instruction.path.clone(),
                    source,
                },
            )?;
        }
        self.active = false;
        Ok(())
    }
}

fn validate_restore_ancestors(root: &Path, destination: &Path) -> Result<(), RunnerError> {
    let root_metadata = fs::symlink_metadata(root).map_err(|source| RunnerError::Instructions {
        operation: "inspect worktree root before restoring",
        path: root.to_path_buf(),
        source,
    })?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(RunnerError::UnsafePath(root.display().to_string()));
    }
    let canonical_root = fs::canonicalize(root).map_err(|source| RunnerError::Instructions {
        operation: "canonicalize worktree root before restoring",
        path: root.to_path_buf(),
        source,
    })?;
    if canonical_root != root {
        return Err(RunnerError::UnsafePath(root.display().to_string()));
    }
    let relative = destination
        .strip_prefix(root)
        .map_err(|_| RunnerError::UnsafePath(destination.display().to_string()))?;
    let mut cursor = root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let std::path::Component::Normal(segment) = component else {
                return Err(RunnerError::UnsafePath(destination.display().to_string()));
            };
            cursor.push(segment);
            let metadata =
                fs::symlink_metadata(&cursor).map_err(|source| RunnerError::Instructions {
                    operation: "inspect restore ancestor",
                    path: cursor.clone(),
                    source,
                })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(RunnerError::UnsafePath(cursor.display().to_string()));
            }
        }
    }
    Ok(())
}

impl Drop for InstructionMask {
    fn drop(&mut self) {
        let _restore_result = self.restore_inner();
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use tempfile::tempdir;

    use super::InstructionMask;

    #[test]
    fn hides_and_restores_instruction_files() {
        let directory = tempdir().expect("temp dir");
        let instruction = directory.path().join("AGENTS.md");
        fs::write(&instruction, "original").expect("write instruction");
        let mask = InstructionMask::hide(directory.path(), [PathBuf::from("AGENTS.md")])
            .expect("hide instructions");
        assert!(!instruction.exists());
        fs::write(&instruction, "agent replacement").expect("write replacement");
        mask.restore().expect("restore instructions");
        assert_eq!(
            fs::read_to_string(instruction).expect("read restored instruction"),
            "original"
        );
    }

    #[test]
    fn rejects_non_instruction_and_traversal_paths() {
        let directory = tempdir().expect("temp dir");
        assert!(InstructionMask::hide(directory.path(), [PathBuf::from("../AGENTS.md")]).is_err());
        assert!(InstructionMask::hide(directory.path(), [PathBuf::from("README.md")]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn restore_refuses_replaced_parent_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("temp dir");
        let outside = tempdir().expect("outside dir");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        fs::write(directory.path().join("nested/AGENTS.md"), "original").expect("instruction");
        fs::write(outside.path().join("AGENTS.md"), "outside").expect("outside instruction");
        let mask = InstructionMask::hide(directory.path(), [PathBuf::from("nested/AGENTS.md")])
            .expect("hide instructions");
        fs::remove_dir(directory.path().join("nested")).expect("remove nested directory");
        symlink(outside.path(), directory.path().join("nested")).expect("replace parent");
        assert!(mask.restore().is_err());
        assert_eq!(
            fs::read_to_string(outside.path().join("AGENTS.md")).expect("outside unchanged"),
            "outside"
        );
    }
}
