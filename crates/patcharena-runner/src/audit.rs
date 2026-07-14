use std::path::{Component, Path};

/// Determine whether an observed command contains a forbidden token sequence.
///
/// Token-sequence matching also detects a command behind wrappers such as
/// `/bin/bash -lc`, while avoiding substring matches such as `pushd` for `push`.
#[must_use]
pub fn command_contains_forbidden(command: &str, forbidden: &str) -> bool {
    let Ok(observed) = shell_words::split(command) else {
        return command.contains(forbidden);
    };
    let Ok(needle) = shell_words::split(forbidden) else {
        return command.contains(forbidden);
    };
    if needle.is_empty() {
        return false;
    }
    invocation_matches(&observed, &needle) || shell_payload_matches(&observed, forbidden)
}

fn invocation_matches(observed: &[String], needle: &[String]) -> bool {
    let mut command_index = 0;
    while observed
        .get(command_index)
        .is_some_and(|token| matches!(executable_name(token), "sudo" | "command"))
    {
        command_index += 1;
        while observed
            .get(command_index)
            .is_some_and(|token| token.starts_with('-'))
        {
            command_index += 1;
        }
    }
    if observed
        .get(command_index)
        .is_some_and(|token| executable_name(token) == "env")
    {
        command_index += 1;
        while observed.get(command_index).is_some_and(|token| {
            token.starts_with('-')
                || token
                    .split_once('=')
                    .is_some_and(|(name, _)| !name.is_empty())
        }) {
            command_index += 1;
        }
    }
    let Some(executable) = observed.get(command_index) else {
        return false;
    };
    if executable_name(executable) != executable_name(&needle[0]) {
        return false;
    }
    let remaining = &observed[command_index + 1..];
    let required = &needle[1..];
    if required.is_empty() {
        return true;
    }
    let mut required_index = 0;
    for token in remaining {
        if token == &required[required_index] {
            required_index += 1;
            if required_index == required.len() {
                return true;
            }
        }
    }
    false
}

fn shell_payload_matches(observed: &[String], forbidden: &str) -> bool {
    let Some(shell) = observed.first() else {
        return false;
    };
    if !matches!(
        executable_name(shell),
        "sh" | "bash" | "dash" | "zsh" | "ksh"
    ) {
        return false;
    }
    observed.windows(2).any(|window| {
        window[0].starts_with('-')
            && window[0].contains('c')
            && command_contains_forbidden(&window[1], forbidden)
    })
}

fn executable_name(value: &str) -> &str {
    value.rsplit(['/', '\\']).next().unwrap_or(value)
}

/// Determine whether a changed repository-relative path is at or below a forbidden path.
#[must_use]
pub fn path_is_forbidden(changed: &Path, forbidden: &Path) -> bool {
    let Some(changed) = normalized_components(changed) else {
        return true;
    };
    let Some(forbidden) = normalized_components(forbidden) else {
        return true;
    };
    !forbidden.is_empty()
        && changed.len() >= forbidden.len()
        && changed[..forbidden.len()] == forbidden
}

/// Extract commands from Codex `exec --json` JSONL events.
///
/// Unknown event types are ignored for forward compatibility. Malformed lines are
/// ignored because the raw stdout artifact remains available for investigation.
#[must_use]
pub fn extract_codex_commands(jsonl: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(jsonl)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .flat_map(|value| {
            let mut commands = Vec::new();
            collect_command_events(&value, false, &mut commands);
            commands
        })
        .collect()
}

fn collect_command_events(
    value: &serde_json::Value,
    in_command_event: bool,
    commands: &mut Vec<String>,
) {
    match value {
        serde_json::Value::Object(object) => {
            let is_command_event = in_command_event
                || object
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| {
                        matches!(kind, "command_execution" | "command" | "shell_command")
                    });
            if is_command_event {
                for key in ["command", "cmd"] {
                    if let Some(command) = object.get(key).and_then(serde_json::Value::as_str) {
                        commands.push(command.to_owned());
                    }
                }
            }
            for nested in object.values() {
                collect_command_events(nested, is_command_event, commands);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_command_events(item, in_command_event, commands);
            }
        }
        _ => {}
    }
}

fn normalized_components(path: &Path) -> Option<Vec<&std::ffi::OsStr>> {
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => normalized.push(segment),
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => return None,
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{command_contains_forbidden, extract_codex_commands, path_is_forbidden};

    #[test]
    fn detects_forbidden_command_behind_shell_wrapper() {
        assert!(command_contains_forbidden(
            "/bin/bash -lc 'git push origin main'",
            "git push"
        ));
        assert!(!command_contains_forbidden("echo git pushd", "git push"));
        assert!(command_contains_forbidden(
            "/usr/bin/git -C . push origin main",
            "git push"
        ));
        assert!(!command_contains_forbidden("rg 'git push' .", "git push"));
    }

    #[test]
    fn matches_forbidden_path_boundary() {
        assert!(path_is_forbidden(Path::new(".env"), Path::new(".env")));
        assert!(path_is_forbidden(
            Path::new(".git/config"),
            Path::new(".git")
        ));
        assert!(!path_is_forbidden(
            Path::new(".github/ci.yml"),
            Path::new(".git")
        ));
        assert!(path_is_forbidden(Path::new("../escape"), Path::new(".env")));
    }

    #[test]
    fn extracts_only_command_execution_events() {
        let events = br#"{"type":"thread.started","thread_id":"x"}
{"type":"item.completed","item":{"type":"command_execution","command":"cargo test"}}
{"type":"item.completed","item":{"type":"agent_message","text":"try git push"}}
"#;
        assert_eq!(extract_codex_commands(events), ["cargo test"]);
    }
}
