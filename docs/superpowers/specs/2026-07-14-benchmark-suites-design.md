# PatchArena Benchmark Suites design

- Date: 2026-07-14
- Target release: 0.3.0
- Status: approved by delegated product authority

## Context

PatchArena 0.2.0 can run one task against one agent (`run`) or several agents
against one task (`battle`). Its immutable run records, detached worktrees,
benchmark identities, adapter registry, and report renderers are strong building
blocks, but an OSS maintainer still has to script repeated battles to evaluate a
representative set of repository tasks. That script would become an unversioned
orchestration layer with weak checkpointing and no trustworthy aggregate view.

Benchmark quality matters at least as much as runner correctness. Recent coding-eval
audits found that ambiguous prompts, overly strict checks, and low-coverage checks can
materially distort results. PatchArena must therefore expose task-level evidence and
completeness rather than hide it behind a single winner score. This design also follows
the broader benchmark principles of fair comparison, reproducibility, usefulness, and
affordability.

References:

- [OpenAI: Separating signal from noise in coding evaluations](https://openai.com/index/separating-signal-from-noise-coding-evaluations/)
- [MLCommons benchmark principles](https://mlcommons.org/benchmarks/)

## Product outcome

An OSS maintainer can define a checked-in suite of PatchArena tasks and run:

```text
patcharena suite run --suite core --agents codex,claude --repeat 3
```

PatchArena validates the complete plan before the first agent invocation, executes
every task/agent cell in its own existing run group, checkpoints progress, and emits
JSON, Markdown, and self-contained HTML reports. The report makes missing evidence,
task-level failures, policy violations, sample sizes, and variability conspicuous.

## Goals

1. Add a versioned, reviewable suite definition containing an ordered set of unique
   task IDs.
2. Execute the Cartesian product of suite tasks and explicitly selected agents with
   one repeat count and one instructions condition.
3. Pin every cell to the same repository commit and to the expected per-task benchmark
   identity before execution begins.
4. Persist the execution plan before the first cell and atomically checkpoint it after
   every attempted cell.
5. Produce a task-by-agent matrix plus transparent per-agent descriptive summaries.
6. Preserve all existing run/group evidence and schema-1 readers.
7. Keep the CLI thin and add new suite modules instead of enlarging the existing
   orchestration and report monoliths.
8. Make cost and safety visible with a dry run and a hard cap on planned agent
   invocations.

## Non-goals

- A hosted service, public leaderboard, or central task registry.
- A composite quality/cost score, rank, or declared winner.
- Parallel execution. Sequential execution remains the predictable default and avoids
  local resource contention.
- Container, VM, network, CPU, memory, or filesystem isolation. Worktrees remain a
  repeatability boundary, not a sandbox.
- Automatic task-quality certification. Reports expose evidence; they do not prove that
  prompts and verification commands are fair.
- Cross-repository suites or automatic repository mutation to create task fixtures.
- Resuming a suite against changed task definitions, policy, configuration, or `HEAD`.

## Alternatives considered

### 1. External wrapper around `battle`

This is initially small, but the wrapper would own plan validation, persistence,
partial-failure semantics, and aggregation. Different wrappers would produce
incompatible evidence. Rejected.

### 2. Expand `battle` to accept multiple tasks

This reduces the number of commands but overloads a deliberately simple one-task
concept. Battle records and output would acquire two-dimensional lifecycle rules, and
the existing command would become harder to explain and preserve. Rejected.

### 3. Add a first-class suite domain

This adds one explicit model and command family while reusing existing run groups as
the only execution evidence. It gives the matrix lifecycle a clear boundary and keeps
`run` and `battle` backward compatible. Selected.

## User interface

### Define a suite

```text
patcharena suite add --id core --task csv-newline --task config-validation
patcharena suite list
```

`suite add` writes `.patcharena/suites/<id>.yaml` with a required schema version,
suite ID, optional description, and ordered task IDs. It refuses overwrite. Suite
definition files are intended to be committed with task definitions.

Equivalent YAML:

```yaml
schema_version: 1
id: core
description: Core repository maintenance tasks
tasks:
  - csv-newline
  - config-validation
```

The parser rejects unknown fields, unsupported schema versions, an empty task list,
duplicate task IDs, unsafe IDs, and more than 100 tasks.

### Validate without spending agent calls

```text
patcharena suite run --suite core --agents codex,claude --repeat 3 --dry-run
```

Dry run loads and validates the repository, configuration, suite, every referenced
task, every agent adapter, the clean Git state, the shared base commit, all expected
benchmark identities, and the total invocation count. It may execute adapter version
probes, but it creates no suite/run/group records and does not invoke an agent on a
task. It prints tasks, agents, repeats, instructions condition, base commit, and the
total number of planned agent invocations.

### Run and resume

```text
patcharena suite run --suite core --agents codex,claude --repeat 3
patcharena suite resume --run <suite-run-id>
```

Agents are always explicit; PatchArena never silently selects every installed adapter.
`repeat` defaults to one for affordability. The plan is rejected when
`tasks * agents * repeat` exceeds 1,000 invocations. The CLI prints the plan before
execution and one concise line per completed cell.

`resume` accepts only a record left in `running` state. It revalidates the current
repository commit, suite fingerprint, per-task benchmark identities, task documents,
agent availability, and configuration-derived runner settings. It executes pending
cells only. A terminal cell is never silently rerun.

### Regenerate or export a report

```text
patcharena suite report --run <suite-run-id> --format html --output report.html
```

Completed runs automatically receive `report.json`, `report.md`, and `report.html`
inside their suite-run directory. `suite report` can regenerate one format from
persisted records and optionally export it to a caller-selected regular file.

## Domain model

New models live in `patcharena-core/src/suite.rs`.

### `SuiteId`

A typed, filesystem-safe ID with the same portable character and reserved-name rules
as `TaskId`. A shared private validator prevents the two ID types from drifting while
keeping their public types distinct.

### `SuiteDefinition`

- `schema_version: u32`
- `id: SuiteId`
- `description: Option<String>`
- `tasks: Vec<TaskId>`

Serialization is strict and deterministic. The suite fingerprint is SHA-256 over the
canonical serialized definition. The fingerprint identifies the suite selection, not
the mutable task contents; expected benchmark identities identify task plus effective
policy without persisting prompt text.

### `SuiteExecution`

- result schema version
- suite-run UUID
- suite ID and suite-definition fingerprint
- repository commit
- ordered task snapshots containing task ID and expected `BenchmarkIdentity`
- ordered agent IDs
- repeat count and instructions-enabled condition
- creation/update/completion timestamps
- lifecycle status
- stable ordered cells

Lifecycle states are `running`, `completed`, `completed_with_errors`, and `aborted`.
Deserialization may map missing legacy status to `legacy_unknown`, but new writers never
emit that value.

### `SuiteCell`

Each cell identifies one task and one agent and is `pending`, `completed`, or `error`.
A completed cell stores its immutable group ID. An error cell stores a bounded,
control-character-sanitized diagnostic. Benchmark failures are not orchestration
errors: if a group finishes with failed verification, its cell is still `completed`
and the report shows the failed runs.

The model validates the exact task/agent Cartesian product, unique cell keys, legal
state transitions, matching counts, UUIDs, bounded diagnostics, and terminal-state
completeness.

## Persistent layout

```text
.patcharena/
├── tasks/                         # checked-in task YAML
├── suites/                        # checked-in suite YAML
├── groups/                        # existing generated group records
├── runs/                          # existing generated per-run evidence
└── suite-runs/                    # generated and ignored
    └── <suite-run-id>/
        ├── suite.json             # atomic lifecycle checkpoint
        ├── report.json
        ├── report.md
        └── report.html
```

`ProjectPaths` gains defaulted `suites_dir` and `suite_runs_dir` fields. Old schema-1
configuration files remain readable because omitted fields receive safe defaults.
Generated battle and suite-run directories are added to `.gitignore`; suite definitions
remain trackable.

Every new create, replace, read, and export operation uses the existing bounded,
symlink-refusing filesystem helpers. A suite-run directory must be newly created below
the configured suite-run root and must never be reused through a link.

## Architecture and component boundaries

### `patcharena-core::suite`

Owns suite IDs, definitions, execution records, lifecycle transitions, validation,
serialization, path derivation, and fingerprinting. It has no process, Git, CLI, or
presentation dependency.

### `patcharena-runner::suite`

Owns plan preflight and sequential cell execution. It composes `AgentRegistry` and
`ArenaRunner`; it does not duplicate worktree or process logic. Benchmark-identity
calculation is exposed through a small runner API so identities can be pinned during
preflight. The runner atomically checkpoints after creation and after every cell.

### `patcharena-report::suite`

Loads only persisted suite, group, and run records. It validates that each referenced
group belongs to the expected task, agent, instructions condition, sample size, and
benchmark identity. It then builds `SuiteReport` and renders JSON, Markdown, and escaped
self-contained HTML. It never invokes an agent or verification command.

### `patcharena-cli::suite`

Parses commands, loads the project, calls the suite runner/report APIs, prints concise
progress, writes requested exports, and maps outcomes to existing exit-code classes.
The top-level `commands.rs` only dispatches to this module.

## Execution flow

1. Discover the repository and load one immutable project configuration snapshot.
2. Load the suite by safe ID and load every referenced task once.
3. Validate unique explicit agents and run adapter availability/version probes.
4. Require a tracked-clean repository and resolve `HEAD` once.
5. Resolve effective runner settings and calculate every expected task benchmark
   identity before any agent call.
6. Materialize the complete ordered cell plan and enforce the 1,000-invocation cap.
7. For a real run, create the suite-run directory and initial `running` checkpoint.
8. For each pending cell, build the selected adapter's `ArenaRunner` and execute one
   existing run group with the requested repeat count.
9. Confirm the returned group identity, task, agent, instructions condition, and count;
   store the group ID or bounded orchestration error; atomically checkpoint.
10. Continue after cell-local orchestration errors. Abort remaining cells if the shared
    base commit or expected identities no longer match, because comparison validity is
    lost.
11. Mark the execution `completed` or `completed_with_errors`, checkpoint, reload all
    evidence through the report layer, and render the three default reports.
12. Return success only when every cell completed and every underlying benchmark run
    succeeded without policy violations. Otherwise return the existing benchmark-failed
    exit code while retaining all evidence.

## Reporting and metrics

The report contains:

- suite ID/run ID, lifecycle status, commit, suite fingerprint, instructions condition,
  agents, tasks, repeats, timestamps, and coverage;
- a task-by-agent matrix showing cell state, successful/requested runs, success rate,
  median duration, median changed files, median diff lines, verification failures, and
  policy violations;
- a per-agent macro success rate: calculate each complete task cell's success rate,
  then average those task rates so every task has equal weight;
- completed/error/missing cell counts and aggregate verification/policy counts;
- direct group and run IDs so every summary can be audited against immutable evidence.

The report does not pool unlike task durations into a comparative performance score,
does not infer statistical significance from small or dependent samples, and does not
declare a winner. Incomplete or incompatible evidence is an explicit report error or
visible missing cell, never an implicit failure or zero.

Stable ordering is suite task order followed by CLI agent order. JSON is the canonical
machine-readable report. Markdown is review-oriented. HTML uses no remote assets and
escapes every suite-, task-, agent-, command-, and result-controlled value.

## Failure handling and recovery

- Preflight errors create no execution record and invoke no task agent.
- A setup, agent, or verification failure represented in a completed group is ordinary
  benchmark evidence.
- A cell orchestration error is recorded and later cells continue when the shared
  comparison basis remains valid.
- Atomic checkpoint failure stops immediately; the previous valid checkpoint remains.
- Repository commit or benchmark-identity drift aborts the suite because remaining
  cells would not be comparable.
- Host termination can leave a valid `running` checkpoint. `suite resume` is the only
  supported continuation path.
- Report-generation failure does not discard the completed execution or group evidence;
  `suite report` can retry it.

## Security invariants

- Suite execution adds no shell interpolation and does not widen the environment
  allowlist.
- Suite/task/run IDs are validated before path joins.
- Definitions, checkpoints, and reports refuse symlinks and unsafe ancestors.
- Full preflight occurs before the first potentially expensive task agent invocation.
- Existing output, timeout, path, diff, and command policy ceilings apply independently
  to every underlying process and run.
- Prompts are fingerprinted through existing benchmark identity logic but are not copied
  into suite metadata or reports.
- Error messages stored in suite records are bounded and sanitized.
- Documentation continues to state prominently that PatchArena is not a sandbox.

## Compatibility

- Existing `run`, `battle`, `compare`, and `report` syntax and behavior do not change.
- Existing schema-1 configuration is readable through defaulted new path fields.
- Existing run/group/result JSON is unchanged; suite execution is a new record type
  using the current result schema version.
- No existing report field is removed or reinterpreted.
- Suite public APIs receive serialization and public-surface tests.

## Testing strategy

### Core

- strict YAML parsing, round trips, unsupported versions, ID/path traversal, duplicate
  and empty tasks, task cap, deterministic fingerprint;
- suite execution state transitions, exact Cartesian-product validation, UUIDs, count
  overflow, bounded errors, and future-schema rejection;
- safe definition/checkpoint paths and symlink refusal.

### Runner

- deterministic fake agents over at least two tasks and two agents;
- all cells share one base commit and expected task identities;
- initial and per-cell checkpoints, partial error continuation, abort on identity drift,
  total-invocation cap, dry run with zero group artifacts, and resume of pending cells;
- no duplicate rerun of completed cells.

### Report

- matrix and macro-average calculations from persisted fixtures;
- rejected wrong task/agent/identity/count references and visible error cells;
- stable JSON/Markdown/HTML ordering and hostile-text escaping;
- incomplete/running execution rendering without fabricated metrics.

### CLI

- add/list/dry-run/run/resume/report happy paths with fake agents;
- unknown suite/task/agent, dirty repository, duplicate agents, excessive plan, unsafe
  output, and incompatible resume failures;
- one end-to-end two-task/two-agent suite proving generated JSON, Markdown, and HTML.

### Full verification

```text
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked --workspace --release
```

No test may require network access or an installed external coding-agent CLI.

## Acceptance criteria

1. A checked-in two-task suite can be run against two fake/configured agents with one
   CLI command.
2. Every cell uses an independent existing run group and the same pinned commit.
3. A suite checkpoint exists before execution and remains valid after each cell.
4. A stopped running suite can resume pending cells without rerunning completed cells.
5. Default JSON, Markdown, and escaped standalone HTML reports are generated from
   persisted evidence and show a complete matrix with no winner declaration.
6. Dry run produces no run, group, battle, or suite-run artifact.
7. Old configuration and schema-1 results remain readable.
8. Documentation covers cost, fairness, resume rules, security boundaries, and the
   complete quick start in English and Japanese.
9. The full repository verification sequence passes on the supported Rust toolchain.

## Follow-on subprojects

This release establishes the data and UX seam needed by the other two product pillars
without folding them into one unreviewable change:

1. **Distribution and CI experience:** signed cross-platform release artifacts,
   `cargo binstall` metadata, shell/PowerShell installers, a non-secret fake-agent CI
   example, and machine-readable suite exit policy.
2. **Externally isolated execution:** an explicit container execution profile, global
   wall-clock/resource budgets, network policy, Windows Job Objects, and preserved
   provenance describing the isolation backend.

Each follow-on receives its own design, plan, threat-model review, and compatibility
gate after Benchmark Suites is complete.
