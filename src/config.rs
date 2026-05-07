use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::koto_config::{KOTO_CONFIG_FILE, KotoConfig, Seeds};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    Api,
    /// `claude-cli` is the project's primary backend (matches the agent
    /// defaults shipped under `.kuro/agents/`). It is the `Default` variant
    /// so downstream `Default` derives on `Defaults`/`Step`/`FlowConfig`
    /// produce values that match the runtime's effective default backend.
    #[default]
    ClaudeCli,
    Codex,
    Ollama,
}

impl Backend {
    /// Canonical kebab-case name as used in YAML (e.g. `claude-cli`,
    /// `codex`). Mirrors the `#[serde(rename_all = "kebab-case")]`
    /// deserialization mapping so error messages and the `extra_args` lookup
    /// table stay consistent with the on-disk schema.
    pub fn yaml_name(self) -> &'static str {
        match self {
            Backend::Api => "api",
            Backend::ClaudeCli => "claude-cli",
            Backend::Codex => "codex",
            Backend::Ollama => "ollama",
        }
    }

    /// Parse the YAML form back into a [`Backend`]. Returns `None` for any
    /// other string, which the caller turns into a validation error with the
    /// list of valid backend names.
    pub fn from_yaml_name(s: &str) -> Option<Backend> {
        match s {
            "api" => Some(Backend::Api),
            "claude-cli" => Some(Backend::ClaudeCli),
            "codex" => Some(Backend::Codex),
            "ollama" => Some(Backend::Ollama),
            _ => None,
        }
    }
}

/// Where a step's output should be auto-posted as a GitHub comment.
///
/// Set on a step via `post_comment: pr` or `post_comment: issue`. The runner
/// picks up the target number from the `id` template variable -- consistent
/// with the placeholder convention used by every flow that ships with kuromaku
/// (see the tests in this module that ban `{{pr}}` and `{{issue}}` in flow
/// YAML).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PostCommentTarget {
    Pr,
    Issue,
}

/// Accepts both `"1"` (string) and `1` (integer) in YAML.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

impl Serialize for Version {
    /// Serialize as the canonical string form. The on-disk schema accepts
    /// `version: "1"` and `version: 1`, but the in-memory representation is
    /// always a string -- so the round-trip output is always quoted.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

// --- Raw serde structs (what we deserialize from YAML) ---

/// Role default (maps role name to default agent ID).
#[derive(Debug, Deserialize)]
pub struct RawRoleDefault {
    pub default: String,
}

/// Flow config file format (lives in .kuro/flows/<name>.yaml).
#[derive(Debug, Deserialize)]
pub struct RawFlowConfig {
    pub version: Version,
    pub name: String,
    pub prompt: Option<String>,
    /// Sibling-file source for `prompt:` (issue #258). When set, the
    /// path-aware loader reads the file relative to the flow YAML's
    /// directory and folds the contents into [`RawFlowConfig::prompt`].
    /// Mutually exclusive with `prompt:`. After
    /// [`resolve_raw_external_prompts`] runs this is always `None`.
    #[serde(default)]
    pub prompt_file: Option<String>,
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
    /// Step discriminator. Currently the only value the parser acts on is
    /// `"conversation"` (issue #170). Absent or `"agent"`/`"shell"` falls
    /// back to the existing implicit kind detection (`agent:` -> LLM,
    /// `run:` -> shell). Kept as a free-form string so a future schema
    /// version can add types without a breaking enum migration; unknown
    /// values trigger a validation error so typos surface early.
    #[serde(rename = "type")]
    pub step_type: Option<String>,
    pub agent: Option<String>,
    pub role: Option<String>,
    pub task: Option<String>,
    /// Sibling-file source for `task:` (issue #258). When set, the
    /// path-aware loader reads the file relative to the flow YAML's
    /// directory and folds the contents into [`RawStep::task`].
    /// Mutually exclusive with `task:`. After
    /// [`resolve_raw_external_prompts`] runs this is always `None`.
    #[serde(default)]
    pub task_file: Option<String>,
    /// Shell command to execute via `sh -c` instead of calling an LLM. When
    /// set, the step is a shell step: `agent`, `role`, `task`, `model`, and
    /// `backend` must not be set. stdout is captured as the step output.
    pub run: Option<String>,
    /// Conversation-step participants (issue #170). Used together with
    /// `type: conversation`. Each entry is an agent ID resolved via the
    /// seeds cascade, same as `agent:` and `role:`.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Conversation-step hard cap on total agent turns across the
    /// conversation. Required when `type: conversation`.
    pub max_turns: Option<usize>,
    /// Conversation-step idle timeout in seconds. Defaults to 600 (issue
    /// #169 RouterConfig default).
    pub turn_timeout: Option<u64>,
    #[serde(default)]
    pub input: Vec<String>,
    #[serde(default)]
    pub needs: Vec<String>,
    pub model: Option<String>,
    pub backend: Option<Backend>,
    #[serde(default)]
    pub print_output: bool,
    /// Optional GitHub comment target. When set, the runner posts this
    /// step's output as a PR or issue comment after the step succeeds.
    #[serde(default)]
    pub post_comment: Option<PostCommentTarget>,
    /// Backend-keyed extra CLI arguments. Each entry is a list of literal
    /// argv tokens (no shell parsing) that the runner splices into the
    /// command for the matching backend (#236). String keys are validated
    /// against [`Backend::from_yaml_name`] in [`validate_and_resolve`] so
    /// typos surface at parse time.
    #[serde(default)]
    pub extra_args: HashMap<String, Vec<String>>,
    #[serde(flatten)]
    pub unknown: HashMap<String, serde_yaml::Value>,
}

/// Agent file format (lives in .kuro/agents/<id>.yaml).
#[derive(Debug, Deserialize)]
pub struct RawAgentFile {
    pub name: String,
    pub title: Option<String>,
    /// Optional 1-2 sentence summary above the role. Documentation only --
    /// the runner does not consume it. Issue #267.
    pub description: Option<String>,
    pub role: String,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    pub model: Option<String>,
    /// Capability tier, resolved against `tiers:` in the project config at
    /// load time.
    pub tier: Option<String>,
    pub backend: Option<Backend>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Backend-keyed extra CLI arguments (#236). Same shape as the
    /// step-level field; the runner picks the agent map only when the step
    /// does not declare its own.
    #[serde(default)]
    pub extra_args: HashMap<String, Vec<String>>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlowConfig {
    pub version: String,
    pub name: String,
    pub prompt: Option<String>,
    pub defaults: Defaults,
    pub roles: HashMap<String, String>,
    pub steps: Vec<Step>,
    pub stack: StackConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Defaults {
    pub model: String,
    pub backend: Backend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub title: Option<String>,
    /// Optional 1-2 sentence summary of the agent. Documentation only --
    /// the runner does not inject this anywhere. Tools (list-agents, future
    /// TUI/web UI) can surface it. See issue #267.
    pub description: Option<String>,
    pub role: String,
    pub model: String,
    pub backend: Backend,
    pub rules: Vec<String>,
    pub skills: Vec<String>,
    pub env: HashMap<String, String>,
    /// Backend-keyed extra CLI arguments (#236). Empty map = no overrides.
    /// When a step does not declare its own `extra_args`, the runner uses
    /// this map for whichever backend is effective for the step.
    pub extra_args: HashMap<Backend, Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Step {
    pub id: String,
    /// Agent ID for LLM-backed steps. Empty string for shell steps (where
    /// `run` is `Some`) and conversation steps (where `agents` is
    /// non-empty). Use [`Step::is_shell`] / [`Step::is_conversation`] to
    /// discriminate; never assume non-empty.
    pub agent: String,
    /// Role name when the step uses `role:` instead of a direct `agent:`.
    /// `None` for direct agent assignment or shell steps. Drives the role
    /// cascade in #129.
    pub role: Option<String>,
    pub task: Option<String>,
    /// Shell command for `run:` steps. `None` for LLM-backed steps. When
    /// `Some`, the step bypasses agent/LLM logic entirely: the runner
    /// executes the command via `sh -c`, captures stdout as the output, and
    /// stops the flow on non-zero exit.
    pub run: Option<String>,
    pub input: Vec<String>,
    pub needs: Vec<String>,
    pub model: Option<String>,
    pub backend: Option<Backend>,
    pub print_output: bool,
    pub post_comment: Option<PostCommentTarget>,
    /// Conversation-step participants (issue #170). Non-empty only when the
    /// step is a conversation step. The runner spawns one transport per
    /// participant and routes messages through the messaging Router.
    pub agents: Vec<String>,
    /// Conversation-step hard cap on total turns. Required and `> 0` for
    /// conversation steps; `None` otherwise.
    pub max_turns: Option<usize>,
    /// Conversation-step idle timeout in seconds. `None` means "use the
    /// Router default" (currently 600s -- see RouterConfig).
    pub turn_timeout: Option<u64>,
    /// Backend-keyed extra CLI arguments (#236). Empty map = no step-level
    /// overrides; the runner falls back to the agent's `extra_args`.
    /// Resolution is replace-not-merge: a non-empty step map fully shadows
    /// the agent map, even if the step map has no entry for the effective
    /// backend.
    pub extra_args: HashMap<Backend, Vec<String>>,
}

impl Step {
    /// True when this step is a shell step (has `run:` instead of `agent:`).
    pub fn is_shell(&self) -> bool {
        self.run.is_some()
    }

    /// True when this step is a multi-agent conversation step (issue #170).
    /// Detected by a non-empty `agents:` list -- validation in
    /// [`validate_and_resolve`] guarantees the list is only populated when
    /// `type: conversation` was specified.
    pub fn is_conversation(&self) -> bool {
        !self.agents.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackConfig {
    pub backend: String,
    pub path: String,
}

// --- Graph flow schema (issue #237) ---

/// On-disk flow shape. Either the original linear sequence (`flow:`) or the
/// new state graph (`states:`). Returned by [`load_flow_any_from_str`] for
/// callers that need to handle both shapes; the legacy
/// [`load_flow_from_str`] still returns just [`FlowConfig`] (linear) so
/// existing call sites do not need to switch in lockstep with the schema
/// addition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    Linear(FlowConfig),
    Graph(GraphFlow),
}

/// State-graph flow definition.
///
/// Issue #317 redesign: the top-level key is `graph:` (not `states:`),
/// transitions use `next:` lists (not `edges:`), shell states use
/// `run:` (not `kind: shell` + `command:`), and terminal states use
/// `final:` (not `kind: final` + `description:`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphFlow {
    pub version: Version,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Sibling-file source for `prompt:` (issue #258). The path-aware
    /// loader resolves the file relative to the flow YAML's directory
    /// and folds the contents into [`GraphFlow::prompt`]. Mutually
    /// exclusive with `prompt:`. After
    /// [`resolve_graph_external_prompts`] runs this is always `None`,
    /// so the runtime never has to branch on which way the prompt was
    /// authored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_file: Option<String>,
    /// State ID where the graph starts. Must reference a key in
    /// [`GraphFlow::graph`]; the structural check fires in
    /// [`validate_graph_flow`].
    pub initial: String,
    /// State definitions, keyed by state ID. Order is preserved so a
    /// future Mermaid exporter can render states in declaration order.
    pub graph: IndexMap<String, GraphState>,
}

/// One node in the state graph (issue #317 redesign).
///
/// Three state shapes:
///
/// * **Agent state**: has `role:`, `task:`, and `next:`. The runtime
///   asks the bound agent to pick one of the `next:` targets.
/// * **Shell state**: has `run:` and `next:`. The runtime executes
///   the command via `sh -c` and routes by exit code (`pass`/`fail`
///   reserved reason words in `next:`).
/// * **Final state**: has `final: "description"`. Terminates the run.
/// * **Human state**: has `human: true`. Accepted at schema level but
///   not runtime-supported yet.
///
/// A state with none of these is a dead end, caught by
/// [`validate_graph_reachability`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Sibling-file source for `task:` (issue #258). The path-aware
    /// loader resolves the file relative to the flow YAML's directory
    /// and folds the contents into [`GraphState::task`]. Mutually
    /// exclusive with `task:`. After
    /// [`resolve_graph_external_prompts`] runs this is always `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_file: Option<String>,
    /// Shell command for shell states (replaces `kind: shell` +
    /// `command:`). When present, the state is a deterministic
    /// exit-code-routed gate with no LLM call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Terminal state description (replaces `kind: final` +
    /// `description:`). When present, the state is a terminal that
    /// ends the run. The string value documents intent.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "final")]
    pub final_desc: Option<String>,
    /// Human-handoff marker. When `true`, the state hands off to a
    /// human operator. Accepted at schema level but not runtime-supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human: Option<bool>,
    /// Outgoing transitions. Each entry maps a target state name to an
    /// optional reason string. The YAML key is `next:`.
    #[serde(default, rename = "next", skip_serializing_if = "Option::is_none")]
    pub select: Option<Vec<SelectEntry>>,
}

impl GraphState {
    /// True when this state is a terminal (`final:` present).
    pub fn is_final(&self) -> bool {
        self.final_desc.is_some()
    }

    /// True when this state is a human-handoff (`human: true`).
    pub fn is_human(&self) -> bool {
        self.human == Some(true)
    }

    /// True when this state is a shell gate (`run:` present).
    pub fn is_shell(&self) -> bool {
        self.run.is_some()
    }

    /// True when this state is terminal (final or human).
    pub fn is_terminal(&self) -> bool {
        self.is_final() || self.is_human()
    }
}

/// One entry in a state's `next:` list.
///
/// YAML forms:
/// - `- target` (bare string, no reason)
/// - `- target: "reason"` (single reason)
/// - `- target: ["reason1", "reason2"]` (list of reasons, OR-combined)
/// - `- target: |` (multiline string reason)
///
/// For shell states, `pass` and `fail` are reserved reason words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectEntry {
    pub target: String,
    pub reason: Option<SelectReason>,
}

/// The reason(s) attached to a select entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectReason {
    Single(String),
    List(Vec<String>),
}

impl SelectReason {
    /// Return a human-readable summary of the reason(s).
    pub fn display(&self) -> String {
        match self {
            SelectReason::Single(s) => s.clone(),
            SelectReason::List(v) => v.join(" | "),
        }
    }
}

impl Serialize for SelectEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match &self.reason {
            None => serializer.serialize_str(&self.target),
            Some(SelectReason::Single(s)) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(&self.target, s)?;
                map.end()
            }
            Some(SelectReason::List(v)) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(&self.target, v)?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for SelectEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SelectEntryVisitor;

        impl<'de> serde::de::Visitor<'de> for SelectEntryVisitor {
            type Value = SelectEntry;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string (bare target) or a single-key map (target: reason)")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<SelectEntry, E> {
                Ok(SelectEntry {
                    target: v.to_string(),
                    reason: None,
                })
            }

            fn visit_map<M>(self, mut map: M) -> Result<SelectEntry, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let (key, value): (String, serde_yaml::Value) = map
                    .next_entry()?
                    .ok_or_else(|| serde::de::Error::custom("empty map in select entry"))?;
                if map.next_key::<String>()?.is_some() {
                    return Err(serde::de::Error::custom(
                        "select entry must have exactly one key (the target state name)",
                    ));
                }
                let reason = match value {
                    serde_yaml::Value::String(s) => Some(SelectReason::Single(s)),
                    serde_yaml::Value::Sequence(seq) => {
                        let strings: Result<Vec<String>, _> = seq
                            .into_iter()
                            .map(|v| match v {
                                serde_yaml::Value::String(s) => Ok(s),
                                _ => Err(serde::de::Error::custom(
                                    "select entry reason list must contain only strings",
                                )),
                            })
                            .collect();
                        Some(SelectReason::List(strings?))
                    }
                    serde_yaml::Value::Null => None,
                    _ => {
                        return Err(serde::de::Error::custom(
                            "select entry value must be a string, list of strings, or null",
                        ));
                    }
                };
                Ok(SelectEntry {
                    target: key,
                    reason,
                })
            }
        }

        deserializer.deserialize_any(SelectEntryVisitor)
    }
}

/// Probe struct used by [`load_flow_any_from_str`] to decide which shape
/// the YAML is in before committing to a full parse. Looking at just
/// these two top-level fields lets us emit the "pick one" error before
/// the stricter graph parser flags graph-only fields as unknown on a
/// linear flow (or vice versa), giving the user a clearer message.
#[derive(Deserialize)]
struct FlowShapeProbe {
    #[serde(default)]
    flow: Option<serde_yaml::Value>,
    #[serde(default)]
    graph: Option<serde_yaml::Value>,
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
    load_flow_from_str_with_project(contents, role_overrides, &HashMap::new())
}

/// Load flow from YAML string with role overrides AND project-level role
/// bindings (from the project config). Steps that reference a role only
/// defined at the project level resolve through `project_roles` so the flow
/// does not need to redeclare every role used.
pub fn load_flow_from_str_with_project(
    contents: &str,
    role_overrides: &HashMap<String, String>,
    project_roles: &HashMap<String, String>,
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
    validate_and_resolve(raw, role_overrides, project_roles)
}

/// Load a flow YAML that may be either linear (`flow:`) or graph
/// (`states:`) shaped (issue #237).
///
/// Dispatches on which top-level field is present:
/// * `flow:` only -> [`Flow::Linear`] (delegates to [`load_flow_from_str`])
/// * `states:` only -> [`Flow::Graph`] (delegates to [`load_graph_flow_from_str`])
/// * both set -> hard error ("pick one"), per issue #237 acceptance criteria
/// * neither set -> hard error naming both fields
///
/// New callers that want graph support use this entry. Existing call
/// sites keep using [`load_flow_from_str`] until the runtime side of the
/// graph schema lands.
#[allow(dead_code)]
pub fn load_flow_any_from_str(contents: &str) -> Result<Flow, ConfigError> {
    let probe: FlowShapeProbe = serde_yaml::from_str(contents)?;
    match (probe.flow.is_some(), probe.graph.is_some()) {
        (true, true) => Err(ConfigError::Validation(
            "flow file declares both 'flow:' and 'graph:' -- pick one (linear flow vs state graph)"
                .to_string(),
        )),
        (false, false) => Err(ConfigError::Validation(
            "flow file must declare either 'flow:' (linear) or 'graph:' (state graph)".to_string(),
        )),
        (true, false) => Ok(Flow::Linear(load_flow_from_str(contents)?)),
        (false, true) => Ok(Flow::Graph(load_graph_flow_from_str(contents)?)),
    }
}

/// Load a graph-shaped flow YAML.
///
/// Schema-only validation (issue #237): version match, `initial:`
/// references a known state, every `edge.to:` references a known state,
/// and each state has at least one of `edges:` or `kind:`. Reachability
/// and dead-end checks are deferred to the validator issue.
#[allow(dead_code)]
pub fn load_graph_flow_from_str(contents: &str) -> Result<GraphFlow, ConfigError> {
    let raw: GraphFlow = serde_yaml::from_str(contents)?;
    validate_graph_flow(&raw)?;
    Ok(raw)
}

// --- External-prompt resolution (issue #258) ---

/// Read a sibling prompt file and fold it into `value`.
///
/// `value` and `file_field` are the inline (`task:` / `prompt:`) and
/// external-file (`task_file:` / `prompt_file:`) fields of the same
/// element. After this returns `Ok`, `file_field` is always `None` and
/// `value` is `Some(_)` whenever the YAML had either field.
///
/// Returns `ConfigError::Validation` when both fields are set, when the
/// path escapes `base_dir` (absolute, contains a `..` component, or
/// canonicalizes outside the base), or when the file is missing.
/// Other I/O errors (permissions, unreadable bytes) are surfaced as
/// `ConfigError::Io` with a message that names the locator and field.
///
/// `locator` and `field` are used to build human-readable error
/// messages (e.g. `"flow '<path>' state 'design': task_file '<rel>' ..."`).
fn resolve_prompt_field(
    value: &mut Option<String>,
    file_field: &mut Option<String>,
    base_dir: &Path,
    locator: &str,
    field: &str,
) -> Result<(), ConfigError> {
    let file_field_name = format!("{field}_file");
    let Some(rel) = file_field.take() else {
        // No external file -- nothing to resolve. Inline `value` (if
        // any) stays as-is.
        return Ok(());
    };

    if value.is_some() {
        return Err(ConfigError::Validation(format!(
            "{locator}: both '{field}' and '{file_field_name}' set -- pick one"
        )));
    }

    // Component-level traversal guard. Runs before any I/O so the `..`
    // case is rejected deterministically even when the file does not
    // exist on disk.
    let rel_path = Path::new(&rel);
    for component in rel_path.components() {
        match component {
            std::path::Component::ParentDir => {
                return Err(ConfigError::Validation(format!(
                    "{locator}: {file_field_name} '{rel}' escapes the flow directory ('..' is not allowed)"
                )));
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(ConfigError::Validation(format!(
                    "{locator}: {file_field_name} '{rel}' must be a relative path under the flow directory"
                )));
            }
            _ => {}
        }
    }

    let joined = base_dir.join(rel_path);

    // Read with a hand-rolled error path so missing files become a
    // Validation error (with the flow locator and the relative path
    // the user wrote) instead of a bare std::io::Error string. Other
    // I/O errors keep the original kind via ConfigError::Io.
    let contents = match std::fs::read_to_string(&joined) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfigError::Validation(format!(
                "{locator}: {file_field_name} '{rel}' not found (looked in '{}')",
                joined.display()
            )));
        }
        Err(e) => {
            return Err(ConfigError::Io(std::io::Error::new(
                e.kind(),
                format!("{locator}: failed to read {file_field_name} '{rel}': {e}"),
            )));
        }
    };

    // Belt-and-suspenders symlink-escape check. The component walk
    // above rejects literal `..`, but a symlink inside `base_dir`
    // could still point outside. Both sides are canonicalized so we
    // compare resolved forms; if either canonicalize call fails the
    // check is skipped (e.g. base_dir lives in a tempdir on a
    // platform without canonicalize support) -- the read already
    // succeeded, so the file exists, and the component check guards
    // the literal-traversal case.
    if let (Ok(canon_base), Ok(canon_target)) = (base_dir.canonicalize(), joined.canonicalize())
        && !canon_target.starts_with(&canon_base)
    {
        return Err(ConfigError::Validation(format!(
            "{locator}: {file_field_name} '{rel}' resolves to '{}' which is outside the flow directory '{}'",
            canon_target.display(),
            canon_base.display()
        )));
    }

    *value = Some(contents);
    Ok(())
}

/// Fold every `prompt_file` / `task_file` on a [`RawFlowConfig`] into
/// the matching inline field, reading siblings of `base_dir` (issue
/// #258).
///
/// Runs after `serde_yaml::from_str` and before [`validate_and_resolve`]
/// so the rest of the resolver sees a flow indistinguishable from one
/// that was authored entirely inline. Variable substitution is **not**
/// applied here -- it stays in `runner::flow_api::substitute_vars`,
/// which already runs over `prompt:` / `task:` after parse, so external
/// content gets the same `{{vars.X}}` treatment automatically.
///
/// `flow_path_display` is used in error messages so the user can jump
/// straight to the offending YAML when a file is missing or escapes
/// the flow directory.
pub fn resolve_raw_external_prompts(
    raw: &mut RawFlowConfig,
    base_dir: &Path,
    flow_path_display: &str,
) -> Result<(), ConfigError> {
    resolve_prompt_field(
        &mut raw.prompt,
        &mut raw.prompt_file,
        base_dir,
        &format!("flow '{flow_path_display}'"),
        "prompt",
    )?;
    for (id, step) in raw.flow.iter_mut() {
        resolve_prompt_field(
            &mut step.task,
            &mut step.task_file,
            base_dir,
            &format!("flow '{flow_path_display}' step '{id}'"),
            "task",
        )?;
    }
    Ok(())
}

/// Fold every `prompt_file` / `task_file` on a [`GraphFlow`] into the
/// matching inline field (issue #258). Same contract as
/// [`resolve_raw_external_prompts`]. Operating on the resolved
/// [`GraphFlow`] is safe because the graph format does not have a
/// raw/resolved split -- the on-disk schema is the runtime shape.
///
/// After this returns `Ok`, `prompt_file` and every state's
/// `task_file` are guaranteed `None`, so the runtime never has to
/// branch on which way the prompt was authored.
pub fn resolve_graph_external_prompts(
    graph: &mut GraphFlow,
    base_dir: &Path,
    flow_path_display: &str,
) -> Result<(), ConfigError> {
    resolve_prompt_field(
        &mut graph.prompt,
        &mut graph.prompt_file,
        base_dir,
        &format!("flow '{flow_path_display}'"),
        "prompt",
    )?;
    for (id, state) in graph.graph.iter_mut() {
        resolve_prompt_field(
            &mut state.task,
            &mut state.task_file,
            base_dir,
            &format!("flow '{flow_path_display}' state '{id}'"),
            "task",
        )?;
    }
    Ok(())
}

/// Helper: pick the directory that contains `path`, falling back to
/// `"."` when `path` has no parent (e.g. a bare `flow.yaml` in the
/// current directory). Used by the path-aware loaders to feed the
/// external-prompt resolver.
///
/// Exposed as `flow_base_dir_for` so callers like the runner -- which
/// already has the flow YAML in memory but still wants to resolve
/// sibling prompt files -- can compute the same base directory the
/// path-aware loaders use.
pub fn flow_base_dir_for(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

/// Path-aware variant of [`load_flow_from_str`] (issue #258).
///
/// Reads `path`, parses the linear flow, resolves any
/// `prompt_file:` / `task_file:` references against the flow's
/// directory, then validates and returns the resolved
/// [`FlowConfig`]. Existing string-based loaders keep working for
/// schema-layer unit tests; only callers that need to honour
/// external prompt files have to switch.
#[allow(dead_code)]
pub fn load_flow_from_path(path: &Path) -> Result<FlowConfig, ConfigError> {
    load_flow_from_path_with_project(path, &HashMap::new(), &HashMap::new())
}

/// Path-aware variant of [`load_flow_from_str_with_project`] (issue
/// #258). Same semantics as [`load_flow_from_path`] but accepts the
/// CLI role overrides and project-level role bindings used by the
/// runner.
pub fn load_flow_from_path_with_project(
    path: &Path,
    role_overrides: &HashMap<String, String>,
    project_roles: &HashMap<String, String>,
) -> Result<FlowConfig, ConfigError> {
    let contents = std::fs::read_to_string(path)?;
    load_flow_from_str_with_project_at(
        &contents,
        flow_base_dir_for(path),
        &path.display().to_string(),
        role_overrides,
        project_roles,
    )
}

/// String-based loader that still resolves external-prompt files
/// against `base_dir` (issue #258). Used by the runner, which already
/// has the flow YAML in memory for the manifest hash and would
/// otherwise read the file twice.
pub fn load_flow_from_str_with_project_at(
    contents: &str,
    base_dir: &Path,
    flow_path_display: &str,
    role_overrides: &HashMap<String, String>,
    project_roles: &HashMap<String, String>,
) -> Result<FlowConfig, ConfigError> {
    let mut raw: RawFlowConfig = serde_yaml::from_str(contents)?;
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
    resolve_raw_external_prompts(&mut raw, base_dir, flow_path_display)?;
    validate_and_resolve(raw, role_overrides, project_roles)
}

/// Path-aware variant of [`load_flow_any_from_str`] (issue #258).
///
/// Reads `path`, dispatches on `flow:` vs `states:` like the
/// string-based loader, then resolves any external prompt files
/// against `path`'s directory. Linear flows go through
/// [`load_flow_from_str_with_project_at`]; graph flows are parsed
/// and validated, then [`resolve_graph_external_prompts`] folds the
/// `*_file` fields in.
pub fn load_flow_any_from_path(path: &Path) -> Result<Flow, ConfigError> {
    // Markdown files are always graph flows -- skip the YAML probe.
    if path.extension().and_then(|e| e.to_str()) == Some("md") {
        let contents = std::fs::read_to_string(path)?;
        let flow = crate::config_md::load_graph_flow_from_md(&contents)?;
        return Ok(Flow::Graph(flow));
    }

    let contents = std::fs::read_to_string(path)?;
    let probe: FlowShapeProbe = serde_yaml::from_str(&contents)?;
    let flow_path_display = path.display().to_string();
    let base_dir = flow_base_dir_for(path);
    match (probe.flow.is_some(), probe.graph.is_some()) {
        (true, true) => Err(ConfigError::Validation(
            "flow file declares both 'flow:' and 'graph:' -- pick one (linear flow vs state graph)"
                .to_string(),
        )),
        (false, false) => Err(ConfigError::Validation(
            "flow file must declare either 'flow:' (linear) or 'graph:' (state graph)".to_string(),
        )),
        (true, false) => Ok(Flow::Linear(load_flow_from_str_with_project_at(
            &contents,
            base_dir,
            &flow_path_display,
            &HashMap::new(),
            &HashMap::new(),
        )?)),
        (false, true) => {
            let mut graph = load_graph_flow_from_str(&contents)?;
            resolve_graph_external_prompts(&mut graph, base_dir, &flow_path_display)?;
            Ok(Flow::Graph(graph))
        }
    }
}

/// Structural validation for [`GraphFlow`] (issue #317 redesign).
///
/// Checks:
/// - version must be "1"
/// - `initial:` must reference a key in `graph:`
/// - all select targets must exist in `graph:`
/// - each target at most once per state's select list
/// - shell states (`run:` present): exactly 2 select entries, one pass one fail
/// - agent states: at least 2 select entries, non-empty reasons
/// - `final:` states must have non-empty description string
/// - no self-loops on shell states
pub(crate) fn validate_graph_flow(g: &GraphFlow) -> Result<(), ConfigError> {
    if g.version.0 != "1" {
        return Err(ConfigError::Validation(format!(
            "unsupported version '{}', expected '1'",
            g.version.0
        )));
    }

    if !g.graph.contains_key(&g.initial) {
        return Err(ConfigError::Validation(format!(
            "graph flow 'initial' references unknown state '{}' (known states: {})",
            g.initial,
            sorted_state_ids(&g.graph).join(", ")
        )));
    }

    for (id, state) in &g.graph {
        // Validate select targets reference known states
        if let Some(entries) = &state.select {
            let mut seen_targets: HashSet<&str> = HashSet::new();
            for entry in entries {
                if !g.graph.contains_key(&entry.target) {
                    return Err(ConfigError::Validation(format!(
                        "graph state '{id}' select targets unknown state '{}' (known states: {})",
                        entry.target,
                        sorted_state_ids(&g.graph).join(", ")
                    )));
                }
                if !seen_targets.insert(&entry.target) {
                    return Err(ConfigError::Validation(format!(
                        "graph state '{id}' lists target '{}' more than once in select",
                        entry.target
                    )));
                }
            }
        }
        validate_state_semantics(id, state)?;
    }

    Ok(())
}

/// Schema-level checks for graph states (issue #317 redesign).
///
/// Validates the consistency rules for each state shape:
///
/// * **Shell states** (`run:` present): must not declare `role:`,
///   `task:`, or `task_file:`. Must have exactly 2 select entries,
///   one with `pass` reason and one with `fail` reason. No self-loops.
///   `run:` must be non-empty.
/// * **Final states** (`final:` present): the description string must
///   be non-empty. Must not have `next:`, `role:`, `task:`, `run:`.
/// * **Human states** (`human: true`): schema-accepted, may have `next:`.
/// * **Agent states** (none of the above): must have `next:` with at
///   least 2 entries, each with a non-empty reason.
/// * Mutual exclusion: `run:`, `final:`, and `human:` are pairwise
///   exclusive.
fn validate_state_semantics(id: &str, state: &GraphState) -> Result<(), ConfigError> {
    // Count how many discriminator fields are set
    let mut kinds: Vec<&str> = Vec::new();
    if state.run.is_some() {
        kinds.push("run");
    }
    if state.final_desc.is_some() {
        kinds.push("final");
    }
    if state.is_human() {
        kinds.push("human");
    }
    if kinds.len() > 1 {
        return Err(ConfigError::Validation(format!(
            "graph state '{id}' has conflicting fields {kinds:?} -- use exactly one of 'run', 'final', or 'human'"
        )));
    }

    if state.is_shell() {
        // Shell state validation
        match state.run.as_deref().map(str::trim) {
            Some(cmd) if !cmd.is_empty() => {}
            _ => {
                return Err(ConfigError::Validation(format!(
                    "graph state '{id}' has `run:` but the command is empty"
                )));
            }
        }
        if state.role.is_some() {
            return Err(ConfigError::Validation(format!(
                "graph state '{id}' has `run:` and must not declare `role:` -- shell states have no agent"
            )));
        }
        if state.task.is_some() || state.task_file.is_some() {
            return Err(ConfigError::Validation(format!(
                "graph state '{id}' has `run:` and must not declare `task:` or `task_file:` -- shell states do not run an LLM"
            )));
        }
        let entries = state.select.as_ref().ok_or_else(|| {
            ConfigError::Validation(format!(
                "graph state '{id}' has `run:` and must declare `next:` with both a `pass` and a `fail` target"
            ))
        })?;
        if entries.len() != 2 {
            return Err(ConfigError::Validation(format!(
                "graph state '{id}' has `run:` and must have exactly 2 select entries (one pass, one fail), got {}",
                entries.len()
            )));
        }
        let mut has_pass = false;
        let mut has_fail = false;
        for entry in entries {
            let reason_text = entry
                .reason
                .as_ref()
                .map(|r| r.display())
                .unwrap_or_default();
            if reason_text == "pass" {
                has_pass = true;
            } else if reason_text == "fail" {
                has_fail = true;
            } else {
                return Err(ConfigError::Validation(format!(
                    "graph state '{id}' select entry '{}' must have reason 'pass' or 'fail' -- shell states are exit-code-routed",
                    entry.target
                )));
            }
            if entry.target == id {
                return Err(ConfigError::Validation(format!(
                    "graph state '{id}' select entry '{}' is a self-loop -- a shell state cannot recover from its own failure, route to a different state",
                    entry.target
                )));
            }
        }
        if !has_pass {
            return Err(ConfigError::Validation(format!(
                "graph state '{id}' has `run:` and must declare a select entry with reason 'pass'"
            )));
        }
        if !has_fail {
            return Err(ConfigError::Validation(format!(
                "graph state '{id}' has `run:` and must declare a select entry with reason 'fail'"
            )));
        }
    } else if state.is_final() {
        // Final state validation
        if let Some(desc) = &state.final_desc
            && desc.trim().is_empty()
        {
            return Err(ConfigError::Validation(format!(
                "graph state '{id}' has `final:` with an empty description"
            )));
        }
        if state.role.is_some() || state.task.is_some() || state.select.is_some() {
            return Err(ConfigError::Validation(format!(
                "graph state '{id}' is `final:` and must not declare `role:`, `task:`, or `next:`"
            )));
        }
    } else if state.is_human() {
        // Human state: accepted at schema level, may have select for resume patterns
    } else {
        // Agent state validation
        if state.role.is_none() {
            // Not an error at schema level -- will be caught by dead-end
            // detection if there's also no select. The runner validates
            // role binding separately.
        }
        if let Some(entries) = &state.select {
            if entries.len() < 2 {
                return Err(ConfigError::Validation(format!(
                    "graph state '{id}' must have at least 2 select entries (got {})",
                    entries.len()
                )));
            }
            for entry in entries {
                if entry.reason.is_none() {
                    return Err(ConfigError::Validation(format!(
                        "graph state '{id}' select entry '{}' must have a non-empty reason",
                        entry.target
                    )));
                }
                let reason_text = entry.reason.as_ref().unwrap().display();
                if reason_text.trim().is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "graph state '{id}' select entry '{}' must have a non-empty reason",
                        entry.target
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Helper: produce a sorted list of state IDs for inclusion in error
/// messages. Sorting keeps the error output stable across runs (IndexMap
/// preserves insertion order, which is fine for users editing the YAML
/// but unhelpful for diffable error messages).
fn sorted_state_ids(states: &IndexMap<String, GraphState>) -> Vec<String> {
    let mut keys: Vec<String> = states.keys().cloned().collect();
    keys.sort();
    keys
}

// --- Graph reachability + dead-end validation (issue #238) ---

/// Outcome of [`validate_graph_reachability`].
///
/// Errors and warnings are tracked separately because they have different
/// blocking semantics: a dead-end leaves the runtime stuck with no
/// transition to take and must block execution; an unreachable state is
/// usually a sign of mid-edit work and is not a reason to refuse to run.
/// Callers (CLI, runner pre-flight) decide how to format and where to
/// route each list -- the validator does not print.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphValidationReport {
    /// Hard errors. Non-empty means the flow must not run.
    pub errors: Vec<String>,
    /// Soft warnings. The flow may still run (or pass `kuro validate`)
    /// but the user should know.
    pub warnings: Vec<String>,
}

impl GraphValidationReport {
    /// True when there are no hard errors. Warnings are allowed.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Walk the graph from `initial:` to surface dead-ends and unreachable
/// states (issue #238).
///
/// Schema-level checks (referenced by [`validate_graph_flow`]) must have
/// passed before calling this -- typically via [`load_graph_flow_from_str`].
/// That guarantees `initial:` and every `edge.to:` resolve, so this
/// function can index `states` without re-checking.
///
/// The walk is a plain BFS (a [`Vec`] used as a stack would also work --
/// only the visited-set semantics matter, not the traversal order). After
/// the walk:
///
/// * Every non-terminal state with no outgoing edges becomes an *error*.
///   Terminal kinds for this purpose are `kind: final` and `kind: human`
///   (a human-handoff state may legitimately stop the run via operator
///   abort).
/// * Every state not reached from `initial:` becomes a *warning*. A
///   common cause is mid-edit work where the wiring is not done yet --
///   not a reason to block execution.
///
/// Self-loops (an edge whose `to:` is the same state) are allowed; the
/// visited-set short-circuits naturally.
pub fn validate_graph_reachability(g: &GraphFlow) -> GraphValidationReport {
    let mut report = GraphValidationReport::default();

    // BFS from `initial:`.
    let mut visited: HashSet<&str> = HashSet::new();
    let mut frontier: Vec<&str> = Vec::new();
    visited.insert(g.initial.as_str());
    frontier.push(g.initial.as_str());
    while let Some(id) = frontier.pop() {
        let Some(state) = g.graph.get(id) else {
            continue;
        };
        let Some(entries) = &state.select else {
            continue;
        };
        for entry in entries {
            if visited.insert(entry.target.as_str()) {
                frontier.push(entry.target.as_str());
            }
        }
    }

    // Dead-end detection + terminal description warning.
    for (id, state) in &g.graph {
        let has_select = state.select.as_ref().is_some_and(|e| !e.is_empty());
        let is_terminal = state.is_terminal();
        if !has_select && !is_terminal {
            report.errors.push(format!(
                "graph state '{id}' is a dead end: no select entries and not a terminal state"
            ));
        }
        // Final states with empty description: the description is in
        // the `final:` field itself, so we check its content.
        if state.is_final()
            && let Some(desc) = &state.final_desc
            && desc.trim().is_empty()
        {
            report.warnings.push(format!(
                "graph state '{id}' is terminal ('final') but has an empty description -- intent should be visible at the terminal"
            ));
        }
        // Human states don't carry a description field in the new schema
        // (the concept is accepted but minimal). No warning needed.
    }

    // Unreachable detection.
    let mut unreachable: Vec<&str> = g
        .graph
        .keys()
        .map(String::as_str)
        .filter(|k| !visited.contains(k))
        .collect();
    unreachable.sort_unstable();
    for id in unreachable {
        report.warnings.push(format!(
            "graph state '{id}' is unreachable from initial state '{}'",
            g.initial
        ));
    }

    report
}

/// Parse just the role names from a flow YAML (for CLI arg partitioning).
pub fn parse_role_names(contents: &str) -> Result<HashSet<String>, ConfigError> {
    let raw: RawFlowConfig = serde_yaml::from_str(contents)?;
    Ok(raw.roles.keys().cloned().collect())
}

/// Load a single agent file from a single `.kuro/` directory. Test-only
/// convenience wrapper -- internally builds a single-seed list and delegates
/// to [`load_agent_file_with_seeds`]. Production callers should construct a
/// [`Seeds`] up front (run_up/run_task already do).
#[cfg(test)]
pub fn load_agent_file(
    koto_dir: &Path,
    agent_id: &str,
    defaults: &Defaults,
    koto_config: Option<&KotoConfig>,
) -> Result<Agent, ConfigError> {
    let seeds = Seeds {
        seeds: vec![crate::koto_config::Seed {
            source: crate::koto_config::SeedSource::Local {
                display: koto_dir.display().to_string(),
                path: koto_dir.to_path_buf(),
            },
        }],
    };
    let (agent, _origin_seed, _sha) =
        load_agent_file_with_seeds(&seeds, agent_id, defaults, koto_config)?;
    Ok(agent)
}

/// Load a single agent file via the seed list.
///
/// Walks the configured seeds top-to-bottom and reads the first
/// `agents/<agent_id>.yaml` it finds. Returns the loaded [`Agent`] together
/// with the index of the seed it came from -- callers use that for the
/// resolution audit ("loaded from seed X").
///
/// Agent IDs may contain `/` (e.g. `coding/rust/Sage`); each path segment is
/// pushed onto the seed's `agents/` directory using `PathBuf::push` so the
/// lookup is cross-platform safe.
///
/// `koto_config` is the optional project-level config. When present and the
/// agent declares a `tier:` field, the tier resolves to a concrete
/// `<provider>/<model-id>` string. When absent and a tier is declared, this
/// returns a validation error -- a tier-bound agent cannot be used without
/// a tiers map.
pub fn load_agent_file_with_seeds(
    seeds: &Seeds,
    agent_id: &str,
    defaults: &Defaults,
    koto_config: Option<&KotoConfig>,
) -> Result<(Agent, usize, String), ConfigError> {
    let rel = agent_rel_path(agent_id);
    let (seed_idx, path) = seeds
        .find(&rel)
        .map_err(|e| ConfigError::Validation(e.message()))?
        .ok_or_else(|| ConfigError::Validation(seeds.not_found_message("agent", agent_id)))?;

    let contents = std::fs::read_to_string(&path)?;
    // Hash the bytes we just loaded -- not the file on disk later. If the
    // user edits the agent file mid-run, the manifest must record the bytes
    // the LLM actually saw, not whatever happens to be on disk when the
    // manifest is written. See review on PR #158.
    let source_sha256 = crate::stack::sha256_hex(contents.as_bytes());
    let raw: RawAgentFile = serde_yaml::from_str(&contents)?;
    warn_unknown_fields(&format!("agent file '{agent_id}'"), &raw.unknown);

    let model = resolve_agent_model(&raw, defaults, koto_config)?;
    let extra_args = validate_extra_args(raw.extra_args, &format!("agent file '{agent_id}'"))?;

    Ok((
        Agent {
            id: agent_id.to_string(),
            name: raw.name,
            title: raw.title,
            description: raw.description,
            role: raw.role,
            model,
            backend: raw.backend.unwrap_or(defaults.backend),
            rules: raw.rules,
            skills: raw.skills,
            env: raw.env,
            extra_args,
        },
        seed_idx,
        source_sha256,
    ))
}

/// Convert a string-keyed `extra_args` map (raw YAML form) into a
/// [`Backend`]-keyed map. Unknown keys produce a validation error that names
/// the offending key and lists the supported backend names. The `api` backend
/// is rejected explicitly: it talks to the HTTP API rather than spawning a
/// CLI, so there is no argv slot for extra arguments to slot into. Without
/// the explicit reject, `api` keys would parse cleanly and then get silently
/// dropped at command-build time -- a confusing footgun. Empty entries
/// (`backend: []`) are kept as-is so callers can detect an explicit "clear
/// extra_args for this backend" intent if a future feature wants it; they
/// simply produce no argv tokens at command-build time.
fn validate_extra_args(
    raw: HashMap<String, Vec<String>>,
    context: &str,
) -> Result<HashMap<Backend, Vec<String>>, ConfigError> {
    let mut out = HashMap::with_capacity(raw.len());
    for (key, val) in raw {
        let backend = Backend::from_yaml_name(&key).ok_or_else(|| {
            ConfigError::Validation(format!(
                "unknown backend in extra_args: '{key}' (valid: claude-cli, codex, ollama) in {context}"
            ))
        })?;
        if backend == Backend::Api {
            return Err(ConfigError::Validation(format!(
                "extra_args is not supported for backend 'api' in {context} -- the api backend talks to the HTTP API, not a CLI, so there is no argv to extend"
            )));
        }
        out.insert(backend, val);
    }
    Ok(out)
}

/// Build the relative path for an agent ID, splitting on `/` so nested
/// agent IDs (`coding/rust/Sage`) resolve to `agents/coding/rust/Sage.yaml`.
/// The split-and-push approach keeps the lookup portable -- on Windows the
/// resulting `PathBuf` uses native separators.
fn agent_rel_path(agent_id: &str) -> PathBuf {
    let parts: Vec<&str> = agent_id.split('/').collect();
    let last = parts.last().copied().unwrap_or("");
    let mut p = PathBuf::from("agents");
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        p.push(part);
    }
    p.push(format!("{last}.yaml"));
    p
}

/// Resolve the model string for an agent.
///
/// Precedence (highest first):
/// 1. `tier:` resolved through the project config's `tiers:` map
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
            // `.message()` returns the inner payload without the Display
            // "validation error:" prefix, so wrapping in `ConfigError::Validation`
            // does not duplicate it.
            .map_err(|e| ConfigError::Validation(e.message())),
        (Some(tier_name), None) => Err(ConfigError::Validation(format!(
            "agent \"{}\" declares tier \"{tier_name}\" but no {KOTO_CONFIG_FILE} found",
            raw.name
        ))),
        (None, _) => Ok(raw.model.clone().unwrap_or_else(|| defaults.model.clone())),
    }
}

/// Load all agents referenced by the flow steps, walking the configured seed
/// list. Returns the loaded agents in step order plus two parallel maps keyed
/// by agent ID: the seed index each agent came from (used by the audit to
/// show overlay decisions) and the SHA-256 of the bytes that were actually
/// read from disk. The hash is captured here -- not at manifest-write time --
/// so the manifest records what the LLM saw, not what's on disk later.
#[allow(clippy::type_complexity)]
pub fn load_agents_for_flow_with_seeds(
    seeds: &Seeds,
    config: &FlowConfig,
    koto_config: Option<&KotoConfig>,
) -> Result<(Vec<Agent>, HashMap<String, usize>, HashMap<String, String>), ConfigError> {
    let mut agents: Vec<Agent> = Vec::new();
    let mut origins: HashMap<String, usize> = HashMap::new();
    let mut hashes: HashMap<String, String> = HashMap::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for step in &config.steps {
        // Shell steps (issue #23) have no agent. Skip the load so we don't
        // try to resolve an empty path under `agents/`.
        if step.is_shell() {
            continue;
        }
        // Conversation steps (issue #170) carry their participants in
        // `agents:` instead of the singular `agent:` field. Iterate the
        // list and dedupe via the same `seen` set so an agent that
        // appears in both an agent step and a conversation step is loaded
        // exactly once.
        if step.is_conversation() {
            for agent_id in &step.agents {
                if seen.insert(agent_id.clone()) {
                    let (agent, seed_idx, sha) =
                        load_agent_file_with_seeds(seeds, agent_id, &config.defaults, koto_config)?;
                    origins.insert(agent.id.clone(), seed_idx);
                    hashes.insert(agent.id.clone(), sha);
                    agents.push(agent);
                }
            }
            continue;
        }
        if seen.insert(step.agent.clone()) {
            let (agent, seed_idx, sha) =
                load_agent_file_with_seeds(seeds, &step.agent, &config.defaults, koto_config)?;
            origins.insert(agent.id.clone(), seed_idx);
            hashes.insert(agent.id.clone(), sha);
            agents.push(agent);
        }
    }

    Ok((agents, origins, hashes))
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
    project_roles: &HashMap<String, String>,
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
            // Conversation step (issue #170) is selected by `type:
            // conversation`. We dispatch on it BEFORE the
            // agent/role/run/conversation kind check so the error messages
            // can be tailored to the step kind ("conversation step needs
            // `agents:`" beats a generic "must specify one of agent, role,
            // run").
            //
            // Unknown `type:` values are rejected up front so a typo
            // surfaces here, not as a silent fallback to LLM mode.
            if let Some(ref t) = s.step_type {
                match t.as_str() {
                    "conversation" => {
                        return resolve_conversation_step(id, s);
                    }
                    "agent" | "shell" => {
                        // Explicit aliases for the implicit kinds. Fall
                        // through to the agent/role/run logic below.
                    }
                    other => {
                        return Err(ConfigError::Validation(format!(
                            "step '{id}' has unknown type '{other}' -- expected 'agent', 'shell', or 'conversation'"
                        )));
                    }
                }
            }

            // Conversation-only fields used outside a conversation step are
            // always a config error -- they would otherwise be silently
            // dropped.
            if !s.agents.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "step '{id}' sets 'agents:' but is not a conversation step -- add 'type: conversation'"
                )));
            }
            if s.max_turns.is_some() {
                return Err(ConfigError::Validation(format!(
                    "step '{id}' sets 'max_turns:' but is not a conversation step -- add 'type: conversation'"
                )));
            }
            if s.turn_timeout.is_some() {
                return Err(ConfigError::Validation(format!(
                    "step '{id}' sets 'turn_timeout:' but is not a conversation step -- add 'type: conversation'"
                )));
            }

            // Exactly one of `agent`, `role`, `run` must be set. Listing each
            // present field in the error makes the conflict obvious instead of
            // forcing the user to guess which two collided.
            let mut kinds: Vec<&str> = Vec::new();
            if s.agent.is_some() {
                kinds.push("agent");
            }
            if s.role.is_some() {
                kinds.push("role");
            }
            if s.run.is_some() {
                kinds.push("run");
            }
            match kinds.len() {
                0 => {
                    return Err(ConfigError::Validation(format!(
                        "step '{id}' must specify one of 'agent', 'role', or 'run'"
                    )));
                }
                1 => {}
                _ => {
                    return Err(ConfigError::Validation(format!(
                        "step '{id}' has conflicting fields {kinds:?} -- use exactly one of 'agent', 'role', or 'run'"
                    )));
                }
            }

            // Build the merged `needs` list once -- both LLM and shell steps
            // treat `input:` as an ordering edge in addition to its prompt
            // semantics (shell steps ignore the prompt side).
            let mut needs: Vec<String> = s.needs.clone();
            for input_dep in &s.input {
                if !needs.contains(input_dep) {
                    needs.push(input_dep.clone());
                }
            }

            // #236: validate the backend keys on the step's extra_args before
            // we commit to a Step. Done here -- not after the kind check --
            // so the error mentions the step ID even when the field is set
            // on a step that ends up being rejected for some other reason.
            let step_extra_args =
                validate_extra_args(s.extra_args.clone(), &format!("step '{id}'"))?;

            if let Some(run_command) = s.run {
                // Shell step: reject fields that have no meaning for shell
                // execution. We don't silently drop them -- the user almost
                // certainly intended an LLM step and got the type wrong.
                if s.task.is_some() {
                    return Err(ConfigError::Validation(format!(
                        "step '{id}' uses 'run' and cannot also set 'task' -- the shell command is the task"
                    )));
                }
                if s.model.is_some() {
                    return Err(ConfigError::Validation(format!(
                        "step '{id}' uses 'run' and cannot set 'model' -- shell steps don't call an LLM"
                    )));
                }
                if s.backend.is_some() {
                    return Err(ConfigError::Validation(format!(
                        "step '{id}' uses 'run' and cannot set 'backend' -- shell steps don't call an LLM"
                    )));
                }
                if !s.extra_args.is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "step '{id}' uses 'run' and cannot set 'extra_args' -- shell steps don't call an LLM"
                    )));
                }
                return Ok(Step {
                    id,
                    agent: String::new(),
                    role: None,
                    task: None,
                    run: Some(run_command),
                    input: s.input,
                    needs,
                    model: None,
                    backend: None,
                    print_output: s.print_output,
                    post_comment: s.post_comment,
                    agents: Vec::new(),
                    max_turns: None,
                    turn_timeout: None,
                    extra_args: HashMap::new(),
                });
            }

            if let Some(agent_id) = s.agent {
                // Direct agent assignment -- bypass roles
                return Ok(Step {
                    id,
                    agent: agent_id,
                    role: None,
                    task: s.task,
                    run: None,
                    input: s.input,
                    needs,
                    model: s.model,
                    backend: s.backend,
                    print_output: s.print_output,
                    post_comment: s.post_comment,
                    agents: Vec::new(),
                    max_turns: None,
                    turn_timeout: None,
                    extra_args: step_extra_args,
                });
            }

            // Role path -- only remaining case after the kind check above.
            let role_name = s.role.expect("role must be Some after kind check");
            // Resolve role to agent ID. Flow-level roles win over project-level
            // (project config) roles; project-level acts as the inherited
            // default so a flow can omit roles it does not need to override.
            if !resolved_roles.contains_key(&role_name)
                && let Some(project_agent) = project_roles.get(&role_name)
            {
                resolved_roles.insert(role_name.clone(), project_agent.clone());
            }
            let agent_id = resolved_roles.get(&role_name).ok_or_else(|| {
                let mut available: Vec<&str> = resolved_roles
                    .keys()
                    .map(|s| s.as_str())
                    .chain(project_roles.keys().map(|s| s.as_str()))
                    .collect();
                available.sort();
                available.dedup();
                ConfigError::Validation(format!(
                    "step '{id}' references undefined role '{role_name}' (available: {})",
                    available.join(", ")
                ))
            })?;

            Ok(Step {
                id,
                agent: agent_id.clone(),
                role: Some(role_name),
                task: s.task,
                run: None,
                input: s.input,
                needs,
                model: s.model,
                backend: s.backend,
                print_output: s.print_output,
                post_comment: s.post_comment,
                agents: Vec::new(),
                max_turns: None,
                turn_timeout: None,
                extra_args: step_extra_args,
            })
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

/// Resolve a `type: conversation` step (issue #170).
///
/// A conversation step uses the messaging Router to drive a multi-agent
/// dialogue. Validation rules:
///
/// * `agents:` must contain at least 2 entries (a "conversation" with one
///   participant is just an agent step).
/// * Duplicate agent IDs are rejected -- the Router uses the agent ID as a
///   routing key, so duplicates would conflict.
/// * `max_turns:` must be present and `> 0`. The default is intentionally
///   not provided here: the user must make the cap explicit because
///   conversations can otherwise burn budget unchecked.
/// * `agent:`, `role:`, and `run:` must not be set -- the participant list
///   lives in `agents:`.
/// * `model:` and `backend:` on the step itself are also rejected: each
///   participant is configured by its own agent file. Allowing a
///   step-level override would be ambiguous (does it apply to all
///   participants, only the first, etc).
///
/// `task:` is allowed and becomes the Router's initial broadcast prompt.
/// `input:` continues to work as ordering edges; the runner injects upstream
/// outputs into the broadcast prompt the same way it does for agent steps.
fn resolve_conversation_step(id: String, s: RawStep) -> Result<Step, ConfigError> {
    if s.agent.is_some() {
        return Err(ConfigError::Validation(format!(
            "conversation step '{id}' must not set 'agent:' -- use 'agents:' to list participants"
        )));
    }
    if s.role.is_some() {
        return Err(ConfigError::Validation(format!(
            "conversation step '{id}' must not set 'role:' -- use 'agents:' to list participants"
        )));
    }
    if s.run.is_some() {
        return Err(ConfigError::Validation(format!(
            "conversation step '{id}' must not set 'run:' -- conversation steps drive an LLM dialogue, not a shell command"
        )));
    }
    if s.model.is_some() {
        return Err(ConfigError::Validation(format!(
            "conversation step '{id}' must not set 'model:' -- model is configured per agent in agents/<Name>.yaml"
        )));
    }
    if s.backend.is_some() {
        return Err(ConfigError::Validation(format!(
            "conversation step '{id}' must not set 'backend:' -- backend is configured per agent in agents/<Name>.yaml"
        )));
    }
    // Step-level extra_args on a conversation would be ambiguous: the step
    // resolves to N agents that may use different backends. Per-agent
    // extra_args (in the agent file) is the correct knob; reject the
    // step-level form to avoid silently dropping user intent (#236).
    if !s.extra_args.is_empty() {
        return Err(ConfigError::Validation(format!(
            "conversation step '{id}' must not set 'extra_args:' -- extra_args is configured per agent in agents/<Name>.yaml because conversation participants may use different backends"
        )));
    }
    if s.agents.len() < 2 {
        return Err(ConfigError::Validation(format!(
            "conversation step '{id}' requires at least 2 entries in 'agents:' (got {})",
            s.agents.len()
        )));
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for a in &s.agents {
        if a.trim().is_empty() {
            return Err(ConfigError::Validation(format!(
                "conversation step '{id}' has an empty agent ID in 'agents:'"
            )));
        }
        if !seen.insert(a.as_str()) {
            return Err(ConfigError::Validation(format!(
                "conversation step '{id}' lists agent '{a}' more than once -- each participant must be unique"
            )));
        }
    }
    let max_turns = match s.max_turns {
        Some(n) if n > 0 => n,
        Some(_) => {
            return Err(ConfigError::Validation(format!(
                "conversation step '{id}' has 'max_turns: 0' -- must be > 0"
            )));
        }
        None => {
            return Err(ConfigError::Validation(format!(
                "conversation step '{id}' must specify 'max_turns:' (the per-conversation turn cap)"
            )));
        }
    };

    // Merge `input:` into `needs` -- same edge semantics as agent/shell
    // steps.
    let mut needs: Vec<String> = s.needs.clone();
    for input_dep in &s.input {
        if !needs.contains(input_dep) {
            needs.push(input_dep.clone());
        }
    }

    Ok(Step {
        id,
        agent: String::new(),
        role: None,
        task: s.task,
        run: None,
        input: s.input,
        needs,
        model: None,
        backend: None,
        print_output: s.print_output,
        post_comment: s.post_comment,
        agents: s.agents,
        max_turns: Some(max_turns),
        turn_timeout: s.turn_timeout,
        extra_args: HashMap::new(),
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
    fn post_comment_field_parses_pr() {
        let yaml = r#"
version: "1"
name: test
flow:
  consensus:
    agent: facilitator
    post_comment: pr
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(
            config.steps[0].post_comment,
            Some(PostCommentTarget::Pr),
            "post_comment: pr should parse to PostCommentTarget::Pr"
        );
    }

    #[test]
    fn post_comment_field_parses_issue() {
        let yaml = r#"
version: "1"
name: test
flow:
  notify:
    agent: facilitator
    post_comment: issue
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(
            config.steps[0].post_comment,
            Some(PostCommentTarget::Issue),
            "post_comment: issue should parse to PostCommentTarget::Issue"
        );
    }

    #[test]
    fn post_comment_field_defaults_to_none_for_backwards_compat() {
        // No post_comment field -> Step::post_comment is None. This is the
        // backwards-compatibility guarantee in the issue acceptance criteria:
        // existing flows without post_comment must keep working unchanged.
        let yaml = r#"
version: "1"
name: test
flow:
  step1:
    agent: dev
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.steps[0].post_comment, None);
    }

    #[test]
    fn post_comment_invalid_value_errors() {
        let yaml = r#"
version: "1"
name: test
flow:
  step1:
    agent: dev
    post_comment: discord
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("post_comment") || msg.contains("variant"),
            "expected error to mention post_comment or variant, got: {msg}"
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
        // No description in this file -- field stays None (issue #267).
        assert_eq!(agent.description, None);
    }

    #[test]
    fn load_agent_file_accepts_description() {
        // Issue #267: agent files may carry an optional `description:` field
        // for documentation. It must round-trip through serde and not trigger
        // the unknown-field warning. The runner does not act on it.
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("Levi.yaml"),
            r#"
name: Levi
description: "Senior systems architect. Surveys boundaries and module seams."
role: "You design module boundaries before code is written."
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "default-model".to_string(),
            backend: Backend::ClaudeCli,
        };
        let agent = load_agent_file(dir.path(), "Levi", &defaults, None).unwrap();
        assert_eq!(
            agent.description.as_deref(),
            Some("Senior systems architect. Surveys boundaries and module seams.")
        );
        // Sanity: the rest of the file still loads as before.
        assert_eq!(agent.name, "Levi");
        assert_eq!(
            agent.role,
            "You design module boundaries before code is written."
        );
    }

    #[test]
    fn raw_agent_file_does_not_warn_on_description() {
        // Guard the "no warning" behavior at the parse layer: if the
        // serde-flatten unknown map ever ends up containing `description`
        // again (e.g. someone removes the field), this test catches it
        // without depending on stderr capture.
        let yaml = r#"
name: Levi
description: "A short summary."
role: "Some role."
"#;
        let raw: RawAgentFile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(raw.description.as_deref(), Some("A short summary."));
        assert!(
            !raw.unknown.contains_key("description"),
            "description must be a known field, not collected by serde-flatten"
        );
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
        assert!(
            msg.contains(&format!("no {KOTO_CONFIG_FILE} found")),
            "got: {msg}"
        );
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
            msg.contains(&format!(
                r#"tier "deep-thought" not defined in {KOTO_CONFIG_FILE}"#
            )),
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
        // Backward compat: agents without tier ignore the project config entirely.
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
        // The seed-aware loader reports the missing agent and the seeds
        // it searched. The single-dir wrapper used here yields one searched
        // path -- the temp dir.
        let err = load_agent_file(dir.path(), "ghost", &defaults, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("agent \"ghost\" not found"), "got: {msg}");
        assert!(msg.contains("seeds:"), "got: {msg}");
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
    fn project_role_inherited_when_flow_does_not_redeclare() {
        // Step references a role only defined at the project level. The flow
        // must inherit the binding rather than reject it -- this is the core
        // of the project-roles feature.
        let yaml = r#"
version: "1"
name: test
flow:
  review:
    role: reviewer
"#;
        let mut project = HashMap::new();
        project.insert("reviewer".to_string(), "Bella".to_string());
        let config = load_flow_from_str_with_project(yaml, &HashMap::new(), &project).unwrap();
        assert_eq!(config.steps[0].agent, "Bella");
        assert_eq!(config.roles.get("reviewer").unwrap(), "Bella");
    }

    #[test]
    fn flow_role_wins_over_project_role() {
        // Flow-level declaration overrides the project-level binding.
        let yaml = r#"
version: "1"
name: test
roles:
  reviewer: { default: Sage }
flow:
  review:
    role: reviewer
"#;
        let mut project = HashMap::new();
        project.insert("reviewer".to_string(), "Bella".to_string());
        let config = load_flow_from_str_with_project(yaml, &HashMap::new(), &project).unwrap();
        assert_eq!(config.steps[0].agent, "Sage");
    }

    #[test]
    fn role_unknown_in_flow_and_project_errors() {
        let yaml = r#"
version: "1"
name: test
flow:
  review:
    role: phantom
"#;
        let mut project = HashMap::new();
        project.insert("reviewer".to_string(), "Bella".to_string());
        let err = load_flow_from_str_with_project(yaml, &HashMap::new(), &project).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("undefined role 'phantom'"), "got: {msg}");
        // Available list should mention the project-level role.
        assert!(msg.contains("reviewer"), "got: {msg}");
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
        let msg = err.to_string();
        assert!(
            msg.contains("conflicting fields") && msg.contains("agent") && msg.contains("role"),
            "got: {msg}"
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
                .contains("must specify one of 'agent', 'role', or 'run'"),
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

    // --- multi-source agent lookup (issue #130) ---

    use crate::koto_config::{Seed, SeedSource, Seeds};

    /// Helper: build a [`Seeds`] from a list of local directories.
    fn local_seeds(dirs: &[&Path]) -> Seeds {
        Seeds {
            seeds: dirs
                .iter()
                .map(|d| Seed {
                    source: SeedSource::Local {
                        display: d.display().to_string(),
                        path: d.to_path_buf(),
                    },
                })
                .collect(),
        }
    }

    /// Helper: write a minimal agent YAML at `<seed>/agents/<rel>.yaml`.
    fn write_agent(seed: &Path, rel: &str, name: &str) {
        let path = seed.join("agents").join(format!("{rel}.yaml"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("name: {name}\nrole: \"do work\"\nmodel: claude-sonnet-4-5\n"),
        )
        .unwrap();
    }

    #[test]
    fn agent_lookup_walks_seeds_top_to_bottom() {
        // First seed wins: when both seeds define the same agent, the upstream
        // copy is loaded.
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        write_agent(dir1.path(), "Sage", "Sage-from-1");
        write_agent(dir2.path(), "Sage", "Sage-from-2");

        let seeds = local_seeds(&[dir1.path(), dir2.path()]);
        let defaults = Defaults {
            model: "default-model".to_string(),
            backend: Backend::ClaudeCli,
        };
        let (agent, idx, _sha) =
            load_agent_file_with_seeds(&seeds, "Sage", &defaults, None).unwrap();

        assert_eq!(idx, 0);
        assert_eq!(agent.name, "Sage-from-1");
    }

    #[test]
    fn agent_lookup_falls_through_to_later_seed() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        write_agent(dir2.path(), "Bella", "Bella-from-2");

        let seeds = local_seeds(&[dir1.path(), dir2.path()]);
        let defaults = Defaults {
            model: "default-model".to_string(),
            backend: Backend::ClaudeCli,
        };
        let (agent, idx, _sha) =
            load_agent_file_with_seeds(&seeds, "Bella", &defaults, None).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(agent.name, "Bella-from-2");
    }

    #[test]
    fn agent_lookup_supports_arbitrary_depth_path() {
        // `coding/rust/Sage` -> `agents/coding/rust/Sage.yaml`. No globbing,
        // no recursive scan -- just path join, as specified in the issue.
        let dir = tempfile::tempdir().unwrap();
        write_agent(dir.path(), "coding/rust/Sage", "RustSage");

        let seeds = local_seeds(&[dir.path()]);
        let defaults = Defaults {
            model: "default-model".to_string(),
            backend: Backend::ClaudeCli,
        };
        let (agent, _, _) =
            load_agent_file_with_seeds(&seeds, "coding/rust/Sage", &defaults, None).unwrap();
        assert_eq!(agent.name, "RustSage");
        // The agent ID retains the full nested form.
        assert_eq!(agent.id, "coding/rust/Sage");
    }

    #[test]
    fn agent_lookup_missing_lists_searched_paths() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let seeds = local_seeds(&[dir1.path(), dir2.path()]);
        let defaults = Defaults {
            model: "m".to_string(),
            backend: Backend::ClaudeCli,
        };
        let err =
            load_agent_file_with_seeds(&seeds, "coding/rust/Sage", &defaults, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("agent \"coding/rust/Sage\" not found"),
            "got: {msg}"
        );
        assert!(
            msg.contains(&dir1.path().display().to_string()),
            "missing dir1 in error: {msg}"
        );
        assert!(
            msg.contains(&dir2.path().display().to_string()),
            "missing dir2 in error: {msg}"
        );
    }

    #[test]
    fn load_agents_for_flow_records_seed_origins() {
        // Two-seed setup: the flow uses two agents; one comes from the upper
        // seed, the other from the lower. The origins map records exactly one
        // index per agent.
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        write_agent(dir1.path(), "Alpha", "Alpha");
        write_agent(dir2.path(), "Beta", "Beta");

        let seeds = local_seeds(&[dir1.path(), dir2.path()]);

        let flow_yaml = r#"
version: "1"
name: t
flow:
  s1:
    agent: Alpha
  s2:
    agent: Beta
"#;
        let flow = load_flow_from_str(flow_yaml).unwrap();
        let (agents, origins, _hashes) =
            load_agents_for_flow_with_seeds(&seeds, &flow, None).unwrap();

        assert_eq!(agents.len(), 2);
        assert_eq!(*origins.get("Alpha").unwrap(), 0);
        assert_eq!(*origins.get("Beta").unwrap(), 1);
    }

    #[test]
    fn load_agents_hashes_bytes_loaded_not_disk_at_call_time() {
        // Audit promise: the manifest must record the hash of the agent bytes
        // the LLM saw, not whatever happens to be on disk later. Simulate a
        // mid-run edit by overwriting the agent file after load and verify
        // the captured hash still matches the original bytes.
        let dir = tempfile::tempdir().unwrap();
        write_agent(dir.path(), "Sage", "Sage-original");

        let original_bytes = std::fs::read(dir.path().join("agents/Sage.yaml")).unwrap();
        let original_hash = crate::stack::sha256_hex(&original_bytes);

        let seeds = local_seeds(&[dir.path()]);
        let flow_yaml = r#"
version: "1"
name: t
flow:
  s1:
    agent: Sage
"#;
        let flow = load_flow_from_str(flow_yaml).unwrap();
        let (_agents, _origins, hashes) =
            load_agents_for_flow_with_seeds(&seeds, &flow, None).unwrap();

        // Now stomp the file as if the user edited it mid-run.
        std::fs::write(
            dir.path().join("agents/Sage.yaml"),
            "name: Sage-rewritten\nrole: r\n",
        )
        .unwrap();

        let captured = hashes.get("Sage").expect("agent hash recorded");
        assert_eq!(
            captured, &original_hash,
            "captured hash must reflect the bytes loaded into the runner, not the file on disk now"
        );
    }

    #[test]
    fn agent_lookup_overlay_wins_for_first_seed() {
        // Overlay semantics: whole-file replacement, never field-level merge.
        // Both agents have the same ID `Sage` but different model values; the
        // resolved Agent must mirror seed 1 entirely.
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(dir1.path().join("agents")).unwrap();
        std::fs::create_dir_all(dir2.path().join("agents")).unwrap();
        std::fs::write(
            dir1.path().join("agents/Sage.yaml"),
            "name: Sage\nrole: r\nmodel: from-seed-1\n",
        )
        .unwrap();
        std::fs::write(
            dir2.path().join("agents/Sage.yaml"),
            "name: Sage\nrole: r\nmodel: from-seed-2\n",
        )
        .unwrap();

        let seeds = local_seeds(&[dir1.path(), dir2.path()]);
        let defaults = Defaults {
            model: "fallback".to_string(),
            backend: Backend::ClaudeCli,
        };
        let (agent, idx, _sha) =
            load_agent_file_with_seeds(&seeds, "Sage", &defaults, None).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(agent.model, "from-seed-1");
    }

    // --- Shell steps (issue #23) ---

    #[test]
    fn run_step_parses_basic() {
        let yaml = r#"
version: "1"
name: test
flow:
  fetch:
    run: echo hello
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.steps.len(), 1);
        let step = &config.steps[0];
        assert_eq!(step.id, "fetch");
        assert!(step.is_shell());
        assert_eq!(step.run.as_deref(), Some("echo hello"));
        assert_eq!(step.agent, "");
        assert!(step.role.is_none());
    }

    #[test]
    fn run_step_with_input_creates_dependency() {
        // Acceptance: shell steps participate in DAG validation. The `input:`
        // field on a shell step should still produce a `needs:` edge.
        let yaml = r#"
version: "1"
name: test
flow:
  fetch:
    run: echo hi
  follow:
    run: echo bye
    input: [fetch]
"#;
        let config = load_flow_from_str(yaml).unwrap();
        let follow = &config.steps[1];
        assert_eq!(follow.input, vec!["fetch"]);
        assert!(follow.needs.contains(&"fetch".to_string()));
    }

    #[test]
    fn run_step_mixed_with_agent_step_in_same_flow() {
        // Acceptance: shell steps coexist with LLM steps; the example in the
        // issue mixes them. Output of the shell step is consumed via input:.
        let yaml = r#"
version: "1"
name: test
flow:
  fetch:
    run: gh pr diff 67
  review:
    agent: Levi
    input: [fetch]
    task: "Evaluate the diff"
"#;
        let config = load_flow_from_str(yaml).unwrap();
        assert_eq!(config.steps.len(), 2);
        assert!(config.steps[0].is_shell());
        assert!(!config.steps[1].is_shell());
        assert_eq!(config.steps[1].agent, "Levi");
    }

    #[test]
    fn run_and_agent_together_errors() {
        let yaml = r#"
version: "1"
name: test
flow:
  bad:
    run: echo hi
    agent: Levi
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflicting fields") && msg.contains("agent") && msg.contains("run"),
            "got: {msg}"
        );
    }

    #[test]
    fn run_and_role_together_errors() {
        let yaml = r#"
version: "1"
name: test
roles:
  coder: { default: Kai }
flow:
  bad:
    run: echo hi
    role: coder
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflicting fields") && msg.contains("role") && msg.contains("run"),
            "got: {msg}"
        );
    }

    #[test]
    fn run_with_task_errors() {
        // task: only makes sense for LLM steps -- the run command itself is
        // the task description for shell steps.
        let yaml = r#"
version: "1"
name: test
flow:
  bad:
    run: echo hi
    task: "do something"
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("cannot also set 'task'"),
            "got: {err}"
        );
    }

    #[test]
    fn run_with_model_errors() {
        let yaml = r#"
version: "1"
name: test
flow:
  bad:
    run: echo hi
    model: claude-opus-4-5
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(err.to_string().contains("cannot set 'model'"), "got: {err}");
    }

    #[test]
    fn run_with_backend_errors() {
        let yaml = r#"
version: "1"
name: test
flow:
  bad:
    run: echo hi
    backend: api
"#;
        let err = load_flow_from_str(yaml).unwrap_err();
        assert!(
            err.to_string().contains("cannot set 'backend'"),
            "got: {err}"
        );
    }

    #[test]
    fn run_step_skipped_in_agent_loading() {
        // Shell steps must not be looked up in agents/. Without this guard,
        // an empty-string agent ID would cause a path-traversal-like lookup.
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("Levi.yaml"),
            "name: Levi\nrole: architect\n",
        )
        .unwrap();

        let yaml = r#"
version: "1"
name: test
flow:
  fetch:
    run: echo hi
  review:
    agent: Levi
    input: [fetch]
"#;
        let flow = load_flow_from_str(yaml).unwrap();
        let seeds = Seeds {
            seeds: vec![crate::koto_config::Seed {
                source: crate::koto_config::SeedSource::Local {
                    display: dir.path().display().to_string(),
                    path: dir.path().to_path_buf(),
                },
            }],
        };
        let (agents, _, _) = load_agents_for_flow_with_seeds(&seeds, &flow, None).unwrap();
        assert_eq!(agents.len(), 1, "shell step must not produce an agent load");
        assert_eq!(agents[0].id, "Levi");
    }

    // --- Conversation step parsing/validation tests (issue #170) ---

    const CONVERSATION_OK: &str = r#"
version: "1"
name: chat
flow:
  debate:
    type: conversation
    agents: [Levi, Mika]
    max_turns: 6
    turn_timeout: 120
    task: "Discuss the architecture"
"#;

    #[test]
    fn conversation_step_parses_minimal() {
        let flow = load_flow_from_str(CONVERSATION_OK).unwrap();
        assert_eq!(flow.steps.len(), 1);
        let step = &flow.steps[0];
        assert!(step.is_conversation());
        assert_eq!(step.id, "debate");
        assert_eq!(step.agents, vec!["Levi", "Mika"]);
        assert_eq!(step.max_turns, Some(6));
        assert_eq!(step.turn_timeout, Some(120));
        assert_eq!(step.task.as_deref(), Some("Discuss the architecture"));
        // Conversation steps must not carry agent/role/run.
        assert!(step.agent.is_empty());
        assert!(step.role.is_none());
        assert!(step.run.is_none());
    }

    #[test]
    fn conversation_step_requires_two_agents() {
        let yaml = r#"
version: "1"
name: chat
flow:
  debate:
    type: conversation
    agents: [Levi]
    max_turns: 4
"#;
        let err = load_flow_from_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("at least 2 entries") && err.contains("debate"),
            "expected min-2-agents error, got: {err}"
        );
    }

    #[test]
    fn conversation_step_requires_max_turns() {
        let yaml = r#"
version: "1"
name: chat
flow:
  debate:
    type: conversation
    agents: [Levi, Mika]
"#;
        let err = load_flow_from_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("max_turns") && err.contains("debate"),
            "expected missing-max_turns error, got: {err}"
        );
    }

    #[test]
    fn conversation_step_rejects_zero_max_turns() {
        let yaml = r#"
version: "1"
name: chat
flow:
  debate:
    type: conversation
    agents: [Levi, Mika]
    max_turns: 0
"#;
        let err = load_flow_from_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("max_turns: 0") || err.contains("must be > 0"),
            "expected max_turns: 0 rejection, got: {err}"
        );
    }

    #[test]
    fn conversation_step_rejects_duplicate_agents() {
        let yaml = r#"
version: "1"
name: chat
flow:
  debate:
    type: conversation
    agents: [Levi, Levi]
    max_turns: 4
"#;
        let err = load_flow_from_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("more than once") && err.contains("Levi"),
            "expected duplicate-agent error, got: {err}"
        );
    }

    #[test]
    fn conversation_step_rejects_agent_field() {
        let yaml = r#"
version: "1"
name: chat
flow:
  debate:
    type: conversation
    agent: Levi
    agents: [Levi, Mika]
    max_turns: 4
"#;
        let err = load_flow_from_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("must not set 'agent:'"),
            "expected agent-field rejection, got: {err}"
        );
    }

    #[test]
    fn conversation_step_rejects_run_field() {
        let yaml = r#"
version: "1"
name: chat
flow:
  debate:
    type: conversation
    run: "echo hi"
    agents: [Levi, Mika]
    max_turns: 4
"#;
        let err = load_flow_from_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("must not set 'run:'"),
            "expected run-field rejection, got: {err}"
        );
    }

    #[test]
    fn conversation_step_rejects_step_level_model() {
        let yaml = r#"
version: "1"
name: chat
flow:
  debate:
    type: conversation
    model: claude-opus-4-5
    agents: [Levi, Mika]
    max_turns: 4
"#;
        let err = load_flow_from_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("must not set 'model:'"),
            "expected model rejection, got: {err}"
        );
    }

    #[test]
    fn conversation_step_unknown_type_errors() {
        let yaml = r#"
version: "1"
name: chat
flow:
  debate:
    type: chitchat
    agents: [Levi, Mika]
    max_turns: 4
"#;
        let err = load_flow_from_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("unknown type") && err.contains("chitchat"),
            "expected unknown-type error, got: {err}"
        );
    }

    #[test]
    fn agents_field_requires_conversation_type() {
        // `agents:` outside `type: conversation` must fail with a hint.
        let yaml = r#"
version: "1"
name: chat
flow:
  oops:
    agent: Levi
    agents: [Levi, Mika]
    max_turns: 4
"#;
        let err = load_flow_from_str(yaml).unwrap_err().to_string();
        assert!(
            err.contains("type: conversation"),
            "expected hint pointing to type: conversation, got: {err}"
        );
    }

    #[test]
    fn conversation_step_input_merges_into_needs() {
        let yaml = r#"
version: "1"
name: chat
flow:
  brief:
    agent: Levi
  debate:
    type: conversation
    agents: [Levi, Mika]
    max_turns: 4
    input: [brief]
"#;
        let flow = load_flow_from_str(yaml).unwrap();
        let debate = flow.steps.iter().find(|s| s.id == "debate").unwrap();
        assert_eq!(debate.input, vec!["brief"]);
        assert!(
            debate.needs.contains(&"brief".to_string()),
            "input dependency must merge into needs: {:?}",
            debate.needs
        );
    }

    // --- #236: extra_args (backend-keyed escape hatch) ---

    #[test]
    fn agent_yaml_parses_extra_args_map() {
        // Acceptance: an agent's `extra_args:` block keyed by backend name
        // round-trips into a Backend-keyed HashMap with the verbatim token list.
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("Babis.yaml"),
            r#"
name: Babis
role: "Senior dev"
backend: codex
model: gpt-5.5
extra_args:
  codex: ["-c", "model_reasoning_effort=high"]
  claude-cli: ["--allowed-tools", "Read"]
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "default".to_string(),
            backend: Backend::ClaudeCli,
        };
        let agent = load_agent_file(dir.path(), "Babis", &defaults, None).unwrap();

        assert_eq!(agent.backend, Backend::Codex);
        assert_eq!(
            agent.extra_args.get(&Backend::Codex),
            Some(&vec![
                "-c".to_string(),
                "model_reasoning_effort=high".to_string()
            ])
        );
        assert_eq!(
            agent.extra_args.get(&Backend::ClaudeCli),
            Some(&vec!["--allowed-tools".to_string(), "Read".to_string()])
        );
        assert!(agent.extra_args.get(&Backend::Ollama).is_none());
    }

    #[test]
    fn step_extra_args_overrides_agent_extra_args() {
        // Acceptance: when a step declares its own extra_args, it REPLACES
        // (no merge) the agent-level map for the matching backend. The runner
        // chooses the step map when it is non-empty -- this test asserts the
        // raw structure, the runner-level resolution is covered by
        // resolve_extra_args usage in run_step_via_executor.
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("Babis.yaml"),
            r#"
name: Babis
role: "Senior dev"
backend: codex
model: gpt-5.5
extra_args:
  codex: ["-c", "from_agent=true"]
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join(".kuro").join("flows").join("override.yaml"),
            "",
        )
        .ok();

        let yaml = r#"
version: "1"
name: t
defaults:
  model: gpt-5.5
  backend: codex
flow:
  build:
    agent: Babis
    extra_args:
      codex: ["-c", "from_step=true"]
"#;
        let flow = load_flow_from_str(yaml).unwrap();
        let step = &flow.steps[0];
        // Step extra_args carries the override.
        assert_eq!(
            step.extra_args.get(&Backend::Codex),
            Some(&vec!["-c".to_string(), "from_step=true".to_string()])
        );

        // Agent extra_args is unaffected -- the override lives at step level only.
        let defaults = Defaults {
            model: "gpt-5.5".to_string(),
            backend: Backend::Codex,
        };
        let agent = load_agent_file(dir.path(), "Babis", &defaults, None).unwrap();
        assert_eq!(
            agent.extra_args.get(&Backend::Codex),
            Some(&vec!["-c".to_string(), "from_agent=true".to_string()])
        );

        // Sanity: the two maps are not equal -- replacement, not merge.
        assert_ne!(step.extra_args, agent.extra_args);
    }

    #[test]
    fn unknown_backend_key_in_extra_args_is_rejected() {
        // Acceptance: an unknown backend key must surface a validation error
        // that names the offending key and lists supported backends.
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("Bad.yaml"),
            r#"
name: Bad
role: "broken"
extra_args:
  not-a-backend: ["--flag"]
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "default".to_string(),
            backend: Backend::ClaudeCli,
        };
        let err = load_agent_file(dir.path(), "Bad", &defaults, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not-a-backend"),
            "error must name the offending key: {msg}"
        );
        assert!(
            msg.contains("claude-cli") && msg.contains("codex") && msg.contains("ollama"),
            "error must list CLI-backend names: {msg}"
        );
        // 'api' is intentionally absent from the unknown-backend error: it has
        // its own dedicated rejection path because extra_args has no meaning
        // for an HTTP backend, so advertising it here would mislead users.
        assert!(
            !msg.contains("api"),
            "'api' must not appear in the unknown-backend hint -- it has its own rejection: {msg}"
        );

        // Same check at the step level.
        let step_yaml = r#"
version: "1"
name: t
flow:
  go:
    agent: dev
    extra_args:
      bogus: ["--x"]
"#;
        let err = load_flow_from_str(step_yaml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "error must name key: {msg}");
        assert!(
            msg.contains("claude-cli"),
            "error must list valid backends: {msg}"
        );
    }

    #[test]
    fn api_backend_in_extra_args_is_rejected_with_dedicated_error() {
        // Acceptance (PR #243 review): the api backend has no CLI argv to
        // extend. Accepting `api: [...]` and silently dropping it at
        // command-build time would mislead users -- and the runner's
        // "extra_args (...): [...]" log line would advertise tokens that were
        // never applied. Reject up front in validation so the misuse is
        // caught at parse time rather than at runtime.
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("Net.yaml"),
            r#"
name: Net
role: "calls api"
backend: api
extra_args:
  api: ["--temperature", "0.2"]
"#,
        )
        .unwrap();

        let defaults = Defaults {
            model: "default".to_string(),
            backend: Backend::Api,
        };
        let err = load_agent_file(dir.path(), "Net", &defaults, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not supported") && msg.contains("'api'"),
            "agent-level error must explain api is unsupported: {msg}"
        );
        assert!(
            msg.contains("Net"),
            "agent-level error must name the file context: {msg}"
        );

        // Same rejection at the step level so a step-only override is caught
        // even when the agent file is clean.
        let step_yaml = r#"
version: "1"
name: t
flow:
  go:
    agent: dev
    extra_args:
      api: ["--whatever"]
"#;
        let err = load_flow_from_str(step_yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not supported") && msg.contains("'api'"),
            "step-level error must explain api is unsupported: {msg}"
        );
        assert!(
            msg.contains("step 'go'"),
            "step-level error must name the step id: {msg}"
        );
    }

    // --- Graph flow schema tests (issue #237) ---

    /// Reusable minimal-but-complete graph YAML covering every shape the
    /// schema accepts: a non-terminal state with edges, a re-entrant edge,
    /// a `kind: human` state with edges (the resume-* pattern from the
    /// design doc), and a `kind: final` state with no edges.
    const GRAPH_CONFIG: &str = r#"
version: "1"
name: example-graph
prompt: |
  Top-level instruction shared by all states.

initial: start

graph:
  start:
    role: developer
    task: |
      Do the first thing.
    next:
      - middle: "Things went well."
      - aborted: "Cannot proceed."

  middle:
    role: reviewer
    task: |
      Check the result.
    next:
      - done: "Looks good."
      - start: "Needs another round."

  human_review:
    human: true
    next:
      - middle: "Operator unblocks the review."
      - aborted: "Operator aborts the run."

  done:
    final: "Happy-path exit -- review approved."

  aborted:
    final: "Early exit -- aborted from start or human_review."
"#;

    #[test]
    fn graph_minimal_parses() {
        let flow = load_flow_any_from_str(GRAPH_CONFIG).unwrap();
        let g = match flow {
            Flow::Graph(g) => g,
            Flow::Linear(_) => panic!("expected graph variant, got linear"),
        };
        assert_eq!(g.name, "example-graph");
        assert_eq!(g.version.0, "1");
        assert_eq!(g.initial, "start");
        let ids: Vec<&str> = g.graph.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            vec!["start", "middle", "human_review", "done", "aborted"]
        );

        // Spot-check a regular state: select entries parse.
        let start = &g.graph["start"];
        assert_eq!(start.role.as_deref(), Some("developer"));
        let select = start.select.as_ref().unwrap();
        assert_eq!(select[0].target, "middle");

        // Final state has final_desc, no select.
        let done = &g.graph["done"];
        assert!(done.is_final());
        assert!(done.select.is_none());

        // Human state carries human: true and has select entries.
        let hr = &g.graph["human_review"];
        assert!(hr.is_human());
        assert!(hr.select.is_some());
    }

    #[test]
    fn graph_linear_via_polymorphic_loader() {
        // Acceptance: the polymorphic loader still returns the linear
        // shape unchanged when the YAML uses `flow:`. This anchors the
        // "existing linear-flow tests still pass unchanged" guarantee at
        // the new entry point as well as the legacy one.
        let flow = load_flow_any_from_str(MINIMAL_CONFIG).unwrap();
        match flow {
            Flow::Linear(c) => {
                assert_eq!(c.name, "minimal");
                assert_eq!(c.steps.len(), 1);
            }
            Flow::Graph(_) => panic!("expected linear variant for `flow:` YAML"),
        }
    }

    #[test]
    fn graph_missing_initial_errors() {
        let yaml = r#"
version: "1"
name: no-initial
graph:
  done:
    final: "end"
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("initial"),
            "error must name the missing 'initial' field: {msg}"
        );
    }

    #[test]
    fn graph_unknown_initial_target_errors() {
        let yaml = r#"
version: "1"
name: bad-initial
initial: nowhere
graph:
  done:
    final: "end"
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("initial") && msg.contains("'nowhere'"),
            "error must name field and offending state-id: {msg}"
        );
        assert!(
            msg.contains("done"),
            "error must list the known states for orientation: {msg}"
        );
    }

    #[test]
    fn graph_select_to_unknown_state_errors() {
        let yaml = r#"
version: "1"
name: bad-select
initial: start
graph:
  start:
    role: developer
    next:
      - ghost: "Goes nowhere real."
      - done: "ok"
  done:
    final: "end"
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("start") && msg.contains("'ghost'"),
            "error must name source state and target: {msg}"
        );
    }

    #[test]
    fn graph_mixed_flow_and_graph_is_hard_error() {
        let yaml = r#"
version: "1"
name: mixed
initial: start
flow:
  whatever:
    agent: dev
graph:
  start:
    final: "end"
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("both") && msg.contains("'flow:'") && msg.contains("'graph:'"),
            "error must explicitly call out the conflict: {msg}"
        );
        assert!(
            msg.contains("pick one"),
            "error must direct the user to pick one shape: {msg}"
        );
    }

    #[test]
    fn graph_neither_flow_nor_graph_errors() {
        let yaml = r#"
version: "1"
name: empty-shape
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("'flow:'") && msg.contains("'graph:'"),
            "error must list both possible top-level shapes: {msg}"
        );
    }

    #[test]
    fn graph_state_without_select_or_final_is_dead_end() {
        // Validator-only test: build the graph directly so a parser change
        // (deny_unknown_fields tightening, schema-level reject of edgeless
        // agent states) cannot break this assertion. The validator must
        // flag a state with no `next:` and no `final:` regardless of how
        // it was authored.
        let flow = graph(
            "lonely",
            vec![(
                "lonely",
                GraphState {
                    role: Some("developer".to_string()),
                    ..Default::default()
                },
            )],
        );
        let report = validate_graph_reachability(&flow);
        assert!(!report.is_ok(), "expected dead-end error, got ok");
        let msg = &report.errors[0];
        assert!(
            msg.contains("'lonely'"),
            "error must name the offending state: {msg}"
        );
        assert!(
            msg.contains("dead end"),
            "error must classify the issue as a dead end: {msg}"
        );
    }

    #[test]
    fn graph_select_entry_parses_bare_string() {
        let yaml = r#"
version: "1"
name: bare
initial: start
graph:
  start:
    run: "true"
    next:
      - done: pass
      - back: fail
  done:
    final: "end"
  back:
    run: "false"
    next:
      - done: pass
      - start: fail
"#;
        let flow = load_flow_any_from_str(yaml).unwrap();
        let g = match flow {
            Flow::Graph(g) => g,
            Flow::Linear(_) => panic!("expected graph"),
        };
        let select = g.graph["start"].select.as_ref().unwrap();
        assert_eq!(select[0].target, "done");
        assert_eq!(
            select[0].reason,
            Some(SelectReason::Single("pass".to_string()))
        );
    }

    #[test]
    fn graph_select_entry_parses_list_reason() {
        let yaml = r#"
version: "1"
name: list-reason
initial: start
graph:
  start:
    role: developer
    next:
      - done: ["reason one", "reason two"]
      - back: "fallback"
  done:
    final: "end"
  back:
    role: developer
    next:
      - done: "ok"
      - start: "retry"
"#;
        let flow = load_flow_any_from_str(yaml).unwrap();
        let g = match flow {
            Flow::Graph(g) => g,
            Flow::Linear(_) => panic!("expected graph"),
        };
        let select = g.graph["start"].select.as_ref().unwrap();
        assert_eq!(
            select[0].reason,
            Some(SelectReason::List(vec![
                "reason one".to_string(),
                "reason two".to_string()
            ]))
        );
    }

    #[test]
    fn graph_unknown_field_is_rejected() {
        let yaml = r#"
version: "1"
name: typo
initial: start
graph:
  start:
    role: developer
    typo_field: oops
    next:
      - done: "ok"
      - start: "retry"
  done:
    final: "end"
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("typo_field"),
            "deny_unknown_fields must surface the unknown key: {msg}"
        );
    }

    #[test]
    fn graph_conflicting_discriminators_errors() {
        // A state cannot be both final and a shell state. Drive the
        // validator directly so the assertion is about validator
        // semantics, not loader plumbing -- a parser-side change must
        // not be able to mask or displace this error.
        let mut graph_states: IndexMap<String, GraphState> = IndexMap::new();
        graph_states.insert(
            "start".to_string(),
            GraphState {
                run: Some("true".to_string()),
                final_desc: Some("end".to_string()),
                ..Default::default()
            },
        );
        let g = GraphFlow {
            version: Version("1".to_string()),
            name: "bad".to_string(),
            initial: "start".to_string(),
            graph: graph_states,
            ..Default::default()
        };
        let err = validate_graph_flow(&g).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflicting"),
            "error must indicate the conflict: {msg}"
        );
    }

    #[test]
    fn graph_unsupported_version_errors() {
        let yaml = r#"
version: "2"
name: future
initial: done
graph:
  done:
    final: "end"
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unsupported version '2'"),
            "error must name the unsupported version: {msg}"
        );
    }

    #[test]
    fn graph_round_trip_serde() {
        // Acceptance: round-trip serde -- deserialize the canonical
        // GRAPH_CONFIG to a GraphFlow, serialize it back to YAML,
        // re-parse, and assert the structures are equal. Byte-equal
        // YAML is intentionally NOT asserted (quoting/indent normalisation
        // would make the test brittle without adding signal).
        let parsed = load_graph_flow_from_str(GRAPH_CONFIG).unwrap();
        let yaml = serde_yaml::to_string(&parsed).expect("serialize round-trip");
        let reparsed = load_graph_flow_from_str(&yaml).expect("re-parse round-trip");
        assert_eq!(parsed, reparsed);
    }

    // --- Graph reachability + dead-end validator tests (issue #238) ---

    /// Build a `GraphFlow` programmatically. Useful for the dead-end
    /// test, which needs a state shape that the YAML schema validator
    /// rejects (no edges and no kind) -- we cannot express it as a YAML
    /// fixture and round-trip, so we construct it in memory.
    fn graph(initial: &str, states: Vec<(&str, GraphState)>) -> GraphFlow {
        let mut map: IndexMap<String, GraphState> = IndexMap::new();
        for (id, state) in states {
            map.insert(id.to_string(), state);
        }
        GraphFlow {
            version: Version("1".to_string()),
            name: "test".to_string(),
            initial: initial.to_string(),
            graph: map,
            ..Default::default()
        }
    }

    fn state_with_select(targets: Vec<(&str, &str)>) -> GraphState {
        let entries: Vec<SelectEntry> = targets
            .iter()
            .map(|(target, reason)| SelectEntry {
                target: (*target).to_string(),
                reason: Some(SelectReason::Single((*reason).to_string())),
            })
            .collect();
        GraphState {
            role: Some("developer".to_string()),
            select: Some(entries),
            ..Default::default()
        }
    }

    fn state_final() -> GraphState {
        GraphState {
            final_desc: Some("Test terminal state.".to_string()),
            ..Default::default()
        }
    }

    fn state_final_no_description() -> GraphState {
        GraphState {
            final_desc: Some("".to_string()),
            ..Default::default()
        }
    }

    /// Agent state with role, task, and outgoing edges. Used by the
    /// canonical-fixture migration where states need a task body, not
    /// just a role. Reasons attached as `SelectReason::Single`.
    fn state_agent(role: &str, task: &str, targets: Vec<(&str, &str)>) -> GraphState {
        let entries: Vec<SelectEntry> = targets
            .iter()
            .map(|(target, reason)| SelectEntry {
                target: (*target).to_string(),
                reason: Some(SelectReason::Single((*reason).to_string())),
            })
            .collect();
        GraphState {
            role: Some(role.to_string()),
            task: Some(task.to_string()),
            select: Some(entries),
            ..Default::default()
        }
    }

    /// Final state with an explicit description. The plain `state_final()`
    /// helper hard-codes one description; the canonical fixture needs two
    /// distinct ones.
    fn state_final_with(desc: &str) -> GraphState {
        GraphState {
            final_desc: Some(desc.to_string()),
            ..Default::default()
        }
    }

    fn state_human(targets: Vec<(&str, &str)>) -> GraphState {
        let entries: Vec<SelectEntry> = targets
            .iter()
            .map(|(target, reason)| SelectEntry {
                target: (*target).to_string(),
                reason: Some(SelectReason::Single((*reason).to_string())),
            })
            .collect();
        GraphState {
            human: Some(true),
            select: Some(entries),
            ..Default::default()
        }
    }

    /// Acceptance: a graph where every state is either reachable + has
    /// edges OR `kind: final` validates clean -- no errors, no warnings.
    #[test]
    fn validate_clean_graph() {
        let g = graph(
            "start",
            vec![
                ("start", state_with_select(vec![("done", "looks good")])),
                ("done", state_final()),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(
            report.is_ok(),
            "expected ok, got errors: {:?}",
            report.errors
        );
        assert!(
            report.warnings.is_empty(),
            "expected no warnings, got: {:?}",
            report.warnings
        );
    }

    #[test]
    fn validate_dead_end_state_errors() {
        let stuck = GraphState {
            role: Some("developer".to_string()),
            // no final, no select -- dead end
            ..Default::default()
        };
        let g = graph(
            "start",
            vec![
                ("start", state_with_select(vec![("stuck", "go there")])),
                ("stuck", stuck),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(!report.is_ok(), "expected dead-end error, got ok");
        assert_eq!(report.errors.len(), 1, "errors: {:?}", report.errors);
        assert!(
            report.errors[0].contains("'stuck'"),
            "error must name the dead-end state: {}",
            report.errors[0]
        );
        assert!(
            report.errors[0].contains("dead end"),
            "error must classify the issue as a dead end: {}",
            report.errors[0]
        );
    }

    /// Acceptance: an unreachable state produces a warning but the
    /// report still validates as ok (warnings only).
    #[test]
    fn validate_unreachable_state_warns_only() {
        let g = graph(
            "start",
            vec![
                ("start", state_with_select(vec![("done", "looks good")])),
                ("done", state_final()),
                ("orphan", state_with_select(vec![("done", "loops back")])),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(
            report.is_ok(),
            "unreachable-only must not error, got: {:?}",
            report.errors
        );
        assert_eq!(report.warnings.len(), 1, "warnings: {:?}", report.warnings);
        assert!(
            report.warnings[0].contains("'orphan'"),
            "warning must name the unreachable state: {}",
            report.warnings[0]
        );
        assert!(
            report.warnings[0].contains("'start'"),
            "warning must name the initial state for context: {}",
            report.warnings[0]
        );
    }

    /// Acceptance: a self-loop edge (state pointing back to itself) is
    /// allowed -- not a dead end and not flagged. Pins behavior so
    /// future changes do not accidentally tighten this.
    #[test]
    fn validate_self_loop_edge_allowed() {
        let g = graph(
            "start",
            vec![
                (
                    "start",
                    state_with_select(vec![("start", "retry"), ("done", "finish")]),
                ),
                ("done", state_final()),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(
            report.is_ok(),
            "self-loop must not error, got: {:?}",
            report.errors
        );
        assert!(
            report.warnings.is_empty(),
            "self-loop must not warn, got: {:?}",
            report.warnings
        );
    }

    /// Acceptance: a graph that is just a single `kind: final` state
    /// targeted by `initial:` validates clean. Trivial case but on the
    /// acceptance list -- pin it so a future "must have at least one
    /// edge" rule does not creep in.
    #[test]
    fn validate_single_final_state_only() {
        let g = graph("done", vec![("done", state_final())]);
        let report = validate_graph_reachability(&g);
        assert!(report.is_ok(), "errors: {:?}", report.errors);
        assert!(
            report.warnings.is_empty(),
            "warnings: {:?}",
            report.warnings
        );
    }

    /// Edge case: a `kind: human` state with no outgoing edges is
    /// terminal-ish (operator may abort) and must NOT be flagged as a
    /// dead end. Issue #238 explicitly lists `human` alongside `final`
    /// as terminal for dead-end purposes.
    #[test]
    fn validate_human_state_without_select_not_dead_end() {
        let hr = GraphState {
            human: Some(true),
            ..Default::default()
        };
        let g = graph(
            "start",
            vec![
                ("start", state_with_select(vec![("operator", "handoff")])),
                ("operator", hr),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(report.is_ok(), "errors: {:?}", report.errors);
    }

    /// Edge case: a `kind: human` state WITH edges (the resume-* /
    /// abort pattern from the design doc and seed fixture) validates
    /// clean -- both reachable and not flagged.
    #[test]
    fn validate_human_state_with_resume_edges_clean() {
        let g = graph(
            "start",
            vec![
                ("start", state_with_select(vec![("operator", "handoff")])),
                ("operator", state_human(vec![("done", "resume")])),
                ("done", state_final()),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(report.is_ok(), "errors: {:?}", report.errors);
        assert!(
            report.warnings.is_empty(),
            "warnings: {:?}",
            report.warnings
        );
    }

    /// Multiple dead-ends should all be reported in one pass so the
    /// user can fix them together rather than playing whack-a-mole.
    #[test]
    fn validate_multiple_dead_ends_all_reported() {
        let dead = GraphState {
            role: Some("developer".to_string()),
            ..Default::default()
        };
        let g = graph(
            "start",
            vec![
                (
                    "start",
                    state_with_select(vec![("first", "go a"), ("second", "go b")]),
                ),
                ("first", dead.clone()),
                ("second", dead),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert_eq!(report.errors.len(), 2, "errors: {:?}", report.errors);
        // Declaration order: 'first' before 'second'.
        assert!(report.errors[0].contains("'first'"));
        assert!(report.errors[1].contains("'second'"));
    }

    /// Acceptance: the canonical graph fixture (every shape: agent
    /// non-terminal, re-entrant, human, two finals) validates clean.
    /// Anchors that this shape is valid at the graph-validation layer
    /// independent of any parser behaviour.
    ///
    /// The shape mirrors the YAML `GRAPH_CONFIG` constant field-for-field,
    /// but the two are intentionally decoupled now: parser/serializer
    /// drift is covered by `graph_round_trip_serde`, validator behaviour
    /// is covered here. If you edit `GRAPH_CONFIG`, audit this fixture
    /// too -- they no longer auto-track.
    #[test]
    fn validate_canonical_graph_fixture_clean() {
        let mut graph_states: IndexMap<String, GraphState> = IndexMap::new();
        graph_states.insert(
            "start".to_string(),
            state_agent(
                "developer",
                "Do the first thing.\n",
                vec![
                    ("middle", "Things went well."),
                    ("aborted", "Cannot proceed."),
                ],
            ),
        );
        graph_states.insert(
            "middle".to_string(),
            state_agent(
                "reviewer",
                "Check the result.\n",
                vec![("done", "Looks good."), ("start", "Needs another round.")],
            ),
        );
        graph_states.insert(
            "human_review".to_string(),
            state_human(vec![
                ("middle", "Operator unblocks the review."),
                ("aborted", "Operator aborts the run."),
            ]),
        );
        graph_states.insert(
            "done".to_string(),
            state_final_with("Happy-path exit -- review approved."),
        );
        graph_states.insert(
            "aborted".to_string(),
            state_final_with("Early exit -- aborted from start or human_review."),
        );
        let g = GraphFlow {
            version: Version("1".to_string()),
            name: "example-graph".to_string(),
            prompt: Some("Top-level instruction shared by all states.\n".to_string()),
            initial: "start".to_string(),
            graph: graph_states,
            ..Default::default()
        };
        let report = validate_graph_reachability(&g);
        assert!(
            report.is_ok(),
            "canonical fixture must validate clean, errors: {:?}",
            report.errors
        );
        // human_review is unreachable in the fixture (no edge points to
        // it). That is a deliberate part of the fixture -- it shows the
        // human-state shape, not a wired-up handoff. Pin the warning so
        // a future fixture edit that wires it up updates this test too.
        assert_eq!(
            report.warnings.len(),
            1,
            "expected one unreachable warning for human_review, got: {:?}",
            report.warnings
        );
        assert!(report.warnings[0].contains("'human_review'"));
    }

    // --- GraphState description schema + validation tests (issue #260) ---

    #[test]
    fn graph_final_state_description_parses() {
        let yaml = r#"
version: "1"
name: described
initial: start
graph:
  start:
    role: developer
    next:
      - done: "Looks good."
      - ask_human: "Need a person."
  ask_human:
    human: true
  done:
    final: "Happy-path exit."
"#;
        let g = load_graph_flow_from_str(yaml).expect("must parse");
        assert_eq!(
            g.graph["done"].final_desc.as_deref(),
            Some("Happy-path exit.")
        );
    }

    #[test]
    fn graph_deny_unknown_fields_still_enforced() {
        let yaml = r#"
version: "1"
name: typo
initial: start
graph:
  start:
    role: developer
    descriptionn: oops
    next:
      - done: "ok"
      - start: "retry"
  done:
    final: "done"
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("descriptionn"),
            "deny_unknown_fields must still flag the typo: {msg}"
        );
    }

    #[test]
    fn validate_terminal_final_with_empty_description_warns() {
        let g = graph(
            "start",
            vec![
                ("start", state_with_select(vec![("done", "go")])),
                ("done", state_final_no_description()),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(
            report.is_ok(),
            "empty description is a warning, not an error; got errors: {:?}",
            report.errors
        );
        assert_eq!(report.warnings.len(), 1, "warnings: {:?}", report.warnings);
        let w = &report.warnings[0];
        assert!(w.contains("'done'"), "warning must name the state: {w}");
    }

    #[test]
    fn validate_terminal_with_description_does_not_warn() {
        let g = graph(
            "start",
            vec![
                ("start", state_with_select(vec![("done", "go")])),
                ("done", state_final()),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(report.is_ok(), "errors: {:?}", report.errors);
        assert!(
            report.warnings.is_empty(),
            "terminal with description must not warn: {:?}",
            report.warnings
        );
    }

    #[test]
    fn graph_final_desc_round_trips_via_serde() {
        let yaml = r#"
version: "1"
name: rt
initial: start
graph:
  start:
    role: developer
    next:
      - done: "ok"
      - start: "retry"
  done:
    final: "All good."
"#;
        let parsed = load_graph_flow_from_str(yaml).unwrap();
        let serialized = serde_yaml::to_string(&parsed).expect("serialize");
        let reparsed = load_graph_flow_from_str(&serialized).expect("re-parse");
        assert_eq!(parsed, reparsed);
        assert_eq!(
            reparsed.graph["done"].final_desc.as_deref(),
            Some("All good.")
        );
    }

    // --- External-prompt resolution tests (issue #258) ---

    /// Build a tempdir with `<dir>/flow.yaml` and any extra sibling
    /// files. Returns the tempdir guard and the absolute path to the
    /// flow YAML.
    fn write_flow_with_siblings(
        flow_yaml: &str,
        siblings: &[(&str, &str)],
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let flow_path = tmp.path().join("flow.yaml");
        std::fs::write(&flow_path, flow_yaml).expect("write flow.yaml");
        for (rel, contents) in siblings {
            let abs = tmp.path().join(rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).expect("mkdir sibling parent");
            }
            std::fs::write(&abs, contents).expect("write sibling");
        }
        (tmp, flow_path)
    }

    #[test]
    fn graph_flow_parses_prompt_file_field() {
        let yaml = r#"
version: "1"
name: g
prompt_file: prompts/intro.md
initial: start
graph:
  start:
    role: dev
    next:
      - done: "continue"
      - start: "retry"
  done:
    final: "end"
"#;
        let g = load_graph_flow_from_str(yaml).unwrap();
        assert_eq!(g.prompt_file.as_deref(), Some("prompts/intro.md"));
        assert!(g.prompt.is_none());
    }

    #[test]
    fn graph_state_parses_task_file_field() {
        let yaml = r#"
version: "1"
name: g
initial: start
graph:
  start:
    role: dev
    task_file: prompts/start.md
    next:
      - done: "continue"
      - start: "retry"
  done:
    final: "end"
"#;
        let g = load_graph_flow_from_str(yaml).unwrap();
        assert_eq!(
            g.graph["start"].task_file.as_deref(),
            Some("prompts/start.md")
        );
        assert!(g.graph["start"].task.is_none());
    }

    #[test]
    fn linear_flow_parses_prompt_file_and_task_file_fields() {
        // AC1, AC2 on the linear shape.
        let yaml = r#"
version: "1"
name: lin
prompt_file: prompts/intro.md
flow:
  build:
    agent: dev
    task_file: prompts/build.md
"#;
        let raw: RawFlowConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(raw.prompt_file.as_deref(), Some("prompts/intro.md"));
        assert_eq!(
            raw.flow["build"].task_file.as_deref(),
            Some("prompts/build.md")
        );
    }

    #[test]
    fn graph_flow_resolves_task_file_from_sibling() {
        let yaml = r#"
version: "1"
name: g
initial: design
graph:
  design:
    role: architect
    task_file: prompts/design.md
    next:
      - done: "continue"
      - design: "retry"
  done:
    final: "end"
"#;
        let (tmp, path) =
            write_flow_with_siblings(yaml, &[("prompts/design.md", "Design the thing.\n")]);
        let parsed = load_flow_any_from_path(&path).unwrap();
        let Flow::Graph(g) = parsed else {
            panic!("expected graph flow");
        };
        assert_eq!(
            g.graph["design"].task.as_deref(),
            Some("Design the thing.\n")
        );
        assert!(g.graph["design"].task_file.is_none());
        drop(tmp);
    }

    #[test]
    fn linear_flow_resolves_task_file_from_sibling() {
        let yaml = r#"
version: "1"
name: lin
flow:
  build:
    agent: dev
    task_file: prompts/build.md
"#;
        let (tmp, path) =
            write_flow_with_siblings(yaml, &[("prompts/build.md", "Build the thing.")]);
        let cfg = load_flow_from_path(&path).unwrap();
        assert_eq!(cfg.steps[0].task.as_deref(), Some("Build the thing."));
        drop(tmp);
    }

    #[test]
    fn task_and_task_file_together_is_validation_error_graph() {
        let yaml = r#"
version: "1"
name: g
initial: design
graph:
  design:
    role: architect
    task: inline
    task_file: prompts/design.md
    next:
      - done: "continue"
      - design: "retry"
  done:
    final: "end"
"#;
        let (tmp, path) = write_flow_with_siblings(yaml, &[("prompts/design.md", "external")]);
        let err = load_flow_any_from_path(&path).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("'task' and 'task_file'"), "got: {msg}");
        assert!(msg.contains("design"), "should name state ID, got: {msg}");
        drop(tmp);
    }

    #[test]
    fn prompt_and_prompt_file_together_is_validation_error_linear() {
        // AC3: mutual exclusion on linear, top-level prompt.
        let yaml = r#"
version: "1"
name: lin
prompt: inline
prompt_file: prompts/intro.md
flow:
  build:
    agent: dev
"#;
        let (tmp, path) = write_flow_with_siblings(yaml, &[("prompts/intro.md", "external")]);
        let err = load_flow_from_path(&path).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("'prompt' and 'prompt_file'"), "got: {msg}");
        drop(tmp);
    }

    #[test]
    fn missing_task_file_reports_flow_path_and_state_id() {
        let yaml = r#"
version: "1"
name: g
initial: design
graph:
  design:
    role: architect
    task_file: prompts/missing.md
    next:
      - done: "continue"
      - design: "retry"
  done:
    final: "end"
"#;
        let (tmp, path) = write_flow_with_siblings(yaml, &[]);
        let err = load_flow_any_from_path(&path).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"), "should mention not-found: {msg}");
        assert!(msg.contains("design"), "should name state: {msg}");
        assert!(
            msg.contains("prompts/missing.md"),
            "should name relative path: {msg}"
        );
        assert!(
            msg.contains(&path.display().to_string()),
            "should embed flow path: {msg}"
        );
        drop(tmp);
    }

    #[test]
    fn parent_dir_traversal_is_rejected_before_io() {
        // AC5: '..' rejected. The file does not exist anywhere on
        // disk, so the test confirms the traversal error fires before
        // any read attempt -- i.e. the message is the traversal one,
        // not a generic 'not found'.
        let yaml = r#"
version: "1"
name: g
initial: design
graph:
  design:
    role: architect
    task_file: ../escape.md
    next:
      - done: "continue"
      - design: "retry"
  done:
    final: "end"
"#;
        let (tmp, path) = write_flow_with_siblings(yaml, &[]);
        let err = load_flow_any_from_path(&path).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("'..' is not allowed") || msg.contains("escapes the flow directory"),
            "got: {msg}"
        );
        drop(tmp);
    }

    #[test]
    fn absolute_path_is_rejected() {
        // AC5: absolute path rejected.
        let yaml = r#"
version: "1"
name: lin
flow:
  build:
    agent: dev
    task_file: /etc/passwd
"#;
        let (tmp, path) = write_flow_with_siblings(yaml, &[]);
        let err = load_flow_from_path(&path).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("must be a relative path"), "got: {msg}");
        drop(tmp);
    }

    #[test]
    fn nested_parent_dir_in_path_is_rejected() {
        // AC5: `prompts/../prompts/x.md` is rejected by the
        // component check -- not flattened first.
        let yaml = r#"
version: "1"
name: lin
flow:
  build:
    agent: dev
    task_file: prompts/../escape.md
"#;
        let (tmp, path) =
            write_flow_with_siblings(yaml, &[("prompts/build.md", "x"), ("escape.md", "x")]);
        let err = load_flow_from_path(&path).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("'..' is not allowed"), "got: {msg}");
        drop(tmp);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_outside_base_dir_is_rejected() {
        // AC5: symlink-escape via canonicalize. Build a flow dir that
        // contains a symlink pointing to a file in a sibling dir, and
        // expect the loader to reject it after canonicalizing both
        // sides.
        let outer = tempfile::tempdir().expect("outer tempdir");
        let outside = outer.path().join("outside.md");
        std::fs::write(&outside, "secret").expect("write outside");

        let flow_dir = outer.path().join("flow_dir");
        std::fs::create_dir(&flow_dir).expect("mk flow dir");
        let link = flow_dir.join("link.md");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");

        let yaml = r#"
version: "1"
name: lin
flow:
  build:
    agent: dev
    task_file: link.md
"#;
        let flow_path = flow_dir.join("flow.yaml");
        std::fs::write(&flow_path, yaml).expect("write flow");

        let err = load_flow_from_path(&flow_path).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("outside the flow directory"),
            "expected escape error, got: {msg}"
        );
    }

    #[test]
    fn external_prompt_content_is_substituted_with_vars() {
        // AC4: variable substitution still applies after the file is
        // loaded. The resolver does not run substitute_vars itself --
        // the runner does -- but the contract is that the resolved
        // `task:` carries placeholders that the runner substitutes
        // later. This test covers the resolver side: it verifies the
        // file content lands in `task` so subsequent
        // `substitute_vars` would pick it up. The runner side is
        // covered indirectly by the existing substitute_vars tests
        // running on `step.task`.
        let yaml = r#"
version: "1"
name: lin
flow:
  build:
    agent: dev
    task_file: t.md
"#;
        let (tmp, path) = write_flow_with_siblings(yaml, &[("t.md", "Issue #{{vars.id}}")]);
        let cfg = load_flow_from_path(&path).unwrap();
        assert_eq!(cfg.steps[0].task.as_deref(), Some("Issue #{{vars.id}}"));
        drop(tmp);
    }

    #[test]
    fn resolver_clears_file_field_after_resolution() {
        // After `resolve_graph_external_prompts` runs, every *_file
        // field is None. The runtime relies on this invariant
        // (`debug_assert` in `execute_graph_flow_setup`).
        let yaml = r#"
version: "1"
name: g
prompt_file: intro.md
initial: design
graph:
  design:
    role: architect
    task_file: design.md
    next:
      - done: "continue"
      - design: "retry"
  done:
    final: "end"
"#;
        let (tmp, path) =
            write_flow_with_siblings(yaml, &[("intro.md", "intro"), ("design.md", "design")]);
        let parsed = load_flow_any_from_path(&path).unwrap();
        let Flow::Graph(g) = parsed else {
            panic!("expected graph");
        };
        assert!(g.prompt_file.is_none());
        assert!(g.graph.values().all(|s| s.task_file.is_none()));
        assert_eq!(g.prompt.as_deref(), Some("intro"));
        assert_eq!(g.graph["design"].task.as_deref(), Some("design"));
        drop(tmp);
    }

    // --- `kind: shell` schema validation (issue #310) -------------------
    //
    // The shell-state contract is enforced at parse time so a misshapen
    // verify gate is rejected long before the runner spawns anything.
    // Each test pins one rule from `validate_shell_state_semantics` and
    // names the field the user would need to fix.

    /// Helper: minimal three-state graph with a `kind: shell` `verify`
    /// state between `a` and the two terminal states. Tests mutate the
    /// returned YAML to flip one field at a time.
    fn shell_yaml(verify_block: &str) -> String {
        format!(
            r#"
version: "1"
name: shell-test
initial: a
graph:
  a:
    role: developer
    next:
      - verify: "run verify"
      - done: "skip verify"
{verify_block}
  done:
    final: "pass"
  back:
    role: developer
    next:
      - verify: "try again"
      - done: "give up"
"#
        )
    }

    #[test]
    fn shell_state_rejects_empty_run() {
        let yaml = shell_yaml(
            r#"  verify:
    run: "   "
    next:
      - done: pass
      - back: fail
"#,
        );
        let err = load_flow_any_from_str(&yaml).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn shell_state_rejects_role() {
        let yaml = shell_yaml(
            r#"  verify:
    run: "true"
    role: developer
    next:
      - done: pass
      - back: fail
"#,
        );
        let err = load_flow_any_from_str(&yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("verify") && msg.contains("role"),
            "expected error mentioning role on shell state, got: {msg}"
        );
    }

    #[test]
    fn shell_state_rejects_task() {
        let yaml = shell_yaml(
            r#"  verify:
    run: "true"
    task: "do the thing"
    next:
      - done: pass
      - back: fail
"#,
        );
        let err = load_flow_any_from_str(&yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("verify") && msg.contains("task"),
            "expected error mentioning task on shell state, got: {msg}"
        );
    }

    #[test]
    fn shell_state_requires_pass_entry() {
        let yaml = shell_yaml(
            r#"  verify:
    run: "true"
    next:
      - back: fail
      - done: fail
"#,
        );
        let err = load_flow_any_from_str(&yaml).unwrap_err();
        assert!(err.to_string().contains("pass"));
    }

    #[test]
    fn shell_state_requires_fail_entry() {
        let yaml = shell_yaml(
            r#"  verify:
    run: "true"
    next:
      - done: pass
      - back: pass
"#,
        );
        let err = load_flow_any_from_str(&yaml).unwrap_err();
        assert!(err.to_string().contains("fail"));
    }

    #[test]
    fn shell_state_rejects_self_loop() {
        let yaml = shell_yaml(
            r#"  verify:
    run: "true"
    next:
      - done: pass
      - verify: fail
"#,
        );
        let err = load_flow_any_from_str(&yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("self-loop") && msg.contains("verify"),
            "expected self-loop error naming state, got: {msg}"
        );
    }

    #[test]
    fn shell_state_round_trips() {
        let yaml = shell_yaml(
            r#"  verify:
    run: "just lint && just test"
    next:
      - done: pass
      - back: fail
"#,
        );
        let flow = load_flow_any_from_str(&yaml).unwrap();
        let g = match flow {
            Flow::Graph(g) => g,
            Flow::Linear(_) => panic!("expected graph"),
        };
        let verify = &g.graph["verify"];
        assert!(verify.is_shell());
        assert_eq!(verify.run.as_deref(), Some("just lint && just test"));
        let select = verify.select.as_ref().unwrap();
        assert_eq!(select[0].target, "done");
        assert_eq!(select[1].target, "back");
    }

    // --- Default impl guards (issue #305) ---
    //
    // These tests pin two things:
    //
    // 1. Every type in the issue's list (and its transitive `Default`
    //    dependencies) actually implements `Default`. If anyone removes a
    //    derive, the build breaks here -- not in some downstream test
    //    fixture with a confusing message.
    //
    // 2. `GraphState` / `GraphEdge` / `GraphFlow` -- the only types in the
    //    list that derive serde -- round-trip cleanly when constructed via
    //    `Default::default()`. A future field that is added without
    //    `#[serde(skip_serializing_if = "Option::is_none")]` (or with a
    //    `#[serde(default = "fn")]` whose return value diverges from the
    //    `Default` derive) would silently change what
    //    `GraphState::default()` emits. The round-trip equality check
    //    catches that divergence.

    #[test]
    fn default_impls_present_for_listed_types() {
        let _: Backend = Backend::default();
        let _: Version = Version::default();
        let _: Defaults = Defaults::default();
        let _: StackConfig = StackConfig::default();
        let _: Step = Step::default();
        let _: FlowConfig = FlowConfig::default();
        let _: GraphFlow = GraphFlow::default();
        let _: GraphState = GraphState::default();
    }

    /// Backend default is `claude-cli`. Pin this so a future tier-zero
    /// change cannot silently flip it -- `claude-cli` is what every
    /// `Defaults::default()` and `Step::default()` resolves to today.
    #[test]
    fn backend_default_is_claude_cli() {
        assert_eq!(Backend::default(), Backend::ClaudeCli);
        assert_eq!(Defaults::default().backend, Backend::ClaudeCli);
    }

    /// Round-trip: `GraphState::default()` survives serialize +
    /// deserialize unchanged. All current fields are `Option<_>` with
    /// `skip_serializing_if = "Option::is_none"`, so the emitted YAML is
    /// `{}`. Adding a non-`Option` field -- or an `Option` field without
    /// `skip_serializing_if` -- would change the YAML shape, and the
    /// re-parse would no longer equal the original (or would fail to
    /// parse with `deny_unknown_fields`). Either way, this test is the
    /// alarm.
    #[test]
    fn graph_state_default_serde_round_trip() {
        let original = GraphState::default();
        let yaml = serde_yaml::to_string(&original).expect("serialize");
        let parsed: GraphState = serde_yaml::from_str(&yaml).expect("re-parse");
        assert_eq!(original, parsed);
    }

    /// Round-trip: `GraphFlow::default()`. `version`, `name`, and
    /// `initial` are required -- they serialize to empty strings, and
    /// `graph` to an empty mapping. The reparse must equal the
    /// original. Like the other round-trip tests, this fires when a
    /// future field's `Default` and serde defaults disagree.
    #[test]
    fn graph_flow_default_serde_round_trip() {
        let original = GraphFlow::default();
        let yaml = serde_yaml::to_string(&original).expect("serialize");
        let parsed: GraphFlow = serde_yaml::from_str(&yaml).expect("re-parse");
        assert_eq!(original, parsed);
    }
}
