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
                "role name '{role_name}' collides with template placeholder {{{{{{{}}}}}}}",
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
                        ConfigError::Validation(format!(
                            "step '{id}' references undefined role '{role_name}'"
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
fn extract_placeholders(prompt: &str) -> HashSet<String> {
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
prompt: "Fix issue #{{issue}}"
roles:
  issue: { default: Noah }
flow:
  code:
    role: issue
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
}
