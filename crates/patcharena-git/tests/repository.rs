//! Integration tests using real temporary Git repositories and worktrees.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use patcharena_git::{GitError, Repository, safe_artifact_path, validate_relative_path};
use tempfile::TempDir;

struct TestRepository {
    _temp: TempDir,
    root: PathBuf,
    repository: Repository,
}

impl TestRepository {
    fn new(files: &[(&str, &[u8])]) -> Self {
        let temp = tempfile::tempdir().expect("create repository tempdir");
        let root = temp.path().join("repository");
        fs::create_dir(&root).expect("create repository directory");
        git(&root, ["init", "--quiet"]);
        git(&root, ["config", "user.name", "PatchArena Tests"]);
        git(
            &root,
            ["config", "user.email", "patcharena@example.invalid"],
        );
        for (relative, contents) in files {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create fixture parent");
            }
            fs::write(path, contents).expect("write committed fixture");
        }
        git(&root, ["add", "--all"]);
        git(&root, ["commit", "--quiet", "-m", "initial fixture"]);
        let repository = Repository::discover(root.join("tracked.txt").parent().unwrap_or(&root))
            .expect("discover repository");
        Self {
            _temp: temp,
            root,
            repository,
        }
    }
}

fn git<I, S>(directory: &Path, args: I) -> Vec<u8>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .expect("execute Git fixture command");
    assert!(
        output.status.success(),
        "Git fixture command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn discovery_and_clean_checks_distinguish_untracked_files() {
    let fixture = TestRepository::new(&[("tracked.txt", b"original\n")]);
    let nested = fixture.root.join("nested/deeper");
    fs::create_dir_all(&nested).expect("create nested discovery directory");
    let discovered = Repository::discover(&nested).expect("discover from nested directory");
    assert_eq!(discovered.root(), fixture.repository.root());
    assert!(discovered.is_clean().expect("check initial clean state"));

    fs::write(fixture.root.join("untracked.txt"), "local metadata\n")
        .expect("write untracked file");
    assert!(!discovered.is_clean().expect("check untracked state"));
    discovered
        .ensure_tracked_clean()
        .expect("untracked file should be permitted by tracked-only check");

    fs::write(fixture.root.join("tracked.txt"), "modified\n").expect("modify tracked file");
    let error = discovered
        .ensure_tracked_clean()
        .expect_err("tracked modification must fail");
    assert!(matches!(error, GitError::DirtyRepository { .. }));
}

#[test]
fn captures_tracked_untracked_deleted_and_binary_changes_without_touching_index() {
    let fixture = TestRepository::new(&[
        ("tracked.txt", b"one\ntwo\n"),
        ("deleted.txt", b"remove me\n"),
    ]);
    fs::write(
        fixture.root.join("tracked.txt"),
        "one\nreplacement\nextra\n",
    )
    .expect("modify text file");
    fs::remove_file(fixture.root.join("deleted.txt")).expect("delete tracked file");
    fs::write(fixture.root.join("new.txt"), "new\nlines\n").expect("write untracked text");
    fs::write(fixture.root.join("binary.bin"), [0_u8, 1, 2, 0, 255]).expect("write binary file");

    let status_before = fixture
        .repository
        .status_porcelain()
        .expect("capture status before diff");
    let capture = fixture
        .repository
        .capture_diff()
        .expect("capture full diff");
    let status_after = fixture
        .repository
        .status_porcelain()
        .expect("capture status after diff");

    assert_eq!(status_after, status_before, "real index must be untouched");
    assert_eq!(capture.stats.changed_files, 4);
    assert_eq!(capture.stats.added_lines, 4);
    assert_eq!(capture.stats.deleted_lines, 2);
    assert_eq!(capture.stats.binary_files, 1);
    assert_eq!(capture.stats.total_lines(), 6);

    let mut paths = capture.changed_paths;
    paths.sort();
    assert_eq!(
        paths,
        ["binary.bin", "deleted.txt", "new.txt", "tracked.txt"]
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    );
    assert!(
        capture
            .patch
            .windows(b"diff --git".len())
            .any(|window| window == b"diff --git")
    );
    assert!(
        capture
            .patch
            .windows(b"GIT binary patch".len())
            .any(|window| window == b"GIT binary patch")
    );
}

#[test]
fn detached_worktree_has_raii_and_explicit_cleanup() {
    let fixture = TestRepository::new(&[("tracked.txt", b"committed\n")]);
    #[cfg(unix)]
    install_post_checkout_hook(&fixture.root);
    let worktree_parent = tempfile::tempdir().expect("create worktree parent");
    let destination = worktree_parent.path().join("detached");
    let expected_commit = fixture
        .repository
        .resolve_commit("HEAD")
        .expect("resolve fixture head");

    let worktree = fixture
        .repository
        .create_detached_worktree(&destination, None)
        .expect("create detached worktree");
    assert_eq!(worktree.path(), destination.canonicalize().unwrap());
    assert_eq!(worktree.commit(), expected_commit);
    assert_eq!(
        worktree
            .repository()
            .resolve_commit("HEAD")
            .expect("resolve worktree head"),
        expected_commit
    );
    assert!(worktree.path().join("tracked.txt").is_file());
    #[cfg(unix)]
    assert!(
        !worktree.path().join("hook-ran").exists(),
        "internal Git commands must not execute repository hooks"
    );
    git(
        &fixture.root,
        [
            OsStr::new("worktree"),
            OsStr::new("lock"),
            destination.as_os_str(),
            OsStr::new("--reason"),
            OsStr::new("exercise forced RAII cleanup"),
        ],
    );
    worktree.close().expect("explicitly remove worktree");
    assert!(!destination.exists());

    let dropped_destination = worktree_parent.path().join("dropped");
    {
        let dropped = fixture
            .repository
            .create_detached_worktree(&dropped_destination, Some("HEAD"))
            .expect("create second worktree");
        assert!(dropped.path().is_dir());
    }
    assert!(!dropped_destination.exists());

    let listing = git(&fixture.root, ["worktree", "list", "--porcelain"]);
    assert!(!String::from_utf8_lossy(&listing).contains("detached"));
    assert!(!String::from_utf8_lossy(&listing).contains("dropped"));
}

#[cfg(unix)]
fn install_post_checkout_hook(repository_root: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let hook = repository_root.join(".git/hooks/post-checkout");
    fs::write(&hook, "#!/bin/sh\n: > hook-ran\n").expect("write post-checkout hook");
    let mut permissions = fs::metadata(&hook).expect("inspect hook").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(hook, permissions).expect("make hook executable");
}

#[test]
fn worktree_destination_must_be_absolute_and_absent() {
    let fixture = TestRepository::new(&[("tracked.txt", b"committed\n")]);
    let relative_error = fixture
        .repository
        .create_detached_worktree("relative-worktree", None)
        .expect_err("relative destination must fail");
    assert!(matches!(
        relative_error,
        GitError::InvalidWorktreeDestination { .. }
    ));

    let existing = tempfile::tempdir().expect("create existing destination");
    let existing_error = fixture
        .repository
        .create_detached_worktree(existing.path(), None)
        .expect_err("existing destination must fail");
    assert!(matches!(
        existing_error,
        GitError::InvalidWorktreeDestination { .. }
    ));
}

#[test]
fn artifact_paths_reject_traversal_and_intermediate_files() {
    let root = tempfile::tempdir().expect("create artifact root");
    fs::create_dir(root.path().join("run")).expect("create run directory");
    fs::write(root.path().join("plain-file"), "not a directory").expect("write plain file");

    let safe = safe_artifact_path(root.path(), "run/result.json").expect("resolve safe path");
    assert_eq!(
        safe,
        root.path().canonicalize().unwrap().join("run/result.json")
    );
    assert!(validate_relative_path("../outside").is_err());
    assert!(safe_artifact_path(root.path(), "../outside").is_err());
    assert!(matches!(
        safe_artifact_path(root.path(), "plain-file/result.json"),
        Err(GitError::NonDirectoryComponent { .. })
    ));
}

#[cfg(unix)]
#[test]
fn artifact_paths_and_diff_reject_links_but_preserve_non_utf8_names() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let artifact_root = tempfile::tempdir().expect("create artifact root");
    let outside = tempfile::tempdir().expect("create outside directory");
    symlink(outside.path(), artifact_root.path().join("escape")).expect("create test symlink");
    assert!(matches!(
        safe_artifact_path(artifact_root.path(), "escape/result.json"),
        Err(GitError::SymlinkComponent { .. })
    ));

    let fixture = TestRepository::new(&[("tracked.txt", b"committed\n")]);
    let raw_name = OsString::from_vec(b"non-utf8-\xff.txt".to_vec());
    fs::write(fixture.root.join(&raw_name), "content\n").expect("write non-UTF-8 path");
    let capture = fixture
        .repository
        .capture_diff()
        .expect("capture unusual path");
    assert_eq!(capture.stats.changed_files, 1);
    assert_eq!(capture.changed_paths, vec![PathBuf::from(raw_name)]);
}
