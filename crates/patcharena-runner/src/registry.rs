//! Agent discovery and project configuration registry.

use crate::{
    AdapterRunner, AgentDescriptor, AgentRunner, ClaudeAdapter, CodexAdapter, CustomAdapter,
    GeminiAdapter, RunnerError,
};
use patcharena_core::{AgentConfig, ProjectConfig};
use std::{collections::BTreeMap, path::Path, sync::Arc};

/// Built-in and configured agent registry.
pub struct AgentRegistry {
    agents: BTreeMap<String, Arc<dyn crate::AgentAdapter>>,
}
impl std::fmt::Debug for AgentRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentRegistry")
            .field("agents", &self.agents.keys().collect::<Vec<_>>())
            .finish()
    }
}
impl AgentRegistry {
    /// Construct a registry from built-ins plus validated project configuration.
    pub fn from_config(config: &ProjectConfig) -> Result<Self, RunnerError> {
        Self::from_project(config, Path::new("."))
    }

    /// Construct a registry, resolving custom executable detection from a repository root.
    pub fn from_project(
        config: &ProjectConfig,
        repository_root: &Path,
    ) -> Result<Self, RunnerError> {
        let mut agents: BTreeMap<String, Arc<dyn crate::AgentAdapter>> = BTreeMap::new();
        agents.insert("codex".into(), Arc::new(CodexAdapter::default()));
        agents.insert("claude".into(), Arc::new(ClaudeAdapter::default()));
        agents.insert("gemini".into(), Arc::new(GeminiAdapter::default()));
        for (id, value) in &config.agents {
            match value {
                AgentConfig::Custom {
                    command,
                    args,
                    timeout_seconds,
                } => {
                    agents.insert(
                        id.clone(),
                        Arc::new(CustomAdapter::new_in(
                            id,
                            command,
                            args.clone(),
                            *timeout_seconds,
                            repository_root,
                        )?),
                    );
                }
            }
        }
        Ok(Self { agents })
    }
    /// Return descriptors sorted by stable ID.
    pub fn list(&self) -> Vec<AgentDescriptor> {
        self.agents
            .values()
            .map(|agent| agent.descriptor().clone())
            .collect()
    }
    /// Resolve one agent ID to an executable runner.
    pub fn runner(&self, id: &str) -> Result<Arc<dyn AgentRunner>, RunnerError> {
        self.agents
            .get(id)
            .cloned()
            .map(|agent| Arc::new(AdapterRunner::new(agent)) as Arc<dyn AgentRunner>)
            .ok_or_else(|| RunnerError::Agent(format!("unknown agent `{id}`")))
    }
    /// Return one descriptor.
    pub fn descriptor(&self, id: &str) -> Option<AgentDescriptor> {
        self.agents.get(id).map(|agent| agent.descriptor().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::AgentRegistry;
    use patcharena_core::{AgentConfig, ProjectConfig};

    #[test]
    fn registry_contains_builtins_and_configured_agents() {
        let mut config = ProjectConfig::default();
        config.agents.insert(
            "local".into(),
            AgentConfig::Custom {
                command: "missing-local-agent".into(),
                args: Vec::new(),
                timeout_seconds: None,
            },
        );
        let registry = AgentRegistry::from_config(&config).expect("registry");
        let ids = registry
            .list()
            .into_iter()
            .map(|value| value.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["claude", "codex", "gemini", "local"]);
        assert!(registry.runner("unknown").is_err());
    }
}
