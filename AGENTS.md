# PatchArena agent guide

This repository is a Rust 2024 workspace. Keep changes small, testable, and aligned with the security boundary documented in `docs/threat-model.md`.

## Workspace map

- `crates/patcharena-cli`: command parsing, user-facing diagnostics, and orchestration.
- `crates/patcharena-core`: configuration, task and result models, validation, and shared errors.
- `crates/patcharena-git`: repository discovery, temporary worktree lifecycle, and diff statistics.
- `crates/patcharena-runner`: process execution plus `AgentRunner`, Codex, and deterministic fake-agent implementations.
- `crates/patcharena-report`: comparisons and Markdown, JSON, and self-contained HTML rendering.
- `fixtures/`: deterministic test repositories and inputs; never depend on network services.
- `examples/`: small, runnable examples for users.
- `docs/`: architecture and security decisions.

Keep dependency flow toward `patcharena-core`; avoid making lower-level crates depend on the CLI.

## Build and verification

From the workspace root:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked --workspace --release
```

Run the smallest relevant test while iterating, then run all four commands before handing off a change. Tests must not require an installed Codex CLI or network access.

## Coding conventions

- Preserve the workspace MSRV and Rust 2024 edition.
- Do not add `unsafe` code.
- Prefer typed paths, arguments, and domain models over ad hoc strings.
- Pass executable arguments directly to `tokio::process::Command`. Task command strings are tokenized into argv and must not gain implicit shell evaluation; an explicit `sh -c` remains an operator-controlled trust boundary.
- Return contextual errors; do not use panics for normal failures or silently discard errors.
- Document public APIs and keep logging structured with `tracing`.
- Keep serialization backward-aware: result records require `schema_version`, and schema changes need tests and documentation.

## Security invariants

Do not weaken path containment checks, environment-variable allowlisting, output bounds, timeouts, forbidden-operation checks, run-directory permissions, or worktree cleanup. Never copy secret files such as `.env` into a worktree as a convenience. Never follow a user-controlled symlink when creating, writing, collecting, or deleting artifacts. Do not replace argument-array process execution with an interpolated shell command.

PatchArena is not a sandbox; never describe policy checks or worktrees as one.

## Generated files

`Cargo.lock` is tracked because the workspace ships a CLI. Update it only through Cargo and include intentional dependency changes in the review. Do not hand-edit `.patcharena/` run artifacts, generated reports, snapshots, or fixture Git metadata. Generated test artifacts belong in temporary directories and must not be committed.

## Before committing

- Confirm the diff contains no credentials, local paths, run logs, or `.patcharena/` data.
- Add regression tests for behavior changes, including failure paths.
- Update README, examples, architecture, or threat model when a user-visible contract or security assumption changes.
- Run the full verification sequence above and report any command that could not run.
