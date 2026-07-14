## Problem

<!-- What user-visible or maintenance problem does this solve? -->

## Approach

<!-- Summarize the smallest important design decisions. -->

## Security and compatibility

<!-- Address process, path, Git, secret, artifact, schema, and MSRV impact as applicable. -->

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --locked --workspace --all-features`
- [ ] `cargo build --locked --workspace --release`
- [ ] User-facing behavior and `CHANGELOG.md` are updated when needed
- [ ] No secrets, generated run artifacts, or unsupported benchmark claims are included
