# Release checklist

PatchArena has not published a release yet. This checklist is intentionally manual so a maintainer
reviews every externally visible artifact. Workspace packages currently set `publish = false` and
are not prepared for crates.io publication.

1. Decide the version and intended compatibility guarantees.
2. Move relevant entries from `CHANGELOG.md`'s `Unreleased` section into a dated version section.
3. Confirm `Cargo.toml`, `Cargo.lock`, README requirements, and schema documentation agree.
4. Run the release gate from a clean checkout:

   ```bash
   cargo fmt --all -- --check
   cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
   cargo test --locked --workspace --all-features
   cargo build --locked --workspace --release
   ```

5. Confirm the Rust 1.85 MSRV CI job succeeds.
6. Run release-binary smoke checks in a disposable Git repository: `--version`, `init`, `doctor`,
   `task list`, and all three report formats.
7. Review the repository for credentials, `.patcharena` artifacts, local paths, and unsupported
   benchmark claims.
8. Review dependency licenses and known advisories using the project's chosen release tooling.
9. Create a signed or annotated tag only after the commit and changelog are final.
10. Publish release notes that link the changelog, identify known limitations, and include artifact
    checksums when binaries are provided.

Enabling crates.io publication is a separate change. It requires intentional package ownership,
repository metadata, versioned internal dependencies, package-content review, and removal of
`publish = false`; do not combine it casually with an ordinary source release.
