use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use patcharena_core::{SuiteCellStatus, SuiteExecution, SuiteExecutionStatus, SuiteTaskSnapshot};
use serde::{Deserialize, Serialize};

use crate::{
    GroupReport, MetricStats, ReportError, escape_html, escape_markdown, load_selection,
    validate_completed_group,
};

/// One task-and-agent cell in a benchmark suite report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuiteMatrixCell {
    /// Stable task ID.
    pub task_id: String,
    /// Stable agent registry ID.
    pub agent_id: String,
    /// Persisted orchestration status.
    pub status: SuiteCellStatus,
    /// Immutable run-group UUID when the cell completed.
    pub group_id: Option<String>,
    /// Successful observed runs; zero when no group evidence exists.
    pub successful_runs: usize,
    /// Runs requested for this cell.
    pub requested_runs: usize,
    /// Observed success rate, absent when no complete group evidence exists.
    pub success_rate: Option<f64>,
    /// Median observed wall-clock duration, absent without complete evidence.
    pub median_duration_ms: Option<f64>,
    /// Median observed changed-file count, absent without complete evidence.
    pub median_changed_files: Option<f64>,
    /// Median observed added-plus-deleted line count, absent without complete evidence.
    pub median_diff_lines: Option<f64>,
    /// Failed verification commands in completed group evidence.
    pub verification_failures: usize,
    /// Policy violations in completed group evidence.
    pub violation_count: usize,
    /// Bounded orchestration diagnostic for an error cell.
    pub error: Option<String>,
}

/// Evidence coverage and task-macro aggregates for one suite agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuiteAgentSummary {
    /// Stable agent registry ID.
    pub agent_id: String,
    /// Task cells backed by complete run groups.
    pub completed_tasks: usize,
    /// Task cells that ended in orchestration errors.
    pub error_tasks: usize,
    /// Task cells that have not been attempted.
    pub pending_tasks: usize,
    /// Successful invocations across complete task cells.
    pub successful_runs: usize,
    /// Observed invocations across complete task cells.
    pub total_runs: usize,
    /// Arithmetic mean of complete task-cell success rates.
    pub macro_success_rate: Option<f64>,
    /// Failed verification commands across complete task cells.
    pub verification_failures: usize,
    /// Policy violations across complete task cells.
    pub violation_count: usize,
}

/// Auditable task-by-agent report for one persisted suite execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuiteReport {
    /// Suite-report schema version.
    pub schema_version: u32,
    /// Persisted suite-execution UUID.
    pub suite_run_id: String,
    /// Stable suite-definition ID.
    pub suite_id: String,
    /// Optional purpose copied from the current matching suite definition.
    pub description: Option<String>,
    /// Persisted suite execution status.
    pub status: SuiteExecutionStatus,
    /// Full Git commit shared by every cell.
    pub repository_commit: String,
    /// SHA-256 fingerprint of the suite definition.
    pub suite_fingerprint: String,
    /// Whether repository instruction files were visible to every agent.
    pub instructions_enabled: bool,
    /// Requested independent invocations per cell.
    pub repeat: u32,
    /// RFC 3339 suite creation time.
    pub created_at: String,
    /// RFC 3339 terminal time, absent for a running suite.
    pub completed_at: Option<String>,
    /// Stable task order.
    pub tasks: Vec<String>,
    /// Stable agent order and aggregates.
    pub agents: Vec<SuiteAgentSummary>,
    /// Stable task-major, agent-minor cell matrix.
    pub cells: Vec<SuiteMatrixCell>,
}

impl SuiteReport {
    /// Build a report only when completed group evidence exactly matches the execution record.
    pub fn new(execution: SuiteExecution, groups: Vec<GroupReport>) -> Result<Self, ReportError> {
        execution.validate()?;
        let requested_runs = usize::try_from(execution.repeat).map_err(|_| {
            ReportError::Incompatible("suite repeat count is not representable".to_owned())
        })?;
        let mut groups_by_id = HashMap::with_capacity(groups.len());
        for group in groups {
            let group_id = group.group_id.clone();
            if groups_by_id.insert(group_id.clone(), group).is_some() {
                return Err(ReportError::Incompatible(format!(
                    "group `{group_id}` was supplied more than once"
                )));
            }
        }

        let mut cells = Vec::with_capacity(execution.cells.len());
        for cell in &execution.cells {
            let snapshot = execution
                .tasks
                .iter()
                .find(|task| task.task_id == cell.task_id)
                .ok_or_else(|| {
                    ReportError::Incompatible(format!(
                        "suite cell references unknown task `{}`",
                        cell.task_id
                    ))
                })?;
            let matrix_cell = match cell.status {
                SuiteCellStatus::Completed => {
                    let group_id = cell.group_id.as_deref().ok_or_else(|| {
                        ReportError::Incompatible(format!(
                            "completed cell `{}` / `{}` has no group ID",
                            cell.task_id, cell.agent_id
                        ))
                    })?;
                    let group = groups_by_id.remove(group_id).ok_or_else(|| {
                        ReportError::NotFound(format!(
                            "group `{group_id}` for suite cell `{}` / `{}`",
                            cell.task_id, cell.agent_id
                        ))
                    })?;
                    validate_cell_group(
                        &group,
                        snapshot,
                        &cell.agent_id,
                        execution.instructions_enabled,
                        requested_runs,
                    )?;
                    completed_matrix_cell(group)
                }
                SuiteCellStatus::Pending | SuiteCellStatus::Error => SuiteMatrixCell {
                    task_id: cell.task_id.to_string(),
                    agent_id: cell.agent_id.clone(),
                    status: cell.status,
                    group_id: None,
                    successful_runs: 0,
                    requested_runs,
                    success_rate: None,
                    median_duration_ms: None,
                    median_changed_files: None,
                    median_diff_lines: None,
                    verification_failures: 0,
                    violation_count: 0,
                    error: cell.error.clone(),
                },
            };
            cells.push(matrix_cell);
        }

        if let Some(group_id) = groups_by_id.keys().min() {
            return Err(ReportError::Incompatible(format!(
                "group `{group_id}` is not referenced by the suite execution"
            )));
        }

        let agents = execution
            .agents
            .iter()
            .map(|agent_id| summarize_agent(agent_id, &cells))
            .collect();
        Ok(Self {
            schema_version: 1,
            suite_run_id: execution.suite_run_id,
            suite_id: execution.suite_id.into_inner(),
            description: None,
            status: execution.status,
            repository_commit: execution.repository_commit,
            suite_fingerprint: execution.suite_fingerprint,
            instructions_enabled: execution.instructions_enabled,
            repeat: execution.repeat,
            created_at: execution.created_at.to_rfc3339(),
            completed_at: execution.completed_at.map(|value| value.to_rfc3339()),
            tasks: execution
                .tasks
                .into_iter()
                .map(|task| task.task_id.into_inner())
                .collect(),
            agents,
            cells,
        })
    }

    /// Find an agent summary by its stable registry ID.
    #[must_use]
    pub fn agent(&self, agent_id: &str) -> Option<&SuiteAgentSummary> {
        self.agents.iter().find(|agent| agent.agent_id == agent_id)
    }

    /// Return true only for a complete suite whose every observed run passed without violations.
    #[must_use]
    pub fn all_benchmarks_succeeded(&self) -> bool {
        self.status == SuiteExecutionStatus::Completed
            && !self.cells.is_empty()
            && self.cells.iter().all(|cell| {
                cell.status == SuiteCellStatus::Completed
                    && cell.successful_runs == cell.requested_runs
                    && cell.success_rate.is_some_and(|rate| rate == 1.0)
                    && cell.verification_failures == 0
                    && cell.violation_count == 0
            })
    }

    /// Render stable pretty JSON with a trailing newline.
    pub fn to_json(&self) -> Result<String, ReportError> {
        Ok(format!("{}\n", serde_json::to_string_pretty(self)?))
    }

    /// Render a provenance-first Markdown matrix and evidence table.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut output = String::from("# PatchArena suite report\n\n");
        if let Some(description) = &self.description {
            output.push_str(&escape_markdown(description));
            output.push_str("\n\n");
        }
        output.push_str("## Provenance\n\n");
        output.push_str(&format!(
            "- Suite: `{}`\n- Suite run: `{}`\n- Status: {}\n- Repository commit: `{}`\n- Suite fingerprint: `{}`\n- Repetitions per cell: {}\n- Repository instructions: {}\n- Created: `{}`\n",
            escape_markdown(&self.suite_id),
            escape_markdown(&self.suite_run_id),
            execution_status_label(self.status),
            escape_markdown(&self.repository_commit),
            escape_markdown(&self.suite_fingerprint),
            self.repeat,
            if self.instructions_enabled { "enabled" } else { "hidden" },
            escape_markdown(&self.created_at),
        ));
        if let Some(completed_at) = &self.completed_at {
            output.push_str(&format!(
                "- Completed: `{}`\n",
                escape_markdown(completed_at)
            ));
        }

        let (completed, errors, pending) = coverage(&self.cells);
        output.push_str(&format!(
            "\n## Coverage\n\n- Complete groups: {completed}/{}\n- Orchestration errors: {errors}\n- Pending cells: {pending}\n\n",
            self.cells.len()
        ));
        output.push_str("## Agent summaries\n\n");
        output.push_str("| Agent | Complete tasks | Errors | Pending | Successful runs | Observed runs | Task-macro success | Verification failures | Violations |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|\n");
        for agent in &self.agents {
            output.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                escape_markdown(&agent.agent_id),
                agent.completed_tasks,
                agent.error_tasks,
                agent.pending_tasks,
                agent.successful_runs,
                agent.total_runs,
                optional_percent(agent.macro_success_rate),
                agent.verification_failures,
                agent.violation_count,
            ));
        }

        output.push_str("\n## Task-by-agent matrix\n\n| Task |");
        for agent in &self.agents {
            output.push_str(&format!(" {} |", escape_markdown(&agent.agent_id)));
        }
        output.push_str("\n|---|");
        for _ in &self.agents {
            output.push_str("---:|");
        }
        output.push('\n');
        for (task_index, task) in self.tasks.iter().enumerate() {
            output.push_str(&format!("| {} |", escape_markdown(task)));
            for agent_index in 0..self.agents.len() {
                let index = task_index * self.agents.len() + agent_index;
                output.push_str(&format!(" {} |", markdown_cell(&self.cells[index])));
            }
            output.push('\n');
        }

        output.push_str("\n## Cell evidence\n\n| Task | Agent | Status | Runs | Success | Median duration (ms) | Median files | Median diff lines | Verify failures | Violations | Group / error |\n|---|---|---|---:|---:|---:|---:|---:|---:|---:|---|\n");
        for cell in &self.cells {
            let group_or_error = cell.group_id.as_ref().map_or_else(
                || {
                    cell.error
                        .as_ref()
                        .map_or_else(|| "—".to_owned(), |error| escape_markdown(error))
                },
                |group_id| format!("`{}`", escape_markdown(group_id)),
            );
            output.push_str(&format!(
                "| {} | {} | {} | {}/{} | {} | {} | {} | {} | {} | {} | {} |\n",
                escape_markdown(&cell.task_id),
                escape_markdown(&cell.agent_id),
                cell_status_label(cell.status),
                cell.successful_runs,
                cell.requested_runs,
                optional_percent(cell.success_rate),
                optional_metric(cell.median_duration_ms),
                optional_metric(cell.median_changed_files),
                optional_metric(cell.median_diff_lines),
                cell.verification_failures,
                cell.violation_count,
                group_or_error,
            ));
        }
        output
    }

    /// Render a standalone HTML document with embedded CSS and escaped labels.
    #[must_use]
    pub fn to_html(&self) -> String {
        let description = self.description.as_ref().map_or_else(String::new, |value| {
            format!("<p class=\"description\">{}</p>", escape_html(value))
        });
        let completed_at = self
            .completed_at
            .as_ref()
            .map_or_else(|| "running".to_owned(), |value| escape_html(value));
        let (completed, errors, pending) = coverage(&self.cells);

        let mut summary_rows = String::new();
        for agent in &self.agents {
            summary_rows.push_str(&format!(
                "<tr><th>{}</th><td>{}</td><td>{}</td><td>{}</td><td>{}/{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape_html(&agent.agent_id),
                agent.completed_tasks,
                agent.error_tasks,
                agent.pending_tasks,
                agent.successful_runs,
                agent.total_runs,
                optional_percent(agent.macro_success_rate),
                agent.verification_failures,
                agent.violation_count,
            ));
        }

        let mut matrix_head = String::from("<tr><th>Task</th>");
        for agent in &self.agents {
            matrix_head.push_str(&format!("<th>{}</th>", escape_html(&agent.agent_id)));
        }
        matrix_head.push_str("</tr>");
        let mut matrix_rows = String::new();
        for (task_index, task) in self.tasks.iter().enumerate() {
            matrix_rows.push_str(&format!("<tr><th>{}</th>", escape_html(task)));
            for agent_index in 0..self.agents.len() {
                let index = task_index * self.agents.len() + agent_index;
                matrix_rows.push_str(&html_matrix_cell(&self.cells[index]));
            }
            matrix_rows.push_str("</tr>");
        }

        let mut detail_rows = String::new();
        for cell in &self.cells {
            let evidence = cell.group_id.as_ref().map_or_else(
                || {
                    cell.error
                        .as_ref()
                        .map_or_else(|| "—".to_owned(), |error| escape_html(error))
                },
                |group_id| format!("<code>{}</code>", escape_html(group_id)),
            );
            detail_rows.push_str(&format!(
                "<tr><th>{}</th><td>{}</td><td>{}</td><td>{}/{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"evidence\">{}</td></tr>",
                escape_html(&cell.task_id),
                escape_html(&cell.agent_id),
                cell_status_label(cell.status),
                cell.successful_runs,
                cell.requested_runs,
                optional_percent(cell.success_rate),
                optional_metric(cell.median_duration_ms),
                optional_metric(cell.median_changed_files),
                optional_metric(cell.median_diff_lines),
                cell.verification_failures,
                cell.violation_count,
                evidence,
            ));
        }

        format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>PatchArena suite report</title><style>{SUITE_REPORT_CSS}</style></head><body><main><header class=\"hero\"><p class=\"eyebrow\">Reproducible coding-agent benchmark</p><h1>{}</h1>{description}<div class=\"status\">{}</div></header><section><h2>Provenance</h2><dl><div><dt>Suite run</dt><dd><code>{}</code></dd></div><div><dt>Repository commit</dt><dd><code>{}</code></dd></div><div><dt>Suite fingerprint</dt><dd><code>{}</code></dd></div><div><dt>Repetitions</dt><dd>{}</dd></div><div><dt>Instructions</dt><dd>{}</dd></div><div><dt>Created / completed</dt><dd>{} / {completed_at}</dd></div></dl></section><section><h2>Coverage</h2><div class=\"cards\"><article><strong>{completed}/{}</strong><span>complete groups</span></article><article><strong>{errors}</strong><span>orchestration errors</span></article><article><strong>{pending}</strong><span>pending cells</span></article></div></section><section><h2>Agent summaries</h2><div class=\"scroll\"><table><thead><tr><th>Agent</th><th>Complete</th><th>Errors</th><th>Pending</th><th>Runs</th><th>Task-macro success</th><th>Verify failures</th><th>Violations</th></tr></thead><tbody>{summary_rows}</tbody></table></div></section><section><h2>Task-by-agent matrix</h2><div class=\"scroll\"><table class=\"matrix\"><thead>{matrix_head}</thead><tbody>{matrix_rows}</tbody></table></div></section><section><h2>Cell evidence</h2><div class=\"scroll\"><table><thead><tr><th>Task</th><th>Agent</th><th>Status</th><th>Runs</th><th>Success</th><th>Duration ms</th><th>Files</th><th>Diff lines</th><th>Verify failures</th><th>Violations</th><th>Group / error</th></tr></thead><tbody>{detail_rows}</tbody></table></div></section></main></body></html>\n",
            escape_html(&self.suite_id),
            execution_status_label(self.status),
            escape_html(&self.suite_run_id),
            escape_html(&self.repository_commit),
            escape_html(&self.suite_fingerprint),
            self.repeat,
            if self.instructions_enabled {
                "enabled"
            } else {
                "hidden"
            },
            escape_html(&self.created_at),
            self.cells.len(),
        )
    }
}

/// Load only the completed groups referenced by a persisted suite execution and build a report.
pub fn load_suite_report(
    execution: SuiteExecution,
    description: Option<String>,
    runs_directory: impl AsRef<Path>,
    groups_directory: impl AsRef<Path>,
) -> Result<SuiteReport, ReportError> {
    execution.validate()?;
    let mut groups = Vec::new();
    for cell in &execution.cells {
        if cell.status == SuiteCellStatus::Completed {
            let group_id = cell.group_id.as_deref().ok_or_else(|| {
                ReportError::Incompatible(format!(
                    "completed cell `{}` / `{}` has no group ID",
                    cell.task_id, cell.agent_id
                ))
            })?;
            groups.push(load_selection(
                runs_directory.as_ref(),
                groups_directory.as_ref(),
                group_id,
            )?);
        }
    }
    let mut report = SuiteReport::new(execution, groups)?;
    report.description = description;
    Ok(report)
}

fn validate_cell_group(
    group: &GroupReport,
    task: &SuiteTaskSnapshot,
    agent_id: &str,
    instructions_enabled: bool,
    requested_runs: usize,
) -> Result<(), ReportError> {
    validate_completed_group("suite cell", group)?;
    if group.task_id != task.task_id.as_str() {
        return Err(ReportError::Incompatible(format!(
            "group `{}` records task `{}` instead of `{}`",
            group.group_id, group.task_id, task.task_id
        )));
    }
    if group.agent != agent_id {
        return Err(ReportError::Incompatible(format!(
            "group `{}` records agent `{}` instead of `{agent_id}`",
            group.group_id, group.agent
        )));
    }
    if group.instructions_enabled != instructions_enabled {
        return Err(ReportError::Incompatible(format!(
            "group `{}` has a different repository-instructions policy",
            group.group_id
        )));
    }
    if group.benchmark_identity.as_ref() != Some(&task.benchmark_identity) {
        return Err(ReportError::Incompatible(format!(
            "group `{}` has a different or missing benchmark identity",
            group.group_id
        )));
    }
    if group.run_count != requested_runs || group.requested_run_count != Some(requested_runs) {
        return Err(ReportError::Incompatible(format!(
            "group `{}` does not contain the suite's {requested_runs} requested runs",
            group.group_id
        )));
    }
    validate_group_aggregate(group)
}

fn validate_group_aggregate(group: &GroupReport) -> Result<(), ReportError> {
    if group.run_count != group.runs.len() {
        return Err(ReportError::Incompatible(format!(
            "group `{}` run count does not match its evidence",
            group.group_id
        )));
    }
    let mut run_ids = HashSet::with_capacity(group.runs.len());
    if let Some(run) = group
        .runs
        .iter()
        .find(|run| !run_ids.insert(run.run_id.as_str()))
    {
        return Err(ReportError::Incompatible(format!(
            "group `{}` repeats run `{}`",
            group.group_id, run.run_id
        )));
    }
    let successful_runs = group.runs.iter().filter(|run| run.success).count();
    let expected_success_rate = successful_runs as f64 / group.run_count as f64;
    let duration_ms = MetricStats::from_values(group.runs.iter().map(|run| run.duration_ms));
    let changed_files = MetricStats::from_values(group.runs.iter().map(|run| run.changed_files));
    let diff_lines = MetricStats::from_values(group.runs.iter().map(|run| run.diff_lines()));
    let verification_failures = group
        .runs
        .iter()
        .flat_map(|run| &run.verification)
        .filter(|verification| !verification.success)
        .count();
    let violation_count: usize = group.runs.iter().map(|run| run.violations.len()).sum();
    if group.success_rate != expected_success_rate
        || group.duration_ms != duration_ms
        || group.changed_files != changed_files
        || group.diff_lines != diff_lines
        || group.verification_failures != verification_failures
        || group.violation_count != violation_count
    {
        return Err(ReportError::Incompatible(format!(
            "group `{}` aggregate does not match its run evidence",
            group.group_id
        )));
    }
    Ok(())
}

fn completed_matrix_cell(group: GroupReport) -> SuiteMatrixCell {
    SuiteMatrixCell {
        task_id: group.task_id,
        agent_id: group.agent,
        status: SuiteCellStatus::Completed,
        group_id: Some(group.group_id),
        successful_runs: group.runs.iter().filter(|run| run.success).count(),
        requested_runs: group.run_count,
        success_rate: Some(group.success_rate),
        median_duration_ms: Some(group.duration_ms.median),
        median_changed_files: Some(group.changed_files.median),
        median_diff_lines: Some(group.diff_lines.median),
        verification_failures: group.verification_failures,
        violation_count: group.violation_count,
        error: None,
    }
}

fn summarize_agent(agent_id: &str, cells: &[SuiteMatrixCell]) -> SuiteAgentSummary {
    let agent_cells = cells
        .iter()
        .filter(|cell| cell.agent_id == agent_id)
        .collect::<Vec<_>>();
    let completed = agent_cells
        .iter()
        .copied()
        .filter(|cell| cell.status == SuiteCellStatus::Completed)
        .collect::<Vec<_>>();
    let completed_tasks = completed.len();
    let macro_success_rate = if completed_tasks == 0 {
        None
    } else {
        Some(
            completed
                .iter()
                .filter_map(|cell| cell.success_rate)
                .sum::<f64>()
                / completed_tasks as f64,
        )
    };
    SuiteAgentSummary {
        agent_id: agent_id.to_owned(),
        completed_tasks,
        error_tasks: agent_cells
            .iter()
            .filter(|cell| cell.status == SuiteCellStatus::Error)
            .count(),
        pending_tasks: agent_cells
            .iter()
            .filter(|cell| cell.status == SuiteCellStatus::Pending)
            .count(),
        successful_runs: completed.iter().map(|cell| cell.successful_runs).sum(),
        total_runs: completed.iter().map(|cell| cell.requested_runs).sum(),
        macro_success_rate,
        verification_failures: completed
            .iter()
            .map(|cell| cell.verification_failures)
            .sum(),
        violation_count: completed.iter().map(|cell| cell.violation_count).sum(),
    }
}

fn coverage(cells: &[SuiteMatrixCell]) -> (usize, usize, usize) {
    let completed = cells
        .iter()
        .filter(|cell| cell.status == SuiteCellStatus::Completed)
        .count();
    let errors = cells
        .iter()
        .filter(|cell| cell.status == SuiteCellStatus::Error)
        .count();
    let pending = cells.len().saturating_sub(completed + errors);
    (completed, errors, pending)
}

fn optional_percent(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), |rate| format!("{:.1}%", rate * 100.0))
}

fn optional_metric(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), |metric| format!("{metric:.1}"))
}

fn markdown_cell(cell: &SuiteMatrixCell) -> String {
    match cell.status {
        SuiteCellStatus::Completed => format!(
            "{} ({}/{})",
            optional_percent(cell.success_rate),
            cell.successful_runs,
            cell.requested_runs
        ),
        SuiteCellStatus::Error => format!(
            "error: {}",
            cell.error
                .as_ref()
                .map_or("unknown error".to_owned(), |value| escape_markdown(value))
        ),
        SuiteCellStatus::Pending => "pending".to_owned(),
    }
}

fn html_matrix_cell(cell: &SuiteMatrixCell) -> String {
    let (class, value) = match cell.status {
        SuiteCellStatus::Completed => {
            let class = if cell.success_rate == Some(1.0) {
                "pass"
            } else {
                "attention"
            };
            (
                class,
                format!(
                    "{}<small>{}/{}</small>",
                    optional_percent(cell.success_rate),
                    cell.successful_runs,
                    cell.requested_runs
                ),
            )
        }
        SuiteCellStatus::Error => (
            "error",
            format!(
                "error<small>{}</small>",
                cell.error
                    .as_ref()
                    .map_or("unknown error".to_owned(), |value| escape_html(value))
            ),
        ),
        SuiteCellStatus::Pending => ("pending", "pending".to_owned()),
    };
    format!("<td><span class=\"cell {class}\">{value}</span></td>")
}

fn execution_status_label(status: SuiteExecutionStatus) -> &'static str {
    match status {
        SuiteExecutionStatus::LegacyUnknown => "legacy_unknown",
        SuiteExecutionStatus::Running => "running",
        SuiteExecutionStatus::Completed => "completed",
        SuiteExecutionStatus::CompletedWithErrors => "completed_with_errors",
        SuiteExecutionStatus::Aborted => "aborted",
    }
}

fn cell_status_label(status: SuiteCellStatus) -> &'static str {
    match status {
        SuiteCellStatus::Pending => "pending",
        SuiteCellStatus::Completed => "completed",
        SuiteCellStatus::Error => "error",
    }
}

const SUITE_REPORT_CSS: &str = r#"
:root{color-scheme:light;--ink:#13201b;--muted:#617068;--paper:#f4f1e8;--panel:#fffdf8;--line:#d6d2c7;--green:#176b4d;--green-bg:#e5f3eb;--amber:#8a4b08;--amber-bg:#fff0d7;--red:#9b2c2c;--red-bg:#fee8e7;--gray:#626b67;--gray-bg:#ebeeec}*{box-sizing:border-box}body{margin:0;background:var(--paper);color:var(--ink);font:15px/1.55 ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}main{width:min(1180px,calc(100% - 32px));margin:48px auto 80px}.hero{padding:36px;border:1px solid var(--line);border-radius:20px;background:linear-gradient(135deg,#fffdf8,#e8f2eb);box-shadow:0 18px 44px #24382d16}.eyebrow{margin:0 0 6px;color:var(--green);font-size:12px;font-weight:800;letter-spacing:.12em;text-transform:uppercase}h1{margin:0;font:700 clamp(32px,6vw,64px)/1.02 ui-serif,Georgia,serif}h2{margin:0 0 18px;font:700 25px/1.2 ui-serif,Georgia,serif}.description{max-width:70ch;color:var(--muted)}.status{display:inline-block;margin-top:12px;padding:5px 10px;border-radius:999px;background:#13201b;color:white;font-weight:700}section{margin-top:24px;padding:26px;border:1px solid var(--line);border-radius:16px;background:var(--panel)}dl{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:12px;margin:0}dl div{padding:14px;border-radius:10px;background:#f5f4ef}dt{color:var(--muted);font-size:12px;font-weight:700;text-transform:uppercase}dd{margin:5px 0 0;overflow-wrap:anywhere}.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:12px}.cards article{display:flex;flex-direction:column;padding:18px;border-radius:12px;background:#f5f4ef}.cards strong{font-size:24px}.cards span{color:var(--muted)}.scroll{overflow-x:auto}table{width:100%;border-collapse:collapse;white-space:nowrap}th,td{padding:11px 12px;border-bottom:1px solid var(--line);text-align:right;vertical-align:top}th:first-child,td:first-child,td.evidence{text-align:left}thead th{color:var(--muted);font-size:12px;text-transform:uppercase}.matrix th,.matrix td{text-align:center}.matrix th:first-child{text-align:left}.cell{display:inline-flex;min-width:104px;flex-direction:column;gap:2px;padding:7px 9px;border-radius:9px;font-weight:750}.cell small{font-weight:500;white-space:normal}.pass{color:var(--green);background:var(--green-bg)}.attention{color:var(--amber);background:var(--amber-bg)}.error{color:var(--red);background:var(--red-bg)}.pending{color:var(--gray);background:var(--gray-bg)}code{font-family:ui-monospace,SFMono-Regular,Consolas,monospace;font-size:.9em;overflow-wrap:anywhere}@media(max-width:640px){main{margin-top:16px}.hero,section{padding:20px}}
"#;

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{TimeZone, Utc};
    use patcharena_core::{
        ArtifactPaths, BenchmarkIdentity, CommandOutcome, RunGroup, RunResult, SuiteCellStatus,
        SuiteExecution, SuiteId, SuiteTaskSnapshot, TaskId,
    };
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::{SuiteReport, load_suite_report};
    use crate::{GroupReport, RunDetail, VerificationDetail};

    fn run(id: &str, success: bool, duration_ms: u64) -> RunDetail {
        RunDetail {
            run_id: id.to_owned(),
            success,
            duration_ms,
            changed_files: 2,
            added_lines: 5,
            deleted_lines: 3,
            verification: vec![VerificationDetail {
                command: "cargo test".to_owned(),
                success,
                exit_code: Some(i32::from(!success)),
                duration_ms: 10,
            }],
            violations: if success {
                Vec::new()
            } else {
                vec!["verification failed".to_owned()]
            },
            errors: Vec::new(),
            artifact_directory: id.to_owned(),
        }
    }

    fn identity(task_index: usize) -> BenchmarkIdentity {
        BenchmarkIdentity {
            repository_commit: "a".repeat(40),
            task_fingerprint: format!("{task_index:064x}"),
        }
    }

    fn fixture(
        tasks: &[&str],
        agents: &[&str],
        rates: &[f64],
    ) -> (SuiteExecution, Vec<GroupReport>) {
        assert_eq!(rates.len(), tasks.len() * agents.len());
        let snapshots = tasks
            .iter()
            .enumerate()
            .map(|(index, task)| {
                SuiteTaskSnapshot::new(TaskId::new(*task).unwrap(), identity(index + 1)).unwrap()
            })
            .collect();
        let now = Utc.with_ymd_and_hms(2026, 7, 14, 0, 0, 0).unwrap();
        let mut execution = SuiteExecution::new(
            "0.3.0",
            SuiteId::new("quality").unwrap(),
            "b".repeat(64),
            "a".repeat(40),
            snapshots,
            agents.iter().map(ToString::to_string).collect(),
            2,
            true,
            now,
        )
        .unwrap();
        let mut groups = Vec::new();
        for (index, ((task, agent), rate)) in tasks
            .iter()
            .flat_map(|task| agents.iter().map(move |agent| (*task, *agent)))
            .zip(rates)
            .enumerate()
        {
            let group_id = Uuid::from_u128(index as u128 + 1).to_string();
            let successes = (*rate * 2.0).round() as usize;
            let mut group = GroupReport::from_details(
                group_id.clone(),
                task,
                agent,
                true,
                (0..2)
                    .map(|run_index| {
                        run(
                            &format!("{index}-{run_index}"),
                            run_index < successes,
                            10 + run_index as u64,
                        )
                    })
                    .collect(),
            )
            .unwrap();
            let task_index = tasks.iter().position(|value| value == &task).unwrap();
            group.benchmark_identity = Some(identity(task_index + 1));
            execution.complete_cell(task, agent, group_id, now).unwrap();
            groups.push(group);
        }
        execution.mark_finished(now).unwrap();
        (execution, groups)
    }

    #[test]
    fn suite_report_builds_matrix_and_task_macro_average() {
        let (execution, groups) =
            fixture(&["one", "two"], &["alpha", "beta"], &[1.0, 0.5, 0.0, 1.0]);
        let report = SuiteReport::new(execution, groups).expect("suite report");

        assert_eq!(report.cells.len(), 4);
        assert_eq!(report.agent("alpha").unwrap().macro_success_rate, Some(0.5));
        assert_eq!(report.agent("beta").unwrap().macro_success_rate, Some(0.75));
        assert!(!report.to_markdown().contains("winner"));
    }

    #[test]
    fn suite_report_rejects_wrong_group_identity() {
        let (execution, mut groups) = fixture(&["one"], &["alpha"], &[1.0]);
        groups[0].agent = "different".to_owned();

        assert!(SuiteReport::new(execution, groups).is_err());
    }

    #[test]
    fn suite_report_rejects_unreferenced_and_duplicate_groups() {
        let (execution, groups) = fixture(&["one"], &["alpha"], &[1.0]);
        let mut extra = groups[0].clone();
        extra.group_id = Uuid::from_u128(99).to_string();
        assert!(SuiteReport::new(execution.clone(), vec![groups[0].clone(), extra]).is_err());
        assert!(SuiteReport::new(execution, vec![groups[0].clone(), groups[0].clone()]).is_err());
    }

    #[test]
    fn suite_report_keeps_error_metrics_missing() {
        let snapshots =
            vec![SuiteTaskSnapshot::new(TaskId::new("one").unwrap(), identity(1)).unwrap()];
        let now = Utc.with_ymd_and_hms(2026, 7, 14, 0, 0, 0).unwrap();
        let mut execution = SuiteExecution::new(
            "0.3.0",
            SuiteId::new("quality").unwrap(),
            "b".repeat(64),
            "a".repeat(40),
            snapshots,
            vec!["alpha".to_owned()],
            2,
            true,
            now,
        )
        .unwrap();
        execution
            .error_cell("one", "alpha", "agent did not start", now)
            .unwrap();
        execution.mark_finished(now).unwrap();

        let report = SuiteReport::new(execution, Vec::new()).unwrap();
        assert_eq!(report.cells[0].status, SuiteCellStatus::Error);
        assert_eq!(report.cells[0].success_rate, None);
        assert_eq!(report.cells[0].median_duration_ms, None);
        assert_eq!(report.agent("alpha").unwrap().error_tasks, 1);
        assert!(!report.all_benchmarks_succeeded());
    }

    #[test]
    fn suite_html_escapes_untrusted_labels() {
        let (execution, groups) = fixture(&["one"], &["alpha"], &[1.0]);
        let mut report = SuiteReport::new(execution, groups).unwrap();
        report.description = Some("<script>alert(1)</script>".to_owned());
        let html = report.to_html();

        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn suite_report_loads_only_persisted_group_evidence() {
        let directory = tempdir().unwrap();
        let runs = directory.path().join("runs");
        let groups = directory.path().join("groups");
        fs::create_dir(&runs).unwrap();
        fs::create_dir(&groups).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 14, 0, 0, 0).unwrap();
        let task_id = TaskId::new("one").unwrap();
        let mut execution = SuiteExecution::new(
            "0.3.0",
            SuiteId::new("quality").unwrap(),
            "b".repeat(64),
            "a".repeat(40),
            vec![SuiteTaskSnapshot::new(task_id.clone(), identity(1)).unwrap()],
            vec!["alpha".to_owned()],
            1,
            true,
            now,
        )
        .unwrap();
        let mut group = RunGroup::new(task_id.clone(), "alpha", now, 1).unwrap();
        group.benchmark_identity = Some(identity(1));
        let run_id = Uuid::from_u128(100).to_string();
        group.push_run_id(run_id.clone()).unwrap();
        group.mark_completed().unwrap();
        group
            .save_new(groups.join(format!("{}.json", group.group_id)))
            .unwrap();

        let result = RunResult {
            schema_version: 1,
            patcharena_version: Some("0.3.0".to_owned()),
            run_id: run_id.clone(),
            group_id: Some(group.group_id.clone()),
            task_id,
            agent: "alpha".to_owned(),
            agent_metadata: None,
            execution_metadata: None,
            instructions_enabled: true,
            benchmark_identity: Some(identity(1)),
            started_at: now,
            finished_at: now,
            duration_ms: 12,
            success: true,
            exit_code: Some(0),
            changed_files: 1,
            added_lines: 2,
            deleted_lines: 1,
            setup: Vec::new(),
            agent_outcome: None,
            verification: vec![CommandOutcome::exited("cargo test", 0, 10)],
            audit: Vec::new(),
            violations: Vec::new(),
            artifacts: ArtifactPaths::default(),
            error: None,
        };
        let run_directory = runs.join(&run_id);
        fs::create_dir(&run_directory).unwrap();
        result.save_new(run_directory.join("result.json")).unwrap();
        execution
            .complete_cell("one", "alpha", group.group_id.clone(), now)
            .unwrap();
        execution.mark_finished(now).unwrap();

        let report = load_suite_report(
            execution,
            Some("Persisted evidence".to_owned()),
            &runs,
            &groups,
        )
        .unwrap();
        assert!(report.all_benchmarks_succeeded());
        assert_eq!(report.description.as_deref(), Some("Persisted evidence"));
        assert_eq!(
            report.cells[0].group_id.as_deref(),
            Some(group.group_id.as_str())
        );
    }
}
