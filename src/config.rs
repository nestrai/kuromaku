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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    Api,
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
    /// Backend-keyed extra CLI arguments (#236). Empty map = no overrides.
    /// When a step does not declare its own `extra_args`, the runner uses
    /// this map for whichever backend is effective for the step.
    pub extra_args: HashMap<Backend, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
/// Schema-only for now (issue #237): no runtime, no reachability/dead-end
/// validator. The driver and validator that turn this into executable
/// behaviour live in follow-up issues. Validation here covers structural
/// references only -- `initial:` exists, every `edge.to:` exists, and each
/// state has either outgoing edges or a `kind:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphFlow {
    pub version: Version,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// State ID where the graph starts. Must reference a key in
    /// [`GraphFlow::states`]; the structural check fires in
    /// [`validate_graph_flow`].
    pub initial: String,
    /// State definitions, keyed by state ID. Order is preserved so a
    /// future Mermaid exporter can render states in declaration order.
    pub states: IndexMap<String, GraphState>,
}

/// One node in the state graph.
///
/// A non-terminal state declares `edges:`. A terminal state declares
/// `kind: final`. A human-handoff state declares `kind: human` (and may
/// also declare `edges:` for the resume-* targets shown in the design
/// doc). The validator rejects a state with neither edges nor kind --
/// that shape would be a silent dead end at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<StateKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Outgoing transitions, keyed by edge name. Optional because
    /// `kind: final` states do not need any. Order is preserved so the
    /// runtime (when it lands) can present choices to an agent in
    /// declaration order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edges: Option<IndexMap<String, GraphEdge>>,
}

/// Terminal-state discriminator. `final` ends the run; `human` hands off
/// to a human operator. Both are accepted at the schema level here --
/// runtime semantics for `human` (resume, abort) are owned by the driver
/// issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateKind {
    Final,
    Human,
}

/// One outgoing transition from a state. Both fields are required:
/// `to:` so the runtime knows where to go, `description:` so the agent
/// (or human) understands when to pick this edge. Missing either is a
/// serde-level error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphEdge {
    pub to: String,
    pub description: String,
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
    states: Option<serde_yaml::Value>,
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
    match (probe.flow.is_some(), probe.states.is_some()) {
        (true, true) => Err(ConfigError::Validation(
            "flow file declares both 'flow:' and 'states:' -- pick one (linear flow vs state graph)"
                .to_string(),
        )),
        (false, false) => Err(ConfigError::Validation(
            "flow file must declare either 'flow:' (linear) or 'states:' (state graph)"
                .to_string(),
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

/// Structural validation for [`GraphFlow`].
///
/// Walks the state map twice: first to confirm `initial:` resolves, then
/// per state to confirm every edge target resolves and the
/// edges-or-kind requirement holds. Errors are written to name the
/// offending state ID and field so the user can jump straight to the
/// problem in the YAML.
fn validate_graph_flow(g: &GraphFlow) -> Result<(), ConfigError> {
    if g.version.0 != "1" {
        return Err(ConfigError::Validation(format!(
            "unsupported version '{}', expected '1'",
            g.version.0
        )));
    }

    if !g.states.contains_key(&g.initial) {
        return Err(ConfigError::Validation(format!(
            "graph flow 'initial' references unknown state '{}' (known states: {})",
            g.initial,
            sorted_state_ids(&g.states).join(", ")
        )));
    }

    for (id, state) in &g.states {
        let has_edges = state.edges.as_ref().is_some_and(|e| !e.is_empty());
        let has_kind = state.kind.is_some();
        if !has_edges && !has_kind {
            return Err(ConfigError::Validation(format!(
                "graph state '{id}' has neither 'edges:' nor 'kind:' -- a state must declare at least one outgoing edge or be marked 'kind: final' / 'kind: human'"
            )));
        }
        if let Some(edges) = &state.edges {
            for (edge_name, edge) in edges {
                if !g.states.contains_key(&edge.to) {
                    return Err(ConfigError::Validation(format!(
                        "graph state '{id}' edge '{edge_name}' targets unknown state '{}' (known states: {})",
                        edge.to,
                        sorted_state_ids(&g.states).join(", ")
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

    // BFS from `initial:`. The schema validator guarantees `initial:` and
    // every `edge.to:` resolve, so we can deref-and-index without a
    // contains_key probe.
    let mut visited: HashSet<&str> = HashSet::new();
    let mut frontier: Vec<&str> = Vec::new();
    visited.insert(g.initial.as_str());
    frontier.push(g.initial.as_str());
    while let Some(id) = frontier.pop() {
        let Some(state) = g.states.get(id) else {
            // Defensive: schema check should make this unreachable, but
            // an in-memory `GraphFlow` constructed without validation
            // could still hit it. Skip rather than panic.
            continue;
        };
        let Some(edges) = &state.edges else { continue };
        for edge in edges.values() {
            if visited.insert(edge.to.as_str()) {
                frontier.push(edge.to.as_str());
            }
        }
    }

    // Dead-end detection. Walk every state (declaration order via
    // IndexMap), not just the reachable ones, so the user sees every
    // dead-end in one validation pass even if some are unreachable too.
    // Iterating in declaration order keeps error output stable for tests
    // and matches the user's mental model of the YAML file.
    for (id, state) in &g.states {
        let has_edges = state.edges.as_ref().is_some_and(|e| !e.is_empty());
        let is_terminal = matches!(state.kind, Some(StateKind::Final) | Some(StateKind::Human));
        if !has_edges && !is_terminal {
            report.errors.push(format!(
                "graph state '{id}' is a dead end: no outgoing edges and not 'kind: final' or 'kind: human'"
            ));
        }
    }

    // Unreachable detection. Sort lexically for stable output -- BFS
    // visits in non-deterministic order (HashSet iteration), so we need
    // an explicit sort here for diffable error messages.
    let mut unreachable: Vec<&str> = g
        .states
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

states:
  start:
    role: developer
    task: |
      Do the first thing.
    edges:
      ok:
        to: middle
        description: Things went well.
      stuck:
        to: aborted
        description: Cannot proceed.

  middle:
    role: reviewer
    task: |
      Check the result.
    edges:
      approved:
        to: done
        description: Looks good.
      rework:
        to: start
        description: Needs another round.

  human_review:
    kind: human
    edges:
      resume:
        to: middle
        description: Operator unblocks the review.
      abort:
        to: aborted
        description: Operator aborts the run.

  done:
    kind: final

  aborted:
    kind: final
"#;

    #[test]
    fn graph_minimal_parses() {
        // Acceptance: adding `states:` instead of `flow:` parses into a
        // graph variant of the flow config.
        let flow = load_flow_any_from_str(GRAPH_CONFIG).unwrap();
        let g = match flow {
            Flow::Graph(g) => g,
            Flow::Linear(_) => panic!("expected graph variant, got linear"),
        };
        assert_eq!(g.name, "example-graph");
        assert_eq!(g.version.0, "1");
        assert_eq!(g.initial, "start");
        // Order is preserved from declaration order.
        let ids: Vec<&str> = g.states.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            vec!["start", "middle", "human_review", "done", "aborted"]
        );

        // Spot-check a regular state: edges parse with `to:` and
        // `description:` populated.
        let start = &g.states["start"];
        assert_eq!(start.role.as_deref(), Some("developer"));
        let edges = start.edges.as_ref().unwrap();
        assert_eq!(edges["ok"].to, "middle");
        assert_eq!(edges["ok"].description, "Things went well.");

        // Final state has no edges, just kind.
        let done = &g.states["done"];
        assert_eq!(done.kind, Some(StateKind::Final));
        assert!(done.edges.is_none());

        // Human state carries kind AND edges (the design doc resume-*
        // pattern).
        let hr = &g.states["human_review"];
        assert_eq!(hr.kind, Some(StateKind::Human));
        assert!(hr.edges.is_some());
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
        // Acceptance: `initial:` is required when `states:` is present;
        // the error message names the field.
        let yaml = r#"
version: "1"
name: no-initial
states:
  done:
    kind: final
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
        // Acceptance: `initial:` referencing an unknown state produces a
        // structural error referencing the offending state-id and field.
        let yaml = r#"
version: "1"
name: bad-initial
initial: nowhere
states:
  done:
    kind: final
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
    fn graph_edge_to_unknown_state_errors() {
        // Acceptance: an edge `to:` referencing an unknown state produces
        // a structural error referencing the offending state-id, edge
        // name, and target.
        let yaml = r#"
version: "1"
name: bad-edge
initial: start
states:
  start:
    role: developer
    edges:
      ok:
        to: ghost
        description: Goes nowhere real.
  done:
    kind: final
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("start") && msg.contains("'ok'") && msg.contains("'ghost'"),
            "error must name source state, edge name, and target: {msg}"
        );
    }

    #[test]
    fn graph_mixed_flow_and_states_is_hard_error() {
        // Acceptance: a flow file with both `flow:` and `states:` is a
        // hard error -- "pick one".
        let yaml = r#"
version: "1"
name: mixed
initial: start
flow:
  whatever:
    agent: dev
states:
  start:
    kind: final
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("both") && msg.contains("'flow:'") && msg.contains("'states:'"),
            "error must explicitly call out the conflict: {msg}"
        );
        assert!(
            msg.contains("pick one"),
            "error must direct the user to pick one shape: {msg}"
        );
    }

    #[test]
    fn graph_neither_flow_nor_states_errors() {
        // Implied by the dispatch: a YAML that declares neither shape is
        // not a valid flow file. Errors must name both fields so the
        // user knows the choice they need to make.
        let yaml = r#"
version: "1"
name: empty-shape
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("'flow:'") && msg.contains("'states:'"),
            "error must list both possible top-level shapes: {msg}"
        );
    }

    #[test]
    fn graph_state_without_edges_or_kind_errors() {
        // Acceptance: each state has either `edges:` (non-final) or
        // `kind: final` / `kind: human`. A state with neither is a
        // silent dead end at runtime -- reject it at parse time.
        let yaml = r#"
version: "1"
name: dangling-state
initial: lonely
states:
  lonely:
    role: developer
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("'lonely'"),
            "error must name the offending state: {msg}"
        );
        assert!(
            msg.contains("edges") && msg.contains("kind"),
            "error must explain what is required (edges or kind): {msg}"
        );
    }

    #[test]
    fn graph_edge_missing_to_is_serde_error() {
        // The `to:` field is required by the GraphEdge serde derive;
        // omitting it surfaces a parse error from serde, not a custom
        // validation. We assert the error mentions `to` so the user can
        // locate the omission.
        let yaml = r#"
version: "1"
name: bad-edge
initial: start
states:
  start:
    edges:
      ok:
        description: Missing the target.
  done:
    kind: final
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("to"),
            "serde error must mention the missing 'to' field: {msg}"
        );
    }

    #[test]
    fn graph_edge_missing_description_is_serde_error() {
        // Same contract as the `to:` test: `description:` is required so
        // the runtime can show the agent/human a meaningful menu of
        // choices. Omission must fail at parse, not silently default.
        let yaml = r#"
version: "1"
name: bad-edge
initial: start
states:
  start:
    edges:
      ok:
        to: done
  done:
    kind: final
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("description"),
            "serde error must mention the missing 'description' field: {msg}"
        );
    }

    #[test]
    fn graph_unknown_field_is_rejected() {
        // `deny_unknown_fields` on GraphFlow / GraphState / GraphEdge
        // makes typos fail at parse time. We anchor that here so future
        // edits do not silently relax the schema.
        let yaml = r#"
version: "1"
name: typo
initial: start
states:
  start:
    role: developer
    typo_field: oops
    edges:
      ok:
        to: done
        description: ok
  done:
    kind: final
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("typo_field"),
            "deny_unknown_fields must surface the unknown key: {msg}"
        );
    }

    #[test]
    fn graph_unknown_kind_value_errors() {
        // `kind:` is an enum (`final` | `human`); any other value must
        // fail at parse time so a typo cannot silently disable the
        // terminal-state semantics.
        let yaml = r#"
version: "1"
name: bad-kind
initial: done
states:
  done:
    kind: end
"#;
        let err = load_flow_any_from_str(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("end") || msg.contains("variant"),
            "error must indicate the invalid kind value: {msg}"
        );
    }

    #[test]
    fn graph_unsupported_version_errors() {
        // Same version policy as the linear loader: only "1" is
        // accepted today. Mirrored here so the graph loader cannot
        // silently drift to a different policy.
        let yaml = r#"
version: "2"
name: future
initial: done
states:
  done:
    kind: final
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
            prompt: None,
            initial: initial.to_string(),
            states: map,
        }
    }

    fn state_with_edges(edges: Vec<(&str, &str)>) -> GraphState {
        let mut map: IndexMap<String, GraphEdge> = IndexMap::new();
        for (name, to) in edges {
            map.insert(
                name.to_string(),
                GraphEdge {
                    to: to.to_string(),
                    description: format!("go to {to}"),
                },
            );
        }
        GraphState {
            kind: None,
            role: Some("developer".to_string()),
            task: None,
            edges: Some(map),
        }
    }

    fn state_final() -> GraphState {
        GraphState {
            kind: Some(StateKind::Final),
            role: None,
            task: None,
            edges: None,
        }
    }

    fn state_human(edges: Vec<(&str, &str)>) -> GraphState {
        let mut s = state_with_edges(edges);
        s.kind = Some(StateKind::Human);
        s.role = None;
        s
    }

    /// Acceptance: a graph where every state is either reachable + has
    /// edges OR `kind: final` validates clean -- no errors, no warnings.
    #[test]
    fn validate_clean_graph() {
        let g = graph(
            "start",
            vec![
                ("start", state_with_edges(vec![("ok", "done")])),
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

    /// Acceptance: a graph with a non-final state and zero outgoing
    /// edges fails validation with a message naming the state. The YAML
    /// schema rejects this shape, so the test constructs `GraphFlow`
    /// programmatically -- the validator must still catch it as
    /// defense-in-depth.
    #[test]
    fn validate_dead_end_state_errors() {
        let stuck = GraphState {
            kind: None,
            role: Some("developer".to_string()),
            task: None,
            edges: None, // no kind, no edges -- dead end
        };
        let g = graph(
            "start",
            vec![
                ("start", state_with_edges(vec![("go", "stuck")])),
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
                ("start", state_with_edges(vec![("ok", "done")])),
                ("done", state_final()),
                ("orphan", state_with_edges(vec![("loop", "done")])),
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
                    state_with_edges(vec![("retry", "start"), ("done", "done")]),
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
    fn validate_human_state_without_edges_not_dead_end() {
        let mut hr = state_final();
        hr.kind = Some(StateKind::Human);
        let g = graph(
            "start",
            vec![
                ("start", state_with_edges(vec![("handoff", "operator")])),
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
                ("start", state_with_edges(vec![("handoff", "operator")])),
                ("operator", state_human(vec![("resume", "done")])),
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
            kind: None,
            role: Some("developer".to_string()),
            task: None,
            edges: None,
        };
        let g = graph(
            "start",
            vec![
                (
                    "start",
                    state_with_edges(vec![("a", "first"), ("b", "second")]),
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

    /// Acceptance: the canonical GRAPH_CONFIG fixture (every shape:
    /// non-terminal, re-entrant, human, two finals) validates clean.
    /// Anchors that the existing schema fixture is also valid at the
    /// graph-validation layer, not just the schema layer.
    #[test]
    fn validate_canonical_graph_fixture_clean() {
        let g = load_graph_flow_from_str(GRAPH_CONFIG).unwrap();
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
}
