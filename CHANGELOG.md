# Changelog

All notable changes to PatchArena will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases are
intended to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once a first public
version is tagged.

## [Unreleased]

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

[Unreleased]: https://github.com/lasder-ca/PatchArena/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/lasder-ca/PatchArena/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/lasder-ca/PatchArena/releases/tag/v0.1.0
