use super::configuration_hash;
use crate::{
    AgentAdapter, AgentContext, AgentDescriptor, AgentInvocation, RunnerError, detect_version,
};
use std::path::PathBuf;

/// Adapter for Google Gemini CLI.
#[derive(Debug, Clone)]
pub struct GeminiAdapter {
    descriptor: AgentDescriptor,
}
impl GeminiAdapter {
    /// Build an adapter and best-effort detect its version.
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        let executable = executable.into();
        Self {
            descriptor: AgentDescriptor {
                id: "gemini".into(),
                display_name: "Google Gemini CLI".into(),
                cli_version: detect_version(&executable, &["--version"]).ok(),
                adapter_version: env!("CARGO_PKG_VERSION").into(),
                config_hash: configuration_hash(&[&executable.to_string_lossy()]),
                executable,
            },
        }
    }
}
impl Default for GeminiAdapter {
    fn default() -> Self {
        Self::new("gemini")
    }
}
impl AgentAdapter for GeminiAdapter {
    fn descriptor(&self) -> &AgentDescriptor {
        &self.descriptor
    }
    fn build_invocation(&self, context: &AgentContext) -> Result<AgentInvocation, RunnerError> {
        let args = vec![
            "--approval-mode".into(),
            "yolo".into(),
            "--output-format".into(),
            "stream-json".into(),
        ];
        let audit = shell_words::join(
            std::iter::once(self.descriptor.executable.to_string_lossy().into_owned()).chain(
                args.iter()
                    .map(|v: &std::ffi::OsString| v.to_string_lossy().into_owned()),
            ),
        );
        Ok(AgentInvocation {
            program: self.descriptor.executable.clone().into_os_string(),
            args,
            stdin: Some(context.prompt.as_bytes().to_vec()),
            audit_command: audit,
            prompt_file: None,
        })
    }
}
