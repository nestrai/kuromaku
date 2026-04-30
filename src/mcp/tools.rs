//! Tool registry and the [`Tool`] trait subsequent subtasks plug into.
//!
//! The scaffold ships an empty registry so `tools/list` returns `[]` and
//! `tools/call` returns `unknown_tool`. Tools land in follow-up issues:
//!
//! - workflow tools (#196): `implement_issue`, `review_pr`, `rework_pr`
//! - discovery tools (#197): `list_agents`, `list_flows`, `load_agent`
//! - execution tools (#198): `run_flow`, `show_output`
//! - human injection (#199): `send_message`
//!
//! ## Schema evolution rule
//!
//! Once a tool ships, its `input_schema` may add **optional** parameters but
//! must never break existing ones (no rename, no required-by-default flips,
//! no type changes). New required fields land as new tools. This rule is
//! enforced socially in code review -- it is documented here so reviewers
//! have a single line to reference.
//!
//! ## Naming rule
//!
//! Tool names are `verb_noun`, snake_case, ASCII. The registry rejects
//! names that violate this at registration time -- catching the mistake in
//! tests rather than after a tool ships.
//!
//! ## Session scope (forward-looking)
//!
//! `send_message` and `run_flow` will need per-session state so each MCP
//! connection sees only the runs it started (team review, #195). The
//! registry is currently stateless; the slot for session state will land
//! with the first tool that needs it (#198 / #199).

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::Value;

use super::error::{McpError, McpErrorCode};
use super::protocol::ToolDescriptor;

/// Pluggable tool. Each implementation is a small struct with no shared
/// state -- everything it needs (config seeds, runner handles) is loaded
/// per-call from the documented `pub` API of `runner`, `resolver`, `config`,
/// `messaging::router` and `stack`. See module docs for the dependency rule.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable tool name. Must be `verb_noun` snake_case ASCII, validated by
    /// [`ToolRegistry::register`].
    fn name(&self) -> &'static str;

    /// What the tool does, when NOT to call it, and one example invocation.
    /// MCP clients surface this verbatim to their users -- write it for a
    /// human, not for a parser. Empty descriptions are rejected at
    /// registration.
    fn description(&self) -> &'static str;

    /// JSON Schema (draft 7) for `arguments` on `tools/call`. Returning an
    /// `object` schema with a `properties` map is the only shape MCP
    /// clients render reliably.
    fn input_schema(&self) -> Value;

    /// Execute. `arguments` is the raw JSON the client sent -- validate
    /// against `input_schema()` before proceeding. Errors must use the
    /// stable [`McpErrorCode`] catalog so clients can route on them.
    async fn call(&self, arguments: Value) -> Result<Value, McpError>;
}

/// Registry: name -> boxed tool. Backed by a `BTreeMap` so `tools/list`
/// is deterministically sorted -- helps reproducibility for clients that
/// snapshot capabilities.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. Validates the name shape and rejects duplicates and
    /// empty descriptions before insertion -- errors surface in the test
    /// build, never in production.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> Result<(), McpError> {
        let name = tool.name();
        if !is_valid_tool_name(name) {
            return Err(McpError::with_details(
                McpErrorCode::InvalidToolName,
                serde_json::json!({"name": name}),
            ));
        }
        if tool.description().trim().is_empty() {
            return Err(McpError::with_details(
                McpErrorCode::InvalidToolName,
                serde_json::json!({"name": name, "reason": "empty description"}),
            ));
        }
        if self.tools.contains_key(name) {
            return Err(McpError::with_details(
                McpErrorCode::InvalidToolName,
                serde_json::json!({"name": name, "reason": "duplicate"}),
            ));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools
            .values()
            .map(|t| ToolDescriptor {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }
}

/// `verb_noun` snake_case ASCII: lowercase letter or digit, separated by
/// single underscores, contains at least one underscore (the verb/noun
/// separator). Rejects leading/trailing underscores, double underscores,
/// uppercase, and non-ASCII.
pub fn is_valid_tool_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes[0] == b'_' || bytes[bytes.len() - 1] == b'_' {
        return false;
    }
    let mut prev_underscore = false;
    let mut has_underscore = false;
    for &b in bytes {
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_';
        if !ok {
            return false;
        }
        if b == b'_' {
            if prev_underscore {
                return false;
            }
            prev_underscore = true;
            has_underscore = true;
        } else {
            prev_underscore = false;
        }
    }
    has_underscore
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            self.description
        }
        fn input_schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _arguments: Value) -> Result<Value, McpError> {
            Ok(serde_json::json!({"ok": true}))
        }
    }

    #[test]
    fn empty_registry_lists_no_tools() {
        let reg = ToolRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.descriptors().is_empty());
        assert!(reg.get("anything").is_none());
    }

    #[test]
    fn register_accepts_valid_name() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(DummyTool {
            name: "list_flows",
            description: "List configured flows.",
        }))
        .unwrap();
        assert_eq!(reg.len(), 1);
        assert!(reg.get("list_flows").is_some());
    }

    #[test]
    fn register_rejects_camel_case() {
        let mut reg = ToolRegistry::new();
        let err = reg
            .register(Box::new(DummyTool {
                name: "listFlows",
                description: "x",
            }))
            .unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidToolName);
    }

    #[test]
    fn register_rejects_no_underscore() {
        let mut reg = ToolRegistry::new();
        let err = reg
            .register(Box::new(DummyTool {
                name: "list",
                description: "x",
            }))
            .unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidToolName);
    }

    #[test]
    fn register_rejects_empty_description() {
        let mut reg = ToolRegistry::new();
        let err = reg
            .register(Box::new(DummyTool {
                name: "list_flows",
                description: "   ",
            }))
            .unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidToolName);
    }

    #[test]
    fn register_rejects_duplicate() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(DummyTool {
            name: "list_flows",
            description: "ok",
        }))
        .unwrap();
        let err = reg
            .register(Box::new(DummyTool {
                name: "list_flows",
                description: "ok",
            }))
            .unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidToolName);
    }

    #[test]
    fn name_validator_table() {
        // Table-driven: valid names left, invalid names right. Adding a new
        // accepted shape requires an explicit row here.
        let valid = [
            "list_flows",
            "run_flow",
            "implement_issue",
            "show_output_v2",
        ];
        let invalid = [
            "",
            "list",        // no underscore
            "_list_flows", // leading underscore
            "list_flows_", // trailing underscore
            "list__flows", // double underscore
            "ListFlows",   // camelCase
            "list-flows",  // dash
            "list flows",  // space
            "lista_fluxø", // non-ASCII
        ];
        for n in valid {
            assert!(is_valid_tool_name(n), "expected valid: {n}");
        }
        for n in invalid {
            assert!(!is_valid_tool_name(n), "expected invalid: {n}");
        }
    }

    #[test]
    fn descriptors_sorted_alphabetically() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(DummyTool {
            name: "run_flow",
            description: "x",
        }))
        .unwrap();
        reg.register(Box::new(DummyTool {
            name: "list_flows",
            description: "x",
        }))
        .unwrap();
        let names: Vec<String> = reg.descriptors().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["list_flows", "run_flow"]);
    }
}
