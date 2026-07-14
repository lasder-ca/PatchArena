//! End-to-end CLI tests for initialization and task persistence.

use std::{fs, path::Path, process::Command as StdCommand};

use chrono::Utc;
use patcharena_core::{
    ArtifactPaths, BenchmarkIdentity, CURRENT_RESULT_SCHEMA_VERSION, CommandOutcome, RunGroup,
    RunResult, TaskCommand as CoreTaskCommand, TaskDefinition, TaskId,
};
use predicates::prelude::*;
use tempfile::TempDir;
use uuid::Uuid;

fn repository() -> TempDir {
    let directory = tempfile::tempdir().expect("temp repository");
    let output = StdCommand::new("git")
        .args(["init", "--quiet"])
        .current_dir(directory.path())
        .output()
        .expect("git init");
    assert!(output.status.success());
    directory
}

fn binary() -> assert_cmd::Command {
    assert_cmd::cargo::cargo_bin_cmd!("patcharena")
}

fn init(directory: &Path) {
    binary()
        .current_dir(directory)
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("initialized PatchArena"));
}

#[test]
fn help_lists_the_mvp_commands() {
    binary()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("task"))
        .stdout(predicate::str::contains("run"))
        .stdout(predicate::str::contains("compare"))
        .stdout(predicate::str::contains("report"))
        .stdout(predicate::str::contains("doctor"));
}

#[test]
fn init_is_idempotent_and_does_not_overwrite_config() {
    let directory = repository();
    init(directory.path());
    let config_path = directory.path().join("patcharena.toml");
    let original = fs::read(&config_path).expect("read config");
    binary()
        .current_dir(directory.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("kept existing"));
    assert_eq!(fs::read(config_path).expect("read config again"), original);
    assert!(directory.path().join(".patcharena/tasks").is_dir());
    assert!(directory.path().join(".patcharena/runs").is_dir());
    assert!(directory.path().join(".patcharena/groups").is_dir());
}

#[test]
fn adds_lists_and_rejects_traversal_task_ids() {
    let directory = repository();
    init(directory.path());
    let prompt = directory.path().join("prompt.md");
    fs::write(&prompt, "Fix the regression.\n").expect("write prompt");
    binary()
        .current_dir(directory.path())
        .args([
            "task",
            "add",
            "--id",
            "csv-newline-regression",
            "--prompt-file",
        ])
        .arg(&prompt)
        .args(["--verify", "cargo test csv_export"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added task"));
    binary()
        .current_dir(directory.path())
        .args(["task", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("csv-newline-regression"));

    binary()
        .current_dir(directory.path())
        .args(["task", "add", "--id", "../escape", "--prompt-file"])
        .arg(&prompt)
        .args(["--verify", "cargo test"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("invalid task ID"));
    assert!(!directory.path().join("escape.yaml").exists());
}

#[test]
fn run_rejects_a_task_document_with_a_different_id_before_agent_discovery() {
    let directory = repository();
    init(directory.path());
    let task = TaskDefinition::new(
        TaskId::new("declared-id").expect("declared task id"),
        "Do not execute this mismatched task.",
        [CoreTaskCommand::new("true", std::iter::empty::<&str>())],
    )
    .expect("task definition");
    task.save_new(directory.path().join(".patcharena/tasks/requested-id.yaml"))
        .expect("save mismatched task");
    binary()
        .current_dir(directory.path())
        .args(["run", "--task", "requested-id"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains(
            "requested `requested-id` but task document declares `declared-id`",
        ));
}

fn store_group(directory: &Path, success: bool, fingerprint_digit: char) -> RunGroup {
    let task_id = TaskId::new("report-task").expect("task id");
    let mut group = RunGroup::new(task_id.clone(), "fake", Utc::now(), 1).expect("group");
    group.benchmark_identity = Some(BenchmarkIdentity {
        repository_commit: "0".repeat(40),
        task_fingerprint: fingerprint_digit.to_string().repeat(64),
    });
    let run_id = Uuid::new_v4().to_string();
    group.push_run_id(run_id.clone()).expect("group run");
    group.mark_completed().expect("complete group");
    let exit_code = i32::from(!success);
    let now = Utc::now();
    let result = RunResult {
        schema_version: CURRENT_RESULT_SCHEMA_VERSION,
        run_id: run_id.clone(),
        group_id: Some(group.group_id.clone()),
        task_id,
        agent: "fake".to_owned(),
        instructions_enabled: true,
        benchmark_identity: group.benchmark_identity.clone(),
        started_at: now,
        finished_at: now,
        duration_ms: if success { 10 } else { 30 },
        success,
        exit_code: Some(exit_code),
        changed_files: 1,
        added_lines: 2,
        deleted_lines: 1,
        setup: Vec::new(),
        agent_outcome: Some(CommandOutcome::exited("fake agent", exit_code, 5)),
        verification: vec![CommandOutcome::exited("cargo test", exit_code, 5)],
        audit: Vec::new(),
        violations: Vec::new(),
        artifacts: ArtifactPaths::default(),
        error: (!success).then(|| "verification failed".to_owned()),
    };
    let run_directory = directory.join(".patcharena/runs").join(&run_id);
    fs::create_dir(&run_directory).expect("run directory");
    result
        .save_new(run_directory.join("result.json"))
        .expect("save result");
    group
        .save_new(
            directory
                .join(".patcharena/groups")
                .join(format!("{}.json", group.group_id)),
        )
        .expect("save group");
    group
}

#[test]
fn compare_and_html_report_use_persisted_results() {
    let directory = repository();
    init(directory.path());
    let baseline = store_group(directory.path(), false, '1');
    let candidate = store_group(directory.path(), true, '1');
    let comparison = directory.path().join("comparison.json");
    binary()
        .current_dir(directory.path())
        .args([
            "compare",
            "--baseline",
            &baseline.group_id,
            "--candidate",
            &candidate.group_id,
            "--output",
        ])
        .arg(&comparison)
        .assert()
        .success()
        .stdout(predicate::str::contains("delta: +100.0 pp success"));
    assert!(
        fs::read_to_string(&comparison)
            .expect("comparison JSON")
            .contains("\"schema_version\": 1")
    );

    let html = directory.path().join("report.html");
    binary()
        .current_dir(directory.path())
        .args([
            "report",
            "--format",
            "html",
            "--group",
            &candidate.group_id,
            "--output",
        ])
        .arg(&html)
        .assert()
        .success();
    let html = fs::read_to_string(html).expect("HTML report");
    assert!(html.starts_with("<!doctype html>"));
    assert!(!html.contains("https://"));
}

#[test]
fn compare_rejects_different_benchmark_inputs() {
    let directory = repository();
    init(directory.path());
    let baseline = store_group(directory.path(), false, '1');
    let candidate = store_group(directory.path(), true, '2');
    binary()
        .current_dir(directory.path())
        .args([
            "compare",
            "--baseline",
            &baseline.group_id,
            "--candidate",
            &candidate.group_id,
        ])
        .assert()
        .code(7)
        .stderr(predicate::str::contains(
            "repository commit or task/policy fingerprint differs",
        ));
}

#[cfg(unix)]
#[test]
fn refuses_metadata_directory_symlink_escape() {
    use std::os::unix::fs::symlink;

    let directory = repository();
    init(directory.path());
    let tasks = directory.path().join(".patcharena/tasks");
    fs::remove_dir(&tasks).expect("remove empty tasks directory");
    let outside = tempfile::tempdir().expect("outside directory");
    symlink(outside.path(), &tasks).expect("tasks symlink");
    let prompt = directory.path().join("prompt.md");
    fs::write(&prompt, "Fix it.\n").expect("write prompt");
    binary()
        .current_dir(directory.path())
        .args(["task", "add", "--id", "escape-test", "--prompt-file"])
        .arg(&prompt)
        .args(["--verify", "cargo test"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("symlink"));
    assert!(!outside.path().join("escape-test.yaml").exists());

    binary()
        .current_dir(directory.path())
        .arg("doctor")
        .assert()
        .code(4)
        .stdout(predicate::str::contains(
            ".patcharena writable: prerequisite check failed: refusing non-directory or symlink component",
        ));
}
