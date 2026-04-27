use std::collections::{HashMap, HashSet};
use std::path::Path;

use indexmap::IndexMap;
use serde::Deserialize;

// --- Errors ---

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse config: {0}")]
    Parse(#[from] serde_yaml::Error),

    #[error("validation error: {0}")]
    Validation(String),
}

// --- Types ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    Api,
    ClaudeCli,
    Codex,
    Ollama,
}

/// Accepts both `"1"` (string) and `1` (integer) in YAML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version(pub String);

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct VersionVisitor;

        impl serde::de::Visitor<'_> for VersionVisitor {
            type Value = Version;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a version string or integer")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Version, E> {
                Ok(Version(v.to_string()))
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Version, E> {
                Ok(Version(v.to_string()))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Version, E> {
                Ok(Version(v.to_string()))
            }
        }

        deserializer.deserialize_any(VersionVisitor)
    }
}

// --- Raw serde structs (what we deserialize from YAML) ---

/// Role default (maps role name to default agent ID).
#[derive(Debug, Deserialize)]
pub struct RawRoleDefault {
    pub default: String,
}

/// Flow config file format (lives in .koto/flows/<name>.yaml).
#[derive(Debug, Deserialize)]
pub struct RawFlowConfig {
    pub version: Version,
    pub name: String,
    pub prompt: Option<String>,
    #[serde(default)]
    pub defaults: Option<RawDefaults>,
    #[serde(default)]
    pub roles: HashMap<String, RawRoleDefault>,
    pub flow: IndexMap<String, RawStep>,
    #[serde(default)]
    pub stack: Option<RawStackConfig>,
    #[serde(flatten)]
    pub unknown: HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
pub struct RawDefaults {
    pub model: Option<String>,
    pub backend: Option<Backend>,
    #[serde(flatten)]
    pub unknown: HashMap<String, serde_yaml::Value>,
}

/// A step in the flow map. The key in the map is the step ID.
#[derive(Debug, Deserialize)]
pub struct RawStep {
    pub agent: Option<String>,
    pub role: Option<String>,
    pub task: Option<String>,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    pub model: Option<String>,
    pub backend: Option<Backend>,
    #[serde(default)]
    pub print_output: bool,
    #[serde(flatten)]
    pub unknown: HashMap<String, serde_yaml::Value>,
}

/// Agent file format (lives in .koto/agents/<id>.yaml).
#[derive(Debug, Deserialize)]
pub struct RawAgentFile {
    pub name: String,
    pub title: Option<String>,
    pub role: String,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    pub model: Option<String>,
    pub backend: Option<Backend>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(flatten)]
    pub unknown: HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
pub struct RawStackConfig {
    pub backend: Option<String>,
    pub path: Option<String>,
    #[serde(flatten)]
    pub unknown: HashMap<String, serde_yaml::Value>,
}

// --- Resolved structs (after validation and defaults) ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowConfig {
    pub version: String,
    pub name: String,
    pub prompt: Option<String>,
    pub defaults: Defaults,
    pub roles: HashMap<String, String>,
    pub steps: Vec<Step>,
    pub stack: StackConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defaults {
    pub model: String,
    pub backend: Backend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub title: Option<String>,
    pub role: String,
    pub model: String,
    pub backend: Backend,
    pub rules: Vec<String>,
    pub skills: Vec<String>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    pub agent: String,
    pub task: Option<String>,
    pub input: Vec<String>,
    pub needs: Vec<String>,
    pub model: Option<String>,
    pub backend: Option<Backend>,
    pub print_output: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackConfig {
    pub backend: String,
    pub path: String,
}

// --- Constants ---

const DEFAULT_MODEL: &str = "claude-sonnet-4-5";
const DEFAULT_BACKEND: Backend = Backend::ClaudeCli;
const DEFAULT_STACK_BACKEND: &str = "local";

// --- Loading ---

#[allow(dead_code)]
pub fn load_flow(path: &Path) -> Result<FlowConfig, ConfigError> {
    let contents = std::fs::read_to_string(path)?;
    load_flow_from_str(&contents)
}

#[allow(dead_code)]
pub fn load_flow_from_str(contents: &str) -> Result<FlowConfig, ConfigError> {
    load_flow_from_str_with_overrides(contents, &HashMap::new())
}

/// Load flow from YAML string with role overrides.
pub fn load_flow_from_str_with_overrides(
    contents: &str,
    role_overrides: &HashMap<String, String>,
) -> Result<FlowConfig, ConfigError> {
    let raw: RawFlowConfig = serde_yaml::from_str(contents)?;
    warn_unknown_fields("top-level", &raw.unknown);
    if let Some(ref defaults) = raw.defaults {
        warn_unknown_fields("defaults", &defaults.unknown);
    }
    for (id, step) in &raw.flow {
        warn_unknown_fields(&format!("step '{id}'"), &step.unknown);
    }
    if let Some(ref stack) = raw.stack {
        warn_unknown_fields("stack", &stack.unknown);
    }
    validate_and_resolve(raw, role_overrides)
}

/// Parse just the role names from a flow YAML (for CLI arg partitioning).
pub fn parse_role_names(contents: &str) -> Result<HashSet<String>, ConfigError> {
    let raw: RawFlowConfig = serde_yaml::from_str(contents)?;
    Ok(raw.roles.keys().cloned().collect())
}

/// Load a single agent file from .koto/agents/<agent_id>.yaml.
pub fn load_agent_file(
    koto_dir: &Path,
    agent_id: &str,
    defaults: &Defaults,
) -> Result<Agent, ConfigError> {
    let path = koto_dir.join("agents").join(format!("{agent_id}.yaml"));
    if !path.exists() {
        return Err(ConfigError::Validation(format!(
            "agent file not found: {} (expected at {})",
            agent_id,
            path.display()
        )));
    }
    let contents = std::fs::read_to_string(&path)?;
    let raw: RawAgentFile = serde_yaml::from_str(&contents)?;
    warn_unknown_fields(&format!("agent file '{agent_id}'"), &raw.unknown);

    Ok(Agent {
        id: agent_id.to_string(),
        name: raw.name,
        title: raw.title,
        role: raw.role,
        model: raw.model.unwrap_or_else(|| defaults.model.clone()),
        backend: raw.backend.unwrap_or(defaults.backend),
        rules: raw.rules,
        skills: raw.skills,
        env: raw.env,
    })
}

/// Load all agents referenced by the flow steps.
pub fn load_agents_for_flow(
    koto_dir: &Path,
    config: &FlowConfig,
) -> Result<Vec<Agent>, ConfigError> {
    let mut agents: Vec<Agent> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for step in &config.steps {
        if seen.insert(step.agent.clone()) {
            let agent = load_agent_file(koto_dir, &step.agent, &config.defaults)?;
            agents.push(agent);
        }
    }

    Ok(agents)
}

fn warn_unknown_fields(context: &str, fields: &HashMap<String, serde_yaml::Value>) {
    for key in fields.keys() {
        eprintln!("warning: unknown field '{key}' in {context}");
    }
}

// --- Validation and resolution ---

fn validate_and_resolve(
    raw: RawFlowConfig,
    role_overrides: &HashMap<String, String>,
) -> Result<FlowConfig, ConfigError> {
    if raw.version.0 != "1" {
        return Err(ConfigError::Validation(format!(
            "unsupported version '{}', expected '1'",
            raw.version.0
        )));
    }

    let defaults = Defaults {
        model: raw
            .defaults
            .as_ref()
            .and_then(|d| d.model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        backend: raw
            .defaults
            .as_ref()
            .and_then(|d| d.backend)
            .unwrap_or(DEFAULT_BACKEND),
    };

    // Build resolved roles map (default + overrides)
    let mut resolved_roles: HashMap<String, String> = HashMap::new();
    for (role_name, role_default) in &raw.roles {
        let agent_id = role_overrides
            .get(role_name)
            .unwrap_or(&role_default.default)
            .clone();
        resolved_roles.insert(role_name.clone(), agent_id);
    }

    // Validate resolved roles have non-empty agent IDs
    for (role_name, agent_id) in &resolved_roles {
        if agent_id.trim().is_empty() {
            return Err(ConfigError::Validation(format!(
                "role '{role_name}' resolves to empty agent ID"
            )));
        }
    }

    // Extract placeholders from flow prompt to detect role-placeholder collisions
    let prompt_placeholders = if let Some(ref prompt) = raw.prompt {
        extract_placeholders(prompt)
    } else {
        HashSet::new()
    };

    // Validate role names don't collide with template placeholders
    for role_name in raw.roles.keys() {
        if prompt_placeholders.contains(role_name) {
            return Err(ConfigError::Validation(format!(
                "role name '{role_name}' collides with template placeholder {{{{{}}}}}",
                role_name
            )));
        }
    }

    // Collect step IDs for reference validation
    let step_ids: HashSet<&str> = raw.flow.keys().map(|k| k.as_str()).collect();

    // Validate step references
    for (id, step) in &raw.flow {
        for dep in step.input.iter().chain(step.needs.iter()) {
            if !step_ids.contains(dep.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "step '{id}' references unknown step '{dep}' in input/needs"
                )));
            }
        }
    }

    // Validate and resolve steps
    let steps: Vec<Step> = raw
        .flow
        .into_iter()
        .map(|(id, s)| {
            // Validate: exactly one of agent or role must be set
            match (&s.agent, &s.role) {
                (Some(_), Some(_)) => Err(ConfigError::Validation(format!(
                    "step '{id}' has both 'agent' and 'role' -- use one or the other"
                ))),
                (None, None) => Err(ConfigError::Validation(format!(
                    "step '{id}' must specify either 'agent' or 'role'"
                ))),
                (Some(agent_id), None) => {
                    // Direct agent assignment -- bypass roles
                    let mut needs: Vec<String> = s.needs;
                    for input_dep in &s.input {
                        if !needs.contains(input_dep) {
                            needs.push(input_dep.clone());
                        }
                    }
                    Ok(Step {
                        id,
                        agent: agent_id.clone(),
                        task: s.task,
                        input: s.input,
                        needs,
                        model: s.model,
                        backend: s.backend,
                        print_output: s.print_output,
                    })
                }
                (None, Some(role_name)) => {
                    // Resolve role to agent ID
                    let agent_id = resolved_roles.get(role_name).ok_or_else(|| {
                        let available = resolved_roles.keys()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ");
                        ConfigError::Validation(format!(
                            "step '{id}' references undefined role '{role_name}' (available: {available})"
                        ))
                    })?;

                    let mut needs: Vec<String> = s.needs;
                    for input_dep in &s.input {
                        if !needs.contains(input_dep) {
                            needs.push(input_dep.clone());
                        }
                    }
                    Ok(Step {
                        id,
                        agent: agent_id.clone(),
                        task: s.task,
                        input: s.input,
                        needs,
                        model: s.model,
                        backend: s.backend,
                        print_output: s.print_output,
                    })
                }
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    let stack = StackConfig {
        backend: raw
            .stack
            .as_ref()
            .and_then(|s| s.backend.clone())
            .unwrap_or_else(|| DEFAULT_STACK_BACKEND.to_string()),
        path: raw
            .stack
            .as_ref()
            .and_then(|s| s.path.clone())
            .unwrap_or_default(),
    };

    Ok(FlowConfig {
        version: raw.version.0,
        name: raw.name,
        prompt: raw.prompt,
        defaults,
        roles: resolved_roles,
        steps,
        stack,
    })
}

/// Extract `{{placeholder}}` names from a prompt template.
pub fn extract_placeholders(prompt: &str) -> HashSet<String> {
    let re = regex_lite::Regex::new(r"\{\{([a-zA-Z_][a-zA-Z0-9_]*)\}\}").unwrap();
    re.captures_iter(prompt)
        .map(|cap| cap[1].to_string())
        .collect()
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_CONFIG: &str = r#"
version: "1"
name: planning-team

defaults:
  model: claude-opus-4-5
  backend: api

flow:
  design:
    agent: architect
  review:
    agent: reviewer
    input: [design]
    task: "Check architecture decisions"

stack:
  backend: local
  path: /tmp/test-stack
"#;

    const MINIMAL_CONFIG: &str = r#"
version: "1"
name: minimal

flow:
  code:
    agent: dev
"#;

    #[test]
    fn full_config_parses() {
        let config = load_flow_from_str(FULL_CONFIG).unwrap();
        assert_eq!(config.name, "planning-team");
        assert_eq!(config.version, "1");
        assert_eq!(config.steps.len(), 2);
        assert_eq!(config.steps[0].id, "design");
        assert_eq!(config.steps[0].agent, "architect");
        assert_eq!(config.steps[1].id, "review");
        assert_eq!(
            config.steps[1].task.as_deref(),
            Some("Check architecture decisions")
        );
        assert_eq!(config.stack.backend, "local");
        assert_eq!(config.stack.path, "/tmp/test-stack");
    }

    #[test]
    fn minimal_config_parses() {
        let config = load_flow_from_str(MINIMAL_CONFIG).unwrap();
        assert_eq!(config.name, "minimal");
        assert_eq!(config.steps.len(), 1);
        assert_eq!(config.steps[0].id, "code");
        assert_eq!(config.steps[0].agent, "dev");
        assert_eq!(config.defaults.model, DEFAULT_MODEL);
        assert_eq!(config.defaults.backend, Backend::ClaudeCli);
        assert_eq!(config.stack.backend, DEFAULT_STACK_BACKEND);
        assert_eq!(config.stack.path, "");
    }

    #[test]
    fn missing_required_field_name() {
        let yaml = r#"
version: "1"
flow:
  code:
    agent: dev
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("name"),
            "error should mention 'name': {}",
            err
        );
    }

    #[test]
    fn missing_required_field_flow() {
        let yaml = r#"
version: "1"
name: test
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("flow"),
            "error should mention 'flow': {}",
            err
        );
    }

    #[test]
    fn unknown_field_warns_but_succeeds() {
        let yaml = r#"
version: "1"
name: test
extra_field: hello
flow:
  code:
    agent: dev
    unknown_prop: 42
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.name, "test");
    }

    #[test]
    fn input_implies_needs() {
        let yaml = r#"
version: "1"
name: test
flow:
  first:
    agent: dev
  second:
    agent: dev
    input: [first]
"#;
        let config = load_flow_from_str(yaml).unwrap();
        let second = &config.steps[1];
        assert_eq!(second.input, vec!["first"]);
        assert!(second.needs.contains(&"first".to_string()));
    }

    #[test]
    fn version_as_integer() {
        let yaml = r#"
version: 1
name: test
flow:
  code:
    agent: dev
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.version, "1");
    }

    #[test]
    fn step_references_nonexistent_step_in_needs() {
        let yaml = r#"
version: "1"
name: test
flow:
  code:
    agent: dev
    needs: [phantom]
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("unknown step 'phantom'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn step_references_nonexistent_step_in_input() {
        let yaml = r#"
version: "1"
name: test
flow:
  code:
    agent: dev
    input: [phantom]
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("unknown step 'phantom'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn backend_enum_parses() {
        let yaml = r#"
version: "1"
name: test
flow:
  code:
    agent: dev
    backend: api
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.steps[0].backend, Some(Backend::Api));
    }

    #[test]
    fn unsupported_version_errors() {
        let yaml = r#"
version: "2"
name: test
flow:
  code:
    agent: dev
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("unsupported version '2'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn task_field_parses() {
        let yaml = r#"
version: "1"
name: test
flow:
  review:
    agent: reviewer
    task: "Check error handling"
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(
            config.steps[0].task.as_deref(),
            Some("Check error handling")
        );
    }

    #[test]
    fn preserves_insertion_order() {
        let yaml = r#"
version: "1"
name: test
flow:
  design:
    agent: architect
  implement:
    agent: developer
    input: [design]
  review:
    agent: reviewer
    input: [implement]
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.steps[0].id, "design");
        assert_eq!(config.steps[1].id, "implement");
        assert_eq!(config.steps[2].id, "review");
    }

    #[test]
    fn load_agent_file_works() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("kai.yaml"),
            r#"
name: Kai
role: "Senior Rust developer"
rules: [rust-developer, cli-ux]
skills: [testing-patterns]
model: claude-sonnet-4-5
env:
  CARGO_TERM_COLOR: always
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "default-model".to_string(),
            backend: Backend::ClaudeCli,
        };
        let agent = load_agent_file(dir.path(), "kai", &defaults).unwrap();
        assert_eq!(agent.id, "kai");
        assert_eq!(agent.name, "Kai");
        assert_eq!(agent.role, "Senior Rust developer");
        assert_eq!(agent.rules, vec!["rust-developer", "cli-ux"]);
        assert_eq!(agent.skills, vec!["testing-patterns"]);
        assert_eq!(agent.model, "claude-sonnet-4-5");
        assert_eq!(agent.backend, Backend::ClaudeCli);
        assert_eq!(agent.env.get("CARGO_TERM_COLOR").unwrap(), "always");
    }

    #[test]
    fn load_agent_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("alex.yaml"),
            r#"
name: Alex
role: "Code reviewer"
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "claude-opus-4-5".to_string(),
            backend: Backend::Api,
        };
        let agent = load_agent_file(dir.path(), "alex", &defaults).unwrap();
        assert_eq!(agent.model, "claude-opus-4-5");
        assert_eq!(agent.backend, Backend::Api);
        assert!(agent.rules.is_empty());
        assert!(agent.skills.is_empty());
    }

    #[test]
    fn load_agent_file_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let defaults = Defaults {
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
        };
        let err = load_agent_file(dir.path(), "ghost", &defaults).unwrap_err();
        assert!(err.to_string().contains("agent file not found"));
    }

    #[test]
    fn model_defaults_applied_to_flow() {
        let yaml = r#"
version: "1"
name: test
defaults:
  model: custom-model
  backend: api
flow:
  code:
    agent: dev
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.defaults.model, "custom-model");
        assert_eq!(config.defaults.backend, Backend::Api);
    }

    #[test]
    fn role_based_flow_parses() {
        let yaml = r#"
version: "1"
name: test
roles:
  coder: { default: Noah }
  reviewer: { default: Bella }
flow:
  code:
    role: coder
  review:
    role: reviewer
    input: [code]
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.steps.len(), 2);
        assert_eq!(config.steps[0].agent, "Noah");
        assert_eq!(config.steps[1].agent, "Bella");
        assert_eq!(config.roles.get("coder").unwrap(), "Noah");
        assert_eq!(config.roles.get("reviewer").unwrap(), "Bella");
    }

    #[test]
    fn role_with_cli_override() {
        let yaml = r#"
version: "1"
name: test
roles:
  coder: { default: Noah }
flow:
  code:
    role: coder
"#;
        let mut overrides = HashMap::new();
        overrides.insert("coder".to_string(), "Kai".to_string());
        let config = load_flow_from_str_with_overrides(yaml, &overrides).unwrap();
        assert_eq!(config.steps[0].agent, "Kai");
        assert_eq!(config.roles.get("coder").unwrap(), "Kai");
    }

    #[test]
    fn step_with_both_agent_and_role_errors() {
        let yaml = r#"
version: "1"
name: test
roles:
  coder: { default: Noah }
flow:
  code:
    agent: Kai
    role: coder
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("both 'agent' and 'role'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn step_with_neither_agent_nor_role_errors() {
        let yaml = r#"
version: "1"
name: test
flow:
  code:
    task: "Do something"
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string()
                .contains("must specify either 'agent' or 'role'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn undefined_role_in_step_errors() {
        let yaml = r#"
version: "1"
name: test
roles:
  coder: { default: Noah }
flow:
  code:
    role: phantom
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("undefined role 'phantom'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn role_name_collides_with_placeholder_errors() {
        let yaml = r#"
version: "1"
name: test
prompt: "Fix issue #{{id}}"
roles:
  id: { default: Noah }
flow:
  code:
    role: id
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string()
                .contains("collides with template placeholder"),
            "got: {}",
            err
        );
    }

    #[test]
    fn backwards_compat_agent_only() {
        // No roles defined, steps use agent directly
        let yaml = r#"
version: "1"
name: test
flow:
  code:
    agent: Noah
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.steps[0].agent, "Noah");
        assert!(config.roles.is_empty());
    }

    #[test]
    fn mixed_agent_and_role_steps() {
        let yaml = r#"
version: "1"
name: test
roles:
  coder: { default: Noah }
flow:
  code:
    role: coder
  review:
    agent: Bella
    input: [code]
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.steps.len(), 2);
        assert_eq!(config.steps[0].agent, "Noah");
        assert_eq!(config.steps[1].agent, "Bella");
    }

    #[test]
    fn empty_role_default_errors() {
        let yaml = r#"
version: "1"
name: test
roles:
  coder: { default: "" }
flow:
  code:
    role: coder
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string()
                .contains("role 'coder' resolves to empty agent ID"),
            "got: {}",
            err
        );
    }

    #[test]
    fn undefined_role_lists_available() {
        let yaml = r#"
version: "1"
name: test
roles:
  coder: { default: Noah }
  reviewer: { default: Bella }
flow:
  code:
    role: phantom
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("undefined role 'phantom'"), "got: {}", err);
        assert!(msg.contains("available:"), "got: {}", err);
        assert!(
            msg.contains("coder") && msg.contains("reviewer"),
            "got: {}",
            err
        );
    }

    // --- Requirement Verification Tests (Issue #87) ---

    /// Verify Noah's agent definition includes requirement validation instructions
    #[test]
    fn noah_agent_has_requirement_restatement() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let noah_path = koto_dir.join("agents/Noah.yaml");

        if !noah_path.exists() {
            panic!("Noah.yaml not found at {}", noah_path.display());
        }

        let contents = std::fs::read_to_string(&noah_path).expect("failed to read Noah.yaml");
        let agent: RawAgentFile =
            serde_yaml::from_str(&contents).expect("failed to parse Noah.yaml");

        let role_lower = agent.role.to_lowercase();

        // Check for requirement reading instruction
        assert!(
            role_lower.contains("read the requirement"),
            "Noah's role should include 'read the requirement' instruction. Got: {}",
            agent.role
        );

        // Check for edge case handling
        assert!(
            role_lower.contains("edge case"),
            "Noah's role should mention edge case handling. Got: {}",
            agent.role
        );

        // Check for blocking on incomplete/wrong designs
        assert!(
            role_lower.contains("blocked"),
            "Noah's role should include BLOCKED instruction for incomplete designs. Got: {}",
            agent.role
        );
    }

    /// Verify Bella's agent definition prioritizes original requirement verification
    #[test]
    fn bella_agent_prioritizes_original_requirement() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let bella_path = koto_dir.join("agents/Bella.yaml");

        if !bella_path.exists() {
            panic!("Bella.yaml not found at {}", bella_path.display());
        }

        let contents = std::fs::read_to_string(&bella_path).expect("failed to read Bella.yaml");
        let agent: RawAgentFile =
            serde_yaml::from_str(&contents).expect("failed to parse Bella.yaml");

        let role = &agent.role;
        let role_lower = role.to_lowercase();

        // Check for prioritization of original requirement
        assert!(
            role_lower.contains("original requirement") || role_lower.contains("original task"),
            "Bella's role should reference 'ORIGINAL requirement' or 'original task'. Got: {}",
            role
        );

        // Check for negation of design-doc-only validation
        assert!(
            role_lower.contains("not just the design")
                || role_lower.contains("not just")
                || role_lower.contains("intermediate artifact"),
            "Bella's role should warn against validating only against design doc/intermediate artifacts. Got: {}",
            role
        );

        // Check for primary/first check language
        assert!(
            role_lower.contains("primary check")
                || role_lower.contains("first check")
                || role.starts_with("You are a code reviewer. Your primary check:"),
            "Bella's role should indicate original-requirement check is primary. Got: {}",
            role
        );

        // Check for divergence detection instruction
        assert!(
            role_lower.contains("diverged") || role_lower.contains("divergence"),
            "Bella's role should include instruction to flag design divergence. Got: {}",
            role
        );
    }

    /// Verify implement-issue flow has exactly 4 steps
    #[test]
    fn implement_issue_has_four_steps() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/implement-issue.yaml");

        if !flow_path.exists() {
            panic!("implement-issue.yaml not found at {}", flow_path.display());
        }

        let contents =
            std::fs::read_to_string(&flow_path).expect("failed to read implement-issue.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse implement-issue.yaml");

        assert_eq!(
            raw.flow.len(),
            4,
            "implement-issue flow should have exactly 4 steps. Got: {}",
            raw.flow.len()
        );

        // Verify step names
        assert!(
            raw.flow.contains_key("fetch"),
            "implement-issue flow should have 'fetch' step"
        );
        assert!(
            raw.flow.contains_key("implement"),
            "implement-issue flow should have 'implement' step"
        );
        assert!(
            raw.flow.contains_key("review"),
            "implement-issue flow should have 'review' step"
        );
        assert!(
            raw.flow.contains_key("pr"),
            "implement-issue flow should have 'pr' step"
        );
    }

    /// Verify implement-issue flow uses 'id' placeholder not 'issue'
    #[test]
    fn implement_issue_uses_id_placeholder() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/implement-issue.yaml");

        if !flow_path.exists() {
            panic!("implement-issue.yaml not found at {}", flow_path.display());
        }

        let contents =
            std::fs::read_to_string(&flow_path).expect("failed to read implement-issue.yaml");

        // Check that {{id}} is present
        assert!(
            contents.contains("{{id}}"),
            "implement-issue.yaml should use {{{{id}}}} placeholder"
        );

        // Check that {{issue}} is NOT present
        assert!(
            !contents.contains("{{issue}}"),
            "implement-issue.yaml should NOT use {{{{issue}}}} placeholder"
        );

        // Check usage comment
        assert!(
            contents.contains("id=42") || contents.contains("id="),
            "implement-issue.yaml usage comment should show 'id=' not 'issue='"
        );
    }

    /// Verify implement-issue flow implement step includes restatement instruction
    #[test]
    fn implement_issue_implement_step_has_restatement() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/implement-issue.yaml");

        if !flow_path.exists() {
            panic!("implement-issue.yaml not found at {}", flow_path.display());
        }

        let contents =
            std::fs::read_to_string(&flow_path).expect("failed to read implement-issue.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse implement-issue.yaml");

        let implement_step = raw
            .flow
            .get("implement")
            .expect("implement-issue flow should have 'implement' step");

        let task = implement_step
            .task
            .as_ref()
            .expect("implement step should have a task");
        let task_lower = task.to_lowercase();

        assert!(
            task_lower.contains("restate"),
            "implement-issue implement step should include restatement instruction. Got: {}",
            task
        );

        assert!(
            task_lower.contains("assumption"),
            "implement-issue implement step should mention assumptions. Got: {}",
            task
        );
    }

    /// Verify implement-issue flow review step has fetch input and checks acceptance criteria
    #[test]
    fn implement_issue_review_step_checks_acceptance_criteria() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/implement-issue.yaml");

        if !flow_path.exists() {
            panic!("implement-issue.yaml not found at {}", flow_path.display());
        }

        let contents =
            std::fs::read_to_string(&flow_path).expect("failed to read implement-issue.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse implement-issue.yaml");

        let review_step = raw
            .flow
            .get("review")
            .expect("implement-issue flow should have 'review' step");

        // Check inputs include fetch and implement
        assert!(
            review_step.input.contains(&"fetch".to_string()),
            "implement-issue review step should have fetch in input array. Got: {:?}",
            review_step.input
        );

        assert!(
            review_step.input.contains(&"implement".to_string()),
            "implement-issue review step should have implement in input array. Got: {:?}",
            review_step.input
        );

        // Check task mentions acceptance criteria
        let task = review_step
            .task
            .as_ref()
            .expect("review step should have a task");
        let task_lower = task.to_lowercase();

        assert!(
            task_lower.contains("acceptance criteria")
                || task_lower.contains("acceptance criterion"),
            "implement-issue review step should mention acceptance criteria. Got: {}",
            task
        );

        assert!(
            task_lower.contains("original"),
            "implement-issue review step should reference ORIGINAL issue. Got: {}",
            task
        );
    }

    /// Verify implement-issue flow step dependencies are correct
    #[test]
    fn implement_issue_step_dependencies_are_correct() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/implement-issue.yaml");

        if !flow_path.exists() {
            panic!("implement-issue.yaml not found at {}", flow_path.display());
        }

        let contents =
            std::fs::read_to_string(&flow_path).expect("failed to read implement-issue.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse implement-issue.yaml");

        // fetch has no inputs
        let fetch_step = raw
            .flow
            .get("fetch")
            .expect("implement-issue flow should have 'fetch' step");
        assert!(
            fetch_step.input.is_empty(),
            "fetch step should have no inputs. Got: {:?}",
            fetch_step.input
        );

        // implement takes fetch
        let implement_step = raw
            .flow
            .get("implement")
            .expect("implement-issue flow should have 'implement' step");
        assert_eq!(
            implement_step.input,
            vec!["fetch"],
            "implement step should have [fetch] as input. Got: {:?}",
            implement_step.input
        );

        // review takes fetch and implement
        let review_step = raw
            .flow
            .get("review")
            .expect("implement-issue flow should have 'review' step");
        assert!(
            review_step.input.contains(&"fetch".to_string()),
            "review step should have fetch in input. Got: {:?}",
            review_step.input
        );
        assert!(
            review_step.input.contains(&"implement".to_string()),
            "review step should have implement in input. Got: {:?}",
            review_step.input
        );

        // pr takes review
        let pr_step = raw
            .flow
            .get("pr")
            .expect("implement-issue flow should have 'pr' step");
        assert_eq!(
            pr_step.input,
            vec!["review"],
            "pr step should have [review] as input. Got: {:?}",
            pr_step.input
        );
    }

    /// Verify implement-issue flow uses exactly 3 roles
    #[test]
    fn implement_issue_uses_three_roles() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/implement-issue.yaml");

        if !flow_path.exists() {
            panic!("implement-issue.yaml not found at {}", flow_path.display());
        }

        let contents =
            std::fs::read_to_string(&flow_path).expect("failed to read implement-issue.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse implement-issue.yaml");

        assert_eq!(
            raw.roles.len(),
            3,
            "implement-issue flow should have exactly 3 roles. Got: {}",
            raw.roles.len()
        );

        // Verify the expected roles exist
        assert!(
            raw.roles.contains_key("fetcher"),
            "implement-issue flow should have 'fetcher' role"
        );
        assert!(
            raw.roles.contains_key("developer"),
            "implement-issue flow should have 'developer' role"
        );
        assert!(
            raw.roles.contains_key("reviewer"),
            "implement-issue flow should have 'reviewer' role"
        );
    }

    /// Verify implement-issue flow pr step has print_output enabled
    #[test]
    fn implement_issue_pr_step_has_print_output() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/implement-issue.yaml");

        if !flow_path.exists() {
            panic!("implement-issue.yaml not found at {}", flow_path.display());
        }

        let contents =
            std::fs::read_to_string(&flow_path).expect("failed to read implement-issue.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse implement-issue.yaml");

        let pr_step = raw
            .flow
            .get("pr")
            .expect("implement-issue flow should have 'pr' step");

        assert!(
            pr_step.print_output,
            "pr step should have print_output: true"
        );
    }

    /// Verify obsolete flow files have been deleted
    #[test]
    fn obsolete_flows_deleted() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let fix_issue_path = koto_dir.join("flows/fix-issue.yaml");
        let fix_pr_path = koto_dir.join("flows/fix-pr.yaml");

        assert!(!fix_issue_path.exists(), "fix-issue.yaml should be deleted");

        assert!(!fix_pr_path.exists(), "fix-pr.yaml should be deleted");
    }

    /// Verify review-pr flow uses 'id' placeholder not 'pr'
    #[test]
    fn review_pr_uses_id_placeholder() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/review-pr.yaml");

        if !flow_path.exists() {
            panic!("review-pr.yaml not found at {}", flow_path.display());
        }

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read review-pr.yaml");

        // Check that {{id}} is present
        assert!(
            contents.contains("{{id}}"),
            "review-pr.yaml should use {{{{id}}}} placeholder"
        );

        // Check that {{pr}} is NOT present (except potentially in comments mentioning "PR")
        let placeholder_count = contents.matches("{{pr}}").count();
        assert_eq!(
            placeholder_count, 0,
            "review-pr.yaml should NOT use {{{{pr}}}} placeholder. Found {} occurrences",
            placeholder_count
        );

        // Check usage comment
        assert!(
            contents.contains("id=67") || contents.contains("id="),
            "review-pr.yaml usage comment should show 'id=' not 'pr='"
        );
    }

    /// Verify all flows use consistent placeholder naming
    #[test]
    fn flows_use_consistent_placeholders() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flows_dir = koto_dir.join("flows");

        if !flows_dir.exists() {
            panic!("flows directory not found");
        }

        // Read all YAML files in flows directory
        let entries = std::fs::read_dir(&flows_dir).expect("failed to read flows directory");

        for entry in entries {
            let entry = entry.expect("failed to read directory entry");
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }

            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("failed to read {}", path.display()));

            // No flow should use {{issue}} or {{pr}} placeholders
            assert!(
                !contents.contains("{{issue}}"),
                "Flow {} should not use {{{{issue}}}} placeholder",
                path.display()
            );

            assert!(
                !contents.contains("{{pr}}"),
                "Flow {} should not use {{{{pr}}}} placeholder",
                path.display()
            );
        }
    }

    /// Verify requirement-verification.md documentation exists and contains key sections
    #[test]
    fn requirement_verification_docs_exist() {
        let docs_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/requirement-verification.md");

        assert!(
            docs_path.exists(),
            "docs/requirement-verification.md should exist at {}",
            docs_path.display()
        );

        let contents = std::fs::read_to_string(&docs_path)
            .expect("failed to read requirement-verification.md");
        let contents_lower = contents.to_lowercase();

        // Check for required sections
        assert!(
            contents_lower.contains("problem") || contents_lower.contains("## problem"),
            "Documentation should have a Problem section"
        );

        assert!(
            contents_lower.contains("pattern") || contents_lower.contains("## pattern"),
            "Documentation should have a Pattern section"
        );

        assert!(
            contents_lower.contains("for flow authors") || contents_lower.contains("flow authors"),
            "Documentation should include guidance for flow authors"
        );

        assert!(
            contents_lower.contains("agent role") || contents_lower.contains("for agent role"),
            "Documentation should include guidance for agent role design"
        );

        // Check for key concepts
        assert!(
            contents_lower.contains("telephone")
                || contents_lower.contains("drift")
                || contents_lower.contains("diverge"),
            "Documentation should explain the drift/divergence problem"
        );

        assert!(
            contents_lower.contains("original requirement")
                || contents_lower.contains("original task"),
            "Documentation should emphasize original requirement verification"
        );
    }

    // ---- rework-pr flow tests ----

    fn rework_pr_raw() -> RawFlowConfig {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");
        if !flow_path.exists() {
            panic!("rework-pr.yaml not found at {}", flow_path.display());
        }
        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");
        serde_yaml::from_str(&contents).expect("failed to parse rework-pr.yaml")
    }

    fn rework_pr_contents() -> String {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");
        std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml")
    }

    /// Verify rework-pr flow has exactly 4 steps: fetch, fix, verify, push.
    #[test]
    fn rework_pr_has_four_steps() {
        let raw = rework_pr_raw();
        assert_eq!(
            raw.flow.len(),
            4,
            "rework-pr flow should have exactly 4 steps. Got: {}",
            raw.flow.len()
        );
        for step in ["fetch", "fix", "verify", "push"] {
            assert!(
                raw.flow.contains_key(step),
                "rework-pr flow should have '{step}' step"
            );
        }
    }

    /// Verify rework-pr flow uses 'id' placeholder.
    #[test]
    fn rework_pr_uses_id_placeholder() {
        let contents = rework_pr_contents();
        assert!(
            contents.contains("{{id}}"),
            "rework-pr.yaml should use {{{{id}}}} placeholder"
        );
        assert!(
            contents.contains("id=42") || contents.contains("id="),
            "rework-pr.yaml usage comment should show 'id='"
        );
    }

    /// Verify rework-pr flow step dependencies form the expected chain.
    #[test]
    fn rework_pr_step_dependencies_are_correct() {
        let raw = rework_pr_raw();

        let fetch = raw.flow.get("fetch").expect("fetch step");
        assert!(
            fetch.input.is_empty(),
            "fetch step should have no inputs. Got: {:?}",
            fetch.input
        );

        let fix = raw.flow.get("fix").expect("fix step");
        assert_eq!(
            fix.input,
            vec!["fetch"],
            "fix step should depend on [fetch]. Got: {:?}",
            fix.input
        );

        let verify = raw.flow.get("verify").expect("verify step");
        assert!(
            verify.input.contains(&"fetch".to_string())
                && verify.input.contains(&"fix".to_string()),
            "verify step should depend on fetch and fix. Got: {:?}",
            verify.input
        );

        let push = raw.flow.get("push").expect("push step");
        assert_eq!(
            push.input,
            vec!["verify"],
            "push step should depend on [verify]. Got: {:?}",
            push.input
        );
    }

    /// Verify rework-pr flow uses exactly 3 roles: fetcher, developer, verifier.
    #[test]
    fn rework_pr_uses_three_roles() {
        let raw = rework_pr_raw();
        assert_eq!(
            raw.roles.len(),
            3,
            "rework-pr flow should have exactly 3 roles. Got: {}",
            raw.roles.len()
        );
        for role in ["fetcher", "developer", "verifier"] {
            assert!(
                raw.roles.contains_key(role),
                "rework-pr flow should have '{role}' role"
            );
        }
    }

    /// Verify rework-pr push step has print_output enabled.
    #[test]
    fn rework_pr_push_step_has_print_output() {
        let raw = rework_pr_raw();
        let push = raw.flow.get("push").expect("push step");
        assert!(
            push.print_output,
            "push step should have print_output: true"
        );
    }

    /// Verify rework-pr verify step has print_output enabled so the gate
    /// decision is visible to the user.
    #[test]
    fn rework_pr_verify_step_has_print_output() {
        let raw = rework_pr_raw();
        let verify = raw.flow.get("verify").expect("verify step");
        assert!(
            verify.print_output,
            "verify step should have print_output: true so the DONE/INCOMPLETE gate decision is visible"
        );
    }

    /// Verify rework-pr fetch step uses `gh api` for inline review comments,
    /// since `gh pr view --comments` only returns issue-tab comments.
    #[test]
    fn rework_pr_fetch_uses_gh_api_for_inline_comments() {
        let raw = rework_pr_raw();
        let fetch = raw.flow.get("fetch").expect("fetch step");
        let task = fetch.task.as_ref().expect("fetch step should have a task");
        assert!(
            task.contains("gh api") && task.contains("/pulls/") && task.contains("/comments"),
            "fetch step should call `gh api repos/.../pulls/{{id}}/comments` for inline review comments. Got: {task}"
        );
    }

    /// Verify rework-pr fix step instructs the agent to check out the PR's
    /// head branch before committing, otherwise fixups land on main.
    #[test]
    fn rework_pr_fix_step_checks_out_branch() {
        let raw = rework_pr_raw();
        let fix = raw.flow.get("fix").expect("fix step");
        let task = fix.task.as_ref().expect("fix step should have a task");
        assert!(
            task.contains("gh pr checkout"),
            "fix step should instruct `gh pr checkout` before committing. Got: {task}"
        );
    }

    /// Verify rework-pr push step uses the captured branch name, not HEAD.
    #[test]
    fn rework_pr_push_uses_branch_variable() {
        let raw = rework_pr_raw();
        let push = raw.flow.get("push").expect("push step");
        let task = push.task.as_ref().expect("push step should have a task");
        assert!(
            task.contains("headRefName"),
            "push step should fetch headRefName. Got: {task}"
        );
        assert!(
            !task.contains("git push origin HEAD"),
            "push step must not push HEAD -- it should push the captured branch name. Got: {task}"
        );
        assert!(
            task.contains("git push origin \"$branch\"")
                || task.contains("git push origin $branch")
                || task.contains("git push origin {{headRefName}}"),
            "push step should push to the captured branch name. Got: {task}"
        );
    }
}
