use std::{path::PathBuf, sync::Arc};

use crate::{
    AdapterRunner, AgentAdapter, AgentContext, AgentDescriptor, AgentInvocation, RunnerError,
    detect_version,
};

use super::configuration_hash;

/// Adapter for the OpenAI Codex CLI.
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    descriptor: AgentDescriptor,
}

impl CodexAdapter {
    /// Build an adapter and best-effort detect its version.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        let executable = executable.into();
        let cli_version = detect_version(&executable, &["--version"]).ok();
        let hash = configuration_hash(&[&executable.to_string_lossy()]);
        Self {
            descriptor: AgentDescriptor {
                id: "codex".into(),
                display_name: "OpenAI Codex".into(),
                executable,
                cli_version,
                adapter_version: env!("CARGO_PKG_VERSION").into(),
                config_hash: hash,
            },
        }
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new("codex")
    }
}

impl AgentAdapter for CodexAdapter {
    fn descriptor(&self) -> &AgentDescriptor {
        &self.descriptor
    }
    fn build_invocation(&self, context: &AgentContext) -> Result<AgentInvocation, RunnerError> {
        let args = vec![
            "--ask-for-approval".into(),
            "never".into(),
            "exec".into(),
            "--ephemeral".into(),
            "--color".into(),
            "never".into(),
            "--json".into(),
            "--sandbox".into(),
            "workspace-write".into(),
            "--cd".into(),
            context.working_dir.clone().into_os_string(),
            "-".into(),
        ];
        let mut audit = vec![self.descriptor.executable.to_string_lossy().into_owned()];
        audit.extend(
            args.iter()
                .map(|value: &std::ffi::OsString| value.to_string_lossy().into_owned()),
        );
        Ok(AgentInvocation {
            program: self.descriptor.executable.clone().into_os_string(),
            args,
            stdin: Some(context.prompt.as_bytes().to_vec()),
            audit_command: shell_words::join(audit),
            prompt_file: None,
        })
    }
}

/// Backward-compatible runner type for the Codex adapter.
#[derive(Debug, Clone)]
pub struct CodexRunner(AdapterRunner);
impl CodexRunner {
    /// Construct using an executable path or PATH name.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self(AdapterRunner::new(Arc::new(CodexAdapter::new(executable))))
    }
}
impl Default for CodexRunner {
    fn default() -> Self {
        Self::new("codex")
    }
}

#[async_trait::async_trait]
impl crate::AgentRunner for CodexRunner {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn descriptor(&self) -> AgentDescriptor {
        self.0.descriptor()
    }
    fn audit_command(&self, context: &AgentContext) -> String {
        self.0.audit_command(context)
    }
    async fn run(&self, context: &AgentContext) -> Result<crate::AgentExecution, RunnerError> {
        self.0.run(context).await
    }
}
