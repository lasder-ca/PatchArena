//! CLI workflow for reviewable, checkpointed benchmark suites.

use std::collections::HashSet;

use patcharena_core::{
    SuiteCellStatus, SuiteDefinition, SuiteExecution, SuiteId, TaskDefinition, ValidationError,
    load_suites, suite_checkpoint_path, suite_file_path, suite_run_directory, task_file_path,
};
use patcharena_report::{SuiteReport, load_suite_report};
use patcharena_runner::{AgentRegistry, SelectedSuiteAgent, SuiteExecutionOutcome, SuiteRunner};

use crate::commands::{
    EXIT_BENCHMARK_FAILED, EXIT_SUCCESS, Project, create_contained_directory, load_project,
    runner_settings, write_generated_file,
};
use crate::{
    CliError, ReportFormat, SuiteAddArgs, SuiteCommand, SuiteReportArgs, SuiteResumeArgs,
    SuiteRunArgs,
};

/// Execute one suite subcommand.
pub async fn run(command: SuiteCommand) -> Result<u8, CliError> {
    match command {
        SuiteCommand::Add(arguments) => add(arguments),
        SuiteCommand::List => list(),
        SuiteCommand::Run(arguments) => run_suite(arguments).await,
        SuiteCommand::Resume(arguments) => resume(arguments).await,
        SuiteCommand::Report(arguments) => report(arguments),
    }
}

fn add(arguments: SuiteAddArgs) -> Result<u8, CliError> {
    let project = load_project()?;
    let id = SuiteId::new(arguments.id)?;
    let tasks = arguments
        .task
        .into_iter()
        .map(patcharena_core::TaskId::new)
        .collect::<Result<Vec<_>, _>>()?;
    let suite = SuiteDefinition::new(id.clone(), arguments.description, tasks)?;
    load_suite_tasks(&project, &suite)?;
    let destination = suite_file_path(&project.paths.suites_dir, &id);
    suite.save_new(&destination)?;
    println!("added suite `{id}` at {}", destination.display());
    Ok(EXIT_SUCCESS)
}

fn list() -> Result<u8, CliError> {
    let project = load_project()?;
    let suites = load_suites(&project.paths.suites_dir)?;
    if suites.is_empty() {
        println!("no suites configured");
        return Ok(EXIT_SUCCESS);
    }
    println!("ID\tTASKS\tDESCRIPTION");
    for suite in suites {
        println!(
            "{}\t{}\t{}",
            suite.id,
            suite.tasks.len(),
            suite
                .description
                .as_deref()
                .map_or("-".to_owned(), terminal_text)
        );
    }
    Ok(EXIT_SUCCESS)
}

async fn run_suite(arguments: SuiteRunArgs) -> Result<u8, CliError> {
    let project = load_project()?;
    let suite = load_suite(&project, &arguments.suite)?;
    let tasks = load_suite_tasks(&project, &suite)?;
    let registry = AgentRegistry::from_project(&project.config, &project.paths.repository_root)?;
    let agents = selected_agents(&registry, &arguments.agents)?;
    let runner = suite_runner(&project, agents)?;
    let plan = runner.preflight(
        &suite,
        tasks,
        arguments.repeat.get(),
        !arguments.without_instructions,
    )?;
    if arguments.dry_run {
        print_plan(&plan);
        return Ok(EXIT_SUCCESS);
    }
    let outcome = runner.execute(plan).await?;
    finish(&project, &suite, outcome)
}

async fn resume(arguments: SuiteResumeArgs) -> Result<u8, CliError> {
    let project = load_project()?;
    let execution = load_execution(&project, &arguments.run)?;
    let suite = load_suite(&project, execution.suite_id.as_str())?;
    let tasks = load_suite_tasks(&project, &suite)?;
    let registry = AgentRegistry::from_project(&project.config, &project.paths.repository_root)?;
    let agents = selected_agents(&registry, &execution.agents)?;
    let runner = suite_runner(&project, agents)?;
    let outcome = runner.resume(execution, &suite, tasks).await?;
    finish(&project, &suite, outcome)
}

fn report(arguments: SuiteReportArgs) -> Result<u8, CliError> {
    let project = load_project()?;
    let execution = load_execution(&project, &arguments.run)?;
    let suite = load_matching_suite(&project, &execution)?;
    let report = load_suite_report(
        execution,
        suite.description,
        &project.paths.runs_dir,
        &project.paths.groups_dir,
    )?;
    let rendered = render(&report, arguments.format)?;
    if let Some(output) = arguments.output {
        write_generated_file(&output, rendered.as_bytes())?;
        println!(
            "wrote {} suite report to {}",
            format_name(arguments.format),
            output.display()
        );
    } else {
        print!("{rendered}");
    }
    exit_for_report(&report)
}

fn load_suite(project: &Project, requested_id: &str) -> Result<SuiteDefinition, CliError> {
    let id = SuiteId::new(requested_id)?;
    let suite = SuiteDefinition::load(suite_file_path(&project.paths.suites_dir, &id))?;
    if suite.id != id {
        return Err(patcharena_core::CoreError::Validation(ValidationError::new(
            "suite.id",
            format!(
                "requested `{id}` but suite document declares `{}`",
                suite.id
            ),
        ))
        .into());
    }
    Ok(suite)
}

fn load_matching_suite(
    project: &Project,
    execution: &SuiteExecution,
) -> Result<SuiteDefinition, CliError> {
    let suite = load_suite(project, execution.suite_id.as_str())?;
    if suite.fingerprint()? != execution.suite_fingerprint {
        return Err(patcharena_core::CoreError::Validation(ValidationError::new(
            "suite_fingerprint",
            "current suite definition differs from the persisted execution",
        ))
        .into());
    }
    Ok(suite)
}

fn load_suite_tasks(
    project: &Project,
    suite: &SuiteDefinition,
) -> Result<Vec<TaskDefinition>, CliError> {
    suite
        .tasks
        .iter()
        .map(|task_id| {
            let task = TaskDefinition::load(task_file_path(&project.paths.tasks_dir, task_id))?;
            if &task.id != task_id {
                return Err(patcharena_core::CoreError::Validation(ValidationError::new(
                    "task.id",
                    format!(
                        "suite references `{task_id}` but task document declares `{}`",
                        task.id
                    ),
                ))
                .into());
            }
            Ok(task)
        })
        .collect()
}

fn selected_agents(
    registry: &AgentRegistry,
    ids: &[String],
) -> Result<Vec<SelectedSuiteAgent>, CliError> {
    if ids.is_empty() {
        return Err(CliError::Prerequisite(
            "suite requires at least one explicit agent".to_owned(),
        ));
    }
    let mut unique = HashSet::with_capacity(ids.len());
    let mut selected = Vec::with_capacity(ids.len());
    for id in ids {
        if !unique.insert(id.as_str()) {
            return Err(CliError::Prerequisite(format!(
                "suite agent `{id}` was selected more than once"
            )));
        }
        let descriptor = registry
            .descriptor(id)
            .ok_or_else(|| CliError::Prerequisite(format!("unknown agent `{id}`")))?;
        if descriptor.cli_version.is_none() {
            return Err(CliError::Prerequisite(format!(
                "agent `{id}` executable `{}` is unavailable or did not report a version",
                descriptor.executable.display()
            )));
        }
        selected.push(SelectedSuiteAgent {
            id: id.clone(),
            runner: registry.runner(id)?,
        });
    }
    Ok(selected)
}

fn suite_runner(
    project: &Project,
    agents: Vec<SelectedSuiteAgent>,
) -> Result<SuiteRunner, CliError> {
    Ok(SuiteRunner::new(
        project.repository.clone(),
        &project.paths.runs_dir,
        &project.paths.groups_dir,
        &project.paths.suite_runs_dir,
        agents,
        runner_settings(&project.config),
        env!("CARGO_PKG_VERSION"),
    )?)
}

fn load_execution(project: &Project, run_id: &str) -> Result<SuiteExecution, CliError> {
    let checkpoint = suite_checkpoint_path(&project.paths.suite_runs_dir, run_id)?;
    let execution = SuiteExecution::load(checkpoint)?;
    if execution.suite_run_id != run_id {
        return Err(patcharena_core::CoreError::Validation(ValidationError::new(
            "suite_run_id",
            format!(
                "requested `{run_id}` but checkpoint declares `{}`",
                execution.suite_run_id
            ),
        ))
        .into());
    }
    Ok(execution)
}

fn print_plan(plan: &patcharena_runner::SuitePlan) {
    println!("suite: {}", plan.definition.id);
    println!("base commit: {}", plan.repository_commit);
    println!("tasks: {}", plan.tasks.len());
    println!("agents: {} ({})", plan.agents.len(), plan.agents.join(", "));
    println!("repeat: {}", plan.repeat);
    println!(
        "repository instructions: {}",
        if plan.instructions_enabled {
            "enabled"
        } else {
            "hidden"
        }
    );
    println!("invocations: {}", plan.invocation_count);
    println!("dry run: no run, group, or suite-run records were created");
}

fn finish(
    project: &Project,
    suite: &SuiteDefinition,
    outcome: SuiteExecutionOutcome,
) -> Result<u8, CliError> {
    let report = load_suite_report(
        outcome.execution,
        suite.description.clone(),
        &project.paths.runs_dir,
        &project.paths.groups_dir,
    )?;
    let directory = suite_run_directory(&project.paths.suite_runs_dir, &report.suite_run_id)?;
    create_contained_directory(&project.paths.repository_root, &directory)?;
    let json = directory.join("report.json");
    let markdown = directory.join("report.md");
    let html = directory.join("report.html");
    write_generated_file(&json, report.to_json()?.as_bytes())?;
    write_generated_file(&markdown, report.to_markdown().as_bytes())?;
    write_generated_file(&html, report.to_html().as_bytes())?;

    println!("TASK\tAGENT\tSTATUS\tSUCCESS\tGROUP / ERROR");
    for cell in &report.cells {
        let success = cell
            .success_rate
            .map_or_else(|| "-".to_owned(), |rate| format!("{:.1}%", rate * 100.0));
        let evidence = cell
            .group_id
            .as_deref()
            .or(cell.error.as_deref())
            .unwrap_or("-");
        println!(
            "{}\t{}\t{}\t{}\t{}",
            cell.task_id,
            cell.agent_id,
            cell_status(cell.status),
            success,
            terminal_text(evidence)
        );
    }
    println!("suite run: {}", report.suite_run_id);
    println!("checkpoint: {}", outcome.checkpoint_path.display());
    println!("JSON: {}", json.display());
    println!("Markdown: {}", markdown.display());
    println!("HTML: {}", html.display());
    exit_for_report(&report)
}

fn render(report: &SuiteReport, format: ReportFormat) -> Result<String, CliError> {
    Ok(match format {
        ReportFormat::Markdown => report.to_markdown(),
        ReportFormat::Json => report.to_json()?,
        ReportFormat::Html => report.to_html(),
    })
}

fn exit_for_report(report: &SuiteReport) -> Result<u8, CliError> {
    Ok(if report.all_benchmarks_succeeded() {
        EXIT_SUCCESS
    } else {
        EXIT_BENCHMARK_FAILED
    })
}

fn format_name(format: ReportFormat) -> &'static str {
    match format {
        ReportFormat::Markdown => "Markdown",
        ReportFormat::Json => "JSON",
        ReportFormat::Html => "HTML",
    }
}

fn cell_status(status: SuiteCellStatus) -> &'static str {
    match status {
        SuiteCellStatus::Pending => "pending",
        SuiteCellStatus::Completed => "completed",
        SuiteCellStatus::Error => "error",
    }
}

fn terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}
