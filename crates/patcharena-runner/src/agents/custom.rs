use super::configuration_hash;
use crate::{
    AgentAdapter, AgentContext, AgentDescriptor, AgentInvocation, RunnerError, detect_version,
};
use patcharena_core::ensure_safe_relative_path;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

/// Validated project-local custom executable adapter.
#[derive(Debug, Clone)]
pub struct CustomAdapter {
    descriptor: AgentDescriptor,
    args: Vec<String>,
    timeout_seconds: Option<u64>,
    repository_relative: bool,
}
impl CustomAdapter {
    /// Build a custom adapter. Configuration should already have passed core validation.
    pub fn new(
        id: &str,
        command: &str,
        args: Vec<String>,
        timeout_seconds: Option<u64>,
    ) -> Result<Self, RunnerError> {
        Self::new_in(id, command, args, timeout_seconds, Path::new("."))
    }

    /// Build a custom adapter and resolve version detection relative to a repository root.
    pub fn new_in(
        id: &str,
        command: &str,
        args: Vec<String>,
        timeout_seconds: Option<u64>,
        repository_root: &Path,
    ) -> Result<Self, RunnerError> {
        let configured_executable = PathBuf::from(command);
        let repository_relative = command.contains('/') || command.contains('\\');
        let executable = PathBuf::from(command.strip_prefix("./").unwrap_or(command));
        if command.contains('/') || command.contains('\\') {
            ensure_safe_relative_path(&executable)?;
        }
        let detection_executable = if repository_relative {
            repository_root.join(&executable)
        } else {
            configured_executable
        };
        let cli_version = detect_version(&detection_executable, &["--version"]).ok();
        let mut parts = vec![id, command];
        parts.extend(args.iter().map(String::as_str));
        Ok(Self {
            descriptor: AgentDescriptor {
                id: id.into(),
                display_name: id.into(),
                executable,
                cli_version,
                adapter_version: env!("CARGO_PKG_VERSION").into(),
                config_hash: configuration_hash(&parts),
            },
            args,
            timeout_seconds,
            repository_relative,
        })
    }
    /// Optional custom timeout ceiling.
    pub fn timeout_seconds(&self) -> Option<u64> {
        self.timeout_seconds
    }
}
impl AgentAdapter for CustomAdapter {
    fn descriptor(&self) -> &AgentDescriptor {
        &self.descriptor
    }
    fn timeout_seconds(&self) -> Option<u64> {
        self.timeout_seconds
    }
    fn build_invocation(&self, context: &AgentContext) -> Result<AgentInvocation, RunnerError> {
        let prompt_file = context.result_dir.join("prompt.tmp");
        let prompt_file_text = prompt_file.to_string_lossy();
        let workspace_text = context.working_dir.to_string_lossy();
        let result_dir_text = context.result_dir.to_string_lossy();
        let values = [
            ("prompt", context.prompt.as_str()),
            ("prompt_file", prompt_file_text.as_ref()),
            ("workspace", workspace_text.as_ref()),
            ("task_id", &context.task_id),
            ("run_id", &context.run_id),
            ("result_dir", result_dir_text.as_ref()),
        ];
        let mut args = Vec::new();
        let mut audit = Vec::new();
        let mut needs_prompt_file = false;
        for template in &self.args {
            let mut expanded = template.clone();
            let mut redacted = template.clone();
            for (name, value) in values {
                let marker = format!("{{{name}}}");
                if expanded.contains(&marker) {
                    expanded = expanded.replace(&marker, value);
                    redacted = redacted.replace(
                        &marker,
                        if name == "prompt" {
                            "<redacted:prompt>"
                        } else {
                            value
                        },
                    );
                    if name == "prompt_file" {
                        needs_prompt_file = true;
                    }
                }
            }
            args.push(OsString::from(expanded));
            audit.push(redact_sensitive_argument(redacted));
        }
        let program = if self.repository_relative {
            context
                .working_dir
                .join(Path::new(&self.descriptor.executable))
                .into_os_string()
        } else {
            self.descriptor.executable.clone().into_os_string()
        };
        let command =
            shell_words::join(std::iter::once(program.to_string_lossy().into_owned()).chain(audit));
        Ok(AgentInvocation {
            program,
            args,
            stdin: None,
            audit_command: command,
            prompt_file: needs_prompt_file.then_some(prompt_file),
        })
    }
}

fn redact_sensitive_argument(argument: String) -> String {
    let lower = argument.to_ascii_lowercase();
    if [
        "token",
        "secret",
        "password",
        "api-key",
        "api_key",
        "authorization",
    ]
    .iter()
    .any(|name| lower.contains(name))
    {
        if let Some((name, _)) = argument.split_once('=') {
            return format!("{name}=<redacted:secret>");
        }
        return "<redacted:secret>".to_owned();
    }
    argument
}
