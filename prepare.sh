#!/usr/bin/env bash
set -Eeuo pipefail

if [[ -t 1 ]]; then
  COLOR_BLUE=$'\033[1;34m'
  COLOR_GREEN=$'\033[1;32m'
  COLOR_RED=$'\033[1;31m'
  COLOR_RESET=$'\033[0m'
else
  COLOR_BLUE=''
  COLOR_GREEN=''
  COLOR_RED=''
  COLOR_RESET=''
fi

step() {
  printf '%s==>%s %s\n' "$COLOR_BLUE" "$COLOR_RESET" "$*"
}

ok() {
  printf '%sOK%s  %s\n' "$COLOR_GREEN" "$COLOR_RESET" "$*"
}

fail() {
  printf '%sERROR%s %s\n' "$COLOR_RED" "$COLOR_RESET" "$*" >&2
  exit 1
}

on_error() {
  local exit_code=$?
  local line=$1
  local command=$2
  printf '%sERROR%s command failed (exit %d) at line %s: %s\n' \
    "$COLOR_RED" "$COLOR_RESET" "$exit_code" "$line" "$command" >&2
  exit "$exit_code"
}

trap 'on_error "$LINENO" "$BASH_COMMAND"' ERR

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
cd -- "$SCRIPT_DIR"

[[ -f Cargo.toml ]] || fail "Cargo.toml was not found in $SCRIPT_DIR"

step "Checking required commands"
for command_name in git rustc cargo sed; do
  command -v "$command_name" >/dev/null 2>&1 \
    || fail "Required command '$command_name' was not found. Install it manually, then rerun this script."
  ok "Found $command_name"
done

if command -v codex >/dev/null 2>&1; then
  ok "Found codex (required only for production agent runs)"
else
  printf 'WARN Codex CLI was not found; the workspace can still build and test, but patcharena run --agent codex will be unavailable.\n' >&2
fi

if ! cargo clippy --version >/dev/null 2>&1; then
  fail "The Rust Clippy component is unavailable. Install it manually for the active toolchain, then rerun this script."
fi
ok "Found cargo-clippy"

step "Rust toolchain"
rustc --version
cargo --version
cargo clippy --version

if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  ok "Repository: $(git rev-parse --show-toplevel)"
else
  printf 'WARN This directory is not currently inside a Git worktree; Git-dependent tests may be unavailable.\n' >&2
fi

step "Fetching Cargo dependencies"
cargo fetch --locked

step "Building the workspace"
cargo build --locked --workspace

step "Testing the workspace"
cargo test --locked --workspace

step "Running Clippy"
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings

metadata=$(cargo metadata --format-version 1 --no-deps)
target_dir=$(printf '%s\n' "$metadata" | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
[[ -n "$target_dir" ]] || fail "Cargo did not report a target directory."

binary_path="$target_dir/debug/patcharena"
step "Preparation complete"
if [[ -x "$binary_path" ]]; then
  ok "Binary: $binary_path"
else
  printf 'Binary is expected at: %s\n' "$binary_path"
  printf 'If the workspace uses a custom target layout, run: cargo run -p patcharena-cli -- --help\n'
fi

printf '\nNext commands:\n'
printf '  %q doctor\n' "$binary_path"
printf '  %q init\n' "$binary_path"
printf '  cargo run -p patcharena-cli -- --help\n'
