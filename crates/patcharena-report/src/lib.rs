//! Aggregation, comparison, and self-contained report rendering for PatchArena.

#![forbid(unsafe_code)]

mod suite;

pub use suite::{SuiteAgentSummary, SuiteMatrixCell, SuiteReport, load_suite_report};

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use patcharena_core::RunGroupStatus;

/// Summary statistics for an integer-valued benchmark metric.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricStats {
    /// Lowest observation.
    pub min: u64,
    /// Median observation, including half units for even sample counts.
    pub median: f64,
    /// Highest observation.
    pub max: u64,
    /// Arithmetic mean.
    pub mean: f64,
    /// Population standard deviation, used as the variability signal.
    pub standard_deviation: f64,
}

impl MetricStats {
    fn from_values(values: impl IntoIterator<Item = u64>) -> Self {
        let mut values = values.into_iter().collect::<Vec<_>>();
        values.sort_unstable();
        if values.is_empty() {
            return Self {
                min: 0,
                median: 0.0,
                max: 0,
                mean: 0.0,
                standard_deviation: 0.0,
            };
        }
        let count = values.len();
        let mean = values.iter().map(|value| *value as f64).sum::<f64>() / count as f64;
        let median = if count % 2 == 0 {
            (values[count / 2 - 1] as f64 + values[count / 2] as f64) / 2.0
        } else {
            values[count / 2] as f64
        };
        let variance = values
            .iter()
            .map(|value| {
                let distance = *value as f64 - mean;
                distance * distance
            })
            .sum::<f64>()
            / count as f64;
        Self {
            min: values[0],
            median,
            max: values[count - 1],
            mean,
            standard_deviation: variance.sqrt(),
        }
    }
}

/// One verification entry shown in a report.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerificationDetail {
    /// Auditable command rendering.
    pub command: String,
    /// Whether it exited successfully before its deadline.
    pub success: bool,
    /// Exit status when one was available.
    pub exit_code: Option<i32>,
    /// Wall-clock duration.
    pub duration_ms: u64,
}

/// Per-invocation data used by console, Markdown, JSON, and HTML reports.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunDetail {
    /// Run UUID.
    pub run_id: String,
    /// Overall run outcome.
    pub success: bool,
    /// Total wall-clock duration.
    pub duration_ms: u64,
    /// Changed files reported by Git.
    pub changed_files: u64,
    /// Added text lines.
    pub added_lines: u64,
    /// Deleted text lines.
    pub deleted_lines: u64,
    /// Verification commands and outcomes.
    pub verification: Vec<VerificationDetail>,
    /// Policy violation descriptions.
    pub violations: Vec<String>,
    /// Concise errors or failure reasons.
    pub errors: Vec<String>,
    /// Result artifact directory relative to `.patcharena/runs`.
    pub artifact_directory: String,
}

impl RunDetail {
    fn diff_lines(&self) -> u64 {
        self.added_lines.saturating_add(self.deleted_lines)
    }
}

/// Aggregated results for a single run group.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupReport {
    /// Run-group UUID, or a stable synthesized key for legacy individual results.
    pub group_id: String,
    /// Task identifier.
    pub task_id: String,
    /// Agent implementation name.
    pub agent: String,
    /// Whether repository instructions were available to the agent.
    pub instructions_enabled: bool,
    /// Repository revision and task/policy fingerprint, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_identity: Option<patcharena_core::BenchmarkIdentity>,
    /// Number of runs requested when the group was created, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_run_count: Option<usize>,
    /// Persistent lifecycle state of this run group.
    #[serde(default = "completed_group_status")]
    pub status: RunGroupStatus,
    /// Number of invocations.
    pub run_count: usize,
    /// Successful invocations divided by invocation count.
    pub success_rate: f64,
    /// Duration distribution in milliseconds.
    pub duration_ms: MetricStats,
    /// Changed-file distribution.
    pub changed_files: MetricStats,
    /// Added-plus-deleted-line distribution.
    pub diff_lines: MetricStats,
    /// Number of failed verification commands across runs.
    pub verification_failures: usize,
    /// Number of recorded policy violations across runs.
    pub violation_count: usize,
    /// Individual invocation details.
    pub runs: Vec<RunDetail>,
}

impl GroupReport {
    /// Aggregate compatible run details into one group report.
    pub fn from_details(
        group_id: impl Into<String>,
        task_id: impl Into<String>,
        agent: impl Into<String>,
        instructions_enabled: bool,
        runs: Vec<RunDetail>,
    ) -> Result<Self, ReportError> {
        let group_id = group_id.into();
        if runs.is_empty() {
            return Err(ReportError::NotFound(group_id));
        }
        let run_count = runs.len();
        Ok(Self::aggregate_details(
            group_id,
            task_id.into(),
            agent.into(),
            instructions_enabled,
            Some(run_count),
            RunGroupStatus::Completed,
            runs,
        ))
    }

    fn aggregate_details(
        group_id: String,
        task_id: String,
        agent: String,
        instructions_enabled: bool,
        requested_run_count: Option<usize>,
        status: RunGroupStatus,
        mut runs: Vec<RunDetail>,
    ) -> Self {
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        let run_count = runs.len();
        let successes = runs.iter().filter(|run| run.success).count();
        let verification_failures = runs
            .iter()
            .flat_map(|run| &run.verification)
            .filter(|verification| !verification.success)
            .count();
        let violation_count = runs.iter().map(|run| run.violations.len()).sum();
        Self {
            group_id,
            task_id,
            agent,
            instructions_enabled,
            benchmark_identity: None,
            requested_run_count,
            status,
            run_count,
            success_rate: if run_count == 0 {
                0.0
            } else {
                successes as f64 / run_count as f64
            },
            duration_ms: MetricStats::from_values(runs.iter().map(|run| run.duration_ms)),
            changed_files: MetricStats::from_values(runs.iter().map(|run| run.changed_files)),
            diff_lines: MetricStats::from_values(runs.iter().map(RunDetail::diff_lines)),
            verification_failures,
            violation_count,
            runs,
        }
    }
}

fn completed_group_status() -> RunGroupStatus {
    RunGroupStatus::Completed
}

/// A serializable report containing one or more benchmark groups.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Aggregated groups in stable order.
    pub groups: Vec<GroupReport>,
}

impl BenchmarkReport {
    /// Construct a stable report from already-aggregated groups.
    #[must_use]
    pub fn new(mut groups: Vec<GroupReport>) -> Self {
        groups.sort_by(|left, right| left.group_id.cmp(&right.group_id));
        Self {
            schema_version: 1,
            groups,
        }
    }

    /// Render pretty, machine-readable JSON.
    pub fn to_json(&self) -> Result<String, ReportError> {
        Ok(format!("{}\n", serde_json::to_string_pretty(self)?))
    }

    /// Render a human-readable Markdown report.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        let mut output = String::from("# PatchArena report\n\n");
        if self.groups.is_empty() {
            output.push_str("No run results were found.\n");
            return output;
        }
        for group in &self.groups {
            let requested_runs = requested_runs_label(group);
            output.push_str(&format!(
                "## {} · {}\n\n- Group: `{}`\n- Status: {}\n- Runs: {}\n- Success rate: {:.1}%\n- Median duration: {:.1} ms\n- Median changed files: {:.1}\n- Median diff lines: {:.1}\n- Verification failures: {}\n- Policy violations: {}\n- Repository instructions: {}\n\n",
                escape_markdown(&group.task_id),
                escape_markdown(&group.agent),
                escape_markdown(&group.group_id),
                group_status_label(&group.status),
                requested_runs,
                group.success_rate * 100.0,
                group.duration_ms.median,
                group.changed_files.median,
                group.diff_lines.median,
                group.verification_failures,
                group.violation_count,
                if group.instructions_enabled { "enabled" } else { "hidden" },
            ));
            if group.runs.is_empty() {
                output.push_str("_No completed runs are recorded for this group._\n\n");
                continue;
            }
            output.push_str("| Run | Success | Duration (ms) | Files | +Lines | -Lines |\n|---|---:|---:|---:|---:|---:|\n");
            for run in &group.runs {
                output.push_str(&format!(
                    "| `{}` | {} | {} | {} | {} | {} |\n",
                    escape_markdown(&run.run_id),
                    if run.success { "yes" } else { "no" },
                    run.duration_ms,
                    run.changed_files,
                    run.added_lines,
                    run.deleted_lines,
                ));
            }
            output.push('\n');
            for run in &group.runs {
                output.push_str(&format!("### Run `{}`\n\n", escape_markdown(&run.run_id)));
                if run.verification.is_empty() {
                    output.push_str("- Verification: no commands recorded\n");
                } else {
                    for verification in &run.verification {
                        output.push_str(&format!(
                            "- Verification `{}`: {} (exit {}, {} ms)\n",
                            escape_markdown(&verification.command),
                            if verification.success {
                                "passed"
                            } else {
                                "failed"
                            },
                            verification
                                .exit_code
                                .map_or_else(|| "none".to_owned(), |code| code.to_string()),
                            verification.duration_ms,
                        ));
                    }
                }
                for error in &run.errors {
                    output.push_str(&format!("- Error: {}\n", escape_markdown(error)));
                }
                for violation in &run.violations {
                    output.push_str(&format!(
                        "- Policy violation: {}\n",
                        escape_markdown(violation)
                    ));
                }
                output.push('\n');
            }
        }
        output
    }

    /// Render a complete HTML document with embedded CSS and no external assets.
    #[must_use]
    pub fn to_html(&self) -> String {
        let mut groups = String::new();
        if self.groups.is_empty() {
            groups.push_str("<p class=\"empty\">No run results were found.</p>");
        }
        for group in &self.groups {
            let status_class = if group.status == RunGroupStatus::Completed
                && group.success_rate == 1.0
                && group.run_count > 0
            {
                "pass"
            } else {
                "attention"
            };
            let badge = if group.status == RunGroupStatus::Completed {
                format!("{:.1}% success", group.success_rate * 100.0)
            } else {
                group_status_label(&group.status).to_owned()
            };
            let requested_runs = requested_runs_label(group);
            groups.push_str(&format!(
                "<section><header><div><p class=\"eyebrow\">{}</p><h2>{}</h2></div><span class=\"badge {}\">{}</span></header><div class=\"metrics\"><div><span>Runs</span><strong>{}</strong></div><div><span>Median duration</span><strong>{:.1} ms</strong></div><div><span>Changed files</span><strong>{:.1}</strong></div><div><span>Diff lines</span><strong>{:.1}</strong></div><div><span>Verify failures</span><strong>{}</strong></div><div><span>Violations</span><strong>{}</strong></div></div><p class=\"meta\">Group <code>{}</code> · status {} · instructions {}</p>",
                escape_html(&group.agent),
                escape_html(&group.task_id),
                status_class,
                badge,
                requested_runs,
                group.duration_ms.median,
                group.changed_files.median,
                group.diff_lines.median,
                group.verification_failures,
                group.violation_count,
                escape_html(&group.group_id),
                group_status_label(&group.status),
                if group.instructions_enabled { "enabled" } else { "hidden" },
            ));
            if group.runs.is_empty() {
                groups.push_str(
                    "<p class=\"empty\">No completed runs are recorded for this group.</p>",
                );
            }
            for run in &group.runs {
                groups.push_str(&run_html(run));
            }
            groups.push_str("</section>");
        }
        format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>PatchArena report</title><style>{}</style></head><body><main><div class=\"title\"><p class=\"eyebrow\">Reproducible agent benchmark</p><h1>PatchArena report</h1></div>{}</main></body></html>\n",
            REPORT_CSS, groups
        )
    }
}

/// Delta values from a baseline group to a candidate group.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComparisonDelta {
    /// Candidate minus baseline success rate, in percentage points.
    pub success_rate_points: f64,
    /// Candidate minus baseline median duration.
    pub median_duration_ms: f64,
    /// Candidate minus baseline median changed files.
    pub median_changed_files: f64,
    /// Candidate minus baseline median diff lines.
    pub median_diff_lines: f64,
    /// Candidate minus baseline verification-failure count.
    pub verification_failures: i64,
    /// Candidate minus baseline violation count.
    pub violation_count: i64,
    /// Candidate minus baseline duration standard deviation.
    pub duration_variability_ms: f64,
}

/// Machine-readable comparison between two run groups.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// Comparison schema version.
    pub schema_version: u32,
    /// Baseline group summary.
    pub baseline: GroupReport,
    /// Candidate group summary.
    pub candidate: GroupReport,
    /// Candidate-minus-baseline deltas.
    pub delta: ComparisonDelta,
}

impl Comparison {
    /// Compare two compatible group reports.
    ///
    /// Both groups must be complete, their requested and observed sample sizes must agree, and
    /// task IDs, benchmark identities, and sample sizes must match.
    pub fn new(baseline: GroupReport, candidate: GroupReport) -> Result<Self, ReportError> {
        validate_completed_group("baseline", &baseline)?;
        validate_completed_group("candidate", &candidate)?;
        if baseline.task_id != candidate.task_id {
            return Err(ReportError::Incompatible(format!(
                "cannot compare task `{}` with `{}`",
                baseline.task_id, candidate.task_id
            )));
        }
        if baseline.benchmark_identity.is_none()
            || baseline.benchmark_identity != candidate.benchmark_identity
        {
            return Err(ReportError::Incompatible(
                "repository commit or task/policy fingerprint differs (or is missing)".to_owned(),
            ));
        }
        if baseline.run_count != candidate.run_count {
            return Err(ReportError::Incompatible(format!(
                "run counts differ (baseline {}, candidate {}); use equal sample sizes",
                baseline.run_count, candidate.run_count
            )));
        }
        let delta = ComparisonDelta {
            success_rate_points: (candidate.success_rate - baseline.success_rate) * 100.0,
            median_duration_ms: candidate.duration_ms.median - baseline.duration_ms.median,
            median_changed_files: candidate.changed_files.median - baseline.changed_files.median,
            median_diff_lines: candidate.diff_lines.median - baseline.diff_lines.median,
            verification_failures: usize_delta(
                candidate.verification_failures,
                baseline.verification_failures,
            ),
            violation_count: usize_delta(candidate.violation_count, baseline.violation_count),
            duration_variability_ms: candidate.duration_ms.standard_deviation
                - baseline.duration_ms.standard_deviation,
        };
        Ok(Self {
            schema_version: 1,
            baseline,
            candidate,
            delta,
        })
    }

    /// Render pretty JSON with a trailing newline.
    pub fn to_json(&self) -> Result<String, ReportError> {
        Ok(format!("{}\n", serde_json::to_string_pretty(self)?))
    }

    /// Render a concise console comparison.
    #[must_use]
    pub fn to_console(&self) -> String {
        format!(
            "baseline {} (n={}): {:.1}% success, {:.1} ms median, {:.1} files, {:.1} diff lines\ncandidate {} (n={}): {:.1}% success, {:.1} ms median, {:.1} files, {:.1} diff lines\ndelta: {:+.1} pp success, {:+.1} ms duration, {:+.1} files, {:+.1} diff lines, {:+} verify failures, {:+} violations, {:+.1} ms duration variability\n",
            sanitize_controls(&self.baseline.group_id),
            self.baseline.run_count,
            self.baseline.success_rate * 100.0,
            self.baseline.duration_ms.median,
            self.baseline.changed_files.median,
            self.baseline.diff_lines.median,
            sanitize_controls(&self.candidate.group_id),
            self.candidate.run_count,
            self.candidate.success_rate * 100.0,
            self.candidate.duration_ms.median,
            self.candidate.changed_files.median,
            self.candidate.diff_lines.median,
            self.delta.success_rate_points,
            self.delta.median_duration_ms,
            self.delta.median_changed_files,
            self.delta.median_diff_lines,
            self.delta.verification_failures,
            self.delta.violation_count,
            self.delta.duration_variability_ms,
        )
    }
}

fn validate_completed_group(label: &str, group: &GroupReport) -> Result<(), ReportError> {
    if group.status != RunGroupStatus::Completed {
        return Err(ReportError::Incompatible(format!(
            "{label} group `{}` has status `{}`; only completed groups can be compared",
            group.group_id,
            group_status_label(&group.status)
        )));
    }
    match group.requested_run_count {
        Some(requested) if requested == group.run_count => Ok(()),
        Some(requested) => Err(ReportError::Incompatible(format!(
            "{label} group `{}` completed {}/{} requested runs",
            group.group_id, group.run_count, requested
        ))),
        None => Err(ReportError::Incompatible(format!(
            "{label} group `{}` does not record its requested run count",
            group.group_id
        ))),
    }
}

fn usize_delta(candidate: usize, baseline: usize) -> i64 {
    let candidate = i64::try_from(candidate).unwrap_or(i64::MAX);
    let baseline = i64::try_from(baseline).unwrap_or(i64::MAX);
    candidate.saturating_sub(baseline)
}

fn escape_markdown(value: &str) -> String {
    escape_html(value)
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('!', "\\!")
        .replace('|', "\\|")
        .replace('`', "&#96;")
        .replace(['\r', '\n'], " ")
}

fn escape_html(value: &str) -> String {
    sanitize_controls(value)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn sanitize_controls(value: &str) -> String {
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

fn group_status_label(status: &RunGroupStatus) -> &'static str {
    match status {
        RunGroupStatus::Unknown => "unknown",
        RunGroupStatus::Running => "running",
        RunGroupStatus::Completed => "completed",
        RunGroupStatus::Aborted => "aborted",
    }
}

fn requested_runs_label(group: &GroupReport) -> String {
    match group.requested_run_count {
        Some(requested) => format!("{} / {requested} requested", group.run_count),
        None => format!("{} / unknown requested", group.run_count),
    }
}

fn run_html(run: &RunDetail) -> String {
    let status = if run.success { "passed" } else { "failed" };
    let verification = if run.verification.is_empty() {
        "<li>No verification commands recorded</li>".to_owned()
    } else {
        run.verification
            .iter()
            .map(|entry| {
                format!(
                    "<li><code>{}</code> — {} ({} ms, exit {})</li>",
                    escape_html(&entry.command),
                    if entry.success { "passed" } else { "failed" },
                    entry.duration_ms,
                    entry
                        .exit_code
                        .map_or_else(|| "none".to_owned(), |code| code.to_string()),
                )
            })
            .collect()
    };
    let issues = run
        .errors
        .iter()
        .chain(&run.violations)
        .map(|issue| format!("<li>{}</li>", escape_html(issue)))
        .collect::<String>();
    format!(
        "<details><summary><span><code>{}</code> · {}</span><span>{} ms · {} files · +{} −{}</span></summary><div class=\"detail\"><h3>Verification</h3><ul>{}</ul><h3>Errors and violations</h3><ul>{}</ul></div></details>",
        escape_html(&run.run_id),
        status,
        run.duration_ms,
        run.changed_files,
        run.added_lines,
        run.deleted_lines,
        verification,
        if issues.is_empty() {
            "<li>None</li>".to_owned()
        } else {
            issues
        },
    )
}

const REPORT_CSS: &str = r#"
:root{color-scheme:light;--ink:#17231d;--muted:#66736c;--paper:#f4f2ea;--card:#fffefa;--line:#d8d7ce;--accent:#23664c;--warn:#9a4d21}*{box-sizing:border-box}body{margin:0;background:var(--paper);color:var(--ink);font:15px/1.55 ui-sans-serif,system-ui,-apple-system,"Segoe UI",sans-serif}main{width:min(1120px,calc(100% - 32px));margin:48px auto 80px}.title{margin-bottom:28px}h1{font:700 clamp(34px,6vw,64px)/1.02 ui-serif,Georgia,serif;margin:4px 0}.eyebrow{text-transform:uppercase;letter-spacing:.13em;color:var(--muted);font-size:12px;margin:0}section{background:var(--card);border:1px solid var(--line);border-radius:18px;padding:24px;margin:20px 0;box-shadow:0 12px 38px #273c3010}header{display:flex;justify-content:space-between;gap:18px;align-items:flex-start}h2{font:650 28px/1.15 ui-serif,Georgia,serif;margin:4px 0}.badge{padding:6px 10px;border-radius:999px;font-weight:650;white-space:nowrap}.badge.pass{background:#dcefe6;color:var(--accent)}.badge.attention{background:#f7e4d8;color:var(--warn)}.metrics{display:grid;grid-template-columns:repeat(6,minmax(100px,1fr));gap:10px;margin:24px 0}.metrics div{background:#f5f5ef;padding:12px;border-radius:10px}.metrics span{display:block;color:var(--muted);font-size:12px}.metrics strong{font-size:18px}.meta{color:var(--muted)}code{font-family:ui-monospace,SFMono-Regular,Consolas,monospace;font-size:.9em}details{border-top:1px solid var(--line);padding:14px 0}summary{display:flex;justify-content:space-between;gap:16px;cursor:pointer}.detail{padding:10px 14px 4px}.detail h3{font-size:13px;text-transform:uppercase;letter-spacing:.08em;margin-top:18px}.detail a{color:var(--accent)}@media(max-width:800px){.metrics{grid-template-columns:repeat(2,1fr)}header,summary{flex-direction:column}.badge{align-self:flex-start}}
"#;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use chrono::Utc;
    use patcharena_core::{
        ArtifactPaths, BenchmarkIdentity, CommandOutcome, RunGroup, RunGroupStatus, RunResult,
        TaskId,
    };
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::{
        BenchmarkReport, Comparison, GroupReport, ReportError, RunDetail, VerificationDetail,
        load_report, load_selection,
    };

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
            violations: Vec::new(),
            errors: if success {
                Vec::new()
            } else {
                vec!["verification failed".to_owned()]
            },
            artifact_directory: id.to_owned(),
        }
    }

    fn persisted_result(run_id: String, group_id: Option<String>) -> RunResult {
        let now = Utc::now();
        RunResult {
            schema_version: 1,
            patcharena_version: None,
            run_id,
            group_id,
            task_id: TaskId::new("report-test").expect("task ID"),
            agent: "codex".to_owned(),
            agent_metadata: None,
            execution_metadata: None,
            instructions_enabled: true,
            benchmark_identity: None,
            started_at: now,
            finished_at: now,
            duration_ms: 1,
            success: true,
            exit_code: Some(0),
            changed_files: 1,
            added_lines: 1,
            deleted_lines: 0,
            setup: Vec::new(),
            agent_outcome: None,
            verification: vec![CommandOutcome::exited("cargo test", 0, 1)],
            audit: Vec::new(),
            violations: Vec::new(),
            artifacts: ArtifactPaths::default(),
            error: None,
        }
    }

    fn save_result(runs_directory: &Path, result: &RunResult, directory_id: &str) {
        let run_directory = runs_directory.join(directory_id);
        fs::create_dir(&run_directory).expect("create run directory");
        result
            .save_new(run_directory.join("result.json"))
            .expect("save result");
    }

    #[test]
    fn aggregates_success_medians_and_variability() {
        let report = GroupReport::from_details(
            "group",
            "task",
            "fake",
            true,
            vec![run("a", true, 10), run("b", false, 30), run("c", true, 20)],
        )
        .expect("aggregate");
        assert!((report.success_rate - 2.0 / 3.0).abs() < f64::EPSILON);
        assert_eq!(report.duration_ms.median, 20.0);
        assert!(report.duration_ms.standard_deviation > 8.0);
        assert_eq!(report.verification_failures, 1);
        assert_eq!(report.requested_run_count, Some(3));
        assert_eq!(report.status, RunGroupStatus::Completed);
    }

    #[test]
    fn renders_markdown_json_and_standalone_html() {
        let group = GroupReport::from_details(
            "group",
            "task<script>",
            "fake [click](javascript:alert(1))\x1b]8;;file:///tmp/invalid\x07",
            true,
            vec![run("run", true, 12)],
        )
        .expect("aggregate");
        let report = BenchmarkReport::new(vec![group]);
        let markdown = report.to_markdown();
        assert!(markdown.contains("PatchArena report"));
        assert!(!markdown.contains("<script>"));
        assert!(markdown.contains("task&lt;script&gt;"));
        assert!(!markdown.contains("[click](javascript:"));
        assert!(!markdown.contains('\x1b'));
        assert!(!markdown.contains('\x07'));
        assert!(markdown.contains("Status: completed"));
        assert!(markdown.contains("Runs: 1 / 1 requested"));
        let json = report.to_json().expect("JSON");
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"requested_run_count\": 1"));
        assert!(json.contains("\"status\": \"completed\""));
        let html = report.to_html();
        assert!(html.starts_with("<!doctype html>"));
        assert!(!html.contains("task<script>"));
        assert!(!html.contains("https://"));
        assert!(!html.contains('\x1b'));
        assert!(!html.contains('\x07'));
    }

    #[test]
    fn compares_candidate_against_baseline() {
        let mut baseline = GroupReport::from_details(
            "base",
            "task",
            "fake",
            true,
            vec![run("one", false, 30), run("two", true, 20)],
        )
        .expect("baseline");
        let mut candidate = GroupReport::from_details(
            "candidate",
            "task",
            "fake",
            false,
            vec![run("three", true, 10), run("four", true, 10)],
        )
        .expect("candidate");
        assert!(Comparison::new(baseline.clone(), candidate.clone()).is_err());
        let identity = BenchmarkIdentity {
            repository_commit: "0".repeat(40),
            task_fingerprint: "1".repeat(64),
        };
        baseline.benchmark_identity = Some(identity.clone());
        candidate.benchmark_identity = Some(identity);
        let comparison = Comparison::new(baseline, candidate).expect("compatible comparison");
        assert_eq!(comparison.delta.success_rate_points, 50.0);
        assert_eq!(comparison.delta.median_duration_ms, -15.0);
        assert!(comparison.to_console().contains("delta:"));
        comparison.to_json().expect("comparison JSON");
    }

    #[test]
    fn comparison_rejects_incomplete_or_mismatched_groups() {
        let identity = BenchmarkIdentity {
            repository_commit: "0".repeat(40),
            task_fingerprint: "1".repeat(64),
        };
        let mut baseline =
            GroupReport::from_details("base", "task", "fake", true, vec![run("one", true, 10)])
                .expect("baseline");
        let mut candidate = GroupReport::from_details(
            "candidate",
            "task",
            "fake",
            true,
            vec![run("two", true, 10)],
        )
        .expect("candidate");
        baseline.benchmark_identity = Some(identity.clone());
        candidate.benchmark_identity = Some(identity);

        baseline.status = RunGroupStatus::Running;
        assert!(matches!(
            Comparison::new(baseline.clone(), candidate.clone()),
            Err(ReportError::Incompatible(message)) if message.contains("only completed groups")
        ));

        baseline.status = RunGroupStatus::Completed;
        baseline.requested_run_count = Some(2);
        assert!(matches!(
            Comparison::new(baseline, candidate),
            Err(ReportError::Incompatible(message)) if message.contains("1/2 requested runs")
        ));
    }

    #[test]
    fn loads_and_renders_empty_incomplete_groups() {
        let directory = tempdir().expect("temporary directory");
        let runs = directory.path().join("runs");
        let groups = directory.path().join("groups");
        fs::create_dir(&runs).expect("runs directory");
        fs::create_dir(&groups).expect("groups directory");

        let statuses = [
            (RunGroupStatus::Running, Some(3)),
            (RunGroupStatus::Aborted, Some(3)),
            (RunGroupStatus::Unknown, None),
        ];
        let mut aborted_group_id = String::new();
        for (status, requested_runs) in statuses {
            let mut group = RunGroup::new(
                TaskId::new("report-test").expect("task ID"),
                "codex",
                Utc::now(),
                3,
            )
            .expect("group");
            group.status = status;
            group.requested_runs = requested_runs;
            if status == RunGroupStatus::Aborted {
                aborted_group_id.clone_from(&group.group_id);
            }
            group
                .save_new(groups.join(format!("{}.json", group.group_id)))
                .expect("save empty group");
        }

        let report = load_report(&runs, &groups).expect("load empty groups");
        assert_eq!(report.groups.len(), 3);
        assert!(report.groups.iter().all(|group| group.run_count == 0));
        let markdown = report.to_markdown();
        assert!(markdown.contains("Status: running"));
        assert!(markdown.contains("Status: aborted"));
        assert!(markdown.contains("Status: unknown"));
        assert!(markdown.contains("No completed runs are recorded"));
        let html = report.to_html();
        assert!(html.contains(">aborted<"));
        assert!(html.contains("No completed runs are recorded"));

        let selected =
            load_selection(&runs, &groups, &aborted_group_id).expect("select empty aborted group");
        assert_eq!(selected.status, RunGroupStatus::Aborted);
        assert_eq!(selected.requested_run_count, Some(3));
        assert_eq!(selected.run_count, 0);
    }

    #[test]
    fn targeted_selection_binds_file_location_to_record_id() {
        let directory = tempdir().expect("temporary directory");
        let runs = directory.path().join("runs");
        let groups = directory.path().join("groups");
        fs::create_dir(&runs).expect("runs directory");
        fs::create_dir(&groups).expect("groups directory");

        let selected_run = Uuid::new_v4().to_string();
        let actual_run = Uuid::new_v4().to_string();
        save_result(&runs, &persisted_result(actual_run, None), &selected_run);
        assert!(matches!(
            load_selection(&runs, &groups, &selected_run),
            Err(ReportError::Incompatible(_))
        ));

        let selected_group = Uuid::new_v4().to_string();
        let group = RunGroup::new(
            TaskId::new("report-test").expect("task ID"),
            "codex",
            Utc::now(),
            1,
        )
        .expect("group");
        assert_ne!(selected_group, group.group_id);
        group
            .save_new(groups.join(format!("{selected_group}.json")))
            .expect("save mismatched group");
        assert!(matches!(
            load_selection(&runs, &groups, &selected_group),
            Err(ReportError::Incompatible(_))
        ));
    }

    #[test]
    fn report_rejects_run_and_group_uuid_collision() {
        let directory = tempdir().expect("temporary directory");
        let runs = directory.path().join("runs");
        let groups = directory.path().join("groups");
        fs::create_dir(&runs).expect("runs directory");
        fs::create_dir(&groups).expect("groups directory");

        let group = RunGroup::new(
            TaskId::new("report-test").expect("task ID"),
            "codex",
            Utc::now(),
            1,
        )
        .expect("group");
        let collision = group.group_id.clone();
        group
            .save_new(groups.join(format!("{collision}.json")))
            .expect("save group");
        save_result(
            &runs,
            &persisted_result(collision.clone(), None),
            &collision,
        );

        assert!(matches!(
            load_selection(&runs, &groups, &collision),
            Err(ReportError::Incompatible(_))
        ));
        assert!(matches!(
            load_report(&runs, &groups),
            Err(ReportError::Incompatible(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn targeted_selection_rejects_symlinked_ancestor_directories() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("temporary directory");
        let outside = tempdir().expect("outside directory");
        let runs = directory.path().join("runs");
        let groups = directory.path().join("groups");
        fs::create_dir(&runs).expect("runs directory");
        fs::create_dir(&groups).expect("groups directory");

        let run_id = Uuid::new_v4().to_string();
        persisted_result(run_id.clone(), None)
            .save_new(outside.path().join("result.json"))
            .expect("outside result");
        symlink(outside.path(), runs.join(&run_id)).expect("run directory symlink");
        assert!(matches!(
            load_selection(&runs, &groups, &run_id),
            Err(ReportError::Incompatible(_))
        ));
        assert!(matches!(
            load_report(&runs, &groups),
            Err(ReportError::Incompatible(_))
        ));

        let symlinked_groups = directory.path().join("symlinked-groups");
        symlink(outside.path(), &symlinked_groups).expect("groups directory symlink");
        assert!(matches!(
            load_selection(&runs, &symlinked_groups, &Uuid::new_v4().to_string()),
            Err(ReportError::Incompatible(_))
        ));

        let symlinked_runs = directory.path().join("symlinked-runs");
        symlink(outside.path(), &symlinked_runs).expect("runs directory symlink");
        assert!(matches!(
            load_report(&symlinked_runs, &groups),
            Err(ReportError::Incompatible(_))
        ));
        assert!(matches!(
            load_report(&runs, &symlinked_groups),
            Err(ReportError::Incompatible(_))
        ));
    }

    #[test]
    fn report_rejects_result_whose_group_metadata_is_missing() {
        let directory = tempdir().expect("temporary directory");
        let runs = directory.path().join("runs");
        let groups = directory.path().join("groups");
        fs::create_dir(&runs).expect("runs directory");
        fs::create_dir(&groups).expect("groups directory");

        let run_id = Uuid::new_v4().to_string();
        let missing_group = Uuid::new_v4().to_string();
        save_result(
            &runs,
            &persisted_result(run_id.clone(), Some(missing_group)),
            &run_id,
        );

        assert!(matches!(
            load_report(&runs, &groups),
            Err(ReportError::Incompatible(_))
        ));
    }

    #[test]
    fn report_keeps_legacy_result_without_group_as_singleton() {
        let directory = tempdir().expect("temporary directory");
        let runs = directory.path().join("runs");
        let groups = directory.path().join("groups");
        fs::create_dir(&runs).expect("runs directory");
        fs::create_dir(&groups).expect("groups directory");

        let run_id = Uuid::new_v4().to_string();
        save_result(&runs, &persisted_result(run_id.clone(), None), &run_id);
        let report = load_report(&runs, &groups).expect("legacy singleton report");
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].group_id, run_id);
    }
}

/// Errors encountered while loading or rendering benchmark results.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    /// A result failed core schema validation.
    #[error(transparent)]
    Core(#[from] patcharena_core::CoreError),
    /// A report file operation failed.
    #[error("failed to {operation} `{path}`: {source}")]
    Io {
        /// Operation being attempted.
        operation: &'static str,
        /// Affected file or directory.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// JSON serialization failed.
    #[error("failed to serialize report JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A requested group or run did not resolve to any result.
    #[error("no benchmark results matched `{0}`")]
    NotFound(String),
    /// Results that cannot form one coherent aggregate were selected.
    #[error("incompatible benchmark results: {0}")]
    Incompatible(String),
}

/// Load all valid run results and group metadata from configured directories.
pub fn load_report(
    runs_directory: impl AsRef<Path>,
    groups_directory: impl AsRef<Path>,
) -> Result<BenchmarkReport, ReportError> {
    let runs_directory = runs_directory.as_ref();
    if !plain_directory_exists(runs_directory, "runs directory")? {
        return Err(ReportError::NotFound(runs_directory.display().to_string()));
    }
    let results = load_results(runs_directory)?;
    let groups_directory = groups_directory.as_ref();
    let groups = if plain_directory_exists(groups_directory, "groups directory")? {
        load_groups(groups_directory)?
    } else {
        Vec::new()
    };
    let group_ids = groups
        .iter()
        .map(|group| group.group_id.as_str())
        .collect::<HashSet<_>>();
    if let Some(collision) = results
        .iter()
        .find(|result| group_ids.contains(result.run_id.as_str()))
    {
        return Err(ReportError::Incompatible(format!(
            "UUID `{}` is used by both a run and a group",
            collision.run_id
        )));
    }
    let mut reports = Vec::new();
    let mut grouped_run_ids = HashSet::new();
    for group in groups {
        let members = group
            .run_ids
            .iter()
            .map(|run_id| {
                results
                    .iter()
                    .find(|result| &result.run_id == run_id)
                    .cloned()
                    .ok_or_else(|| ReportError::NotFound(run_id.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        group.summarize(&members)?;
        for run_id in &group.run_ids {
            if !grouped_run_ids.insert(run_id.clone()) {
                return Err(ReportError::Incompatible(format!(
                    "run `{run_id}` is listed by more than one group"
                )));
            }
        }
        reports.push(group_report(&group, &members)?);
    }
    for result in results {
        if !grouped_run_ids.contains(&result.run_id) {
            if let Some(group_id) = &result.group_id {
                return Err(ReportError::Incompatible(format!(
                    "run `{}` declares group `{group_id}`, but matching group metadata is missing or does not list the run",
                    result.run_id
                )));
            }
            reports.push(single_result_report(&result)?);
        }
    }
    Ok(BenchmarkReport::new(reports))
}

/// Resolve either a run-group UUID or an individual run UUID.
pub fn load_selection(
    runs_directory: impl AsRef<Path>,
    groups_directory: impl AsRef<Path>,
    selector: &str,
) -> Result<GroupReport, ReportError> {
    validate_selector(selector)?;
    let runs_directory = runs_directory.as_ref();
    let group = load_selected_group(groups_directory.as_ref(), selector)?;
    let result = load_selected_result(runs_directory, selector)?;
    if group.is_some() && result.is_some() {
        return Err(ReportError::Incompatible(format!(
            "UUID `{selector}` is used by both a run and a group"
        )));
    }
    if let Some(group) = group {
        let results = group
            .run_ids
            .iter()
            .map(|run_id| {
                load_selected_result(runs_directory, run_id)?
                    .ok_or_else(|| ReportError::NotFound(run_id.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        group.summarize(&results)?;
        return group_report(&group, &results);
    }
    if let Some(result) = result {
        return single_result_report(&result);
    }
    Err(ReportError::NotFound(selector.to_owned()))
}

fn load_selected_group(
    groups_directory: &Path,
    selector: &str,
) -> Result<Option<patcharena_core::RunGroup>, ReportError> {
    if !plain_directory_exists(groups_directory, "groups directory")? {
        return Ok(None);
    }
    let group_path = groups_directory.join(format!("{selector}.json"));
    if !plain_file_exists(&group_path, "group file")? {
        return Ok(None);
    }
    let group = patcharena_core::RunGroup::load(&group_path)?;
    if group.group_id != selector {
        return Err(ReportError::Incompatible(format!(
            "selected group `{selector}` contains ID `{}`",
            group.group_id
        )));
    }
    Ok(Some(group))
}

fn load_selected_result(
    runs_directory: &Path,
    selector: &str,
) -> Result<Option<patcharena_core::RunResult>, ReportError> {
    if !plain_directory_exists(runs_directory, "runs directory")? {
        return Ok(None);
    }
    let run_directory = runs_directory.join(selector);
    if !plain_directory_exists(&run_directory, "run directory")? {
        return Ok(None);
    }
    let result_path = run_directory.join("result.json");
    if !plain_file_exists(&result_path, "result file")? {
        return Ok(None);
    }
    let result = patcharena_core::RunResult::load(&result_path)?;
    if result.run_id != selector {
        return Err(ReportError::Incompatible(format!(
            "selected run `{selector}` contains ID `{}`",
            result.run_id
        )));
    }
    Ok(Some(result))
}

fn plain_directory_exists(path: &Path, description: &str) -> Result<bool, ReportError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(ReportError::Incompatible(format!(
                "{description} `{}` is not a regular directory",
                path.display()
            )))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ReportError::Io {
            operation: "inspect report directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn plain_file_exists(path: &Path, description: &str) -> Result<bool, ReportError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ReportError::Incompatible(format!(
                "{description} `{}` is not a regular file",
                path.display()
            )))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ReportError::Io {
            operation: "inspect report file",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn load_results(runs_directory: &Path) -> Result<Vec<patcharena_core::RunResult>, ReportError> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(runs_directory).map_err(|source| ReportError::Io {
        operation: "list runs directory",
        path: runs_directory.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| ReportError::Io {
            operation: "read runs directory entry",
            path: runs_directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| ReportError::Io {
            operation: "inspect run entry",
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(ReportError::Incompatible(format!(
                "run entry `{}` is a symbolic link",
                path.display()
            )));
        }
        if metadata.is_dir() && path.file_name().is_some_and(|name| name != "groups") {
            let result_path = path.join("result.json");
            match fs::symlink_metadata(&result_path) {
                Ok(result_metadata)
                    if result_metadata.is_file() && !result_metadata.file_type().is_symlink() =>
                {
                    paths.push(result_path);
                }
                Ok(_) => {
                    return Err(ReportError::Incompatible(format!(
                        "result path `{}` is not a regular file",
                        result_path.display()
                    )));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(ReportError::Io {
                        operation: "inspect result file",
                        path: result_path,
                        source,
                    });
                }
            }
        }
    }
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let result = patcharena_core::RunResult::load(&path)?;
            let directory_name = path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if directory_name != result.run_id {
                return Err(ReportError::Incompatible(format!(
                    "run `{}` is stored below directory `{directory_name}`",
                    result.run_id
                )));
            }
            Ok(result)
        })
        .collect()
}

fn load_groups(groups_directory: &Path) -> Result<Vec<patcharena_core::RunGroup>, ReportError> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(groups_directory).map_err(|source| ReportError::Io {
        operation: "list groups directory",
        path: groups_directory.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| ReportError::Io {
            operation: "read groups directory entry",
            path: groups_directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| ReportError::Io {
            operation: "inspect group entry",
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(ReportError::Incompatible(format!(
                "group entry `{}` is a symbolic link",
                path.display()
            )));
        }
        if metadata.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            paths.push(path);
        }
    }
    paths.sort();
    let mut groups = Vec::with_capacity(paths.len());
    let mut ids = HashSet::new();
    for path in paths {
        let group = patcharena_core::RunGroup::load(&path)?;
        let file_id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if file_id != group.group_id {
            return Err(ReportError::Incompatible(format!(
                "group `{}` is stored in `{}`",
                group.group_id,
                path.display()
            )));
        }
        if !ids.insert(group.group_id.clone()) {
            return Err(ReportError::Incompatible(format!(
                "duplicate group ID `{}`",
                group.group_id
            )));
        }
        groups.push(group);
    }
    Ok(groups)
}

fn group_report(
    group: &patcharena_core::RunGroup,
    results: &[patcharena_core::RunResult],
) -> Result<GroupReport, ReportError> {
    let instructions_enabled = results
        .first()
        .map_or(group.instructions_enabled, |result| {
            result.instructions_enabled
        });
    if results
        .iter()
        .any(|result| result.instructions_enabled != instructions_enabled)
    {
        return Err(ReportError::Incompatible(format!(
            "group `{}` mixes instruction modes",
            group.group_id
        )));
    }
    let requested_run_count = group
        .requested_runs
        .map(usize::try_from)
        .transpose()
        .map_err(|_| {
            ReportError::Incompatible(format!(
                "group `{}` requested run count does not fit this platform",
                group.group_id
            ))
        })?;
    let mut report = GroupReport::aggregate_details(
        group.group_id.clone(),
        group.task_id.to_string(),
        group.agent.clone(),
        instructions_enabled,
        requested_run_count,
        group.status,
        results.iter().map(run_detail).collect(),
    );
    report
        .benchmark_identity
        .clone_from(&group.benchmark_identity);
    Ok(report)
}

fn single_result_report(result: &patcharena_core::RunResult) -> Result<GroupReport, ReportError> {
    let mut report = GroupReport::from_details(
        result.run_id.clone(),
        result.task_id.to_string(),
        result.agent.clone(),
        result.instructions_enabled,
        vec![run_detail(result)],
    )?;
    report
        .benchmark_identity
        .clone_from(&result.benchmark_identity);
    Ok(report)
}

fn run_detail(result: &patcharena_core::RunResult) -> RunDetail {
    let mut errors = result.error.iter().cloned().collect::<Vec<_>>();
    errors.extend(
        result
            .setup
            .iter()
            .chain(result.agent_outcome.iter())
            .chain(result.verification.iter())
            .filter_map(|outcome| outcome.error.clone()),
    );
    RunDetail {
        run_id: result.run_id.clone(),
        success: result.success,
        duration_ms: result.duration_ms,
        changed_files: result.changed_files,
        added_lines: result.added_lines,
        deleted_lines: result.deleted_lines,
        verification: result
            .verification
            .iter()
            .map(|outcome| VerificationDetail {
                command: outcome.command.clone(),
                success: outcome.success,
                exit_code: outcome.exit_code,
                duration_ms: outcome.duration_ms,
            })
            .collect(),
        violations: result
            .violations
            .iter()
            .map(|violation| violation.message.clone())
            .collect(),
        errors,
        artifact_directory: result.run_id.clone(),
    }
}

fn validate_selector(selector: &str) -> Result<(), ReportError> {
    uuid::Uuid::parse_str(selector)
        .map(|_| ())
        .map_err(|_| ReportError::NotFound(selector.to_owned()))
}
