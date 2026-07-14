# Changelog

All notable changes to PatchArena will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases are
intended to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once a first public
version is tagged.

## [Unreleased]

## [0.3.0] - 2026-07-14

### Added

- Versioned `.patcharena/suites/*.yaml` definitions for ordered, reviewable multi-task benchmarks.
- `patcharena suite add`, `list`, `run --dry-run`, `run`, `resume`, and `report` for an explicit
  task-by-agent Cartesian workflow.
- Atomic per-cell suite checkpoints, pending-only resume, and automatic JSON, Markdown, and
  self-contained HTML matrix reports derived from persisted run/group evidence.
- Pre-execution plan output and live per-cell progress emitted only after durable checkpoint writes.
- Coverage and equal-task-weight agent summaries that keep pending and orchestration-error metrics
  absent and deliberately make no winner or statistical-significance claim.

### Changed

- Workspace and CLI version advanced from 0.2.0 to 0.3.0 while existing run/config schema-1
  evidence remains readable.
- `patcharena init` and `doctor` now manage and validate suite definition and generated suite-run
  directories.

### Compatibility and security

- Suite preflight pins the committed revision and each task/effective-policy identity, requires
  available explicit agents, rejects duplicates, limits definitions to 100 tasks, and caps a plan
  at 1,000 agent invocations.
- Resume refuses changes to the suite fingerprint, repository commit, task identities, agent order,
  repetition, or instruction condition; identities are rechecked after every group, and completed
  cells are never rerun.
- Suite-run child-directory links are rejected before reads and checkpoint replacements.
- Suite reports reject missing, duplicate, unreferenced, or incompatible group evidence and
  revalidate aggregates against persisted run details while retaining direct group and run IDs.
- Suites retain the existing security boundary: detached worktrees, direct argv construction,
  limits, and audits are defense in depth, not a filesystem, process, network, or cost sandbox.

## [0.2.0] - 2026-07-14

### Added

- Extensible agent adapters and registry commands for Codex CLI, Claude Code, Gemini CLI, and
  shell-free project-local custom agents.
- `patcharena agent list`, `patcharena agent doctor <id>`, and sequential `patcharena battle` with
  independent worktrees, shared base-commit checks, partial-failure continuation, and JSON summaries.
- Additive run metadata for PatchArena/CLI/adapter versions, redacted commands, host OS/architecture,
  repeat index, and agent-configuration hashes.
- English and Japanese documentation for custom agents, fairness, compatibility, and limitations.

### Changed

- Workspace and CLI version advanced from 0.1.0 to 0.2.0 while result/config schema version remains 1.

### Compatibility

- Existing v0.1.x schema-1 run and group JSON remains readable; the legacy string `agent` field is retained.

## [0.1.0] - 2026-07-13

### Added

- Rust workspace with separated CLI, domain, Git, runner, and reporting crates.
- Repository initialization and validated YAML task management.
- Repeated Codex CLI execution in pinned detached Git worktrees.
- Versioned run artifacts, command audits, diff statistics, policy violation detection, and
  incomplete-group lifecycle tracking.
- Compatible-group comparison and Markdown, JSON, and self-contained HTML reports.
- Deterministic fake-agent, process, path, worktree, CLI, comparison, and report tests.
- Linux CI, an explicit Rust 1.85 MSRV job, security documentation, and contributor guidance.
- A complete Japanese README with language navigation from the English README.

[Unreleased]: https://github.com/lasder-ca/PatchArena/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/lasder-ca/PatchArena/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/lasder-ca/PatchArena/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/lasder-ca/PatchArena/releases/tag/v0.1.0
