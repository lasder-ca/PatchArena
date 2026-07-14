use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
};

use chrono::Utc;
use patcharena_core::{
    BattleAgentResult, BattleResult, CURRENT_RESULT_SCHEMA_VERSION, CommandList, ProjectConfig,
    ResolvedProjectPaths, TaskCommand as CoreTaskCommand, TaskDefinition, TaskId, ValidationError,
    load_tasks, task_file_path,
};
use patcharena_git::Repository;
use patcharena_report::{BenchmarkReport, Comparison, load_report, load_selection};
use patcharena_runner::{AgentContext, AgentRegistry, ArenaRunner, RunnerSettings};
use uuid::Uuid;

use crate::{
    AgentCommand, BattleArgs, CliError, Command, CompareArgs, ReportArgs, ReportFormat, RunArgs,
    TaskAddArgs, TaskCommand,
};

pub(crate) const EXIT_SUCCESS: u8 = 0;
const EXIT_PREREQUISITE: u8 = 4;
pub(crate) const EXIT_BENCHMARK_FAILED: u8 = 6;

pub async fn run(command: Command) -> Result<u8, CliError> {
    match command {
        Command::Init => init(),
        Command::Task { command } => match command {
            TaskCommand::Add(arguments) => task_add(*arguments),
            TaskCommand::List => task_list(),
        },
        Command::Agent { command } => agent_command(command),
        Command::Suite { command } => crate::suite::run(command).await,
        Command::Run(arguments) => run_benchmark(arguments).await,
        Command::Battle(arguments) => battle(arguments).await,
        Command::Compare(arguments) => compare(arguments),
        Command::Report(arguments) => report(arguments),
        Command::Doctor => doctor(),
    }
}

fn init() -> Result<u8, CliError> {
    let current_directory = current_directory()?;
    let repository = Repository::discover(&current_directory)?;
    let config_path = repository.root().join(patcharena_core::CONFIG_FILE_NAME);
    let (config, created_config) = if config_path.exists() {
        (ProjectConfig::load(&config_path)?, false)
    } else {
        let config = ProjectConfig::default();
        config.save_new(&config_path)?;
        (config, true)
    };
    let paths = config.resolve_paths(repository.root())?;
    create_project_directories(&paths)?;
    if created_config {
        println!("initialized PatchArena in {}", repository.root().display());
    } else {
        println!(
            "PatchArena already initialized; kept existing {}",
            config_path.display()
        );
    }
    println!("tasks: {}", paths.tasks_dir.display());
    println!("runs:  {}", paths.runs_dir.display());
    Ok(EXIT_SUCCESS)
}

fn task_add(arguments: TaskAddArgs) -> Result<u8, CliError> {
    let project = load_project()?;
    let id = TaskId::new(arguments.id)?;
    let prompt = patcharena_core::read_utf8_limited(&arguments.prompt_file, 1024 * 1024)?;
    let mut task = TaskDefinition::new(
        id.clone(),
        prompt,
        arguments
            .verify
            .into_iter()
            .map(CoreTaskCommand::command_line),
    )?;
    task.setup = CommandList {
        commands: arguments
            .setup
            .into_iter()
            .map(CoreTaskCommand::command_line)
            .collect(),
    };
    task.limits.timeout_seconds = arguments
        .timeout_seconds
        .unwrap_or(project.config.defaults.timeout_seconds);
    task.limits.max_changed_files = arguments
        .max_changed_files
        .unwrap_or(project.config.defaults.max_changed_files);
    task.limits.max_diff_lines = arguments
        .max_diff_lines
        .unwrap_or(project.config.defaults.max_diff_lines);
    task.limits.max_output_bytes = arguments
        .max_output_bytes
        .unwrap_or(project.config.defaults.max_output_bytes);
    append_unique(&mut task.forbidden.commands, arguments.forbidden_commands);
    append_unique(&mut task.forbidden.paths, arguments.forbidden_paths);
    task.validate()?;
    let destination = task_file_path(&project.paths.tasks_dir, &id);
    task.save_new(&destination)?;
    println!("added task `{id}` at {}", destination.display());
    Ok(EXIT_SUCCESS)
}

fn task_list() -> Result<u8, CliError> {
    let project = load_project()?;
    let tasks = load_tasks(&project.paths.tasks_dir)?;
    if tasks.is_empty() {
        println!("no tasks configured");
    } else {
        println!("ID\tVERIFY\tTIMEOUT");
        for task in tasks {
            println!(
                "{}\t{}\t{}s",
                task.id,
                task.verify.commands.len(),
                task.limits.timeout_seconds
            );
        }
    }
    Ok(EXIT_SUCCESS)
}

async fn run_benchmark(arguments: RunArgs) -> Result<u8, CliError> {
    let project = load_project()?;
    let task_id = TaskId::new(arguments.task)?;
    let task = TaskDefinition::load(task_file_path(&project.paths.tasks_dir, &task_id))?;
    if task.id != task_id {
        return Err(patcharena_core::CoreError::Validation(ValidationError::new(
            "task.id",
            format!(
                "requested `{task_id}` but task document declares `{}`",
                task.id
            ),
        ))
        .into());
    }
    let registry = AgentRegistry::from_project(&project.config, &project.paths.repository_root)?;
    let descriptor = registry
        .descriptor(&arguments.agent)
        .ok_or_else(|| CliError::Prerequisite(format!("unknown agent `{}`", arguments.agent)))?;
    if descriptor.cli_version.is_none() {
        return Err(CliError::Prerequisite(format!(
            "agent `{}` executable `{}` is unavailable or did not report a version",
            arguments.agent,
            descriptor.executable.display()
        )));
    }
    let settings = runner_settings(&project.config);
    let agent = registry.runner(&arguments.agent)?;
    let runner = ArenaRunner::new(
        project.repository,
        &project.paths.runs_dir,
        &project.paths.groups_dir,
        agent,
        settings,
    )?;
    let execution = runner
        .run_group(
            &task,
            arguments.repeat.get(),
            !arguments.without_instructions,
        )
        .await?;
    println!("group: {}", execution.group.group_id);
    for result in &execution.results {
        println!(
            "run {}: {} ({} ms, {} files, +{} -{})",
            result.run_id,
            if result.success { "passed" } else { "failed" },
            result.duration_ms,
            result.changed_files,
            result.added_lines,
            result.deleted_lines
        );
    }
    if execution.results.iter().all(|result| result.success) {
        Ok(EXIT_SUCCESS)
    } else {
        Ok(EXIT_BENCHMARK_FAILED)
    }
}

fn agent_command(command: AgentCommand) -> Result<u8, CliError> {
    let project = load_project()?;
    let registry = AgentRegistry::from_project(&project.config, &project.paths.repository_root)?;
    match command {
        AgentCommand::List => {
            println!("ID\tNAME\tSTATUS\tVERSION");
            for agent in registry.list() {
                let (status, version) = agent
                    .cli_version
                    .as_ref()
                    .map_or(("missing", "-"), |version| ("available", version.as_str()));
                println!(
                    "{}\t{}\t{}\t{}",
                    agent.id, agent.display_name, status, version
                );
            }
            Ok(EXIT_SUCCESS)
        }
        AgentCommand::Doctor { agent } => {
            let descriptor = registry
                .descriptor(&agent)
                .ok_or_else(|| CliError::Prerequisite(format!("unknown agent `{agent}`")))?;
            let mut healthy = true;
            println!("agent: {} ({})", descriptor.id, descriptor.display_name);
            healthy &= print_check(
                "CLI presence/version",
                descriptor.cli_version.clone().ok_or_else(|| {
                    format!(
                        "`{}` not found or version detection failed",
                        descriptor.executable.display()
                    )
                }),
            );
            let worktree = check_worktree(&project.repository);
            healthy &= print_check("Detached worktree", worktree);
            let context = AgentContext {
                working_dir: project.paths.repository_root.clone(),
                prompt: "<doctor-prompt>".into(),
                timeout: std::time::Duration::from_secs(1),
                max_output_bytes: 1024,
                env_allowlist: project.config.defaults.environment_allowlist.clone(),
                task_id: "doctor".into(),
                run_id: Uuid::nil().to_string(),
                result_dir: project.paths.state_dir.clone(),
            };
            match registry
                .runner(&agent)
                .map(|runner| runner.audit_command(&context))
            {
                Ok(command) => println!("[ok]   Invocation args: {command}"),
                Err(error) => {
                    println!("[fail] Invocation args: {error}");
                    healthy = false;
                }
            }
            println!(
                "[info] Configuration: validated; auth is checked by the agent CLI at execution time (credentials are never inspected)"
            );
            Ok(if healthy {
                EXIT_SUCCESS
            } else {
                EXIT_PREREQUISITE
            })
        }
    }
}

async fn battle(arguments: BattleArgs) -> Result<u8, CliError> {
    let project = load_project()?;
    if arguments.agents.is_empty() {
        return Err(CliError::Prerequisite(
            "battle requires at least one agent".into(),
        ));
    }
    let mut unique = HashSet::new();
    if arguments.agents.iter().any(|id| !unique.insert(id.clone())) {
        return Err(CliError::Prerequisite(
            "battle agent IDs must be unique".into(),
        ));
    }
    let task_id = TaskId::new(arguments.task)?;
    let task = TaskDefinition::load(task_file_path(&project.paths.tasks_dir, &task_id))?;
    let registry = AgentRegistry::from_project(&project.config, &project.paths.repository_root)?;
    for id in &arguments.agents {
        let descriptor = registry
            .descriptor(id)
            .ok_or_else(|| CliError::Prerequisite(format!("unknown agent `{id}`")))?;
        if descriptor.cli_version.is_none() {
            return Err(CliError::Prerequisite(format!(
                "agent `{id}` is unavailable"
            )));
        }
    }
    project.repository.ensure_tracked_clean()?;
    let base_commit = project.repository.resolve_commit("HEAD")?;
    let battle_id = Uuid::new_v4().to_string();
    let mut entries = Vec::new();
    println!("AGENT\tSTATUS\tRUNS\tGROUP");
    for id in &arguments.agents {
        let runner = ArenaRunner::new(
            project.repository.clone(),
            &project.paths.runs_dir,
            &project.paths.groups_dir,
            registry.runner(id)?,
            runner_settings(&project.config),
        )?;
        match runner
            .run_group(
                &task,
                arguments.repeat.get(),
                !arguments.without_instructions,
            )
            .await
        {
            Ok(execution) => {
                let same_base = execution
                    .group
                    .benchmark_identity
                    .as_ref()
                    .is_some_and(|identity| identity.repository_commit == base_commit);
                let error = if !same_base {
                    Some("base commit changed during battle".to_owned())
                } else if execution.results.iter().any(|result| !result.success) {
                    Some("one or more runs failed".to_owned())
                } else {
                    None
                };
                let run_ids = execution
                    .results
                    .iter()
                    .map(|result| result.run_id.clone())
                    .collect::<Vec<_>>();
                println!(
                    "{id}\t{}\t{}\t{}",
                    if error.is_none() {
                        "completed"
                    } else {
                        "failed"
                    },
                    run_ids.len(),
                    execution.group.group_id
                );
                entries.push(BattleAgentResult {
                    agent_id: id.clone(),
                    group_id: Some(execution.group.group_id),
                    run_ids,
                    error,
                });
            }
            Err(error) => {
                println!("{id}\tfailed\t0\t-");
                entries.push(BattleAgentResult {
                    agent_id: id.clone(),
                    group_id: None,
                    run_ids: Vec::new(),
                    error: Some(error.to_string()),
                });
            }
        }
    }
    let summary = BattleResult {
        schema_version: CURRENT_RESULT_SCHEMA_VERSION,
        patcharena_version: env!("CARGO_PKG_VERSION").into(),
        battle_id: battle_id.clone(),
        task_id,
        base_commit,
        repeat: arguments.repeat.get(),
        created_at: Utc::now(),
        agents: entries,
    };
    let path = project.paths.battles_dir.join(format!("{battle_id}.json"));
    summary.save_new(&path)?;
    println!("battle: {battle_id}\nJSON: {}", path.display());
    Ok(
        if summary.agents.iter().all(|entry| entry.error.is_none()) {
            EXIT_SUCCESS
        } else {
            EXIT_BENCHMARK_FAILED
        },
    )
}

pub(crate) fn runner_settings(config: &ProjectConfig) -> RunnerSettings {
    RunnerSettings {
        timeout_seconds: config.defaults.timeout_seconds,
        max_output_bytes: config.defaults.max_output_bytes,
        max_changed_files: config.defaults.max_changed_files,
        max_diff_lines: config.defaults.max_diff_lines,
        environment_allowlist: config.defaults.environment_allowlist.clone(),
        forbidden_commands: config.security.forbidden_commands.clone(),
        forbidden_paths: config.security.forbidden_paths.clone(),
    }
}

fn compare(arguments: CompareArgs) -> Result<u8, CliError> {
    let project = load_project()?;
    let baseline = load_selection(
        &project.paths.runs_dir,
        &project.paths.groups_dir,
        &arguments.baseline,
    )?;
    let candidate = load_selection(
        &project.paths.runs_dir,
        &project.paths.groups_dir,
        &arguments.candidate,
    )?;
    let comparison = Comparison::new(baseline, candidate)?;
    print!("{}", comparison.to_console());
    let output = if let Some(output) = arguments.output {
        output
    } else {
        let comparisons = project.paths.state_dir.join("comparisons");
        create_contained_directory(&project.paths.repository_root, &comparisons)?;
        comparisons.join(format!(
            "{}-vs-{}.json",
            arguments.baseline, arguments.candidate
        ))
    };
    write_generated_file(&output, comparison.to_json()?.as_bytes())?;
    println!("JSON: {}", output.display());
    Ok(EXIT_SUCCESS)
}

fn report(arguments: ReportArgs) -> Result<u8, CliError> {
    let project = load_project()?;
    let report = if let Some(group) = arguments.group {
        BenchmarkReport::new(vec![load_selection(
            &project.paths.runs_dir,
            &project.paths.groups_dir,
            &group,
        )?])
    } else {
        load_report(&project.paths.runs_dir, &project.paths.groups_dir)?
    };
    let rendered = match arguments.format {
        ReportFormat::Markdown => report.to_markdown(),
        ReportFormat::Json => report.to_json()?,
        ReportFormat::Html => report.to_html(),
    };
    if let Some(output) = arguments.output {
        write_generated_file(&output, rendered.as_bytes())?;
        println!(
            "wrote {} report to {}",
            format_name(arguments.format),
            output.display()
        );
    } else {
        print!("{rendered}");
    }
    Ok(EXIT_SUCCESS)
}

fn doctor() -> Result<u8, CliError> {
    let current_directory = current_directory()?;
    let mut healthy = true;
    healthy &= print_check("Git executable", executable_version("git", &["--version"]));
    healthy &= print_check("Rust compiler", executable_version("rustc", &["--version"]));
    healthy &= print_check("Cargo", executable_version("cargo", &["--version"]));
    healthy &= print_check("Codex CLI", executable_version("codex", &["--version"]));

    let repository = match Repository::discover(&current_directory) {
        Ok(repository) => {
            println!("[ok]   Git repository: {}", repository.root().display());
            Some(repository)
        }
        Err(error) => {
            println!("[fail] Git repository: {error}");
            healthy = false;
            None
        }
    };

    if let Some(repository) = repository {
        let worktree_check = check_worktree(&repository);
        healthy &= print_check("Detached worktree", worktree_check);
        let state_check = check_state_writable(&repository);
        healthy &= print_check(".patcharena writable", state_check);
    } else {
        println!("[skip] Detached worktree: no repository");
        println!("[skip] .patcharena writable: no repository");
    }

    Ok(if healthy {
        EXIT_SUCCESS
    } else {
        EXIT_PREREQUISITE
    })
}

pub(crate) struct Project {
    pub(crate) repository: Repository,
    pub(crate) config: ProjectConfig,
    pub(crate) paths: ResolvedProjectPaths,
}

pub(crate) fn load_project() -> Result<Project, CliError> {
    let repository = Repository::discover(current_directory()?)?;
    let config_path = repository.root().join(patcharena_core::CONFIG_FILE_NAME);
    let config = ProjectConfig::load(config_path)?;
    let paths = config.resolve_paths(repository.root())?;
    create_project_directories(&paths)?;
    Ok(Project {
        repository,
        config,
        paths,
    })
}

fn current_directory() -> Result<PathBuf, CliError> {
    std::env::current_dir().map_err(|source| CliError::Io {
        operation: "read current directory",
        path: PathBuf::from("."),
        source,
    })
}

pub(crate) fn create_project_directories(paths: &ResolvedProjectPaths) -> Result<(), CliError> {
    for directory in [
        &paths.state_dir,
        &paths.tasks_dir,
        &paths.runs_dir,
        &paths.groups_dir,
        &paths.battles_dir,
        &paths.suites_dir,
        &paths.suite_runs_dir,
    ] {
        create_contained_directory(&paths.repository_root, directory)?;
    }
    Ok(())
}

pub(crate) fn create_contained_directory(root: &Path, target: &Path) -> Result<(), CliError> {
    walk_contained_directory(root, target, true)
}

fn validate_contained_directory(root: &Path, target: &Path) -> Result<(), CliError> {
    walk_contained_directory(root, target, false)
}

fn walk_contained_directory(
    root: &Path,
    target: &Path,
    create_missing: bool,
) -> Result<(), CliError> {
    let relative = target.strip_prefix(root).map_err(|_| {
        CliError::Prerequisite(format!(
            "configured path `{}` is outside repository `{}`",
            target.display(),
            root.display()
        ))
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(segment) = component else {
            return Err(CliError::Prerequisite(format!(
                "unsafe configured directory `{}`",
                target.display()
            )));
        };
        current.push(segment);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(CliError::Prerequisite(format!(
                    "refusing non-directory or symlink component `{}`",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_missing => {
                fs::create_dir(&current).map_err(|source| CliError::Io {
                    operation: "create PatchArena directory",
                    path: current.clone(),
                    source,
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(CliError::Prerequisite(format!(
                    "PatchArena directory `{}` is missing; run `patcharena init`",
                    current.display()
                )));
            }
            Err(source) => {
                return Err(CliError::Io {
                    operation: "inspect PatchArena directory",
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn append_unique<T>(destination: &mut Vec<T>, values: Vec<T>)
where
    T: Clone + Eq + std::hash::Hash,
{
    let mut seen = destination.iter().cloned().collect::<HashSet<_>>();
    destination.extend(
        values
            .into_iter()
            .filter(|value| seen.insert(value.clone())),
    );
}

pub(crate) fn write_generated_file(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|source| CliError::Io {
            operation: "create output directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    patcharena_core::atomic_write_replace(path, bytes)?;
    Ok(())
}

fn executable_version(program: &str, arguments: &[&str]) -> Result<String, String> {
    let output = ProcessCommand::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("exited with {:?}", output.status.code()));
    }
    let version = if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    };
    Ok(String::from_utf8_lossy(version).trim().to_owned())
}

fn print_check(label: &str, result: Result<String, String>) -> bool {
    match result {
        Ok(detail) => {
            println!("[ok]   {label}: {detail}");
            true
        }
        Err(error) => {
            println!("[fail] {label}: {error}");
            false
        }
    }
}

fn check_worktree(repository: &Repository) -> Result<String, String> {
    let parent = tempfile::Builder::new()
        .prefix("patcharena-doctor-")
        .tempdir()
        .map_err(|error| error.to_string())?;
    let worktree = repository
        .create_detached_worktree(parent.path().join("worktree"), None)
        .map_err(|error| error.to_string())?;
    worktree.close().map_err(|error| error.to_string())?;
    Ok("create/remove succeeded".to_owned())
}

fn check_state_writable(repository: &Repository) -> Result<String, String> {
    let config_path = repository.root().join(patcharena_core::CONFIG_FILE_NAME);
    let config = ProjectConfig::load(config_path).map_err(|error| error.to_string())?;
    let paths = config
        .resolve_paths(repository.root())
        .map_err(|error| error.to_string())?;
    for directory in [
        &paths.state_dir,
        &paths.tasks_dir,
        &paths.runs_dir,
        &paths.groups_dir,
        &paths.battles_dir,
        &paths.suites_dir,
        &paths.suite_runs_dir,
    ] {
        validate_contained_directory(&paths.repository_root, directory)
            .map_err(|error| error.to_string())?;
    }
    let mut temporary = tempfile::Builder::new()
        .prefix("doctor-")
        .tempfile_in(&paths.state_dir)
        .map_err(|error| error.to_string())?;
    temporary
        .write_all(b"PatchArena doctor write check\n")
        .map_err(|error| error.to_string())?;
    temporary.flush().map_err(|error| error.to_string())?;
    Ok(paths.state_dir.display().to_string())
}

fn format_name(format: ReportFormat) -> &'static str {
    match format {
        ReportFormat::Markdown => "Markdown",
        ReportFormat::Json => "JSON",
        ReportFormat::Html => "HTML",
    }
}
