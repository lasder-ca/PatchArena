# Rust basic example

This example task targets the intentionally broken crate in
`fixtures/broken-rust-project`. Copy that fixture into a disposable Git
repository, run `patcharena init`, then copy `task.yaml` into
`.patcharena/tasks/rust-addition.yaml` and `suite.yaml` into
`.patcharena/suites/rust-basic.yaml`. Commit the task, suite, and fixture before
preflighting a selected installed agent:

```bash
patcharena suite run --suite rust-basic --agents codex --repeat 1 --dry-run
```

The expected repair changes `add` so its existing test passes. The fixture is
not a Cargo workspace member and is never built as part of PatchArena itself.
