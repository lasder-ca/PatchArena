//! Built-in and project-local agent adapters.

mod claude;
mod codex;
mod custom;
mod gemini;

pub use claude::ClaudeAdapter;
pub use codex::{CodexAdapter, CodexRunner};
pub use custom::CustomAdapter;
pub use gemini::GeminiAdapter;

use sha2::{Digest, Sha256};

pub(crate) fn configuration_hash(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.len().to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::{ClaudeAdapter, CodexAdapter, CustomAdapter, GeminiAdapter};
    use crate::{AgentAdapter, AgentContext};
    use std::{path::PathBuf, time::Duration};

    fn context() -> AgentContext {
        AgentContext {
            working_dir: PathBuf::from("/workspace"),
            prompt: "do work; $(unsafe)".into(),
            timeout: Duration::from_secs(60),
            max_output_bytes: 1024,
            env_allowlist: vec!["PATH".into()],
            task_id: "task".into(),
            run_id: "run".into(),
            result_dir: PathBuf::from("/result"),
        }
    }

    #[test]
    fn built_in_adapters_construct_expected_shell_free_arguments() {
        let context = context();
        let codex = CodexAdapter::new("codex-missing")
            .build_invocation(&context)
            .expect("codex args");
        assert_eq!(codex.args.last().and_then(|v| v.to_str()), Some("-"));
        assert_eq!(codex.stdin.as_deref(), Some(context.prompt.as_bytes()));
        let claude = ClaudeAdapter::new("claude-missing")
            .build_invocation(&context)
            .expect("claude args");
        assert!(claude.args.iter().any(|value| value == "--print"));
        let gemini = GeminiAdapter::new("gemini-missing")
            .build_invocation(&context)
            .expect("gemini args");
        assert!(gemini.args.iter().any(|value| value == "--approval-mode"));
    }

    #[test]
    fn custom_placeholders_are_single_argv_values_and_secrets_are_redacted() {
        let adapter = CustomAdapter::new(
            "local",
            "agent",
            vec![
                "--prompt={prompt}".into(),
                "--api-key=super-secret".into(),
                "{prompt_file}".into(),
            ],
            Some(10),
        )
        .expect("custom");
        let invocation = adapter.build_invocation(&context()).expect("invocation");
        assert_eq!(invocation.args[0], "--prompt=do work; $(unsafe)");
        assert!(!invocation.audit_command.contains("super-secret"));
        assert!(!invocation.audit_command.contains("$(unsafe)"));
        assert!(invocation.prompt_file.is_some());
        assert_eq!(adapter.timeout_seconds(), Some(10));

        let relative = CustomAdapter::new("relative", "./bin/agent", Vec::new(), None)
            .expect("relative custom");
        let invocation = relative
            .build_invocation(&context())
            .expect("relative invocation");
        assert_eq!(invocation.program, "/workspace/bin/agent");
    }
}
