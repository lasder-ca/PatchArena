# Contributing to PatchArena

Thank you for helping improve PatchArena. Focused bug reports and pull requests are welcome.
Because this project executes agent-generated code, changes to process execution, paths, Git,
artifacts, and serialized data receive additional scrutiny.

## Before opening an issue

- Search existing issues first.
- Use the bug template for reproducible defects and include platform and tool versions.
- Use the feature template for a concrete problem, not only a proposed implementation.
- Do not post vulnerabilities or real secrets in a public issue. Follow [SECURITY.md](SECURITY.md).
- Do not attach private repositories, prompts, logs, patches, or `.env` files without permission.

## Development setup

PatchArena targets Linux and WSL2 and requires Git plus Rust 1.85.0 or newer. Coding-agent CLIs
are not required for builds or tests; adapter contracts must remain testable without credentials.

```bash
./prepare.sh
cargo run -p patcharena-cli -- --help
```

The workspace responsibilities and security invariants are documented in [AGENTS.md](AGENTS.md).
Architecture and trust boundaries are described in [docs/architecture.md](docs/architecture.md)
and [docs/threat-model.md](docs/threat-model.md).

## Making a change

1. Keep the change narrowly scoped and avoid unrelated rewrites.
2. Add a regression test that fails without the change.
3. Cover failure paths for process, filesystem, Git, schema, and report changes.
4. Preserve argument-array execution, path containment, output limits, and atomic persistence.
5. Update documentation and `CHANGELOG.md` when behavior visible to users changes.
6. Do not commit generated `.patcharena` data, credentials, or unsupported benchmark claims.

Run the full gate before opening a pull request:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked --workspace --release
```

## Pull requests

Explain the problem, the chosen approach, security implications, and exact verification commands.
Keep commits reviewable. A pull request may be asked to split unrelated work or add negative tests.
Passing CI is necessary but does not replace review of the security boundary.

Maintainers preparing a tag should follow [docs/releasing.md](docs/releasing.md). Publishing to
crates.io is intentionally disabled and is not part of the normal pull-request workflow.

By participating, you agree to follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
