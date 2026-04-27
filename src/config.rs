use std::collections::{HashMap, HashSet};
use std::path::Path;

use indexmap::IndexMap;
use serde::Deserialize;

use crate::koto_config::KotoConfig;

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
    /// Capability tier, resolved against `koto.yaml#tiers` at load time.
    pub tier: Option<String>,
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
///
/// `koto_config` is the optional project-level config. When present and the
/// agent declares a `tier:` field, the tier is resolved to a concrete
/// `<provider>/<model-id>` string. When absent and a tier is declared, this
/// returns a validation error -- a tier-bound agent cannot be used without
/// a tiers map.
pub fn load_agent_file(
    koto_dir: &Path,
    agent_id: &str,
    defaults: &Defaults,
    koto_config: Option<&KotoConfig>,
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

    let model = resolve_agent_model(&raw, defaults, koto_config)?;

    Ok(Agent {
        id: agent_id.to_string(),
        name: raw.name,
        title: raw.title,
        role: raw.role,
        model,
        backend: raw.backend.unwrap_or(defaults.backend),
        rules: raw.rules,
        skills: raw.skills,
        env: raw.env,
    })
}

/// Resolve the model string for an agent.
///
/// Precedence (highest first):
/// 1. `tier:` resolved through `koto.yaml#tiers`
/// 2. `model:` literal in the agent YAML
/// 3. `defaults.model` from the flow config
fn resolve_agent_model(
    raw: &RawAgentFile,
    defaults: &Defaults,
    koto_config: Option<&KotoConfig>,
) -> Result<String, ConfigError> {
    match (raw.tier.as_deref(), koto_config) {
        (Some(tier_name), Some(kc)) => kc
            .resolve_tier(tier_name)
            .map(str::to_string)
            // `.message()` returns the inner payload without thiserror's
            // "validation error:" prefix, so wrapping in `ConfigError::Validation`
            // does not duplicate it.
            .map_err(|e| ConfigError::Validation(e.message())),
        (Some(tier_name), None) => Err(ConfigError::Validation(format!(
            "agent \"{}\" declares tier \"{tier_name}\" but no koto.yaml found",
            raw.name
        ))),
        (None, _) => Ok(raw.model.clone().unwrap_or_else(|| defaults.model.clone())),
    }
}

/// Load all agents referenced by the flow steps.
pub fn load_agents_for_flow(
    koto_dir: &Path,
    config: &FlowConfig,
    koto_config: Option<&KotoConfig>,
) -> Result<Vec<Agent>, ConfigError> {
    let mut agents: Vec<Agent> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for step in &config.steps {
        if seen.insert(step.agent.clone()) {
            let agent = load_agent_file(koto_dir, &step.agent, &config.defaults, koto_config)?;
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
        let agent = load_agent_file(dir.path(), "kai", &defaults, None).unwrap();
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
        let agent = load_agent_file(dir.path(), "alex", &defaults, None).unwrap();
        assert_eq!(agent.model, "claude-opus-4-5");
        assert_eq!(agent.backend, Backend::Api);
        assert!(agent.rules.is_empty());
        assert!(agent.skills.is_empty());
    }

    #[test]
    fn load_agent_file_resolves_tier_via_koto_config() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("Sage.yaml"),
            r#"
name: Sage
role: "Reasoning agent"
tier: reasoning
"#,
        )
        .unwrap();

        let koto_config = crate::koto_config::KotoConfig::from_yaml_str(
            r#"
version: "1"
tiers:
  reasoning: claude/opus-4-7
  general: claude/sonnet-4-6
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "fallback".to_string(),
            backend: Backend::ClaudeCli,
        };
        let agent = load_agent_file(dir.path(), "Sage", &defaults, Some(&koto_config)).unwrap();
        assert_eq!(agent.model, "claude/opus-4-7");
    }

    #[test]
    fn load_agent_file_tier_with_no_koto_config_errors() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("Sage.yaml"),
            r#"
name: Sage
role: "Reasoning agent"
tier: reasoning
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "fallback".to_string(),
            backend: Backend::ClaudeCli,
        };
        let err = load_agent_file(dir.path(), "Sage", &defaults, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(r#"agent "Sage" declares tier "reasoning""#),
            "got: {msg}"
        );
        assert!(msg.contains("no koto.yaml found"), "got: {msg}");
    }

    #[test]
    fn load_agent_file_unknown_tier_errors() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("Sage.yaml"),
            r#"
name: Sage
role: "Reasoning agent"
tier: deep-thought
"#,
        )
        .unwrap();

        let koto_config = crate::koto_config::KotoConfig::from_yaml_str(
            r#"
version: "1"
tiers:
  general: claude/sonnet-4-6
  quick: claude/haiku-4-5
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "fallback".to_string(),
            backend: Backend::ClaudeCli,
        };
        let err = load_agent_file(dir.path(), "Sage", &defaults, Some(&koto_config)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(r#"tier "deep-thought" not defined in koto.yaml"#),
            "got: {msg}"
        );
        assert!(msg.contains("general"), "got: {msg}");
        assert!(msg.contains("quick"), "got: {msg}");
    }

    #[test]
    fn load_agent_file_tier_overrides_model_field() {
        // If tier is set, the literal `model:` field is ignored.
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("Sage.yaml"),
            r#"
name: Sage
role: "Reasoning agent"
tier: reasoning
model: should-be-ignored
"#,
        )
        .unwrap();

        let koto_config = crate::koto_config::KotoConfig::from_yaml_str(
            r#"
version: "1"
tiers:
  reasoning: claude/opus-4-7
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "fallback".to_string(),
            backend: Backend::ClaudeCli,
        };
        let agent = load_agent_file(dir.path(), "Sage", &defaults, Some(&koto_config)).unwrap();
        assert_eq!(agent.model, "claude/opus-4-7");
    }

    #[test]
    fn load_agent_file_no_tier_uses_model_field() {
        // Backward compat: agents without tier ignore koto.yaml entirely.
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("kai.yaml"),
            r#"
name: Kai
role: "dev"
model: claude-sonnet-4-5
"#,
        )
        .unwrap();

        let koto_config = crate::koto_config::KotoConfig::from_yaml_str(
            r#"
version: "1"
tiers:
  reasoning: claude/opus-4-7
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "fallback".to_string(),
            backend: Backend::ClaudeCli,
        };
        let agent = load_agent_file(dir.path(), "kai", &defaults, Some(&koto_config)).unwrap();
        assert_eq!(agent.model, "claude-sonnet-4-5");
    }

    #[test]
    fn load_agent_file_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let defaults = Defaults {
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
        };
        let err = load_agent_file(dir.path(), "ghost", &defaults, None).unwrap_err();
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

    /// Verify rework-pr flow has exactly 4 steps with the expected names.
    #[test]
    fn rework_pr_has_four_steps() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");

        if !flow_path.exists() {
            panic!("rework-pr.yaml not found at {}", flow_path.display());
        }

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse rework-pr.yaml");

        assert_eq!(
            raw.flow.len(),
            4,
            "rework-pr flow should have exactly 4 steps. Got: {}",
            raw.flow.len()
        );

        for step in ["fetch", "fix", "verify", "push"] {
            assert!(
                raw.flow.contains_key(step),
                "rework-pr flow should have '{}' step",
                step
            );
        }
    }

    /// Verify rework-pr flow uses `id` placeholder, not `pr` or `issue`,
    /// and the usage comment matches.
    #[test]
    fn rework_pr_uses_id_placeholder() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");

        if !flow_path.exists() {
            panic!("rework-pr.yaml not found at {}", flow_path.display());
        }

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");

        assert!(
            contents.contains("{{id}}"),
            "rework-pr.yaml should use {{{{id}}}} placeholder"
        );
        assert_eq!(
            contents.matches("{{pr}}").count(),
            0,
            "rework-pr.yaml should NOT use {{{{pr}}}} placeholder"
        );
        assert_eq!(
            contents.matches("{{issue}}").count(),
            0,
            "rework-pr.yaml should NOT use {{{{issue}}}} placeholder"
        );
        assert!(
            contents.contains("id="),
            "rework-pr.yaml usage comment should show 'id='"
        );
    }

    /// Verify rework-pr step dependencies match the documented pipeline:
    /// fetch -> fix -> verify -> push.
    #[test]
    fn rework_pr_step_dependencies_are_correct() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse rework-pr.yaml");

        let fetch_step = raw.flow.get("fetch").expect("missing 'fetch' step");
        assert!(
            fetch_step.input.is_empty(),
            "fetch step should have no inputs. Got: {:?}",
            fetch_step.input
        );

        let fix_step = raw.flow.get("fix").expect("missing 'fix' step");
        assert_eq!(
            fix_step.input,
            vec!["fetch"],
            "fix step should have [fetch] as input. Got: {:?}",
            fix_step.input
        );

        let verify_step = raw.flow.get("verify").expect("missing 'verify' step");
        assert!(
            verify_step.input.contains(&"fetch".to_string()),
            "verify step should have fetch in input. Got: {:?}",
            verify_step.input
        );
        assert!(
            verify_step.input.contains(&"fix".to_string()),
            "verify step should have fix in input. Got: {:?}",
            verify_step.input
        );

        let push_step = raw.flow.get("push").expect("missing 'push' step");
        assert!(
            push_step.input.contains(&"verify".to_string()),
            "push step should depend on verify. Got: {:?}",
            push_step.input
        );
    }

    /// Verify rework-pr fix and verify and push steps surface output to the
    /// terminal (developer needs to see what changed and why).
    #[test]
    fn rework_pr_steps_have_print_output() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse rework-pr.yaml");

        for step_name in ["fix", "verify", "push"] {
            let step = raw
                .flow
                .get(step_name)
                .unwrap_or_else(|| panic!("missing '{}' step", step_name));
            assert!(
                step.print_output,
                "{} step should have print_output: true",
                step_name
            );
        }
    }

    /// Verify rework-pr uses three roles (fetcher, developer, reviewer)
    /// matching the implement-issue / review-pr conventions.
    #[test]
    fn rework_pr_uses_three_roles() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse rework-pr.yaml");

        assert_eq!(
            raw.roles.len(),
            3,
            "rework-pr flow should have exactly 3 roles. Got: {}",
            raw.roles.len()
        );

        for role in ["fetcher", "developer", "reviewer"] {
            assert!(
                raw.roles.contains_key(role),
                "rework-pr flow should declare '{}' role",
                role
            );
        }
    }

    /// Verify the fetch step uses the inline-comments API endpoint, not the
    /// issue-tab `gh pr view --comments`. This is a key spec point distilled
    /// from PR #119 review rounds: inline review comments are anchored to
    /// file:line and only the API endpoint surfaces them.
    #[test]
    fn rework_pr_fetch_uses_inline_comments_api() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse rework-pr.yaml");

        let fetch_step = raw.flow.get("fetch").expect("missing 'fetch' step");
        let task = fetch_step.task.as_ref().expect("fetch step needs a task");

        assert!(
            task.contains("gh api repos/{owner}/{repo}/pulls/{{id}}/comments"),
            "fetch step should call the inline review comments API endpoint. Got: {}",
            task
        );
        assert!(
            task.to_lowercase().contains("inline"),
            "fetch step should mention inline review comments. Got: {}",
            task
        );
        assert!(
            task.contains("commit_id"),
            "fetch step should preserve per-comment commit_id (the fixup target). Got: {}",
            task
        );
    }

    /// Verify the fix step requires `gh pr checkout {{id}}` before committing
    /// and stops on checkout failure -- never silently stash.
    #[test]
    fn rework_pr_fix_checks_out_pr_branch() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse rework-pr.yaml");

        let fix_step = raw.flow.get("fix").expect("missing 'fix' step");
        let task = fix_step.task.as_ref().expect("fix step needs a task");
        let task_lower = task.to_lowercase();

        assert!(
            task.contains("gh pr checkout {{id}}"),
            "fix step should run `gh pr checkout {{{{id}}}}`. Got: {}",
            task
        );
        assert!(
            task_lower.contains("checkout fails")
                || task_lower.contains("if checkout fails")
                || task_lower.contains("checkout failure"),
            "fix step should handle checkout failure explicitly. Got: {}",
            task
        );
        assert!(
            task_lower.contains("never commit on main")
                || task_lower.contains("not commit on main"),
            "fix step should warn against committing on main. Got: {}",
            task
        );
        assert!(
            task.contains("--fixup="),
            "fix step should instruct using `git commit --fixup=<sha>`. Got: {}",
            task
        );
    }

    /// Verify the fix step covers the no-comments early-exit path with a
    /// sentinel that downstream verify can map to FINAL_VERDICT: DONE.
    #[test]
    fn rework_pr_fix_handles_no_comments_path() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse rework-pr.yaml");

        let fix_step = raw.flow.get("fix").expect("missing 'fix' step");
        let fix_task = fix_step.task.as_ref().expect("fix step needs a task");

        assert!(
            fix_task.contains("NO_REVIEW_COMMENTS"),
            "fix step should emit a NO_REVIEW_COMMENTS sentinel for the no-comments path. Got: {}",
            fix_task
        );

        let verify_step = raw.flow.get("verify").expect("missing 'verify' step");
        let verify_task = verify_step.task.as_ref().expect("verify step needs a task");

        assert!(
            verify_task.contains("NO_REVIEW_COMMENTS"),
            "verify step should recognise the NO_REVIEW_COMMENTS sentinel. Got: {}",
            verify_task
        );
        assert!(
            verify_task.contains("FINAL_VERDICT: DONE"),
            "verify step should map no-comments path to FINAL_VERDICT: DONE. Got: {}",
            verify_task
        );
    }

    /// Verify the verify step uses FINAL_VERDICT: DONE / INCOMPLETE markers
    /// as single-occurrence gate tokens, matching implement-issue conventions.
    #[test]
    fn rework_pr_verify_uses_final_verdict_marker() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse rework-pr.yaml");

        let verify_step = raw.flow.get("verify").expect("missing 'verify' step");
        let task = verify_step.task.as_ref().expect("verify step needs a task");
        let task_lower = task.to_lowercase();

        assert!(
            task.contains("FINAL_VERDICT: DONE"),
            "verify step should specify the DONE verdict marker. Got: {}",
            task
        );
        assert!(
            task.contains("FINAL_VERDICT: INCOMPLETE"),
            "verify step should specify the INCOMPLETE verdict marker. Got: {}",
            task
        );
        // Correctness, not presence: verify must read the diff.
        assert!(
            task_lower.contains("git show") || task_lower.contains("read the diff"),
            "verify step should require reading the actual diff, not just counting commits. Got: {}",
            task
        );
    }

    /// Verify the push step branches on all three verdict outcomes (DONE,
    /// INCOMPLETE, unclear) and uses the atomic shell command for resolving
    /// the branch name and pushing in a single statement.
    #[test]
    fn rework_pr_push_handles_three_verdicts() {
        let koto_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".koto");
        let flow_path = koto_dir.join("flows/rework-pr.yaml");

        let contents = std::fs::read_to_string(&flow_path).expect("failed to read rework-pr.yaml");
        let raw: RawFlowConfig =
            serde_yaml::from_str(&contents).expect("failed to parse rework-pr.yaml");

        let push_step = raw.flow.get("push").expect("missing 'push' step");
        let task = push_step.task.as_ref().expect("push step needs a task");
        let task_lower = task.to_lowercase();

        // Three verdict branches.
        assert!(
            task.contains("FINAL_VERDICT: DONE"),
            "push step should branch on DONE. Got: {}",
            task
        );
        assert!(
            task.contains("FINAL_VERDICT: INCOMPLETE"),
            "push step should branch on INCOMPLETE. Got: {}",
            task
        );
        assert!(
            task_lower.contains("unclear") || task_lower.contains("neither"),
            "push step should handle the unclear/neither-token branch. Got: {}",
            task
        );

        // Atomic shell command: branch resolution + push in a single statement.
        assert!(
            task.contains(
                "branch=$(gh pr view {{id}} --json headRefName -q .headRefName) && git push origin \"$branch\""
            ),
            "push step should use the atomic `branch=...&& git push origin \"$branch\"` form. Got: {}",
            task
        );

        // Pre-push gate: at least one fixup! commit ahead of origin/$branch.
        assert!(
            task.contains("fixup!"),
            "push step should require at least one fixup! commit ahead of origin. Got: {}",
            task
        );
    }
}
