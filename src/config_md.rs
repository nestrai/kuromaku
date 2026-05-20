//! Markdown parser for graph flow files (issue #320).
//!
//! Reads `.md` flow files and produces the same [`GraphFlow`] struct that the
//! YAML parser in `config.rs` produces. The format is a subset of Markdown
//! with a YAML frontmatter, H1/H2 headings, italic metadata lines, and
//! `->` transition lines. See `docs/graph-flows.md` for the full spec.
//!
//! The parser is a simple line-by-line scanner -- no `pulldown-cmark` or
//! other Markdown AST library needed. The format is regular enough that a
//! state machine over lines is cleaner than fighting with a cmark tree.
//!
//! # Layering: parser vs validator (issue #325)
//!
//! [`parse_md_flow`] produces an **unvalidated** [`GraphFlow`]. It is
//! responsible for *Markdown syntax* and *data-model structural invariants*
//! only. Workflow semantics (target existence, shell `pass`/`fail`
//! routing rules, duplicate select targets, final-state-with-transitions,
//! reachability, and so on) are the exclusive responsibility of
//! [`validate_graph_flow`] in `config.rs`. The public entry point
//! [`load_graph_flow_from_md`] composes the two -- exactly mirroring the
//! YAML side (`load_graph_flow_from_str`).
//!
//! Two checks in this file may look semantic but are not, and stay here:
//!
//! - The duplicate-state-id check in [`ParseState::flush_state`] is a
//!   *structural* invariant of the underlying `IndexMap<String, GraphState>`.
//!   The validator receives an already-merged map and cannot detect this
//!   after the fact (the YAML side has the same blind spot via serde_yaml's
//!   last-wins behaviour). Removing it would be a silent-overwrite
//!   regression in the MD format.
//! - The frontmatter `format: kuromaku-flow/v1` check is parse-time schema
//!   discrimination, equivalent to the YAML `version: 1` check.
//!
//! Everything else in this module is line-classification: H1, H2, italic
//! metadata whitelist, `->` transitions, `---` separators. If you find
//! yourself adding a check that talks about *what the workflow means*,
//! it belongs in `validate_graph_flow`, not here.

use indexmap::IndexMap;

use crate::config::{ConfigError, validate_graph_state_extra_args};
use crate::core::{GraphFlow, GraphState, SelectEntry, SelectReason, Version, validate_graph_flow};

/// Main entry point: parse a Markdown flow file into a [`GraphFlow`].
///
/// Validates the result through the same `validate_graph_flow()` and
/// `validate_graph_reachability()` that the YAML parser uses.
pub fn load_graph_flow_from_md(contents: &str) -> Result<GraphFlow, ConfigError> {
    let flow = parse_md_flow(contents)?;
    validate_graph_flow(&flow)?;
    // The markdown surface has no syntax for per-state `extra_args:`
    // today, but we run the validator anyway so any future MD-shape
    // extension is checked through the same gate as the YAML loader.
    // No-op while every parsed state has an empty `extra_args` map.
    validate_graph_state_extra_args(&flow)?;
    Ok(flow)
}

// --- Parser internals ---

/// Scanner state machine. Transitions are triggered by line content,
/// not by a lookahead -- each line is classified exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Before the opening `---` of frontmatter.
    BeforeFrontmatter,
    /// Inside frontmatter (between `---` lines).
    Frontmatter,
    /// After frontmatter, before H1.
    FlowHeader,
    /// After H1, collecting flow-level prose.
    FlowPrompt,
    /// Just saw `## heading`, about to collect metadata/body.
    StateMeta,
    /// Collecting body text of a state.
    StateBody,
    /// Collecting `->` transition lines.
    Transitions,
    /// Between states (after `---` or flush). Only `##` and `---` allowed.
    BetweenStates,
}

struct ParseState {
    phase: Phase,
    frontmatter_lines: Vec<String>,
    flow_name: Option<String>,
    flow_prompt_lines: Vec<String>,

    // Current state being built
    current_state_id: Option<String>,
    current_role: Option<String>,
    current_run: Option<String>,
    current_final: Option<String>,
    current_body_lines: Vec<String>,
    current_transitions: Vec<SelectEntry>,

    // Accumulated states (in declaration order)
    states: IndexMap<String, GraphState>,
    first_state_id: Option<String>,
}

impl ParseState {
    fn new() -> Self {
        Self {
            phase: Phase::BeforeFrontmatter,
            frontmatter_lines: Vec::new(),
            flow_name: None,
            flow_prompt_lines: Vec::new(),
            current_state_id: None,
            current_role: None,
            current_run: None,
            current_final: None,
            current_body_lines: Vec::new(),
            current_transitions: Vec::new(),
            states: IndexMap::new(),
            first_state_id: None,
        }
    }

    /// Flush the current state (if any) into the states map.
    fn flush_state(&mut self) -> Result<(), ConfigError> {
        let Some(id) = self.current_state_id.take() else {
            return Ok(());
        };

        let body = trim_block(&self.current_body_lines);
        let transitions = if self.current_transitions.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.current_transitions))
        };

        let state = GraphState {
            role: self.current_role.take(),
            task: if body.is_empty() { None } else { Some(body) },
            task_file: None,
            run: self.current_run.take(),
            final_desc: self.current_final.take(),
            human: None,
            select: transitions,
            extra_args: std::collections::HashMap::new(),
        };

        if self.states.contains_key(&id) {
            return Err(ConfigError::Validation(format!(
                "duplicate state '## {id}' in markdown flow"
            )));
        }

        if self.first_state_id.is_none() {
            self.first_state_id = Some(id.clone());
        }
        self.states.insert(id, state);
        self.current_body_lines.clear();
        Ok(())
    }
}

/// Parse the markdown into a [`GraphFlow`]. Does NOT run workflow-semantic
/// validation (the caller does that via [`validate_graph_flow`]).
///
/// Visibility is `pub(crate)` so unit tests in this module can drive the
/// parser in isolation from the validator and pin the layering boundary
/// (see issue #325). Callers outside the crate should use
/// [`load_graph_flow_from_md`].
pub(crate) fn parse_md_flow(contents: &str) -> Result<GraphFlow, ConfigError> {
    let mut st = ParseState::new();

    for (line_num, line) in contents.lines().enumerate() {
        let ln = line_num + 1; // 1-based for error messages
        parse_line(&mut st, line, ln)?;
    }

    // Flush trailing state
    st.flush_state()?;

    // Validate frontmatter
    let format_value = parse_frontmatter(&st.frontmatter_lines)?;
    if format_value != "kuromaku-flow/v1" {
        return Err(ConfigError::Validation(format!(
            "frontmatter 'format' must be 'kuromaku-flow/v1', got '{format_value}'"
        )));
    }

    let name = st
        .flow_name
        .ok_or_else(|| ConfigError::Validation("missing H1 heading (flow name)".to_string()))?;

    let initial = st.first_state_id.ok_or_else(|| {
        ConfigError::Validation("no states defined (need at least one ## heading)".to_string())
    })?;

    let prompt = {
        let p = trim_block(&st.flow_prompt_lines);
        if p.is_empty() { None } else { Some(p) }
    };

    Ok(GraphFlow {
        version: Version("1".to_string()),
        name,
        prompt,
        prompt_file: None,
        initial,
        graph: st.states,
    })
}

fn parse_line(st: &mut ParseState, line: &str, ln: usize) -> Result<(), ConfigError> {
    match st.phase {
        Phase::BeforeFrontmatter => {
            if line.trim() == "---" {
                st.phase = Phase::Frontmatter;
            } else if !line.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "line {ln}: expected YAML frontmatter (---), got: {line}"
                )));
            }
        }

        Phase::Frontmatter => {
            if line.trim() == "---" {
                st.phase = Phase::FlowHeader;
            } else {
                st.frontmatter_lines.push(line.to_string());
            }
        }

        Phase::FlowHeader => {
            if let Some(name) = line.strip_prefix("# ") {
                st.flow_name = Some(name.trim().to_lowercase());
                st.phase = Phase::FlowPrompt;
            } else if line.trim() == "---" || line.trim().is_empty() {
                // Skip decorative separators and blank lines before H1
            } else {
                return Err(ConfigError::Validation(format!(
                    "line {ln}: expected H1 heading (# flow-name), got: {line}"
                )));
            }
        }

        Phase::FlowPrompt => {
            if line.starts_with("## ") {
                start_state(st, line, ln)?;
            } else if line.trim() == "---" {
                // Decorative separator between flow prompt and first state
            } else {
                st.flow_prompt_lines.push(line.to_string());
            }
        }

        Phase::StateMeta => {
            if line.trim().is_empty() {
                // Blank line after ## heading, stay in meta
            } else if is_italic_meta(line) {
                parse_meta_line(st, line, ln)?;
            } else if line.starts_with("-> ") || line.starts_with("->") {
                // No body, straight to transitions
                st.phase = Phase::Transitions;
                parse_transition(st, line, ln)?;
            } else if line.starts_with("## ") {
                // Empty state (e.g. final with no body)
                st.flush_state()?;
                start_state(st, line, ln)?;
            } else if line.trim() == "---" {
                // Decorative separator
            } else {
                // First body line
                st.phase = Phase::StateBody;
                st.current_body_lines.push(line.to_string());
            }
        }

        Phase::StateBody => {
            if line.starts_with("-> ") || line.starts_with("->") {
                st.phase = Phase::Transitions;
                parse_transition(st, line, ln)?;
            } else if line.starts_with("## ") {
                st.flush_state()?;
                start_state(st, line, ln)?;
            } else if line.trim() == "---" {
                st.flush_state()?;
                st.phase = Phase::BetweenStates;
            } else if line.trim() == "### Next" || line.trim() == "### next" {
                // Cosmetic heading before transitions -- skip it
            } else {
                st.current_body_lines.push(line.to_string());
            }
        }

        Phase::Transitions => {
            if line.starts_with("-> ") || line.starts_with("->") {
                parse_transition(st, line, ln)?;
            } else if line.starts_with("## ") {
                st.flush_state()?;
                start_state(st, line, ln)?;
            } else if line.trim() == "---" {
                st.flush_state()?;
                st.phase = Phase::BetweenStates;
            } else if line.trim().is_empty() {
                // Blank line after transitions, ignore
            } else {
                return Err(ConfigError::Validation(format!(
                    "line {ln}: unexpected text after transitions, expected `->`, `##`, or `---`: {line}"
                )));
            }
        }

        Phase::BetweenStates => {
            if line.starts_with("## ") {
                start_state(st, line, ln)?;
            } else if line.trim() == "---" || line.trim().is_empty() {
                // Decorative separators and blank lines between states
            } else {
                return Err(ConfigError::Validation(format!(
                    "line {ln}: unexpected text between states, expected `##` or `---`: {line}"
                )));
            }
        }
    }

    Ok(())
}

fn start_state(st: &mut ParseState, line: &str, _ln: usize) -> Result<(), ConfigError> {
    let heading = line.strip_prefix("## ").unwrap_or(line);
    let id = heading.trim().to_lowercase();
    st.current_state_id = Some(id);
    st.current_role = None;
    st.current_run = None;
    st.current_final = None;
    st.current_body_lines.clear();
    st.current_transitions.clear();
    st.phase = Phase::StateMeta;
    Ok(())
}

/// Check if a line is an italic metadata line: `*key: value*`
/// Only the three known keys are recognized to avoid matching
/// stray italic prose like `*remember: do not delete files*`.
fn is_italic_meta(line: &str) -> bool {
    let trimmed = line.trim();
    if !(trimmed.starts_with('*') && trimmed.ends_with('*')) {
        return false;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    ["role:", "run:", "final:"]
        .iter()
        .any(|k| inner.starts_with(k))
}

/// Parse `*key: value*` metadata line.
fn parse_meta_line(st: &mut ParseState, line: &str, ln: usize) -> Result<(), ConfigError> {
    let trimmed = line.trim();
    // Strip leading and trailing *
    let inner = &trimmed[1..trimmed.len() - 1];
    let (key, value) = inner.split_once(':').ok_or_else(|| {
        ConfigError::Validation(format!("line {ln}: invalid metadata line: {line}"))
    })?;
    let key = key.trim().to_lowercase();
    let value = value.trim().to_string();

    match key.as_str() {
        "role" => st.current_role = Some(value),
        "run" => st.current_run = Some(value),
        "final" => st.current_final = Some(value),
        _ => {
            return Err(ConfigError::Validation(format!(
                "line {ln}: unknown metadata key '{key}' (valid: role, run, final)"
            )));
        }
    }

    Ok(())
}

/// Parse a `-> target: reason` transition line.
///
/// Also handles optional link syntax: `-> [target](#anchor): reason`
fn parse_transition(st: &mut ParseState, line: &str, ln: usize) -> Result<(), ConfigError> {
    let rest = line.strip_prefix("->").unwrap_or(line).trim();
    if rest.is_empty() {
        return Err(ConfigError::Validation(format!(
            "line {ln}: empty transition (expected -> target: reason)"
        )));
    }

    // Strip optional link syntax: [target](#anchor) -> target
    let rest = strip_link_syntax(rest);

    // Split on first `:` to get target and reason
    let (target, reason) = if let Some((t, r)) = rest.split_once(':') {
        (t.trim().to_lowercase(), Some(r.trim().to_string()))
    } else {
        (rest.trim().to_lowercase(), None)
    };

    if target.is_empty() {
        return Err(ConfigError::Validation(format!(
            "line {ln}: empty transition target"
        )));
    }

    let reason = reason.and_then(|r| {
        if r.is_empty() {
            None
        } else {
            Some(SelectReason::Single(r))
        }
    });

    st.current_transitions.push(SelectEntry { target, reason });
    Ok(())
}

/// Strip Markdown link syntax from a transition target.
///
/// `[target](#anchor)` -> `target`
/// `[target](#anchor): reason` -> `target: reason` (colon is outside the link)
/// `target: reason` -> unchanged
fn strip_link_syntax(s: &str) -> String {
    if !s.starts_with('[') {
        return s.to_string();
    }

    // Find the closing ] and the (#...) part
    if let Some(bracket_end) = s.find(']') {
        let target = &s[1..bracket_end];
        let after_bracket = &s[bracket_end + 1..];

        // Skip optional (#anchor) part
        let rest = if after_bracket.starts_with('(') {
            if let Some(paren_end) = after_bracket.find(')') {
                &after_bracket[paren_end + 1..]
            } else {
                after_bracket
            }
        } else {
            after_bracket
        };

        format!("{target}{rest}")
    } else {
        s.to_string()
    }
}

/// Parse the frontmatter lines and extract the `format` field.
fn parse_frontmatter(lines: &[String]) -> Result<String, ConfigError> {
    for line in lines {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("format:") {
            return Ok(value.trim().to_string());
        }
    }
    Err(ConfigError::Validation(
        "frontmatter missing required 'format' field (expected: format: kuromaku-flow/v1)"
            .to_string(),
    ))
}

/// Trim leading/trailing blank lines from a block of text,
/// then join with newlines and trim trailing whitespace.
fn trim_block(lines: &[String]) -> String {
    let start = lines.iter().position(|l| !l.trim().is_empty());
    let end = lines.iter().rposition(|l| !l.trim().is_empty());
    match (start, end) {
        (Some(s), Some(e)) => lines[s..=e].join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::validate_graph_reachability;

    #[test]
    fn parse_minimal_md_flow() {
        let md = r#"---
format: kuromaku-flow/v1
---

# minimal

---

## start
*role: developer*

Do the thing.

-> done: finished
-> start: retry needed

---

## done
*final: the flow is complete*
"#;

        let flow = load_graph_flow_from_md(md).unwrap();
        assert_eq!(flow.name, "minimal");
        assert_eq!(flow.initial, "start");
        assert_eq!(flow.graph.len(), 2);

        let start = &flow.graph["start"];
        assert_eq!(start.role.as_deref(), Some("developer"));
        assert_eq!(start.task.as_deref(), Some("Do the thing."));
        assert_eq!(start.select.as_ref().unwrap().len(), 2);
        assert_eq!(start.select.as_ref().unwrap()[0].target, "done");

        let done = &flow.graph["done"];
        assert!(done.is_final());
        assert_eq!(done.final_desc.as_deref(), Some("the flow is complete"));
    }

    #[test]
    fn parse_full_implement_issue() {
        let md = include_str!("../seeds/github/flows/implement-issue.md");
        let flow = load_graph_flow_from_md(md).unwrap();

        assert_eq!(flow.name, "implement-issue");
        assert_eq!(flow.initial, "design");
        assert_eq!(flow.graph.len(), 7);

        // Check design state
        let design = &flow.graph["design"];
        assert_eq!(design.role.as_deref(), Some("architect"));
        assert!(design.task.as_ref().unwrap().contains("Read issue"));
        let design_targets: Vec<&str> = design
            .select
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| e.target.as_str())
            .collect();
        assert_eq!(design_targets, vec!["implement", "aborted"]);

        // Check verify shell state
        let verify = &flow.graph["verify"];
        assert!(verify.is_shell());
        assert_eq!(verify.run.as_deref(), Some("just lint && just test"));
        let verify_targets: Vec<&str> = verify
            .select
            .as_ref()
            .unwrap()
            .iter()
            .map(|e| e.target.as_str())
            .collect();
        assert_eq!(verify_targets, vec!["create-pr", "implement"]);

        // Check final states
        assert!(flow.graph["done"].is_final());
        assert!(flow.graph["aborted"].is_final());

        // Validate reachability
        let report = validate_graph_reachability(&flow);
        assert!(
            report.is_ok(),
            "implement-issue.md must pass reachability: errors={:?}",
            report.errors
        );
    }

    #[test]
    fn parse_link_syntax_stripped() {
        let md = r#"---
format: kuromaku-flow/v1
---

# link-test

---

## start
*role: developer*

Do something.

-> [done](#done): all good
-> [start](#start): retry

---

## done
*final: finished*
"#;

        let flow = load_graph_flow_from_md(md).unwrap();
        let start = &flow.graph["start"];
        let entries = start.select.as_ref().unwrap();
        assert_eq!(entries[0].target, "done");
        assert_eq!(
            entries[0].reason,
            Some(SelectReason::Single("all good".to_string()))
        );
        assert_eq!(entries[1].target, "start");
        assert_eq!(
            entries[1].reason,
            Some(SelectReason::Single("retry".to_string()))
        );
    }

    #[test]
    fn parse_optional_next_heading_ignored() {
        let md = r#"---
format: kuromaku-flow/v1
---

# next-heading

---

## start
*role: developer*

Do something.

### Next

-> done: finished
-> start: retry

---

## done
*final: end*
"#;

        let flow = load_graph_flow_from_md(md).unwrap();
        let start = &flow.graph["start"];
        // ### Next is cosmetic and stripped from the body
        assert!(!start.task.as_ref().unwrap().contains("### Next"));
        // Transitions still parse correctly
        let entries = start.select.as_ref().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].target, "done");
    }

    #[test]
    fn parse_shell_state_pass_fail() {
        let md = r#"---
format: kuromaku-flow/v1
---

# shell-test

---

## start
*role: developer*

Do the thing.

-> check: ready
-> start: retry

---

## check
*run: make test*

-> done: pass
-> start: fail

---

## done
*final: tests passed*
"#;

        let flow = load_graph_flow_from_md(md).unwrap();
        let check = &flow.graph["check"];
        assert!(check.is_shell());
        assert_eq!(check.run.as_deref(), Some("make test"));
        let entries = check.select.as_ref().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].reason,
            Some(SelectReason::Single("pass".to_string()))
        );
        assert_eq!(
            entries[1].reason,
            Some(SelectReason::Single("fail".to_string()))
        );
    }

    #[test]
    fn parse_final_state() {
        let md = r#"---
format: kuromaku-flow/v1
---

# final-test

---

## start
*role: developer*

Go.

-> done: ok
-> start: retry

---

## done
*final: all work is complete*
"#;

        let flow = load_graph_flow_from_md(md).unwrap();
        let done = &flow.graph["done"];
        assert!(done.is_final());
        assert_eq!(done.final_desc.as_deref(), Some("all work is complete"));
        assert!(done.select.is_none());
    }

    #[test]
    fn parse_frontmatter_required() {
        let md = r#"# no-frontmatter

## start
*role: developer*

Go.

-> done: ok

## done
*final: end*
"#;

        let err = load_graph_flow_from_md(md).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("frontmatter") || msg.contains("---"),
            "error must mention frontmatter: {msg}"
        );
    }

    #[test]
    fn parse_body_with_bullets_not_confused() {
        let md = r#"---
format: kuromaku-flow/v1
---

# bullet-test

---

## start
*role: developer*

Do the following:

- Step one
- Step two
- Step three

Also check:

1. First
2. Second

```bash
echo "hello"
```

-> done: finished
-> start: retry

---

## done
*final: end*
"#;

        let flow = load_graph_flow_from_md(md).unwrap();
        let start = &flow.graph["start"];
        let task = start.task.as_ref().unwrap();
        assert!(task.contains("- Step one"));
        assert!(task.contains("1. First"));
        assert!(task.contains("```bash"));
        // Transitions still parse
        assert_eq!(start.select.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn reject_final_state_with_transitions() {
        // This should be caught by validate_graph_flow, which rejects
        // final states with next: entries.
        let md = r#"---
format: kuromaku-flow/v1
---

# bad-final

---

## start
*role: developer*

Go.

-> done: ok
-> start: retry

---

## done
*final: end*

-> start: oops
"#;

        let err = load_graph_flow_from_md(md).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("done") || msg.contains("final"),
            "error must reference the final state: {msg}"
        );
    }

    #[test]
    fn reject_shell_state_wrong_reasons() {
        // Shell states need exactly pass/fail reasons
        let md = r#"---
format: kuromaku-flow/v1
---

# bad-shell

---

## check
*run: make test*

-> done: success
-> check: failure

---

## done
*final: end*
"#;

        let err = load_graph_flow_from_md(md).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("pass") || msg.contains("fail"),
            "error must mention pass/fail: {msg}"
        );
    }

    // --- Boundary-lock tests (issue #325) ---
    //
    // These tests pin the layering between `parse_md_flow` (Markdown syntax
    // only) and `validate_graph_flow` (workflow semantics). For each
    // semantic rule, the parser must accept the input and the validator
    // must reject it. If the parser ever starts to reject one of these
    // syntactically-clean cases, semantic logic has leaked into the parser
    // and the contract has been broken.

    #[test]
    fn parser_accepts_unknown_target_validator_rejects() {
        let md = r#"---
format: kuromaku-flow/v1
---

# unknown-target

---

## start
*role: developer*

Go.

-> nowhere: this target does not exist
-> start: retry

---

## done
*final: end*
"#;

        // Parser: structurally fine, no unknown-state knowledge.
        let flow = parse_md_flow(md).expect("parser must accept unknown transition target");
        let entries = flow.graph["start"].select.as_ref().unwrap();
        assert_eq!(entries[0].target, "nowhere");

        // Validator: rejects with an "unknown state" message naming the target.
        let err = validate_graph_flow(&flow).expect_err("validator must reject unknown target");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown state") && msg.contains("nowhere"),
            "validator error must mention 'unknown state' and 'nowhere': {msg}"
        );
    }

    #[test]
    fn parser_accepts_duplicate_target_validator_rejects() {
        let md = r#"---
format: kuromaku-flow/v1
---

# duplicate-target

---

## start
*role: developer*

Go.

-> done: first reason
-> done: second reason
-> start: retry

---

## done
*final: end*
"#;

        // Parser: keeps both entries verbatim, no dedup.
        let flow = parse_md_flow(md).expect("parser must accept duplicate transition targets");
        let entries = flow.graph["start"].select.as_ref().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].target, "done");
        assert_eq!(entries[1].target, "done");

        // Validator: rejects with a "more than once" message.
        let err =
            validate_graph_flow(&flow).expect_err("validator must reject duplicate select target");
        let msg = err.to_string();
        assert!(
            msg.contains("more than once"),
            "validator error must mention 'more than once': {msg}"
        );
    }

    #[test]
    fn parser_accepts_shell_wrong_reasons_validator_rejects() {
        let md = r#"---
format: kuromaku-flow/v1
---

# shell-wrong-reasons

---

## check
*run: make test*

-> done: success
-> retry: failure

---

## retry
*role: developer*

Try again.

-> check: ready
-> retry: try once more

---

## done
*final: end*
"#;

        // Parser: shell-state pass/fail rules are not its concern.
        let flow = parse_md_flow(md).expect("parser must accept arbitrary shell-state reasons");
        let check = &flow.graph["check"];
        assert!(check.is_shell());
        let entries = check.select.as_ref().unwrap();
        assert_eq!(
            entries[0].reason,
            Some(SelectReason::Single("success".to_string()))
        );
        assert_eq!(
            entries[1].reason,
            Some(SelectReason::Single("failure".to_string()))
        );

        // Validator: rejects because shell states must use 'pass' / 'fail'.
        let err = validate_graph_flow(&flow)
            .expect_err("validator must reject shell state with non-pass/fail reasons");
        let msg = err.to_string();
        assert!(
            msg.contains("pass") || msg.contains("fail"),
            "validator error must mention pass/fail: {msg}"
        );
    }
}
