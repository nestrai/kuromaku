//! Engine-level validators for graph flows.
//!
//! Two passes:
//!
//! * [`validate_graph_flow`] -- structural / schema check. Rejects the
//!   flow with a hard [`ValidationError`].
//! * [`validate_graph_reachability`] -- BFS dead-end + reachability
//!   pass. Returns a [`GraphValidationReport`] separating errors and
//!   warnings.
//!
//! Both operate on [`GraphFlow`] only; they have no parser or runtime
//! coupling. Issue #324 extracted them from `src/config.rs` to make the
//! `formats -> core -> runtime` dependency direction explicit.

use std::collections::HashSet;

use indexmap::IndexMap;

use super::error::ValidationError;
use super::graph_flow::{GraphFlow, GraphState};

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
// Visibility note: this function is only consumed inside the binary
// crate (parsers `config.rs` and `config_md.rs`). The original lived as
// `pub(crate)` in `config.rs`. Promoted to `pub` here so it can be
// re-exported via `core::mod`'s public surface; in a single-binary
// crate `pub` and `pub(crate)` carry the same effective scope.
pub fn validate_graph_flow(g: &GraphFlow) -> Result<(), ValidationError> {
    if g.version.0 != "1" {
        return Err(ValidationError::new(format!(
            "unsupported version '{}', expected '1'",
            g.version.0
        )));
    }

    if !g.graph.contains_key(&g.initial) {
        return Err(ValidationError::new(format!(
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
                    return Err(ValidationError::new(format!(
                        "graph state '{id}' select targets unknown state '{}' (known states: {})",
                        entry.target,
                        sorted_state_ids(&g.graph).join(", ")
                    )));
                }
                if !seen_targets.insert(&entry.target) {
                    return Err(ValidationError::new(format!(
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
fn validate_state_semantics(id: &str, state: &GraphState) -> Result<(), ValidationError> {
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
        return Err(ValidationError::new(format!(
            "graph state '{id}' has conflicting fields {kinds:?} -- use exactly one of 'run', 'final', or 'human'"
        )));
    }

    if state.is_shell() {
        // Shell state validation
        match state.run.as_deref().map(str::trim) {
            Some(cmd) if !cmd.is_empty() => {}
            _ => {
                return Err(ValidationError::new(format!(
                    "graph state '{id}' has `run:` but the command is empty"
                )));
            }
        }
        if state.role.is_some() {
            return Err(ValidationError::new(format!(
                "graph state '{id}' has `run:` and must not declare `role:` -- shell states have no agent"
            )));
        }
        if state.task.is_some() || state.task_file.is_some() {
            return Err(ValidationError::new(format!(
                "graph state '{id}' has `run:` and must not declare `task:` or `task_file:` -- shell states do not run an LLM"
            )));
        }
        let entries = state.select.as_ref().ok_or_else(|| {
            ValidationError::new(format!(
                "graph state '{id}' has `run:` and must declare `next:` with both a `pass` and a `fail` target"
            ))
        })?;
        if entries.len() != 2 {
            return Err(ValidationError::new(format!(
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
                return Err(ValidationError::new(format!(
                    "graph state '{id}' select entry '{}' must have reason 'pass' or 'fail' -- shell states are exit-code-routed",
                    entry.target
                )));
            }
            if entry.target == id {
                return Err(ValidationError::new(format!(
                    "graph state '{id}' select entry '{}' is a self-loop -- a shell state cannot recover from its own failure, route to a different state",
                    entry.target
                )));
            }
        }
        if !has_pass {
            return Err(ValidationError::new(format!(
                "graph state '{id}' has `run:` and must declare a select entry with reason 'pass'"
            )));
        }
        if !has_fail {
            return Err(ValidationError::new(format!(
                "graph state '{id}' has `run:` and must declare a select entry with reason 'fail'"
            )));
        }
    } else if state.is_final() {
        // Final state validation
        if let Some(desc) = &state.final_desc
            && desc.trim().is_empty()
        {
            return Err(ValidationError::new(format!(
                "graph state '{id}' has `final:` with an empty description"
            )));
        }
        if state.role.is_some() || state.task.is_some() || state.select.is_some() {
            return Err(ValidationError::new(format!(
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
                return Err(ValidationError::new(format!(
                    "graph state '{id}' must have at least 2 select entries (got {})",
                    entries.len()
                )));
            }
            for entry in entries {
                if entry.reason.is_none() {
                    return Err(ValidationError::new(format!(
                        "graph state '{id}' select entry '{}' must have a non-empty reason",
                        entry.target
                    )));
                }
                let reason_text = entry.reason.as_ref().unwrap().display();
                if reason_text.trim().is_empty() {
                    return Err(ValidationError::new(format!(
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
/// passed before calling this -- typically via the parser entry points.
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

#[cfg(test)]
mod tests {
    use super::super::graph_flow::{GraphFlow, GraphState, SelectEntry, SelectReason, Version};
    use super::*;

    fn graph(initial: &str, states: Vec<(&str, GraphState)>) -> GraphFlow {
        let mut graph = IndexMap::new();
        for (id, state) in states {
            graph.insert(id.to_string(), state);
        }
        GraphFlow {
            version: Version("1".to_string()),
            name: "t".to_string(),
            prompt: None,
            prompt_file: None,
            initial: initial.to_string(),
            graph,
        }
    }

    fn state_with_select(targets: Vec<(&str, &str)>) -> GraphState {
        let entries: Vec<SelectEntry> = targets
            .into_iter()
            .map(|(target, reason)| SelectEntry {
                target: target.to_string(),
                reason: Some(SelectReason::Single(reason.to_string())),
            })
            .collect();
        GraphState {
            role: Some("dev".to_string()),
            task: Some("do work".to_string()),
            select: Some(entries),
            ..Default::default()
        }
    }

    fn state_final() -> GraphState {
        GraphState {
            final_desc: Some("done".to_string()),
            ..Default::default()
        }
    }

    fn state_final_no_description() -> GraphState {
        GraphState {
            final_desc: Some(String::new()),
            ..Default::default()
        }
    }

    fn state_human(targets: Vec<(&str, &str)>) -> GraphState {
        let entries: Vec<SelectEntry> = targets
            .into_iter()
            .map(|(target, reason)| SelectEntry {
                target: target.to_string(),
                reason: Some(SelectReason::Single(reason.to_string())),
            })
            .collect();
        let select = if entries.is_empty() {
            None
        } else {
            Some(entries)
        };
        GraphState {
            human: Some(true),
            select,
            ..Default::default()
        }
    }

    #[test]
    fn validate_clean_graph() {
        // initial -> review -> done
        //                   -> rework -> review (loop OK)
        let g = graph(
            "initial",
            vec![
                (
                    "initial",
                    state_with_select(vec![("review", "approve"), ("rework", "needs work")]),
                ),
                (
                    "review",
                    state_with_select(vec![("done", "ship it"), ("rework", "needs work")]),
                ),
                (
                    "rework",
                    state_with_select(vec![("review", "back to review"), ("done", "give up")]),
                ),
                ("done", state_final()),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(report.is_ok(), "expected ok, got {:?}", report);
        assert!(report.warnings.is_empty(), "got warnings: {:?}", report);
    }

    #[test]
    fn validate_unreachable_state_warns_only() {
        let g = graph(
            "initial",
            vec![
                (
                    "initial",
                    state_with_select(vec![("done", "go"), ("alt_done", "alt")]),
                ),
                ("done", state_final()),
                ("alt_done", state_final()),
                (
                    "orphan",
                    state_with_select(vec![("done", "x"), ("alt_done", "y")]),
                ),
            ],
        );
        let report = validate_graph_reachability(&g);
        // Dead-end-only checks: orphan has select, so no error. But it's
        // unreachable from initial, hence a warning.
        assert!(report.is_ok(), "expected ok, got errors: {:?}", report);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("orphan") && w.contains("unreachable")),
            "expected unreachable warning for 'orphan', got: {:?}",
            report.warnings
        );
    }

    #[test]
    fn validate_human_state_without_select_not_dead_end() {
        // Human state with no select must not be flagged as dead-end -- it
        // is a terminal kind.
        let g = graph(
            "initial",
            vec![
                (
                    "initial",
                    state_with_select(vec![("escalate", "needs human"), ("done", "ok")]),
                ),
                ("escalate", state_human(vec![])),
                ("done", state_final()),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(
            report.is_ok(),
            "expected ok (human state is terminal), got: {:?}",
            report
        );
    }

    #[test]
    fn validate_dead_end_state_is_error() {
        // 'stuck' has no select and no final -- pure dead end.
        let g = graph(
            "initial",
            vec![
                (
                    "initial",
                    state_with_select(vec![("stuck", "go"), ("done", "alt")]),
                ),
                ("stuck", GraphState::default()),
                ("done", state_final()),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(
            !report.is_ok(),
            "expected dead-end error, got ok: {:?}",
            report
        );
        assert!(
            report.errors.iter().any(|e| e.contains("stuck")),
            "expected dead-end error for 'stuck', got: {:?}",
            report.errors
        );
    }

    #[test]
    fn validate_final_with_empty_description_warns() {
        let g = graph(
            "initial",
            vec![
                (
                    "initial",
                    state_with_select(vec![("done", "go"), ("alt_done", "alt")]),
                ),
                ("done", state_final_no_description()),
                ("alt_done", state_final()),
            ],
        );
        let report = validate_graph_reachability(&g);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("done") && w.contains("empty description")),
            "expected empty-description warning for 'done', got: {:?}",
            report.warnings
        );
    }

    #[test]
    fn validate_graph_flow_unsupported_version_errors() {
        let mut g = graph(
            "initial",
            vec![
                (
                    "initial",
                    state_with_select(vec![("done", "go"), ("alt_done", "alt")]),
                ),
                ("done", state_final()),
                ("alt_done", state_final()),
            ],
        );
        g.version = Version("2".to_string());
        let err = validate_graph_flow(&g).unwrap_err();
        assert!(
            err.0.contains("unsupported version '2'"),
            "got: {:?}",
            err.0
        );
    }

    #[test]
    fn validate_graph_flow_unknown_initial_errors() {
        let g = graph(
            "missing",
            vec![
                (
                    "initial",
                    state_with_select(vec![("done", "go"), ("alt_done", "alt")]),
                ),
                ("done", state_final()),
                ("alt_done", state_final()),
            ],
        );
        let err = validate_graph_flow(&g).unwrap_err();
        assert!(
            err.0
                .contains("'initial' references unknown state 'missing'"),
            "got: {:?}",
            err.0
        );
    }
}
