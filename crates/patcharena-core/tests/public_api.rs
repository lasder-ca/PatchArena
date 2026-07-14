//! Integration coverage for PatchArena's public core persistence and validation APIs.

use std::fs;
use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use patcharena_core::{
    ArtifactPaths, CommandOutcome, CoreError, ProjectConfig, RunGroup, RunGroupStatus, RunResult,
    SuiteDefinition, SuiteId, TaskCommand, TaskDefinition, TaskId, load_suites, load_tasks,
    suite_file_path, task_file_path,
};
use tempfile::tempdir;
use uuid::Uuid;

fn example_task(id: &str) -> TaskDefinition {
    TaskDefinition::new(
        TaskId::new(id).expect("safe task ID"),
        "Fix the reproducible regression.",
        [TaskCommand::new("cargo", ["test", "--all-targets"])],
    )
    .expect("valid task")
}

fn example_result(run_id: String, group_id: String) -> RunResult {
    RunResult {
        schema_version: 1,
        patcharena_version: None,
        run_id,
        group_id: Some(group_id),
        task_id: TaskId::new("example").expect("task ID"),
        agent: "codex".to_owned(),
        agent_metadata: None,
        execution_metadata: None,
        instructions_enabled: false,
        benchmark_identity: None,
        started_at: Utc.with_ymd_and_hms(2026, 7, 13, 1, 2, 3).unwrap(),
        finished_at: Utc.with_ymd_and_hms(2026, 7, 13, 1, 2, 4).unwrap(),
        duration_ms: 1_000,
        success: true,
        exit_code: Some(0),
        changed_files: 1,
        added_lines: 3,
        deleted_lines: 1,
        setup: Vec::new(),
        agent_outcome: Some(CommandOutcome::exited("codex exec -- -", 0, 900)),
        verification: vec![CommandOutcome::exited("cargo test --all-targets", 0, 100)],
        audit: Vec::new(),
        violations: Vec::new(),
        artifacts: ArtifactPaths::default(),
        error: None,
    }
}

#[test]
fn configuration_create_only_round_trip() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("patcharena.toml");
    let config = ProjectConfig::default();

    config.save_new(&path).expect("save config");
    assert_eq!(ProjectConfig::load(&path).expect("load config"), config);
    let error = config.save_new(&path).expect_err("must not overwrite");
    assert!(matches!(error, CoreError::AlreadyExists { .. }));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let probe = directory.path().join("permission-probe");
        fs::write(&probe, []).expect("create permission probe");
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o600))
            .expect("set probe permissions");
        let filesystem_honors_modes = fs::metadata(&probe)
            .expect("probe metadata")
            .permissions()
            .mode()
            & 0o777
            == 0o600;
        if filesystem_honors_modes {
            assert_eq!(
                fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[test]
fn task_directory_loading_is_sorted_and_filename_bound() {
    let directory = tempdir().expect("temporary directory");
    let beta = example_task("beta");
    let alpha = example_task("alpha");
    beta.save_new(task_file_path(directory.path(), &beta.id))
        .expect("save beta");
    alpha
        .save_new(task_file_path(directory.path(), &alpha.id))
        .expect("save alpha");

    let tasks = load_tasks(directory.path()).expect("load tasks");
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );

    let mismatch = example_task("actual");
    mismatch
        .save_new(directory.path().join("different.yaml"))
        .expect("save mismatch");
    assert!(load_tasks(directory.path()).is_err());
}

#[test]
fn yaml_rejects_path_traversal_in_forbidden_paths() {
    let yaml = r#"
id: traversal
prompt: Fix it.
verify:
  commands: [cargo test]
forbidden:
  commands: [git push]
  paths: [../outside]
"#;
    assert!(TaskDefinition::from_yaml(yaml).is_err());
}

#[test]
fn command_line_tokenization_never_interprets_operators() {
    let command = TaskCommand::command_line("cargo test 'named test' '|' tee output");
    let (program, arguments) = command.to_argv().expect("tokenize");
    assert_eq!(program, "cargo");
    assert_eq!(arguments, ["test", "named test", "|", "tee", "output"]);

    assert!(
        TaskCommand::command_line("cargo test 'unterminated")
            .validate()
            .is_err()
    );
}

#[test]
fn run_and_group_json_round_trip_with_instruction_dimension() {
    let directory = tempdir().expect("temporary directory");
    let group_id = Uuid::new_v4().to_string();
    let run_id = Uuid::new_v4().to_string();
    let result = example_result(run_id.clone(), group_id.clone());
    let result_path = directory.path().join("result.json");
    result.save_new(&result_path).expect("save result");
    assert_eq!(RunResult::load(&result_path).expect("load result"), result);

    let group = RunGroup {
        schema_version: 1,
        group_id,
        task_id: TaskId::new("example").expect("task ID"),
        agent: "codex".to_owned(),
        instructions_enabled: false,
        benchmark_identity: None,
        created_at: Utc.with_ymd_and_hms(2026, 7, 13, 1, 2, 3).unwrap(),
        requested_runs: Some(1),
        status: RunGroupStatus::Completed,
        run_ids: vec![run_id],
    };
    let group_path = directory.path().join("group.json");
    group.save_new(&group_path).expect("save group");
    let loaded = RunGroup::load(&group_path).expect("load group");
    assert_eq!(loaded, group);
    assert_eq!(
        loaded.summarize(&[result]).expect("summary").success_rate,
        1.0
    );
}

#[test]
fn legacy_group_json_loads_with_unknown_status_and_no_requested_count() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("legacy-group.json");
    let group_id = Uuid::new_v4().to_string();
    let run_id = Uuid::new_v4().to_string();
    let legacy = serde_json::json!({
        "schema_version": 1,
        "group_id": group_id,
        "task_id": "example",
        "agent": "codex",
        "instructions_enabled": true,
        "created_at": "2026-07-13T01:02:03Z",
        "run_ids": [run_id]
    });
    fs::write(
        &path,
        serde_json::to_vec_pretty(&legacy).expect("legacy JSON"),
    )
    .expect("write legacy group");

    let loaded = RunGroup::load(path).expect("load legacy group");
    assert_eq!(loaded.requested_runs, None);
    assert_eq!(loaded.status, RunGroupStatus::Unknown);
    loaded.validate().expect("validate legacy group");
}

#[test]
fn older_version_one_json_defaults_instructions_to_enabled() {
    let run_id = Uuid::new_v4().to_string();
    let group_id = Uuid::new_v4().to_string();
    let result = example_result(run_id, group_id);
    let mut value = serde_json::to_value(result).expect("serialize");
    value
        .as_object_mut()
        .expect("result object")
        .remove("instructions_enabled");

    let parsed = RunResult::from_json(&serde_json::to_string(&value).expect("JSON"))
        .expect("backward-compatible parse");
    assert!(parsed.instructions_enabled);
}

#[cfg(unix)]
#[test]
fn task_loader_refuses_symbolic_links() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("temporary directory");
    let outside = tempdir().expect("outside directory");
    let target = outside.path().join("outside.yaml");
    fs::write(&target, example_task("linked").to_yaml().expect("YAML")).expect("write target");
    symlink(&target, directory.path().join("linked.yaml")).expect("symlink");

    assert!(matches!(
        load_tasks(directory.path()),
        Err(CoreError::UnsafePath { .. })
    ));
}

#[test]
fn artifact_paths_must_not_escape_run_directory() {
    let artifacts = ArtifactPaths {
        stdout: PathBuf::from("logs/stdout.log"),
        stderr: PathBuf::from("logs/stderr.log"),
        patch: PathBuf::from("../../outside.diff"),
        audit: None,
    };
    assert!(artifacts.validate().is_err());
}

#[test]
fn suite_definition_round_trips_and_fingerprints_stably() {
    let directory = tempdir().expect("temporary directory");
    let suite = SuiteDefinition::new(
        SuiteId::new("core").expect("suite ID"),
        Some("Core maintenance tasks".to_owned()),
        vec![
            TaskId::new("csv-newline").expect("task ID"),
            TaskId::new("config-validation").expect("task ID"),
        ],
    )
    .expect("suite");
    let path = suite_file_path(directory.path(), &suite.id);
    suite.save_new(&path).expect("save suite");

    let loaded = SuiteDefinition::load(&path).expect("load suite");
    assert_eq!(loaded, suite);
    assert_eq!(loaded.fingerprint().expect("fingerprint").len(), 64);
    assert_eq!(load_suites(directory.path()).expect("load suites"), [suite]);
}

#[test]
fn suite_definition_rejects_empty_duplicate_and_unknown_fields() {
    assert!(SuiteDefinition::new(SuiteId::new("empty").unwrap(), None, vec![]).is_err());
    let repeated = TaskId::new("same").unwrap();
    assert!(
        SuiteDefinition::new(
            SuiteId::new("duplicate").unwrap(),
            None,
            vec![repeated.clone(), repeated],
        )
        .is_err()
    );
    assert!(
        SuiteDefinition::from_yaml("schema_version: 1\nid: core\ntasks: [one]\nunknown: true\n")
            .is_err()
    );
}

#[test]
fn schema_one_config_without_suite_paths_uses_safe_defaults() {
    let config = ProjectConfig::from_toml(
        "schema_version = 1\n[paths]\nstate_dir = '.patcharena'\ntasks_dir = '.patcharena/tasks'\nruns_dir = '.patcharena/runs'\ngroups_dir = '.patcharena/groups'\nbattles_dir = '.patcharena/battles'\n",
    )
    .expect("old config");
    assert_eq!(config.paths.suites_dir, PathBuf::from(".patcharena/suites"));
    assert_eq!(
        config.paths.suite_runs_dir,
        PathBuf::from(".patcharena/suite-runs")
    );
}
