//! End-to-end CLI tests for initialization and task persistence.

use std::{fs, io::Write, path::Path, process::Command as StdCommand};

use chrono::Utc;
use patcharena_core::{
    ArtifactPaths, BattleResult, BenchmarkIdentity, CURRENT_RESULT_SCHEMA_VERSION, CommandOutcome,
    RunGroup, RunResult, SuiteCellStatus, SuiteExecution, SuiteExecutionStatus,
    TaskCommand as CoreTaskCommand, TaskDefinition, TaskId,
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
        .stdout(predicate::str::contains("initialized PatchArena"))
        .stdout(predicate::str::contains("suites:"))
        .stdout(predicate::str::contains("suite runs:"));
}

fn commit_base(directory: &Path) {
    fs::write(directory.join("README.md"), "fixture\n").expect("write base");
    assert!(
        StdCommand::new("git")
            .args(["add", "--all"])
            .current_dir(directory)
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        StdCommand::new("git")
            .args([
                "-c",
                "user.name=PatchArena Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base"
            ])
            .current_dir(directory)
            .status()
            .expect("git commit")
            .success()
    );
}

#[test]
fn help_lists_the_mvp_commands() {
    binary()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("init"))
        .stdout(predicate::str::contains("task"))
        .stdout(predicate::str::contains("agent"))
        .stdout(predicate::str::contains("suite"))
        .stdout(predicate::str::contains("run"))
        .stdout(predicate::str::contains("battle"))
        .stdout(predicate::str::contains("compare"))
        .stdout(predicate::str::contains("report"))
        .stdout(predicate::str::contains("doctor"));
}

#[cfg(unix)]
#[test]
fn battle_continues_after_a_fake_failure_and_keeps_one_base_commit() {
    use std::os::unix::fs::PermissionsExt;

    let directory = repository();
    let failing = directory.path().join("fake-fail");
    fs::write(
        &failing,
        "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ]; then echo 'fake-fail 1.0'; exit 0; fi\nexit 1\n",
    )
    .expect("fake failing agent");
    let mut permissions = fs::metadata(&failing).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&failing, permissions).expect("chmod");
    let nested = directory.path().join("nested");
    fs::create_dir(&nested).expect("nested cwd");
    commit_base(directory.path());
    init(directory.path());
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(directory.path().join("patcharena.toml"))
        .expect("config");
    writeln!(config,"\n[agents.fake-fail]\ntype = \"custom\"\ncommand = \"./fake-fail\"\nargs = []\n\n[agents.fake-ok]\ntype = \"custom\"\ncommand = \"true\"\nargs = []").expect("agents");
    let task = TaskDefinition::new(
        TaskId::new("battle-task").expect("id"),
        "Make no changes.",
        [CoreTaskCommand::new("true", std::iter::empty::<&str>())],
    )
    .expect("task");
    task.save_new(directory.path().join(".patcharena/tasks/battle-task.yaml"))
        .expect("save task");
    binary()
        .current_dir(&nested)
        .args([
            "battle",
            "--task",
            "battle-task",
            "--agents",
            "fake-fail,fake-ok",
            "--repeat",
            "1",
        ])
        .assert()
        .code(6)
        .stdout(predicate::str::contains("fake-fail\tfailed"))
        .stdout(predicate::str::contains("fake-ok\tcompleted"));
    let battle_path = fs::read_dir(directory.path().join(".patcharena/battles"))
        .expect("battles")
        .next()
        .expect("battle entry")
        .expect("entry")
        .path();
    let battle = BattleResult::load(battle_path).expect("battle result");
    assert_eq!(battle.agents.len(), 2);
    assert!(battle.agents[0].error.is_some());
    assert!(battle.agents[1].error.is_none());
    assert_ne!(battle.agents[0].run_ids, battle.agents[1].run_ids);
    for entry in &battle.agents {
        let run = RunResult::load(
            directory
                .path()
                .join(".patcharena/runs")
                .join(&entry.run_ids[0])
                .join("result.json"),
        )
        .expect("run");
        assert_eq!(
            run.benchmark_identity.expect("identity").repository_commit,
            battle.base_commit
        );
    }
}

#[cfg(unix)]
#[test]
fn suite_run_builds_a_two_task_two_agent_evidence_matrix() {
    use std::os::unix::fs::PermissionsExt;

    let directory = repository();
    init(directory.path());
    for agent in ["fake-a", "fake-b"] {
        let executable = directory.path().join(agent);
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nif [ \"${{1:-}}\" = \"--version\" ]; then echo '{agent} 1.0'; exit 0; fi\nexit 0\n"
            ),
        )
        .expect("fake agent");
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
    }
    let mut config = fs::OpenOptions::new()
        .append(true)
        .open(directory.path().join("patcharena.toml"))
        .unwrap();
    writeln!(
        config,
        "\n[agents.fake-a]\ntype = \"custom\"\ncommand = \"./fake-a\"\nargs = []\n\n[agents.fake-b]\ntype = \"custom\"\ncommand = \"./fake-b\"\nargs = []"
    )
    .unwrap();
    drop(config);
    for id in ["one", "two"] {
        let task = TaskDefinition::new(
            TaskId::new(id).unwrap(),
            "Make no changes.",
            [CoreTaskCommand::new("true", std::iter::empty::<&str>())],
        )
        .unwrap();
        task.save_new(
            directory
                .path()
                .join(".patcharena/tasks")
                .join(format!("{id}.yaml")),
        )
        .unwrap();
    }
    binary()
        .current_dir(directory.path())
        .args([
            "suite",
            "add",
            "--id",
            "core",
            "--description",
            "Core regression suite",
            "--task",
            "one",
            "--task",
            "two",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("added suite `core`"));
    binary()
        .current_dir(directory.path())
        .args(["suite", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("core\t2\tCore regression suite"));
    binary()
        .current_dir(directory.path())
        .args([
            "suite", "add", "--id", "core", "--task", "one", "--task", "two",
        ])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("refusing to overwrite"));
    commit_base(directory.path());

    binary()
        .current_dir(directory.path())
        .args([
            "suite",
            "run",
            "--suite",
            "core",
            "--agents",
            "fake-a,fake-a",
            "--dry-run",
        ])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("selected more than once"));

    binary()
        .current_dir(directory.path())
        .args([
            "suite",
            "run",
            "--suite",
            "core",
            "--agents",
            "fake-a,fake-b",
            "--repeat",
            "1",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("invocations: 4"))
        .stdout(predicate::str::contains(
            "no run, group, or suite-run records",
        ));
    assert_eq!(
        fs::read_dir(directory.path().join(".patcharena/suite-runs"))
            .unwrap()
            .count(),
        0
    );

    binary()
        .current_dir(directory.path())
        .args([
            "suite",
            "run",
            "--suite",
            "core",
            "--agents",
            "fake-a,fake-b",
            "--repeat",
            "1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("suite run:"))
        .stdout(predicate::str::contains("HTML:"));

    let suite_run_directories = fs::read_dir(directory.path().join(".patcharena/suite-runs"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(suite_run_directories.len(), 1);
    let suite_run_directory = suite_run_directories[0].path();
    let execution = SuiteExecution::load(suite_run_directory.join("suite.json")).unwrap();
    assert_eq!(execution.cells.len(), 4);
    assert!(execution.cells.iter().all(|cell| cell.group_id.is_some()));
    assert_eq!(
        fs::read_dir(directory.path().join(".patcharena/groups"))
            .unwrap()
            .count(),
        4
    );
    assert_eq!(
        fs::read_dir(directory.path().join(".patcharena/runs"))
            .unwrap()
            .count(),
        4
    );
    let report_json = fs::read_to_string(suite_run_directory.join("report.json")).unwrap();
    let report: patcharena_report::SuiteReport = serde_json::from_str(&report_json).unwrap();
    assert_eq!(report.cells.len(), 4);
    let html = fs::read_to_string(suite_run_directory.join("report.html")).unwrap();
    assert_eq!(html.matches("class=\"cell ").count(), 4);
    assert!(!html.contains("https://"));

    let suite_run_id = execution.suite_run_id.clone();
    let preserved_group_ids = execution.cells[..3]
        .iter()
        .map(|cell| cell.group_id.clone())
        .collect::<Vec<_>>();
    let mut interrupted = execution;
    let last = interrupted.cells.last_mut().unwrap();
    last.status = SuiteCellStatus::Pending;
    last.group_id = None;
    last.error = None;
    interrupted.status = SuiteExecutionStatus::Running;
    interrupted.completed_at = None;
    interrupted.updated_at = Utc::now();
    interrupted
        .save_replace(suite_run_directory.join("suite.json"))
        .unwrap();

    binary()
        .current_dir(directory.path())
        .args(["suite", "resume", "--run", &suite_run_id])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "suite run: {suite_run_id}"
        )));
    let resumed = SuiteExecution::load(suite_run_directory.join("suite.json")).unwrap();
    assert_eq!(
        resumed.cells[..3]
            .iter()
            .map(|cell| cell.group_id.clone())
            .collect::<Vec<_>>(),
        preserved_group_ids
    );
    assert!(resumed.cells.last().unwrap().group_id.is_some());
    assert_eq!(
        fs::read_dir(directory.path().join(".patcharena/groups"))
            .unwrap()
            .count(),
        5
    );

    let export = directory.path().join("suite-export.json");
    binary()
        .current_dir(directory.path())
        .args([
            "suite",
            "report",
            "--run",
            &suite_run_id,
            "--format",
            "json",
            "--output",
        ])
        .arg(&export)
        .assert()
        .success()
        .stdout(predicate::str::contains("wrote JSON suite report"));
    let exported: patcharena_report::SuiteReport =
        serde_json::from_str(&fs::read_to_string(export).unwrap()).unwrap();
    assert_eq!(exported.cells.len(), 4);
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
    assert!(directory.path().join(".patcharena/suites").is_dir());
    assert!(directory.path().join(".patcharena/suite-runs").is_dir());
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
        patcharena_version: None,
        run_id: run_id.clone(),
        group_id: Some(group.group_id.clone()),
        task_id,
        agent: "fake".to_owned(),
        agent_metadata: None,
        execution_metadata: None,
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
