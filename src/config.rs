use std::collections::HashSet;
use std::path::Path;

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

/// We use `flatten` + `HashMap` to capture unknown fields for warnings.
#[derive(Debug, Deserialize)]
pub struct RawFlowConfig {
    pub version: Version,
    pub name: String,
    #[serde(default)]
    pub defaults: Option<RawDefaults>,
    pub agents: Vec<RawAgent>,
    pub stages: Vec<RawStage>,
    #[serde(default)]
    pub state: Option<RawStateConfig>,
    #[serde(flatten)]
    pub unknown: std::collections::HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
pub struct RawDefaults {
    pub model: Option<String>,
    pub backend: Option<Backend>,
    #[serde(flatten)]
    pub unknown: std::collections::HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
pub struct RawAgent {
    pub id: String,
    pub role: String,
    pub model: Option<String>,
    pub backend: Option<Backend>,
    pub rules: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(flatten)]
    pub unknown: std::collections::HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
pub struct RawStage {
    pub id: String,
    pub agent: String,
    pub task: Option<String>,
    pub uses: Option<String>,
    #[serde(default)]
    pub with: Option<std::collections::HashMap<String, serde_yaml::Value>>,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    pub output: Option<String>,
    pub model: Option<String>,
    pub backend: Option<Backend>,
    #[serde(flatten)]
    pub unknown: std::collections::HashMap<String, serde_yaml::Value>,
}

#[derive(Debug, Deserialize)]
pub struct RawStateConfig {
    pub backend: Option<String>,
    pub path: Option<String>,
    #[serde(flatten)]
    pub unknown: std::collections::HashMap<String, serde_yaml::Value>,
}

// --- Resolved structs (after validation and defaults) ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowConfig {
    pub version: String,
    pub name: String,
    pub defaults: Defaults,
    pub agents: Vec<Agent>,
    pub stages: Vec<Stage>,
    pub state: StateConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defaults {
    pub model: String,
    pub backend: Backend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: String,
    pub role: String,
    pub model: String,
    pub backend: Backend,
    pub rules: Option<String>,
    pub skills: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskSource {
    Inline(String),
    Template {
        uses: String,
        with: std::collections::HashMap<String, serde_yaml::Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub id: String,
    pub agent: String,
    pub task: TaskSource,
    pub input: Vec<String>,
    pub needs: Vec<String>,
    pub output: Option<String>,
    pub model: Option<String>,
    pub backend: Option<Backend>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateConfig {
    pub backend: String,
    pub path: String,
}

// --- Constants ---

const DEFAULT_MODEL: &str = "claude-sonnet-4-5";
const DEFAULT_BACKEND: Backend = Backend::ClaudeCli;
const DEFAULT_STATE_BACKEND: &str = "local";
const DEFAULT_STATE_PATH: &str = ".koto/state";

// --- Loading ---

pub fn load_config(path: &Path) -> Result<FlowConfig, ConfigError> {
    let contents = std::fs::read_to_string(path)?;
    load_config_from_str(&contents)
}

pub fn load_config_from_str(contents: &str) -> Result<FlowConfig, ConfigError> {
    let raw: RawFlowConfig = serde_yaml::from_str(contents)?;
    warn_unknown_fields("top-level", &raw.unknown);
    if let Some(ref defaults) = raw.defaults {
        warn_unknown_fields("defaults", &defaults.unknown);
    }
    for agent in &raw.agents {
        warn_unknown_fields(&format!("agent '{}'", agent.id), &agent.unknown);
    }
    for stage in &raw.stages {
        warn_unknown_fields(&format!("stage '{}'", stage.id), &stage.unknown);
    }
    if let Some(ref state) = raw.state {
        warn_unknown_fields("state", &state.unknown);
    }
    validate_and_resolve(raw)
}

fn warn_unknown_fields(
    context: &str,
    fields: &std::collections::HashMap<String, serde_yaml::Value>,
) {
    for key in fields.keys() {
        eprintln!("warning: unknown field '{}' in {}", key, context);
    }
}

// --- Validation and resolution ---

fn validate_and_resolve(raw: RawFlowConfig) -> Result<FlowConfig, ConfigError> {
    // Version check
    if raw.version.0 != "1" {
        return Err(ConfigError::Validation(format!(
            "unsupported version '{}', expected '1'",
            raw.version.0
        )));
    }

    // Resolve defaults
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

    // Check for duplicate agent IDs
    let mut agent_ids = HashSet::new();
    for agent in &raw.agents {
        if !agent_ids.insert(&agent.id) {
            return Err(ConfigError::Validation(format!(
                "duplicate agent id '{}'",
                agent.id
            )));
        }
    }

    // Check for duplicate stage IDs
    let mut stage_ids = HashSet::new();
    for stage in &raw.stages {
        if !stage_ids.insert(&stage.id) {
            return Err(ConfigError::Validation(format!(
                "duplicate stage id '{}'",
                stage.id
            )));
        }
    }

    // Validate stages reference valid agents and stage IDs
    for stage in &raw.stages {
        if !agent_ids.contains(&stage.agent) {
            return Err(ConfigError::Validation(format!(
                "stage '{}' references unknown agent '{}'",
                stage.id, stage.agent
            )));
        }

        for dep in stage.input.iter().chain(stage.needs.iter()) {
            if !stage_ids.contains(dep) {
                return Err(ConfigError::Validation(format!(
                    "stage '{}' references unknown stage '{}' in input/needs",
                    stage.id, dep
                )));
            }
        }

        // task XOR uses
        match (&stage.task, &stage.uses) {
            (None, None) => {
                return Err(ConfigError::Validation(format!(
                    "stage '{}' must have either 'task' or 'uses'",
                    stage.id
                )));
            }
            (Some(_), Some(_)) => {
                return Err(ConfigError::Validation(format!(
                    "stage '{}' cannot have both 'task' and 'uses'",
                    stage.id
                )));
            }
            _ => {}
        }
    }

    // Resolve agents
    let agents: Vec<Agent> = raw
        .agents
        .into_iter()
        .map(|a| Agent {
            model: a.model.unwrap_or_else(|| defaults.model.clone()),
            backend: a.backend.unwrap_or(defaults.backend),
            id: a.id,
            role: a.role,
            rules: a.rules,
            skills: a.skills,
        })
        .collect();

    // Resolve stages -- input implies needs
    let stages: Vec<Stage> = raw
        .stages
        .into_iter()
        .map(|s| {
            let mut needs: Vec<String> = s.needs;
            for input_dep in &s.input {
                if !needs.contains(input_dep) {
                    needs.push(input_dep.clone());
                }
            }

            let task = match (s.task, s.uses) {
                (Some(t), None) => TaskSource::Inline(t),
                (None, Some(u)) => TaskSource::Template {
                    uses: u,
                    with: s.with.unwrap_or_default(),
                },
                _ => unreachable!(), // validated above
            };

            Stage {
                id: s.id,
                agent: s.agent,
                task,
                input: s.input,
                needs,
                output: s.output,
                model: s.model,
                backend: s.backend,
            }
        })
        .collect();

    // Resolve state
    let state = StateConfig {
        backend: raw
            .state
            .as_ref()
            .and_then(|s| s.backend.clone())
            .unwrap_or_else(|| DEFAULT_STATE_BACKEND.to_string()),
        path: raw
            .state
            .as_ref()
            .and_then(|s| s.path.clone())
            .unwrap_or_else(|| DEFAULT_STATE_PATH.to_string()),
    };

    Ok(FlowConfig {
        version: raw.version.0,
        name: raw.name,
        defaults,
        agents,
        stages,
        state,
    })
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

agents:
  - id: architect
    role: "You are a software architect"
    model: claude-opus-4-5
    backend: api
  - id: reviewer
    role: "You are a code reviewer"

stages:
  - id: design
    agent: architect
    task: "Design the system architecture"
    output: design.md
  - id: review
    agent: reviewer
    task: "Review the architecture"
    input: [design]
    needs: [design]

state:
  backend: local
  path: .koto/state
"#;

    const MINIMAL_CONFIG: &str = r#"
version: "1"
name: minimal

agents:
  - id: dev
    role: "You are a developer"

stages:
  - id: code
    agent: dev
    task: "Write code"
"#;

    #[test]
    fn full_config_parses() {
        let config = load_config_from_str(FULL_CONFIG).unwrap();
        assert_eq!(config.name, "planning-team");
        assert_eq!(config.version, "1");
        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.stages.len(), 2);
        assert_eq!(config.agents[0].model, "claude-opus-4-5");
        assert_eq!(config.agents[1].model, "claude-opus-4-5"); // from defaults
        assert_eq!(config.state.backend, "local");
        assert_eq!(config.state.path, ".koto/state");
    }

    #[test]
    fn minimal_config_parses() {
        let config = load_config_from_str(MINIMAL_CONFIG).unwrap();
        assert_eq!(config.name, "minimal");
        assert_eq!(config.agents.len(), 1);
        assert_eq!(config.stages.len(), 1);
        // Global defaults applied
        assert_eq!(config.agents[0].model, DEFAULT_MODEL);
        assert_eq!(config.agents[0].backend, Backend::ClaudeCli);
        assert_eq!(config.defaults.model, DEFAULT_MODEL);
        assert_eq!(config.state.backend, DEFAULT_STATE_BACKEND);
        assert_eq!(config.state.path, DEFAULT_STATE_PATH);
    }

    #[test]
    fn missing_required_field_name() {
        let yaml = r#"
version: "1"
agents:
  - id: dev
    role: "dev"
stages:
  - id: code
    agent: dev
    task: "do it"
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("name"),
            "error should mention 'name': {}",
            err
        );
    }

    #[test]
    fn missing_required_field_agents() {
        let yaml = r#"
version: "1"
name: test
stages:
  - id: code
    agent: dev
    task: "do it"
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("agents"),
            "error should mention 'agents': {}",
            err
        );
    }

    #[test]
    fn missing_agent_role() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
stages:
  - id: code
    agent: dev
    task: "do it"
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("role"),
            "error should mention 'role': {}",
            err
        );
    }

    #[test]
    fn unknown_field_warns_but_succeeds() {
        let yaml = r#"
version: "1"
name: test
extra_field: hello
agents:
  - id: dev
    role: "dev"
    unknown_prop: 42
stages:
  - id: code
    agent: dev
    task: "do it"
"#;
        let config = load_config_from_str(yaml).unwrap();
        assert_eq!(config.name, "test");
    }

    #[test]
    fn duplicate_agent_id_errors() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "developer"
  - id: dev
    role: "another dev"
stages:
  - id: code
    agent: dev
    task: "do it"
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("duplicate agent id 'dev'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn stage_references_nonexistent_agent() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: code
    agent: ghost
    task: "do it"
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("unknown agent 'ghost'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn input_implies_needs() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: first
    agent: dev
    task: "do first thing"
  - id: second
    agent: dev
    task: "do second thing"
    input: [first]
"#;
        let config = load_config_from_str(yaml).unwrap();
        let second = &config.stages[1];
        assert_eq!(second.input, vec!["first"]);
        assert!(second.needs.contains(&"first".to_string()));
    }

    #[test]
    fn version_as_integer() {
        let yaml = r#"
version: 1
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: code
    agent: dev
    task: "do it"
"#;
        let config = load_config_from_str(yaml).unwrap();
        assert_eq!(config.version, "1");
    }

    #[test]
    fn version_as_string() {
        let config = load_config_from_str(MINIMAL_CONFIG).unwrap();
        assert_eq!(config.version, "1");
    }

    #[test]
    fn model_defaults_applied() {
        let yaml = r#"
version: "1"
name: test
defaults:
  model: custom-model
agents:
  - id: dev
    role: "dev"
  - id: senior
    role: "senior"
    model: override-model
stages:
  - id: code
    agent: dev
    task: "do it"
"#;
        let config = load_config_from_str(yaml).unwrap();
        assert_eq!(config.agents[0].model, "custom-model"); // from defaults
        assert_eq!(config.agents[1].model, "override-model"); // agent override
    }

    #[test]
    fn task_and_uses_both_present_errors() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: code
    agent: dev
    task: "do it"
    uses: some-template
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot have both 'task' and 'uses'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn neither_task_nor_uses_errors() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: code
    agent: dev
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string()
                .contains("must have either 'task' or 'uses'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn stage_references_nonexistent_stage_in_needs() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: code
    agent: dev
    task: "do it"
    needs: [phantom]
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("unknown stage 'phantom'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn stage_references_nonexistent_stage_in_input() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: code
    agent: dev
    task: "do it"
    input: [phantom]
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("unknown stage 'phantom'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn backend_enum_parses() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
    backend: claude-cli
  - id: local
    role: "local model"
    backend: ollama
stages:
  - id: code
    agent: dev
    task: "do it"
    backend: api
"#;
        let config = load_config_from_str(yaml).unwrap();
        assert_eq!(config.agents[0].backend, Backend::ClaudeCli);
        assert_eq!(config.agents[1].backend, Backend::Ollama);
        assert_eq!(config.stages[0].backend, Some(Backend::Api));
    }

    #[test]
    fn uses_template_parses() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: code
    agent: dev
    uses: code-review
    with:
      language: rust
"#;
        let config = load_config_from_str(yaml).unwrap();
        match &config.stages[0].task {
            TaskSource::Template { uses, with } => {
                assert_eq!(uses, "code-review");
                assert!(with.contains_key("language"));
            }
            _ => panic!("expected Template task source"),
        }
    }

    #[test]
    fn unsupported_version_errors() {
        let yaml = r#"
version: "2"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: code
    agent: dev
    task: "do it"
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("unsupported version '2'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn duplicate_stage_id_errors() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
stages:
  - id: code
    agent: dev
    task: "first"
  - id: code
    agent: dev
    task: "second"
"#;
        let err = load_config_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("duplicate stage id 'code'"),
            "got: {}",
            err
        );
    }

    #[test]
    fn skills_field_parses() {
        let yaml = r#"
version: "1"
name: test
agents:
  - id: dev
    role: "dev"
    skills: [domain-cli, error-handling]
stages:
  - id: code
    agent: dev
    task: "do it"
"#;
        let config = load_config_from_str(yaml).unwrap();
        assert_eq!(
            config.agents[0].skills,
            vec!["domain-cli", "error-handling"]
        );
    }

    #[test]
    fn skills_field_defaults_to_empty() {
        let config = load_config_from_str(MINIMAL_CONFIG).unwrap();
        assert!(config.agents[0].skills.is_empty());
    }
}
