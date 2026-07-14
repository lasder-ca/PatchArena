//! Git repository isolation and diff collection for PatchArena.
//!
//! Commands are always passed directly to `git` as argument arrays. This crate
//! never invokes a shell and never uses a recursive filesystem deletion command.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use tempfile::TempDir;
use thiserror::Error;

/// A result returned by this crate.
pub type Result<T> = std::result::Result<T, GitError>;

/// Errors produced while inspecting a repository or managing a worktree.
#[derive(Debug, Error)]
pub enum GitError {
    /// The configured Git executable could not be found.
    #[error("Git executable `{program}` was not found")]
    GitUnavailable {
        /// The executable that was requested.
        program: PathBuf,
        /// The underlying process-spawn error.
        #[source]
        source: io::Error,
    },

    /// A filesystem or process I/O operation failed.
    #[error("{action} for `{path}` failed: {source}")]
    Io {
        /// A short description of the failed operation.
        action: &'static str,
        /// The path involved in the operation.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// Git exited unsuccessfully.
    #[error("Git operation `{operation}` failed with exit code {exit_code:?}: {stderr}")]
    CommandFailed {
        /// A non-shell diagnostic rendering of the Git arguments.
        operation: String,
        /// Git's exit code, or `None` when it was terminated by a signal.
        exit_code: Option<i32>,
        /// A bounded, lossy rendering of Git's standard error.
        stderr: String,
    },

    /// A repository could not be found from the requested location.
    #[error("`{start}` is not inside a Git worktree")]
    NotRepository {
        /// The path from which discovery was attempted.
        start: PathBuf,
    },

    /// An operation required a clean repository, but changes were present.
    #[error("repository `{root}` has uncommitted changes")]
    DirtyRepository {
        /// The repository worktree root.
        root: PathBuf,
        /// Raw `git status --porcelain=v1 -z` output.
        status: Vec<u8>,
    },

    /// A revision did not resolve to a commit.
    #[error("revision `{revision}` does not resolve to a commit")]
    InvalidRevision {
        /// The rejected revision expression.
        revision: String,
    },

    /// A detached worktree destination is not suitable for safe creation.
    #[error("invalid worktree destination `{path}`: {reason}")]
    InvalidWorktreeDestination {
        /// The rejected destination.
        path: PathBuf,
        /// The reason the destination was rejected.
        reason: &'static str,
    },

    /// A repository-relative path was absolute, empty, or contained traversal.
    #[error("unsafe relative path `{path}`: {reason}")]
    UnsafeRelativePath {
        /// The rejected path.
        path: PathBuf,
        /// The reason the path was rejected.
        reason: &'static str,
    },

    /// A path below an artifact root encountered a symbolic link.
    #[error("symbolic links are not allowed in artifact paths: `{path}`")]
    SymlinkComponent {
        /// The symbolic-link component that was found.
        path: PathBuf,
    },

    /// An existing intermediate artifact path component was not a directory.
    #[error("artifact path component is not a directory: `{path}`")]
    NonDirectoryComponent {
        /// The non-directory intermediate component.
        path: PathBuf,
    },

    /// A path resolved outside its expected root.
    #[error("path `{path}` resolves outside root `{root}`")]
    PathOutsideRoot {
        /// The trusted containment root.
        root: PathBuf,
        /// The path that escaped the root.
        path: PathBuf,
    },

    /// Git emitted output that could not be parsed without guessing.
    #[error("could not parse Git output: {message}")]
    Parse {
        /// A description of the malformed output.
        message: String,
    },
}

/// Captured output from a successful Git command.
#[derive(Debug)]
pub struct GitCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl GitCommandOutput {
    /// Returns Git's process status.
    #[must_use]
    pub fn status(&self) -> ExitStatus {
        self.status
    }

    /// Returns Git's standard output as unmodified bytes.
    #[must_use]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// Returns Git's standard error as unmodified bytes.
    #[must_use]
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// Consumes the command result and returns its standard output.
    #[must_use]
    pub fn into_stdout(self) -> Vec<u8> {
        self.stdout
    }
}

/// A discovered Git worktree.
#[derive(Clone, Debug)]
pub struct Repository {
    root: PathBuf,
    git_program: PathBuf,
}

impl Repository {
    /// Discovers the enclosing repository using the `git` executable on `PATH`.
    ///
    /// If `start` names a file, discovery begins in its parent directory.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self> {
        Self::discover_with_git(start, "git")
    }

    /// Discovers the enclosing repository using a specific Git executable.
    ///
    /// Supplying the executable explicitly is useful for installations where Git
    /// is not on `PATH`, and for deterministic failure testing.
    pub fn discover_with_git(
        start: impl AsRef<Path>,
        git_program: impl AsRef<Path>,
    ) -> Result<Self> {
        let original_start = start.as_ref().to_path_buf();
        let metadata = fs::metadata(&original_start).map_err(|source| GitError::Io {
            action: "inspect repository discovery path",
            path: original_start.clone(),
            source,
        })?;
        let discovery_dir = if metadata.is_dir() {
            original_start.as_path()
        } else {
            original_start
                .parent()
                .ok_or_else(|| GitError::NotRepository {
                    start: original_start.clone(),
                })?
        };
        let program = git_program.as_ref().to_path_buf();
        let args = [
            OsString::from("rev-parse"),
            OsString::from("--show-toplevel"),
        ];
        let output = match run_git_command(&program, discovery_dir, &args, &[]) {
            Ok(output) => output,
            Err(GitError::CommandFailed { .. }) => {
                return Err(GitError::NotRepository {
                    start: original_start,
                });
            }
            Err(error) => return Err(error),
        };
        let root_bytes = trim_line_ending(output.stdout());
        if root_bytes.is_empty() {
            return Err(GitError::Parse {
                message: "`git rev-parse --show-toplevel` returned an empty path".to_owned(),
            });
        }
        let reported_root = path_from_git_bytes(root_bytes)?;
        let root = fs::canonicalize(&reported_root).map_err(|source| GitError::Io {
            action: "canonicalize repository root",
            path: reported_root,
            source,
        })?;

        Ok(Self {
            root,
            git_program: program,
        })
    }

    /// Returns the canonical worktree root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the configured Git executable.
    #[must_use]
    pub fn git_program(&self) -> &Path {
        &self.git_program
    }

    /// Runs a Git command in the repository without invoking a shell.
    ///
    /// Each iterator item becomes exactly one process argument. A non-zero Git
    /// exit status is returned as [`GitError::CommandFailed`].
    pub fn run_git<I, S>(&self, args: I) -> Result<GitCommandOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect::<Vec<_>>();
        run_git_command(&self.git_program, &self.root, &args, &[])
    }

    /// Returns raw porcelain status, including untracked files.
    ///
    /// The output is NUL-delimited so unusual filenames do not become ambiguous.
    pub fn status_porcelain(&self) -> Result<Vec<u8>> {
        self.run_git(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
            .map(GitCommandOutput::into_stdout)
    }

    /// Returns raw porcelain status for tracked and staged changes only.
    ///
    /// Untracked files are intentionally omitted. This is useful after
    /// `patcharena init`, when local PatchArena metadata may not yet be committed.
    pub fn status_porcelain_tracked(&self) -> Result<Vec<u8>> {
        self.run_git(["status", "--porcelain=v1", "-z", "--untracked-files=no"])
            .map(GitCommandOutput::into_stdout)
    }

    /// Returns `true` when tracked, staged, and untracked changes are absent.
    pub fn is_clean(&self) -> Result<bool> {
        self.status_porcelain().map(|status| status.is_empty())
    }

    /// Verifies that no tracked, staged, or untracked changes are present.
    pub fn ensure_clean(&self) -> Result<()> {
        let status = self.status_porcelain()?;
        if status.is_empty() {
            Ok(())
        } else {
            Err(GitError::DirtyRepository {
                root: self.root.clone(),
                status,
            })
        }
    }

    /// Verifies that tracked and staged files are clean, ignoring untracked files.
    ///
    /// This is less strict than [`Repository::ensure_clean`] and is intended for
    /// callers that deliberately maintain untracked benchmark configuration.
    pub fn ensure_tracked_clean(&self) -> Result<()> {
        let status = self.status_porcelain_tracked()?;
        if status.is_empty() {
            Ok(())
        } else {
            Err(GitError::DirtyRepository {
                root: self.root.clone(),
                status,
            })
        }
    }

    /// Resolves a revision to a full commit object ID.
    ///
    /// Revisions beginning with `-` are rejected before invoking Git so they
    /// cannot be interpreted as command-line options.
    pub fn resolve_commit(&self, revision: &str) -> Result<String> {
        if revision.is_empty() || revision.starts_with('-') {
            return Err(GitError::InvalidRevision {
                revision: revision.to_owned(),
            });
        }
        let expression = format!("{revision}^{{commit}}");
        let output = self.run_git([
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--end-of-options"),
            OsStr::new(&expression),
        ]);
        let output = match output {
            Ok(output) => output,
            Err(GitError::CommandFailed { .. }) => {
                return Err(GitError::InvalidRevision {
                    revision: revision.to_owned(),
                });
            }
            Err(error) => return Err(error),
        };
        let oid = String::from_utf8(trim_line_ending(output.stdout()).to_vec()).map_err(|_| {
            GitError::Parse {
                message: "`git rev-parse` returned a non-UTF-8 object ID".to_owned(),
            }
        })?;
        if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(GitError::Parse {
                message: format!("Git returned malformed object ID `{oid}`"),
            });
        }
        Ok(oid)
    }

    /// Captures one consistent snapshot of all non-ignored working-tree changes.
    ///
    /// The snapshot includes staged, unstaged, deleted, and untracked files. It
    /// uses a temporary alternate Git index, leaving the repository's real index
    /// untouched. Ignored files and uninitialized submodule contents are not
    /// included.
    pub fn capture_diff(&self) -> Result<DiffCapture> {
        let snapshot = DiffSnapshot::new(self)?;
        snapshot.capture()
    }

    /// Captures a binary patch for all non-ignored working-tree changes.
    pub fn diff(&self) -> Result<Vec<u8>> {
        self.capture_diff().map(|capture| capture.patch)
    }

    /// Computes aggregate line and file counts for current changes.
    pub fn diff_stats(&self) -> Result<DiffStats> {
        self.capture_diff().map(|capture| capture.stats)
    }

    /// Returns all changed paths relative to the repository root.
    ///
    /// Rename detection is disabled for this path list so both the old and new
    /// names are reported. This is intentional for forbidden-path checks.
    pub fn changed_paths(&self) -> Result<Vec<PathBuf>> {
        self.capture_diff().map(|capture| capture.changed_paths)
    }

    /// Creates a detached Git worktree at a caller-provided absolute path.
    ///
    /// `destination` must not already exist and must not contain `..`. The
    /// revision defaults to `HEAD` and is resolved to an object ID before it is
    /// passed to `git worktree add`, preventing option injection. Call
    /// [`DetachedWorktree::close`] to observe cleanup errors; dropping the guard
    /// also attempts best-effort cleanup through Git.
    pub fn create_detached_worktree(
        &self,
        destination: impl AsRef<Path>,
        revision: Option<&str>,
    ) -> Result<DetachedWorktree> {
        let destination = destination.as_ref();
        validate_worktree_destination(destination)?;
        let revision = revision.unwrap_or("HEAD");
        let commit = self.resolve_commit(revision)?;
        let args = vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("--detach"),
            destination.as_os_str().to_os_string(),
            OsString::from(&commit),
        ];
        run_git_command(&self.git_program, &self.root, &args, &[])?;

        let canonical_destination = match fs::canonicalize(destination) {
            Ok(path) => path,
            Err(source) => {
                let _ = remove_worktree(self, destination);
                return Err(GitError::Io {
                    action: "canonicalize detached worktree",
                    path: destination.to_path_buf(),
                    source,
                });
            }
        };
        let worktree_repository = Self {
            root: canonical_destination.clone(),
            git_program: self.git_program.clone(),
        };

        Ok(DetachedWorktree {
            source: self.clone(),
            repository: worktree_repository,
            path: canonical_destination,
            commit,
            active: true,
        })
    }
}

/// Aggregate statistics for a Git diff.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiffStats {
    /// The number of diff entries; a detected rename counts as one entry.
    pub changed_files: usize,
    /// The number of added text lines.
    pub added_lines: u64,
    /// The number of deleted text lines.
    pub deleted_lines: u64,
    /// The number of binary diff entries, which have no line counts.
    pub binary_files: usize,
}

impl DiffStats {
    /// Returns the sum of added and deleted text lines.
    #[must_use]
    pub fn total_lines(self) -> u64 {
        self.added_lines.saturating_add(self.deleted_lines)
    }
}

/// A patch and its metadata captured from the same temporary-index snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffCapture {
    /// A `git diff --binary --full-index` patch as raw bytes.
    pub patch: Vec<u8>,
    /// Aggregate statistics for the patch.
    pub stats: DiffStats,
    /// Changed repository-relative paths, with rename source and target included.
    pub changed_paths: Vec<PathBuf>,
}

/// Parses NUL-delimited output from `git diff --numstat -z`.
///
/// Both ordinary records (`added<TAB>deleted<TAB>path<NUL>`) and the expanded
/// rename/copy form are accepted. Paths are skipped without decoding, so a
/// non-UTF-8 filename cannot corrupt the line totals.
pub fn parse_numstat_z(input: &[u8]) -> Result<DiffStats> {
    let fields = input.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut field_index = 0;
    let mut stats = DiffStats::default();

    while field_index < fields.len() {
        let record = fields[field_index];
        field_index += 1;
        if record.is_empty() {
            if fields[field_index..].iter().all(|field| field.is_empty()) {
                break;
            }
            return Err(parse_error("empty numstat record"));
        }

        let mut columns = record.splitn(3, |byte| *byte == b'\t');
        let added = columns.next().unwrap_or_default();
        let deleted = columns
            .next()
            .ok_or_else(|| parse_error("numstat record is missing deleted-line field"))?;
        let path = columns
            .next()
            .ok_or_else(|| parse_error("numstat record is missing path field"))?;

        if path.is_empty() {
            // With `-z`, rename/copy records place the old and new path in the
            // following two NUL-delimited fields.
            let old_path = fields
                .get(field_index)
                .ok_or_else(|| parse_error("rename numstat record is missing old path"))?;
            let new_path = fields
                .get(field_index + 1)
                .ok_or_else(|| parse_error("rename numstat record is missing new path"))?;
            if old_path.is_empty() || new_path.is_empty() {
                return Err(parse_error("rename numstat record contains an empty path"));
            }
            field_index += 2;
        }

        stats.changed_files = stats
            .changed_files
            .checked_add(1)
            .ok_or_else(|| parse_error("changed-file count overflow"))?;
        if added == b"-" || deleted == b"-" {
            if added != b"-" || deleted != b"-" {
                return Err(parse_error(
                    "binary numstat record has inconsistent markers",
                ));
            }
            stats.binary_files = stats
                .binary_files
                .checked_add(1)
                .ok_or_else(|| parse_error("binary-file count overflow"))?;
        } else {
            let added = parse_ascii_u64(added, "added-line count")?;
            let deleted = parse_ascii_u64(deleted, "deleted-line count")?;
            stats.added_lines = stats
                .added_lines
                .checked_add(added)
                .ok_or_else(|| parse_error("added-line count overflow"))?;
            stats.deleted_lines = stats
                .deleted_lines
                .checked_add(deleted)
                .ok_or_else(|| parse_error("deleted-line count overflow"))?;
        }
    }

    Ok(stats)
}

/// Rejects an unsafe repository-relative path.
///
/// Valid paths contain one or more normal components. Absolute paths, root or
/// platform-prefix components, and `..` traversal are rejected.
pub fn validate_relative_path(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let mut saw_component = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => saw_component = true,
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(GitError::UnsafeRelativePath {
                    path: path.to_path_buf(),
                    reason: "parent-directory traversal is not allowed",
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(GitError::UnsafeRelativePath {
                    path: path.to_path_buf(),
                    reason: "absolute paths are not allowed",
                });
            }
        }
    }
    if !saw_component {
        return Err(GitError::UnsafeRelativePath {
            path: path.to_path_buf(),
            reason: "path must contain a filename",
        });
    }
    Ok(())
}

/// Resolves an artifact path beneath an existing root without following links.
///
/// The root and every existing component below it are rejected if they are
/// symbolic links. Existing intermediate components must be directories. The
/// returned path may itself not exist, which permits callers to create a new
/// artifact after validation.
///
/// This is a validation helper rather than a race-free filesystem sandbox. A
/// hostile concurrent process can replace components after validation; callers
/// should keep artifact directories private and create files with no-follow or
/// create-new semantics where available.
pub fn safe_artifact_path(root: impl AsRef<Path>, relative: impl AsRef<Path>) -> Result<PathBuf> {
    let root = root.as_ref();
    let relative = relative.as_ref();
    validate_relative_path(relative)?;

    let root_metadata = fs::symlink_metadata(root).map_err(|source| GitError::Io {
        action: "inspect artifact root",
        path: root.to_path_buf(),
        source,
    })?;
    if root_metadata.file_type().is_symlink() {
        return Err(GitError::SymlinkComponent {
            path: root.to_path_buf(),
        });
    }
    if !root_metadata.is_dir() {
        return Err(GitError::NonDirectoryComponent {
            path: root.to_path_buf(),
        });
    }
    let canonical_root = fs::canonicalize(root).map_err(|source| GitError::Io {
        action: "canonicalize artifact root",
        path: root.to_path_buf(),
        source,
    })?;
    let normal_components = relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(component) => Some(component),
            Component::CurDir => None,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>();
    let mut candidate = canonical_root.clone();
    let mut missing_component_seen = false;
    for (index, component) in normal_components.iter().enumerate() {
        candidate.push(component);
        if missing_component_seen {
            continue;
        }
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(GitError::SymlinkComponent {
                        path: candidate.clone(),
                    });
                }
                if index + 1 < normal_components.len() && !metadata.is_dir() {
                    return Err(GitError::NonDirectoryComponent {
                        path: candidate.clone(),
                    });
                }
                let resolved = fs::canonicalize(&candidate).map_err(|source| GitError::Io {
                    action: "canonicalize artifact path component",
                    path: candidate.clone(),
                    source,
                })?;
                if !resolved.starts_with(&canonical_root) {
                    return Err(GitError::PathOutsideRoot {
                        root: canonical_root,
                        path: resolved,
                    });
                }
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                missing_component_seen = true;
            }
            Err(source) => {
                return Err(GitError::Io {
                    action: "inspect artifact path component",
                    path: candidate,
                    source,
                });
            }
        }
    }
    Ok(candidate)
}

/// An RAII guard for a detached Git worktree.
///
/// Cleanup is performed with `git worktree remove --force`, followed by
/// `git worktree prune`. No standalone recursive deletion command is used.
#[derive(Debug)]
pub struct DetachedWorktree {
    source: Repository,
    repository: Repository,
    path: PathBuf,
    commit: String,
    active: bool,
}

impl DetachedWorktree {
    /// Returns the canonical worktree path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a repository handle rooted in the detached worktree.
    #[must_use]
    pub fn repository(&self) -> &Repository {
        &self.repository
    }

    /// Returns the full commit object ID checked out by the worktree.
    #[must_use]
    pub fn commit(&self) -> &str {
        &self.commit
    }

    /// Removes the worktree and reports any Git cleanup failure.
    ///
    /// This method is preferred over relying on [`Drop`] when the caller needs
    /// positive confirmation that cleanup completed.
    pub fn close(mut self) -> Result<()> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        remove_worktree_only(&self.source, &self.path)?;
        self.active = false;
        prune_worktrees(&self.source)
    }
}

impl Drop for DetachedWorktree {
    fn drop(&mut self) {
        if self.active {
            let _cleanup_result = self.cleanup();
        }
    }
}

struct DiffSnapshot<'repository> {
    repository: &'repository Repository,
    _temp_dir: TempDir,
    index_path: PathBuf,
}

impl<'repository> DiffSnapshot<'repository> {
    fn new(repository: &'repository Repository) -> Result<Self> {
        let temp_dir = tempfile::Builder::new()
            .prefix("patcharena-git-index-")
            .tempdir()
            .map_err(|source| GitError::Io {
                action: "create temporary Git index directory",
                path: std::env::temp_dir(),
                source,
            })?;
        let index_path = temp_dir.path().join("index");
        let snapshot = Self {
            repository,
            _temp_dir: temp_dir,
            index_path,
        };
        snapshot.run([OsStr::new("read-tree"), OsStr::new("HEAD")])?;
        snapshot.run([
            OsStr::new("add"),
            OsStr::new("-A"),
            OsStr::new("--"),
            OsStr::new("."),
        ])?;
        Ok(snapshot)
    }

    fn capture(&self) -> Result<DiffCapture> {
        let patch = self
            .run([
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                "--no-ext-diff",
                "--no-color",
                "--find-renames=50%",
                "HEAD",
                "--",
            ])?
            .into_stdout();
        let numstat = self
            .run([
                "diff",
                "--cached",
                "--numstat",
                "-z",
                "--no-ext-diff",
                "--find-renames=50%",
                "HEAD",
                "--",
            ])?
            .into_stdout();
        let names = self
            .run([
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--no-ext-diff",
                "--no-renames",
                "HEAD",
                "--",
            ])?
            .into_stdout();

        Ok(DiffCapture {
            patch,
            stats: parse_numstat_z(&numstat)?,
            changed_paths: parse_paths_z(&names)?,
        })
    }

    fn run<I, S>(&self, args: I) -> Result<GitCommandOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = args
            .into_iter()
            .map(|argument| argument.as_ref().to_os_string())
            .collect::<Vec<_>>();
        let environment = [(OsStr::new("GIT_INDEX_FILE"), self.index_path.as_os_str())];
        run_git_command(
            &self.repository.git_program,
            &self.repository.root,
            &args,
            &environment,
        )
    }
}

fn remove_worktree(repository: &Repository, path: &Path) -> Result<()> {
    remove_worktree_only(repository, path)?;
    prune_worktrees(repository)
}

fn remove_worktree_only(repository: &Repository, path: &Path) -> Result<()> {
    let remove_args = vec![
        OsString::from("worktree"),
        OsString::from("remove"),
        OsString::from("--force"),
        // A worktree lock requires `--force` twice. The guard targets only the
        // exact worktree it registered, so overriding a lock here is deliberate.
        OsString::from("--force"),
        path.as_os_str().to_os_string(),
    ];
    run_git_command(&repository.git_program, &repository.root, &remove_args, &[])?;
    Ok(())
}

fn prune_worktrees(repository: &Repository) -> Result<()> {
    let prune_args = [
        OsString::from("worktree"),
        OsString::from("prune"),
        OsString::from("--expire"),
        OsString::from("now"),
    ];
    run_git_command(&repository.git_program, &repository.root, &prune_args, &[])?;
    Ok(())
}

fn validate_worktree_destination(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(GitError::InvalidWorktreeDestination {
            path: path.to_path_buf(),
            reason: "path must be absolute",
        });
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(GitError::InvalidWorktreeDestination {
            path: path.to_path_buf(),
            reason: "parent-directory traversal is not allowed",
        });
    }
    match fs::symlink_metadata(path) {
        Ok(_) => {
            return Err(GitError::InvalidWorktreeDestination {
                path: path.to_path_buf(),
                reason: "destination already exists",
            });
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(GitError::Io {
                action: "inspect worktree destination",
                path: path.to_path_buf(),
                source,
            });
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| GitError::InvalidWorktreeDestination {
            path: path.to_path_buf(),
            reason: "destination must have a parent directory",
        })?;
    let parent_metadata = fs::metadata(parent).map_err(|source| GitError::Io {
        action: "inspect worktree destination parent",
        path: parent.to_path_buf(),
        source,
    })?;
    if !parent_metadata.is_dir() {
        return Err(GitError::InvalidWorktreeDestination {
            path: path.to_path_buf(),
            reason: "destination parent is not a directory",
        });
    }
    Ok(())
}

fn run_git_command(
    program: &Path,
    current_dir: &Path,
    args: &[OsString],
    environment: &[(&OsStr, &OsStr)],
) -> Result<GitCommandOutput> {
    // Repository hooks are executable code. Internal inspection and isolation
    // must not run them, so each invocation points hooksPath at a fresh, empty,
    // private directory that remains alive until Git exits.
    let disabled_hooks = tempfile::Builder::new()
        .prefix("patcharena-disabled-hooks-")
        .tempdir()
        .map_err(|source| GitError::Io {
            action: "create disabled Git hooks directory",
            path: std::env::temp_dir(),
            source,
        })?;
    let hooks_config = {
        let mut value = OsString::from("core.hooksPath=");
        value.push(disabled_hooks.path());
        value
    };
    let mut command = Command::new(program);
    command
        .arg("-c")
        .arg(hooks_config)
        .arg("-c")
        .arg("core.fsmonitor=false")
        .args(args)
        .current_dir(current_dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    // Do not let an invoking Git hook or a stale caller environment silently
    // redirect operations to another repository, worktree, object store, or
    // index. Explicit per-command values below are applied after this removal.
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(variable);
    }
    for (key, value) in environment {
        command.env(key, value);
    }

    let operation = render_operation(args);
    let output = command.output().map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            GitError::GitUnavailable {
                program: program.to_path_buf(),
                source,
            }
        } else {
            GitError::Io {
                action: "execute Git command",
                path: program.to_path_buf(),
                source,
            }
        }
    })?;
    if !output.status.success() {
        return Err(GitError::CommandFailed {
            operation,
            exit_code: output.status.code(),
            stderr: bounded_lossy(&output.stderr, 8 * 1024),
        });
    }
    Ok(GitCommandOutput {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn parse_paths_z(input: &[u8]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for path in input.split(|byte| *byte == 0) {
        if path.is_empty() {
            continue;
        }
        let path = path_from_git_bytes(path)?;
        validate_relative_path(&path)?;
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(unix)]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git_bytes(bytes: &[u8]) -> Result<PathBuf> {
    let path = String::from_utf8(bytes.to_vec()).map_err(|_| GitError::Parse {
        message: "Git returned a non-UTF-8 path on this platform".to_owned(),
    })?;
    Ok(PathBuf::from(path))
}

fn parse_ascii_u64(input: &[u8], field: &str) -> Result<u64> {
    if input.is_empty() || !input.iter().all(u8::is_ascii_digit) {
        return Err(parse_error(format!(
            "{field} is not an unsigned decimal integer"
        )));
    }
    let value =
        std::str::from_utf8(input).map_err(|_| parse_error(format!("{field} is not ASCII")))?;
    value
        .parse::<u64>()
        .map_err(|_| parse_error(format!("{field} exceeds u64")))
}

fn parse_error(message: impl Into<String>) -> GitError {
    GitError::Parse {
        message: message.into(),
    }
}

fn trim_line_ending(mut input: &[u8]) -> &[u8] {
    while input
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        input = &input[..input.len() - 1];
    }
    input
}

fn bounded_lossy(input: &[u8], limit: usize) -> String {
    if input.len() <= limit {
        return String::from_utf8_lossy(input).into_owned();
    }
    let mut rendered = String::from_utf8_lossy(&input[..limit]).into_owned();
    rendered.push_str("... [stderr truncated]");
    rendered
}

fn render_operation(args: &[OsString]) -> String {
    if args.is_empty() {
        return "<no arguments>".to_owned();
    }
    args.iter()
        .map(|argument| format!("{:?}", argument.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_binary_and_rename_numstat_records() {
        let input = b"3\t1\talpha.txt\0-\t-\timage.bin\x000\t0\t\0old.txt\0new.txt\0";
        let stats = parse_numstat_z(input).expect("numstat should parse");

        assert_eq!(stats.changed_files, 3);
        assert_eq!(stats.added_lines, 3);
        assert_eq!(stats.deleted_lines, 1);
        assert_eq!(stats.binary_files, 1);
        assert_eq!(stats.total_lines(), 4);
    }

    #[test]
    fn rejects_malformed_numstat() {
        let error =
            parse_numstat_z(b"1\tmissing-path-column\0").expect_err("malformed numstat must fail");

        assert!(matches!(error, GitError::Parse { .. }));
    }

    #[test]
    fn rejects_relative_path_traversal() {
        let error = validate_relative_path("runs/../../secret")
            .expect_err("parent traversal must be rejected");

        assert!(matches!(error, GitError::UnsafeRelativePath { .. }));
        assert!(validate_relative_path("runs/id/result.json").is_ok());
    }
}
