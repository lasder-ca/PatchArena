# Architecture

PatchArena turns a versioned task definition, repository revision, and effective execution policy into repeatable run records, then compares or renders those records. The design keeps domain data independent of process, Git, and presentation concerns so security-sensitive boundaries can be reviewed in isolation.

## Workspace responsibilities

| Crate | Responsibility |
| --- | --- |
| `patcharena-core` | Configuration, YAML task definitions, run/result schemas, validation, identifiers, and shared errors. |
| `patcharena-git` | Repository discovery, temporary Git worktrees, cleanup, patch capture, changed-file enumeration, and diff statistics. |
| `patcharena-runner` | Run orchestration, bounded process execution, the adapter registry, Codex/Claude/Gemini/custom adapters, and deterministic fake runners. |
| `patcharena-report` | Run-group aggregation, comparison, and Markdown, JSON, and self-contained HTML output. |
| `patcharena-cli` | `clap` command routing, tracing setup, user-facing diagnostics, and process exit status. |

The preferred dependency direction is CLI/report/runner/git toward core. Git and process details should not leak into serialized domain models unless they are stable parts of the run schema.

## Run flow

```text
task YAML + repository + config
              |
              v
 validate inputs; resolve effective policy
              |
              v
 record HEAD + task/policy fingerprint
              |
              v
 create a detached temporary worktree
              |
              v
 snapshot forbidden paths + selected Git state
              |
              v
 run setup -> agent -> verification
       |          |          |
       +---- bounded process capture
              |
              v
 resnapshot; inspect status/diff/violations
              |
              v
 preserve bounded artifacts in memory
              |
              v
       remove temporary worktree
              |
              v
 atomically persist result + artifacts
```

Each repeat is a separate run with a UUID. A run group associates repeats of the same benchmark invocation so variance and aggregate statistics can be computed without overwriting individual evidence. The empty group record is created before execution with its requested repeat count and `running` state, atomically replaced after each completed repeat, and finally marked `completed` only when every requested repeat is present. A handled hard failure marks it `aborted`; an abrupt host failure can leave `running`, which remains inspectable but ineligible for comparison.

The detached worktree is a repeatability boundary, not a hard isolation boundary. It is a linked worktree and shares the repository's common Git object database, refs, and configuration. Selected Git state is compared before and after a run to expose some mutations, but PatchArena does not prevent every change to shared Git metadata.

## Effective policy and benchmark identity

Project numeric defaults are enforced as safety ceilings. For timeout, retained output, changed files, and diff lines, orchestration uses `min(task_limit, project_limit)`. A stored task may request stricter limits, but it cannot raise a repository cap. The effective timeout and output cap apply to each setup, agent, and verification process; patch caps apply to the final change set.

Before execution, PatchArena records a benchmark identity with two components:

- the exact commit resolved from repository `HEAD`;
- a SHA-256 fingerprint of the serialized task plus the effective caps, environment allowlist, and merged project/task forbidden commands and paths.

The selected agent and instructions-on/off mode remain separate recorded dimensions so they can be compared as experimental variables. The fingerprint is not a signature and does not capture the toolchain, dependency cache, agent/model configuration, filesystem outside the repository, or network responses.

`compare` requires two completed groups whose observed counts equal their requested repeat counts, the same task ID, an exact benchmark-identity match, and equal sample sizes. A running, aborted, or legacy-unknown group, a missing identity, a different `HEAD` or policy fingerprint, or unequal repeat counts is an incompatibility error rather than a statistical result.

## Persistent layout

`patcharena init` creates repository-local state without replacing existing files:

```text
.patcharena/
├── tasks/                 # YAML task definitions
├── groups/                # repeat-run group metadata
└── runs/
    └── <run-id>/
        ├── result.json    # versioned, machine-readable outcome
        ├── stdout.log     # bounded agent output
        ├── stderr.log     # bounded agent diagnostics
        ├── changes.diff   # captured Git patch
        └── audit.jsonl    # optional command audit stream
patcharena.toml            # repository configuration
```

`schema_version` is mandatory in persisted records. Readers should reject unsupported versions with an actionable error rather than guessing. New optional fields may be added compatibly; breaking changes require a new schema version and migration guidance.

## Command execution

PatchArena constructs all current process invocations without an intermediary shell:

- Git and agent commands are constructed as an executable plus an argument array.
- Task `setup` and `verify` entries are human-authored strings, but PatchArena parses them into a program and argument array. POSIX-style quoting is supported; operators such as `|`, `>`, `&&`, command substitution, and variable expansion are not interpreted.
- The runner applies per-process time and retained-output bounds, a restricted environment, and captures command, exit status, duration, stdout, and stderr for auditability.

On Unix, each launched setup, agent, or verification process is placed in a new process group. Timeout handling attempts to kill that group and falls back to the direct child if necessary. After a direct child exits normally, the runner also signals any remaining members of its owned process group before returning. Descendants that create a different session or process group can escape. Native Windows currently has only direct-child termination, including after normal exit; a Job Object/process-tree implementation is not yet present. Internal Git commands do not have an independent timeout.

This avoids a broad class of shell-injection bugs. If a future task mode deliberately invokes a shell, it must be explicit, disabled by default, and documented as a substantially wider trust boundary. The exact inherited environment is configuration-controlled. Secrets must not be placed in task files, command lines, or benchmark repositories.

## Agent abstraction

`AgentRunner` keeps orchestration independent of a specific CLI. `AgentAdapter` implementations own detection, direct argv construction, output parsing, and metadata; the registry combines built-ins with validated project-local custom agents. The fake runner produces deterministic success, failure, timeout, file-change, forbidden-path, and high-output cases. CI verifies invocation contracts without calling an external AI service.

For `--without-instructions`, orchestration runs setup first, scans at most 100,000 worktree directory entries without following symlinked directories, and temporarily hides every discovered regular `AGENTS.md`, including untracked or ignored files. A scan overflow or an `AGENTS.md` symlink is an error. The mask applies only to the agent phase; files are restored before verification, and instruction/context sources outside the worktree are unaffected.

Adding an agent requires an implementation that accepts a validated request, uses argument-array process construction, obeys cancellation and output limits, records enough metadata to reproduce the invocation, and does not expand the inherited environment by default.

## Reporting

Report generation consumes persisted records only; it does not rerun agents or verification commands. JSON is intended for automation, Markdown for review, and HTML for a portable local report. HTML output embeds its styles, requires no external assets, and must escape all repository-, task-, command-, and agent-controlled text.

Comparisons report success rate, median duration, file and line deltas, verification failures, policy violations, and run-to-run variance. They are produced only for complete, equal-size groups with matching requested/observed counts, task IDs, and benchmark identities. Reports can still render incomplete groups and label their status. Missing or incompatible evidence is reported explicitly rather than treated as success.

## Failure and cleanup behavior

A failed setup, agent, verification, artifact write, or cleanup step remains a failed/incomplete run. Cleanup is scoped to worktrees created by PatchArena and must verify ownership and containment before removal. PatchArena keeps bounded output and the captured patch in memory until cleanup finishes so the final immutable result can include cleanup failure; an abrupt host failure before persistence can still lose that evidence. Group metadata is checkpointed after each completed repeat, records expected membership and lifecycle state, and an abort error carries its group ID. Errors encountered while preserving evidence or cleaning up are surfaced alongside the primary failure.

Worktree separation is useful for repeatability, but it is not a security sandbox. The complete trust model and residual risks are in [threat-model.md](threat-model.md).
