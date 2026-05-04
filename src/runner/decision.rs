//! Agent-decision parser for graph flows.
//!
//! In a graph flow, each non-final state asks the agent to pick one of the
//! state's named edges. The agent answers with a JSON object of the form
//!
//! ```json
//! { "transition": "approved", "reason": "All checks pass." }
//! ```
//!
//! LLMs frequently wrap their last output in a fenced code block or surround
//! it with prose. This module recovers the JSON object from that noise,
//! deserialises it into a typed [`AgentDecision`], and validates the chosen
//! transition against the closed set of edges the runtime exposes.
//!
//! Retry on malformed output, fallback to human-handoff, and prompt
//! construction are explicitly *not* this module's concern -- those live in
//! the graph-flow driver (issue #240).

use serde::Deserialize;
use thiserror::Error;

/// Agent-supplied decision for a graph-flow state.
///
/// Unknown fields (e.g. forthcoming `confidence`, `artifacts`) are silently
/// ignored so future schema additions do not break older runs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AgentDecision {
    /// Edge name the agent picked. Must be one of the current state's edges.
    pub transition: String,
    /// Free-text justification. Surfaced to the user and persisted to the
    /// run record so a later reviewer can see *why* the agent routed this way.
    pub reason: String,
}

/// Errors produced while turning raw agent output into an [`AgentDecision`].
#[derive(Debug, Error)]
pub enum DecisionParseError {
    /// No balanced `{...}` object was found anywhere in the input.
    #[error("no JSON object found in agent output")]
    NoJsonObject,

    /// A `{...}` block was located but failed to deserialise into the
    /// expected schema (malformed JSON, missing field, wrong type).
    #[error("failed to decode agent decision: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

/// Errors produced while validating a parsed decision against the allowed
/// edge set of the current state.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecisionError {
    /// The agent picked a transition the current state does not expose.
    /// Both the offending value and the closed allowed set are included so
    /// the driver can surface them or feed them back into a re-prompt.
    #[error("transition {value:?} is not in allowed set {allowed:?}")]
    UnknownTransition { value: String, allowed: Vec<String> },
}

/// Parse the agent's last raw text into an [`AgentDecision`].
///
/// Accepts:
/// - a bare JSON object,
/// - a JSON object inside a fenced code block (```json ... ``` or ``` ... ```),
/// - a JSON object embedded in surrounding prose,
/// - prose with arbitrary `{...}` snippets (e.g. Rust code blocks) preceding
///   the routing JSON object.
///
/// The graph-flow prompt contract instructs the agent to put the routing JSON
/// "exactly once, on its own line at the end of your reply". To honour that
/// contract robustly we enumerate every top-level balanced `{...}` block in
/// the input and pick the **last** one that successfully deserialises into
/// [`AgentDecision`]. Earlier `{...}` snippets (Rust enums, code samples,
/// example payloads) are skipped.
///
/// JSON string contents are honoured, so a `}` inside a string literal does
/// not close the object.
pub fn parse_agent_decision(raw: &str) -> Result<AgentDecision, DecisionParseError> {
    let candidates = top_level_json_objects(raw);
    if candidates.is_empty() {
        return Err(DecisionParseError::NoJsonObject);
    }

    // Walk from end to start. The contract says the routing JSON is the
    // final object; earlier `{...}` blocks are prose noise.
    let mut last_err: Option<serde_json::Error> = None;
    for slice in candidates.iter().rev() {
        match serde_json::from_str::<AgentDecision>(slice) {
            Ok(decision) => return Ok(decision),
            Err(err) => last_err = Some(err),
        }
    }

    // No candidate deserialised. Surface the error from the last (most
    // likely intended) candidate so the message points at the right block.
    Err(DecisionParseError::InvalidJson(last_err.expect(
        "candidates non-empty implies at least one deserialise attempt",
    )))
}

/// Validate that the agent's chosen transition is in the closed set of edges
/// the runtime exposed for the current state.
pub fn validate_transition(
    decision: &AgentDecision,
    allowed: &[&str],
) -> Result<(), DecisionError> {
    if allowed.iter().any(|edge| *edge == decision.transition) {
        Ok(())
    } else {
        Err(DecisionError::UnknownTransition {
            value: decision.transition.clone(),
            allowed: allowed.iter().map(|s| (*s).to_string()).collect(),
        })
    }
}

/// Collect every top-level balanced `{...}` substring in `raw` in order of
/// appearance.
///
/// Walks the input byte-wise, opens on each top-level `{`, then tracks brace
/// depth until it returns to zero. While inside a JSON string literal
/// (`"..."`) braces are ignored; `\\` and `\"` escape sequences are skipped
/// so the closing quote is detected correctly. Nested objects are *not*
/// returned separately -- only the outermost balanced block is collected.
///
/// An unterminated `{` at the end of input is silently dropped (no half-open
/// candidate is returned). This matches the previous `first_json_object`
/// semantics where unbalanced braces produced `None`.
fn top_level_json_objects(raw: &str) -> Vec<&str> {
    let bytes = raw.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let mut start: Option<usize> = None;
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, &byte) in bytes.iter().enumerate() {
        if start.is_none() {
            if byte == b'{' {
                start = Some(idx);
                depth = 1;
            }
            continue;
        }

        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let s = start.take().unwrap();
                    out.push(&raw[s..=idx]);
                }
            }
            _ => {}
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_json() {
        let raw = r#"{"transition": "approved", "reason": "All acceptance criteria are checked off and tests pass."}"#;
        let decision = parse_agent_decision(raw).expect("bare JSON parses");
        assert_eq!(decision.transition, "approved");
        assert_eq!(
            decision.reason,
            "All acceptance criteria are checked off and tests pass."
        );
    }

    #[test]
    fn parses_fenced_json() {
        let raw = "```json\n{\"transition\": \"approved\", \"reason\": \"ok\"}\n```";
        let decision = parse_agent_decision(raw).expect("fenced JSON parses");
        assert_eq!(decision.transition, "approved");
        assert_eq!(decision.reason, "ok");
    }

    #[test]
    fn parses_unlabeled_fenced_json() {
        let raw = "```\n{\"transition\": \"x\", \"reason\": \"y\"}\n```";
        let decision = parse_agent_decision(raw).expect("unlabeled fenced JSON parses");
        assert_eq!(decision.transition, "x");
        assert_eq!(decision.reason, "y");
    }

    #[test]
    fn parses_json_with_prose_around_it() {
        let raw = r#"Some thoughts here. {"transition": "x", "reason": "y"} done."#;
        let decision = parse_agent_decision(raw).expect("prose-wrapped JSON parses");
        assert_eq!(decision.transition, "x");
        assert_eq!(decision.reason, "y");
    }

    #[test]
    fn parses_with_leading_and_trailing_whitespace() {
        let raw = "\n\n  {\"transition\": \"x\", \"reason\": \"y\"}  \n";
        let decision = parse_agent_decision(raw).expect("whitespace-padded JSON parses");
        assert_eq!(decision.transition, "x");
    }

    #[test]
    fn ignores_unknown_fields() {
        let raw = r#"{"transition": "x", "reason": "y", "confidence": 0.9, "artifacts": {"a": 1}}"#;
        let decision = parse_agent_decision(raw).expect("unknown fields are ignored");
        assert_eq!(decision.transition, "x");
        assert_eq!(decision.reason, "y");
    }

    #[test]
    fn handles_braces_inside_string_literals() {
        // The closing `}` inside the reason string must not terminate the
        // JSON object early.
        let raw = r#"{"transition": "x", "reason": "contains } brace"}"#;
        let decision = parse_agent_decision(raw).expect("braces in strings handled");
        assert_eq!(decision.reason, "contains } brace");
    }

    #[test]
    fn handles_escaped_quote_in_string() {
        // A `\"` inside the string must not close the string early, so the
        // following `}` stays inside the literal.
        let raw = r#"{"transition": "x", "reason": "he said \"hi\" }"}"#;
        let decision = parse_agent_decision(raw).expect("escaped quotes handled");
        assert_eq!(decision.reason, r#"he said "hi" }"#);
    }

    #[test]
    fn picks_routing_json_after_rust_code_snippet() {
        // Real-world failure mode: the design-step prompt asks the agent to
        // write a full design plan AND end the reply with a routing JSON
        // object. Plans frequently contain Rust code snippets like
        // `enum StepKindData { Llm }`. The parser must skip those and pick
        // the routing JSON at the end.
        let raw = "Here is the plan:\n\n\
                   ```rust\n\
                   enum StepKindData { Llm, Shell }\n\
                   struct Step { id: String }\n\
                   ```\n\n\
                   And the routing decision:\n\
                   {\"transition\": \"approved\", \"reason\": \"plan looks solid\"}\n";
        let decision = parse_agent_decision(raw).expect("routing JSON after code parses");
        assert_eq!(decision.transition, "approved");
        assert_eq!(decision.reason, "plan looks solid");
    }

    #[test]
    fn picks_last_decision_when_multiple_routing_jsons_present() {
        // If the agent quotes an example payload earlier in its reply and
        // then emits the real decision at the end, the last one wins.
        let raw = "Example: {\"transition\": \"rejected\", \"reason\": \"example\"}\n\
                   Final: {\"transition\": \"approved\", \"reason\": \"all good\"}";
        let decision = parse_agent_decision(raw).expect("last decision wins");
        assert_eq!(decision.transition, "approved");
        assert_eq!(decision.reason, "all good");
    }

    #[test]
    fn surfaces_decode_error_from_last_candidate_when_all_fail() {
        // If no candidate deserialises, the error must come from the last
        // candidate (the one the agent most likely intended as the
        // decision) so log lines point at the right block.
        let raw = "{\"unrelated\": 1} {\"transition\": 42, \"reason\": \"bad type\"}";
        let err = parse_agent_decision(raw).expect_err("all candidates fail");
        let DecisionParseError::InvalidJson(json_err) = err else {
            panic!("expected InvalidJson, got {err:?}");
        };
        // The error must reference the wrong-type field, not the unrelated
        // earlier object.
        let msg = json_err.to_string();
        assert!(
            msg.contains("transition") || msg.contains("string"),
            "error must point at the routing JSON candidate; got: {msg}"
        );
    }

    #[test]
    fn returns_error_when_reason_missing() {
        let raw = r#"{"transition": "x"}"#;
        let err = parse_agent_decision(raw).expect_err("missing reason must error");
        assert!(matches!(err, DecisionParseError::InvalidJson(_)));
    }

    #[test]
    fn returns_error_when_transition_missing() {
        let raw = r#"{"reason": "y"}"#;
        let err = parse_agent_decision(raw).expect_err("missing transition must error");
        assert!(matches!(err, DecisionParseError::InvalidJson(_)));
    }

    #[test]
    fn returns_error_when_field_has_wrong_type() {
        let raw = r#"{"transition": 42, "reason": "y"}"#;
        let err = parse_agent_decision(raw).expect_err("wrong-type field must error");
        assert!(matches!(err, DecisionParseError::InvalidJson(_)));
    }

    #[test]
    fn returns_error_on_non_json_input() {
        let raw = "not json at all";
        let err = parse_agent_decision(raw).expect_err("non-JSON input must error");
        assert!(matches!(err, DecisionParseError::NoJsonObject));
    }

    #[test]
    fn returns_error_on_unbalanced_braces() {
        // Opening brace with no matching close -- scanner returns None.
        let raw = r#"{"transition": "x", "reason": "y""#;
        let err = parse_agent_decision(raw).expect_err("unbalanced JSON must error");
        assert!(matches!(err, DecisionParseError::NoJsonObject));
    }

    #[test]
    fn validate_transition_accepts_member_of_allowed_set() {
        let decision = AgentDecision {
            transition: "approved".into(),
            reason: "ok".into(),
        };
        validate_transition(&decision, &["approved", "rejected"]).expect("approved is allowed");
    }

    #[test]
    fn validate_transition_rejects_unknown_with_value_and_list() {
        let decision = AgentDecision {
            transition: "totally_made_up".into(),
            reason: "...".into(),
        };
        let err = validate_transition(&decision, &["approved", "rejected"])
            .expect_err("unknown transition must error");

        let DecisionError::UnknownTransition { value, allowed } = err;
        assert_eq!(value, "totally_made_up");
        assert_eq!(
            allowed,
            vec!["approved".to_string(), "rejected".to_string()]
        );

        // The Display impl must surface both the offending value and the
        // allowed list so log lines are self-explanatory.
        let rendered = format!(
            "{}",
            DecisionError::UnknownTransition {
                value: "totally_made_up".into(),
                allowed: vec!["approved".into(), "rejected".into()],
            }
        );
        assert!(rendered.contains("totally_made_up"));
        assert!(rendered.contains("approved"));
        assert!(rendered.contains("rejected"));
    }

    #[test]
    fn validate_transition_rejects_against_empty_allowed_set() {
        let decision = AgentDecision {
            transition: "anything".into(),
            reason: "...".into(),
        };
        let err = validate_transition(&decision, &[])
            .expect_err("empty allowed set must reject everything");

        let DecisionError::UnknownTransition { value, allowed } = err;
        assert_eq!(value, "anything");
        assert!(allowed.is_empty());
    }
}
