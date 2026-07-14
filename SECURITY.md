# Security policy

PatchArena executes commands and invokes coding agents against real repositories. Treat every task prompt, task file, target repository, agent, and generated patch as potentially untrusted.

## Reporting a vulnerability

Please do not disclose an exploitable issue in a public issue, discussion, or pull request. Use the repository's **Security** tab and choose **Report a vulnerability** to open a private GitHub security advisory. If private reporting is unavailable, contact a maintainer through a private channel listed in the repository metadata before sharing reproduction details.

Include, when available:

- the affected PatchArena revision or release;
- the operating system and relevant tool versions;
- a minimal reproduction that does not contain real credentials;
- the expected and observed security boundary;
- the likely impact and any suggested mitigation.

Do not test against repositories or systems you do not own or have permission to assess. Maintainers will acknowledge and triage reports as capacity permits; no fixed response-time SLA is currently offered.

## Supported versions

| Version | Supported |
|---|---|
| Unreleased default branch | Yes |
| Tagged releases | None published yet |

Security fixes are made on the default branch and, once releases exist, on the latest supported
release. This early-stage project does not promise backports to older versions.

## Relevant issue classes

Reports are especially useful for command/argument injection, path traversal, symbolic-link worktree escape, unintended secret propagation, unsafe cleanup, incorrect environment filtering, output-limit or timeout bypass, and report injection.

Vulnerabilities in Git, Rust, Codex CLI, an operating system, or a target repository should normally be reported upstream unless PatchArena makes the issue exploitable through its own behavior.

## Operator guidance

PatchArena is not a full sandbox. Run it as an unprivileged, dedicated user in a disposable VM or container when evaluating untrusted agents or repositories. Never expose production credentials, SSH agent sockets, cloud metadata credentials, signing keys, or a writable production checkout to a benchmark run. Review [the threat model](docs/threat-model.md) before use.
