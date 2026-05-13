//! Graph-flow data shapes (issue #324: extracted from `src/config.rs`).
//!
//! These types describe what a flow IS, independent of how it was parsed.
//! Parsers (`config.rs` for YAML, `config_md.rs` for Markdown, future SDK
//! adapters) deserialize INTO these types; the runtime executes them; the
//! validator (`core::validator`) checks them.
//!
//! Dependency direction: `core` does not import from any parser. Parsers
//! depend inward on `core`.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Accepts both `"1"` (string) and `1` (integer) in YAML.
///
/// Lives in `core` because both linear and graph flows carry a version
/// field and both parsers must emit the same representation. Re-exported
/// from `config` for backward compatibility with existing call sites.
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

// --- Graph flow schema (issue #237) ---

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
    /// [`crate::config::resolve_graph_external_prompts`] runs this is
    /// always `None`, so the runtime never has to branch on which way
    /// the prompt was authored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_file: Option<String>,
    /// State ID where the graph starts. Must reference a key in
    /// [`GraphFlow::graph`]; the structural check fires in
    /// [`crate::core::validator::validate_graph_flow`].
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
/// [`crate::core::validator::validate_graph_reachability`].
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
    /// [`crate::config::resolve_graph_external_prompts`] runs this is
    /// always `None`.
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
    /// Backend-keyed extra CLI arguments for this state (#356, mirrors
    /// the linear-runner step-level field from #236). Same raw YAML
    /// shape as `Step::extra_args` / `Agent::extra_args`: keys are
    /// backend names (`claude-cli`, `codex`, `ollama`), values are
    /// argv tokens spliced into the executor command at runtime.
    ///
    /// Stored in the string-keyed form (not `HashMap<Backend, ...>`)
    /// to avoid pulling `crate::config::Backend` into the `core`
    /// layer; the runner resolves entries via `Backend::yaml_name`.
    /// Validation of the keys happens in the parser
    /// (`crate::config::load_graph_flow_from_str`).
    ///
    /// Cascade with the agent-level `extra_args` is replace-not-merge,
    /// matching `resolve_extra_args` in the linear runner: a non-empty
    /// state map fully shadows the agent map, even if it has no entry
    /// for the effective backend.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra_args: HashMap<String, Vec<String>>,
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
                // The custom impl reads the value as a `serde_yaml::Value` so
                // it can distinguish bare strings, lists, and null. This is a
                // residual format leak (YAML lib in core); flagged for the
                // parser-audit follow-up under epic #323.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_state_default_serde_round_trip() {
        // Default GraphState has no fields set; with `skip_serializing_if =
        // "Option::is_none"` the YAML is just `{}`. Re-deserializing that
        // must yield the same default value.
        let state = GraphState::default();
        let yaml = serde_yaml::to_string(&state).expect("serialize default GraphState");
        let parsed: GraphState = serde_yaml::from_str(&yaml).expect("re-parse default GraphState");
        assert_eq!(state, parsed);
    }

    #[test]
    fn graph_flow_default_serde_round_trip() {
        let flow = GraphFlow::default();
        let yaml = serde_yaml::to_string(&flow).expect("serialize default GraphFlow");
        let parsed: GraphFlow = serde_yaml::from_str(&yaml).expect("re-parse default GraphFlow");
        assert_eq!(flow, parsed);
    }
}
