# Threat model

## Summary

**PatchArena is not a full sandbox.** A Git worktree separates the benchmark checkout from the primary checkout and improves repeatability, but an agent or task command runs with the operating-system authority of the PatchArena process. Policy checks, environment filtering, output bounds, and timeouts reduce risk; they do not provide a kernel, filesystem, process, or network security boundary.

Use a disposable VM or container and an unprivileged dedicated account for untrusted repositories, prompts, tasks, or agents. Do not run PatchArena on a workstation that exposes valuable credentials or writable production resources.

## Assets and security goals

PatchArena aims to protect:

- files outside the temporary worktree and designated run directory;
- repository history, remotes, and the user's Git configuration;
- credentials in environment variables, home-directory files, credential helpers, and agent sockets;
- availability of the host through bounded time and captured output;
- integrity and provenance of task definitions, patches, logs, results, and reports;
- reproducibility: a failure or policy violation must not be silently represented as success.

## Untrusted inputs

Treat all of the following as attacker-controlled:

- the target repository, including hooks, submodules, build scripts, symlinks, and filenames;
- task YAML, prompts, setup commands, and verification commands;
- agent-generated source, commands, output, and filenames;
- existing `.patcharena` contents and imported run records;
- output consumed by Markdown or HTML report generation.

Git, any shell explicitly selected by a task, Codex CLI, compilers, package managers, test programs, and dependencies are privileged external components from PatchArena's perspective.

## Trust boundaries and controls

### Worktree separation and filesystem paths

Runs use a fresh detached Git worktree so repeats do not intentionally edit the user's primary checkout. It remains a linked worktree: the checkout shares the repository's common object database, refs, and configuration. PatchArena compares selected Git state before and after execution, but that is detection rather than prevention and is not a complete audit of the object database, reflogs, hooks, remotes, or network side effects.

Every task ID, run ID, artifact name, forbidden path, and derived path must be validated before joining it to a root. Path traversal is rejected: absolute paths, empty or special components, `..` components, platform prefixes, and paths whose canonical destination escapes the expected root are invalid.

Canonicalization alone is not sufficient when a final path does not yet exist or can be raced. File creation, artifact collection, and cleanup must check existing ancestors and avoid following user-controlled symbolic links. Repository symlinks may point outside the worktree; commands executed inside the worktree can follow them. PatchArena detects forbidden or escaped paths where feasible, but it cannot prevent arbitrary filesystem access by an unconfined child process.

Cleanup is limited to a worktree that PatchArena created and recorded. It must not use a broad recursive deletion against a path derived only from user input. Git worktree metadata is cleaned up without changing the user's global Git configuration.

### Commands, arguments, and shell execution

PatchArena-owned Git and Codex invocations use a program plus an argument array rather than interpolated shell text. Task `setup` and `verify` strings are parsed into a program and arguments without invoking a shell. POSIX-style quoting is supported, but substitution, redirection, pipelines, and shell operators are not evaluated. This reduces command-injection exposure, but a task can still name any executable and pass dangerous arguments to it. Only run task definitions you trust, or run them inside an external sandbox.

Invoking an explicit shell (for example, configuring `sh -c` as the executable) restores shell parsing and its command-substitution, expansion, redirection, and pipeline risks. A future built-in shell mode would widen the trust boundary and must remain opt-in, conspicuous, and independently reviewed.

Detection of dangerous operations such as `git push` and `cargo publish` is defense in depth and creates auditable policy violations. Text matching cannot identify every equivalent operation, indirect script, custom executable, network client, Git alias, or encoded command. Detection must never be described as prevention.

### Environment and secrets

Child processes receive an explicit environment-variable allowlist needed for basic tool operation instead of inheriting the complete parent environment. Adding a variable to that allowlist is a security-sensitive change. In particular, cloud credentials, tokens, credential-helper variables, signing configuration, SSH/GPG agent sockets, and application secrets should remain excluded.

An allowlist cannot protect secrets readable from the filesystem, credential stores, keyrings, cloud metadata endpoints, running processes, or already-checked-in files. PatchArena does not copy untracked files such as `.env` into new worktrees as a convenience, but tracked secret files remain part of the repository. Git tools, build systems, and the agent may also consult files from the user's home directory. Use a clean home directory and external network/filesystem isolation for hostile workloads.

Never put secrets in prompts, task YAML, command-line arguments, repository URLs, filenames, or configuration. They may be persisted in process listings or artifacts.

### Time, process, and output limits

Project numeric defaults are safety ceilings, not merely task templates. The effective timeout and retained-output cap are the smaller of the task and project values and apply separately to each launched setup, agent, and verification process. They are not an overall run deadline or an OS resource quota.

On Unix, each of those processes starts in a new process group. On timeout, the runner attempts to terminate the group, falls back to the direct child if group termination fails, and records failure. After a direct child exits normally, the runner also terminates remaining members of its owned group. A descendant that creates another session or process group can survive either path. Native Windows currently terminates only the direct child and has no Job Object/process-tree boundary, so background descendants can survive normal completion as well as timeouts. Use an external container or supervisor when descendant termination is a security requirement.

Internal Git subprocesses do not currently have a separate deadline. A corrupt repository, filesystem stall, or pathological Git operation can therefore delay a run; an external supervisor should enforce an overall wall-clock limit.

Captured stdout and stderr have byte limits to prevent unbounded in-memory and on-disk logs. Pipe draining after timeout is also bounded so a surviving descendant holding a pipe open cannot block the runner indefinitely. Truncation is recorded and must not turn a failed command into a success. Output limits do not bound files created directly by the agent, compiler caches, repository growth, CPU, memory, process count, or network traffic. Apply OS/container resource quotas for those controls.

### Forbidden paths and operation auditing

Git diff and status provide the primary patch inventory, but do not expose all ignored-file or shared-Git-metadata changes. PatchArena therefore also fingerprints every configured forbidden path before command execution and during final inspection, independently of Git. This can detect a change to an ignored path such as `.env`.

The forbidden-path inventory is deliberately bounded per configured root and per snapshot: at most 10,000 entries and 64 MiB of file data are inspected. It is a post-hoc detector, not access control. It can miss a file created and removed between snapshots, a write outside configured roots, a change beyond the inventory budget, or an equal before/after state. Selected Git commit, refs, local configuration, and staged state are also compared, but this is not complete Git or filesystem auditing.

PatchArena records the commands it launches, their durations, exit codes, bounded output, changed files, diff statistics, and detected violations. Audit records can be incomplete if the host crashes or storage fails. An agent can invoke further commands that PatchArena did not directly launch; complete syscall/process auditing requires an external security facility.

### Benchmark identity and comparison integrity

Each current run records the exact repository `HEAD` commit and a SHA-256 fingerprint of the task plus the resolved PatchArena policy: effective caps, environment allowlist, and merged forbidden commands and paths. `compare` rejects different task IDs, missing or unequal identities, and unequal sample sizes. This prevents accidental aggregation of visibly incompatible records.

The identity is not signed and does not defend against maliciously edited artifacts. It intentionally leaves the agent and instructions-on/off condition outside the fingerprint so those can be experimental variables. It also does not capture toolchain binaries, dependency caches, model/configuration, host state, time-dependent services, or network responses. Operators must control and record those inputs separately before treating a comparison as reproducible.

### Run artifacts, permissions, and logs

Run directories should be created with owner-only permissions where supported. Existing directories and symlinks are rejected rather than reused blindly, and result files should be written without following symlinks. Other operating systems, inherited ACLs, backup tools, and filesystem mounts may weaken mode-bit guarantees.

Logs, patches, HTML reports, JSON records, and error messages can contain secrets from source files, child-process output, expanded commands, URLs, or agent responses. Output capture does not perform reliable secret redaction. Protect `.patcharena/runs`, avoid publishing artifacts without review, and apply retention/deletion policies appropriate to the repository. Report generators must escape untrusted text to prevent HTML/script injection when a report is opened.

## Threats not fully mitigated in the MVP

- arbitrary reads, writes, network access, and process execution by an agent or task command;
- malicious compilers, package scripts, Git filters/hooks, dependencies, or toolchain components;
- symlink races and filesystem features not represented in a Git diff;
- denial of service through CPU, memory, disk, process count, files outside captured output, detached Unix descendants, or descendant processes on native Windows;
- secret access through the filesystem, credential stores, metadata services, or permitted environment variables;
- exfiltration through network access, DNS, logs, patches, or reports;
- hostile repositories exploiting vulnerabilities in PatchArena or external tools;
- semantic policy bypasses that do not match forbidden-command or forbidden-path rules;
- forbidden-path changes outside configured roots, between snapshots, or beyond inventory limits.

## Recommended deployment profile

For untrusted benchmarks:

1. Create an ephemeral VM or rootless container with no host mounts beyond a disposable repository copy.
2. Use a dedicated, unprivileged UID and a clean home directory.
3. Remove secrets, credential helpers, agent sockets, and cloud metadata access.
4. Deny network access unless the benchmark explicitly requires a controlled mirror.
5. Apply CPU, memory, process, file-size, total-disk, and wall-clock limits externally.
6. Review task definitions before execution and inspect artifacts before publishing them.
7. Destroy the environment after the run.

## Security review checklist

Changes to process spawning, environment construction, path resolution, worktree creation/cleanup, artifact writes, HTML escaping, schema parsing, or policy matching require focused negative tests. At minimum, cover traversal, absolute paths, symbolic links, oversized output, timeout/descendant behavior, forbidden paths, invalid schema versions, and secret-like environment variables.
