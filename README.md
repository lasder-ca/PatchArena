# PatchArena

[![CI](https://github.com/lasder-ca/PatchArena/actions/workflows/ci.yml/badge.svg)](https://github.com/lasder-ca/PatchArena/actions/workflows/ci.yml)

**English** | [日本語](README.ja.md)

PatchArena is a reproducible benchmark runner for AI coding agents on real repositories.

It runs a versioned repair task in a fresh Git worktree, captures what happened, verifies the result, and stores machine-readable evidence. Repeating the same task makes it possible to compare success, duration, patch size, verification failures, policy violations, and run-to-run variance instead of judging an agent from a single transcript.

**Project status:** source-only, pre-release software. There is no crates.io package or stable
compatibility promise yet. The on-disk formats are versioned so incompatible data fails
explicitly instead of being silently misread.

[Quick start](#quick-start) · [Task format](#task-definitions) · [Reports](#html-report-example) · [Security](#security) · [Contributing](CONTRIBUTING.md)

> [!WARNING]
> PatchArena is not a full sandbox. An agent and the configured setup or verification programs run with the operating-system permissions of the PatchArena process. Read [Security](#security) and the [threat model](docs/threat-model.md) before using untrusted inputs.

## Why PatchArena

Agent demos often show one successful patch but omit the failed attempts, execution environment,
verification output, and exact repository revision. PatchArena makes those inputs and outcomes
explicit. It is intended for local experiments, agent evaluations, regression suites, and
instruction-on/off comparisons where the evidence must remain inspectable.

PatchArena does not rank models globally, guarantee statistical significance, or make untrusted
code safe to execute. It provides repeatable orchestration and evidence collection; experimental
design and host isolation remain the operator's responsibility.

## How it works

```text
versioned task + committed HEAD + effective policy
                       │
                       ▼
        detached worktree per repetition
                       │
             setup → agent → verify
                       │
                       ▼
       diff + logs + audit + result.json
                       │
              compare and report
```

Each repetition starts from the same pinned commit. PatchArena executes commands without an
implicit shell, records bounded evidence, removes its temporary worktree, and preserves each run
under a UUID. A run group records the requested sample size and whether it completed.

## Status and scope

PatchArena is an early-stage OSS project. The initial command surface is:

- `patcharena init` — create repository-local configuration and state without overwriting existing files;
- `patcharena task add` and `patcharena task list` — create and inspect YAML tasks;
- `patcharena doctor` — check Git, the repository, Codex CLI, Rust, worktrees, and local writability;
- `patcharena run` — execute one task through Codex CLI, optionally repeatedly, and persist evidence;
- `patcharena compare` — compare two persisted run groups;
- `patcharena report` — render persisted results as Markdown, JSON, or self-contained HTML.

Codex CLI is the only production agent integration in the MVP. Tests use deterministic fake agents and do not require Codex or an external service.

## What it records

Each run can record:

- success and command exit status;
- start, finish, and elapsed time;
- changed-file, added-line, and deleted-line counts;
- setup and verification outcomes;
- bounded stdout and stderr;
- the generated Git patch;
- forbidden command/path violations;
- the task, agent, and versioned result schema;
- a benchmark identity containing the exact `HEAD` commit and a task/effective-policy fingerprint.

Aggregating repeats exposes success rate, median duration, and variance. Separate run groups can be used to compare repository instructions—for example, a normal run against one created with `--without-instructions`, which temporarily hides regular `AGENTS.md` files discovered in the worktree after setup—provided the operator controls all other inputs.

## Requirements

- Linux or WSL2 (the primary supported environments)
- Git
- Rust **1.85.0** or newer (the MSRV; Rust 2024 edition)
- Codex CLI for production `run` commands only

The project itself builds and its test suite runs without Codex CLI.

## Installation

PatchArena is currently installed from a source checkout:

```bash
./prepare.sh
cargo install --path crates/patcharena-cli --locked
patcharena --help
```

`prepare.sh` checks prerequisites and then fetches, builds, tests, and lints the workspace. It does not use `sudo`, install packages, or modify the user's Git configuration. During development, use `cargo run -p patcharena-cli -- <arguments>` instead of installing.

To update a source installation, pull or download the desired revision, review its changes, rerun
the verification commands, and repeat `cargo install --path crates/patcharena-cli --locked --force`.

## Quick start

Run these commands inside the Git repository you want to benchmark:

```bash
patcharena init
patcharena doctor

printf '%s\n' \
  'Fix the CSV exporter so it emits exactly one newline per record.' \
  > prompt.md

patcharena task add \
  --id csv-newline-regression \
  --prompt-file prompt.md \
  --verify "cargo test csv_export"

patcharena task list
patcharena run --task csv-newline-regression --agent codex --repeat 3
```

The `run` command prints a group UUID. Keep that ID for `compare` and targeted `report` commands.
Generated data stays below `.patcharena/`; task YAML may be committed, while run artifacts should
normally remain local.

To create a comparison group with repository `AGENTS.md` files temporarily hidden from the agent, add `--without-instructions`. After setup, PatchArena scans the worktree without following symlinked directories and hides every regular file named `AGENTS.md` that it finds, including untracked and ignored files. The scan is limited to 100,000 directory entries; exceeding the limit, or finding an `AGENTS.md` symlink, fails the run instead of silently using a partial mask. Files are restored before verification.

PatchArena records this condition, but the option does not create a context-free agent. It does not hide instructions outside the worktree, other instruction filenames, user/global agent configuration, agent defaults, model-side context, or inputs already observed by setup programs. It therefore does not prove that every other source of agent context is identical.

`init` is idempotent: it keeps an existing valid `patcharena.toml`, reuses safe metadata directories, and does not overwrite existing files. Task definitions may be versioned as part of a benchmark, but keep generated run, group, comparison, and report artifacts—and all secrets—out of version control.

## Command reference

| Command | Purpose |
|---|---|
| `patcharena init` | Create repository-local configuration and state directories. |
| `patcharena task add` | Create a validated task from a prompt file and commands. |
| `patcharena task list` | List available task IDs and limits. |
| `patcharena run` | Execute one or more isolated repetitions. |
| `patcharena compare` | Compare two compatible completed groups or individual runs. |
| `patcharena report` | Render Markdown, JSON, or self-contained HTML. |
| `patcharena doctor` | Check Git, Rust, Codex CLI, worktrees, and state writability. |

Use `patcharena <command> --help` for the authoritative option list. Stable error-category exit
codes are `3` for invalid input or local I/O, `4` for Git or prerequisite failures, `5` for runner
failures, and `7` for report or comparison failures. Clap uses its own standard code for argument
parsing errors.

## Task definitions

Tasks live in `.patcharena/tasks/<id>.yaml`. A complete task can define setup and verification commands, resource and patch-size limits, and forbidden operations:

```yaml
id: csv-newline-regression
prompt: |
  Fix the CSV exporter so it emits exactly one newline per record.

setup:
  commands:
    - cargo build

verify:
  commands:
    - cargo test csv_export
    - cargo clippy --all-targets -- -D warnings

limits:
  timeout_seconds: 600
  max_output_bytes: 10485760
  max_changed_files: 8
  max_diff_lines: 500

forbidden:
  commands:
    - git push
    - cargo publish
  paths:
    - .git
    - .env
```

Command strings are split into an executable and argument array; PatchArena does not evaluate pipes, redirections, variable expansion, or other shell operators. Explicitly invoking a shell such as `sh -c` delegates that parsing—and its risks—to the shell.

Machine-generated tasks can avoid tokenization entirely with the structured form:

```yaml
verify:
  commands:
    - program: cargo
      args: ["test", "csv_export"]
```

Repository defaults are documented in [`patcharena.toml.example`](patcharena.toml.example). Despite the `defaults` name, the numeric project values are safety upper bounds at execution: each effective limit is the smaller of the task value and the project value. A task may therefore tighten a limit but cannot relax the repository cap. When `task add` omits a limit, it copies the current project value into the new task. Timeout and retained-output limits apply separately to each launched setup, agent, and verification process; changed-file and diff-line limits apply to the resulting patch. A project-policy change that changes the effective policy also changes the benchmark fingerprint.

Copy the example only when creating configuration manually; normally `patcharena init` writes a compatible file.

## Results

Each repeat receives a UUID and writes its record below. Group metadata records the requested
repeat count and a `running`, `completed`, or `aborted` state. It is created before the first
repeat and atomically updated after each completed repeat, so a later hard failure leaves earlier
completed evidence discoverable under the group ID reported in the error. A sudden host crash can
leave the state as `running`; that is deliberately treated as incomplete rather than successful.

```text
.patcharena/runs/<run-id>/
├── result.json
├── stdout.log
├── stderr.log
├── changes.diff
└── audit.jsonl
```

`result.json` includes a required `schema_version` so incompatible future formats fail explicitly. It also records the benchmark identity used to decide whether two result sets are comparable. The optional JSON Lines audit artifact records launched-command evidence across run phases. Logs, audits, and patches are evidence, not sanitized publication artifacts: inspect them for secrets before sharing.

## Comparing runs

Compare persisted run-group IDs rather than rerunning an agent. Replace the uppercase placeholders with IDs printed by `patcharena run`:

```bash
patcharena compare \
  --baseline BASELINE_GROUP_ID \
  --candidate CANDIDATE_GROUP_ID \
  --output comparison.json
```

PatchArena accepts a run ID as a one-sample selector, but group IDs are the normal choice. A comparison is rejected unless both sides are complete, their observed run counts equal their requested counts, and they have the same task ID, benchmark identity, and sample size. Legacy groups without completion metadata and malformed records without an identity are incompatible rather than silently mixed.

The identity combines the exact repository `HEAD` commit with a SHA-256 fingerprint of the task definition and resolved execution policy, including effective caps, the environment allowlist, and merged forbidden commands and paths. It intentionally does not include the selected agent or the instructions-on/off condition, so those can be the experimental variable. It is a compatibility guard, not a signed attestation or a complete environment lock: operators must still control toolchains, dependencies, agent/model configuration, credentials, network responses, and other external inputs.

For compatible groups, the comparison covers success rate, median duration, changed files, diff lines, verification failures, detected forbidden operations, and variation between repeats. Missing or incompatible records are errors; they are not counted as successes.

## HTML report example

Generate a screenshot-friendly, single-file report with no external CDN dependency:

```bash
patcharena report \
  --format html \
  --group GROUP_ID \
  --output patcharena-report.html
```

The report renders task and agent identity, completion state, requested and observed repeat counts, success rate, duration, patch size, verification details, errors, policy violations, and per-run evidence from the selected persisted records. Running, aborted, and legacy groups remain inspectable but are not eligible for comparison. This README intentionally does not show invented benchmark numbers; a locally generated report contains only measurements from your own run records. JSON and Markdown are also available:

```bash
patcharena report --format json --output patcharena-report.json
patcharena report --format markdown --output patcharena-report.md
```

## Security

Detached worktrees improve repeatability and reduce accidental edits to the primary checkout, while timeouts, bounded output, environment allowlisting, path validation, and policy checks reduce common failure modes. Linked worktrees still share Git objects, refs, and repository configuration with the primary repository. PatchArena checks selected Git metadata and configured forbidden paths after execution, but these are bounded, post-hoc detectors. They are not a filesystem or Git security boundary.

On Unix, launched setup, agent, and verification processes run in their own process groups. A timeout attempts to terminate the group, and after a direct child exits normally PatchArena also terminates any remaining members of that owned group. A descendant that detaches into another session or process group can still escape. Native Windows currently terminates only the direct child and does not use a Job Object. These controls do **not** prevent an unconfined process from reading or writing other accessible files, consuming all host resources, or using the network.

For untrusted benchmarks, use an ephemeral VM or container with an unprivileged user, clean home directory, no credentials or agent sockets, controlled networking, and OS-enforced resource limits. Dangerous-command and forbidden-path detection is auditable defense in depth, not guaranteed prevention. Run artifacts may contain source code, prompts, URLs, environment-derived text, or other secrets.

See [SECURITY.md](SECURITY.md) for vulnerability reporting and [docs/threat-model.md](docs/threat-model.md) for assumptions, residual risks, and deployment guidance.

## Current limitations

- Only Codex CLI is supported as a production agent.
- Linux and WSL2 are the primary targets; native Windows worktree and process-tree behavior is not yet continuously tested.
- Git worktrees and post-run checks are not a filesystem, process, or network sandbox.
- Unix process-group cleanup covers timeouts and remaining members after normal direct-child exit on a best-effort basis; detached descendants can survive. Native Windows currently terminates only the direct child, including for background descendants.
- Timeouts and output capture do not limit CPU, memory, process count, network traffic, or files written directly by child processes.
- Internal Git subprocesses do not yet have an independent timeout.
- Diff evidence does not include Git-ignored files or uninitialized submodule contents. Independent forbidden-path snapshots can detect changes to configured ignored paths, but each configured root is bounded to 10,000 entries and 64 MiB of file data per snapshot and transient or out-of-budget changes can be missed.
- Policy matching cannot recognize every indirect or semantically equivalent dangerous operation.
- Task commands support quoted arguments, not general shell syntax, unless the task explicitly launches a shell.
- Reports are local artifacts; there is no hosted dashboard or remote result service.
- The benchmark identity pins `HEAD`, the task, and effective PatchArena policy, not the complete execution environment. Reproducibility still depends on pinning the toolchain, dependencies, agent version, model/configuration, and relevant external inputs.

## Roadmap

- Add native Windows Job Object termination, strengthen handling of detached Unix descendants, and document container profiles.
- Add native Windows CI after worktree lifecycle behavior is reliable there.
- Add more production agent adapters behind the existing runner abstraction.
- Improve controlled experiment metadata for instruction-on/off comparisons.
- Add schema migration tooling and richer statistical summaries.
- Add artifact retention and opt-in redaction workflows.

Roadmap items are intentions, not release commitments.

## Contributing

Issues and focused pull requests are welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md),
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md), [AGENTS.md](AGENTS.md),
[the architecture](docs/architecture.md), and [the threat model](docs/threat-model.md) before
changing behavior. Security reports must follow [SECURITY.md](SECURITY.md), not a public issue.

The minimum local verification is:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked --workspace --release
```

Do not include API keys, real run logs, `.env` files, generated `.patcharena` data, or benchmark claims unsupported by reproducible records.

User-visible changes should be recorded in [CHANGELOG.md](CHANGELOG.md) under `Unreleased`.

## License

Licensed under the [Apache License 2.0](LICENSE).
