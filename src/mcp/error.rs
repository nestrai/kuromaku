//! Stable error code catalog.
//!
//! Two layers, one source of truth:
//!
//! 1. JSON-RPC 2.0 transport errors -- well-known numeric codes
//!    (`-32700` parse error, `-32600` invalid request, etc.).
//! 2. Application-level kuromaku errors -- a fixed catalog of string codes
//!    pinned by the team review (issue #195 comments). Wire format uses a
//!    JSON-RPC error code in the `-32000..=-32099` server-reserved range and
//!    puts the stable string code in `data.code`. Volatile substrings
//!    (paths, run-ids, branch names, exit codes) live in `data.details`
//!    so the human-readable `message` stays a deterministic string.
//!
//! Why this shape: deterministic messages let test assertions, log
//! aggregators and downstream tooling match on `message` and `data.code`
//! without false positives from path or timestamp churn.

use serde_json::{Value, json};

use super::protocol::ResponseError;

/// JSON-RPC 2.0 well-known codes plus the kuromaku application catalog.
///
/// `as_jsonrpc_code` returns the numeric wire code; `wire_code` returns the
/// stable string code that goes into `data.code` for application errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpErrorCode {
    // --- JSON-RPC 2.0 transport ---
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    InternalError,

    // --- Application catalog (team review #195) ---
    /// Tool name was not found in the registry.
    UnknownTool,
    /// Tool name is malformed (not snake_case verb_noun) -- raised by the
    /// registry at registration time, never on the wire. Kept in the catalog
    /// for symmetry with future runtime checks.
    InvalidToolName,
    FlowMissing,
    AgentMissing,
    RunNotFound,
    LintFailed,
    TestsFailed,
    GitError,
    GhError,
    GhUnauthenticated,
    BranchDirty,
    ConversationInactive,
}

impl McpErrorCode {
    /// JSON-RPC numeric code. Application-level codes all map to `-32000`
    /// (the top of the server-reserved range) so clients can route by the
    /// wire code without owning the string catalog.
    pub fn as_jsonrpc_code(self) -> i32 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::InternalError => -32603,
            // All application-level errors share the server-reserved code.
            // Differentiation lives in `data.code` (the wire string).
            _ => -32000,
        }
    }

    /// Stable string identifier. Used in `data.code` on application errors
    /// and exposed as a public catalog so future tools (and tests) reference
    /// one source of truth.
    pub fn wire_code(self) -> &'static str {
        match self {
            Self::ParseError => "parse_error",
            Self::InvalidRequest => "invalid_request",
            Self::MethodNotFound => "method_not_found",
            Self::InvalidParams => "invalid_params",
            Self::InternalError => "internal_error",
            Self::UnknownTool => "unknown_tool",
            Self::InvalidToolName => "invalid_tool_name",
            Self::FlowMissing => "flow_missing",
            Self::AgentMissing => "agent_missing",
            Self::RunNotFound => "run_not_found",
            Self::LintFailed => "lint_failed",
            Self::TestsFailed => "tests_failed",
            Self::GitError => "git_error",
            Self::GhError => "gh_error",
            Self::GhUnauthenticated => "gh_unauthenticated",
            Self::BranchDirty => "branch_dirty",
            Self::ConversationInactive => "conversation_inactive",
        }
    }

    /// Deterministic human-readable message. Must not include volatile
    /// substrings -- those go on [`McpError::details`]. Assertions in tests
    /// match this string exactly.
    pub fn message(self) -> &'static str {
        match self {
            Self::ParseError => "parse error",
            Self::InvalidRequest => "invalid request",
            Self::MethodNotFound => "method not found",
            Self::InvalidParams => "invalid params",
            Self::InternalError => "internal error",
            Self::UnknownTool => "unknown tool",
            Self::InvalidToolName => "invalid tool name",
            Self::FlowMissing => "flow missing",
            Self::AgentMissing => "agent missing",
            Self::RunNotFound => "run not found",
            Self::LintFailed => "lint failed",
            Self::TestsFailed => "tests failed",
            Self::GitError => "git error",
            Self::GhError => "gh error",
            Self::GhUnauthenticated => "gh unauthenticated",
            Self::BranchDirty => "branch dirty",
            Self::ConversationInactive => "conversation inactive",
        }
    }
}

/// Application-shaped error returned by tool handlers and the dispatcher.
/// Converts to a JSON-RPC [`ResponseError`] via [`McpError::into_response_error`].
#[derive(Debug, Clone)]
pub struct McpError {
    pub code: McpErrorCode,
    /// Volatile data (paths, IDs, exit codes). Optional -- some errors carry
    /// no extra information.
    pub details: Option<Value>,
}

impl McpError {
    pub fn new(code: McpErrorCode) -> Self {
        Self {
            code,
            details: None,
        }
    }

    pub fn with_details(code: McpErrorCode, details: Value) -> Self {
        Self {
            code,
            details: Some(details),
        }
    }

    /// Convert to the JSON-RPC wire envelope. Application-level errors keep
    /// the stable string code in `data.code` and the volatile substrings in
    /// `data.details`. Transport-level errors omit `data.code` (the numeric
    /// JSON-RPC code already identifies them).
    pub fn into_response_error(self) -> ResponseError {
        let is_application = !matches!(
            self.code,
            McpErrorCode::ParseError
                | McpErrorCode::InvalidRequest
                | McpErrorCode::MethodNotFound
                | McpErrorCode::InvalidParams
                | McpErrorCode::InternalError
        );

        let data = if is_application {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "code".to_string(),
                Value::String(self.code.wire_code().to_string()),
            );
            if let Some(d) = self.details {
                obj.insert("details".to_string(), d);
            }
            Some(Value::Object(obj))
        } else {
            self.details.map(|d| json!({"details": d}))
        };

        ResponseError {
            code: self.code.as_jsonrpc_code(),
            message: self.code.message().to_string(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_error_wraps_stable_code_in_data() {
        let err = McpError::with_details(McpErrorCode::UnknownTool, json!({"name": "foo_bar"}));
        let wire = err.into_response_error();
        assert_eq!(wire.code, -32000);
        assert_eq!(wire.message, "unknown tool");
        let data = wire.data.unwrap();
        assert_eq!(data["code"], "unknown_tool");
        assert_eq!(data["details"]["name"], "foo_bar");
    }

    #[test]
    fn transport_error_omits_wire_code_in_data() {
        let err = McpError::new(McpErrorCode::MethodNotFound);
        let wire = err.into_response_error();
        assert_eq!(wire.code, -32601);
        assert_eq!(wire.message, "method not found");
        assert!(wire.data.is_none());
    }

    #[test]
    fn deterministic_message_has_no_volatile_substring() {
        // Sanity: every catalog entry's message stays static. If anyone adds
        // an interpolation here, this test should be updated AND the value
        // moved into `details`.
        for code in [
            McpErrorCode::FlowMissing,
            McpErrorCode::AgentMissing,
            McpErrorCode::RunNotFound,
            McpErrorCode::LintFailed,
            McpErrorCode::TestsFailed,
            McpErrorCode::GitError,
            McpErrorCode::GhError,
            McpErrorCode::GhUnauthenticated,
            McpErrorCode::BranchDirty,
            McpErrorCode::ConversationInactive,
            McpErrorCode::UnknownTool,
            McpErrorCode::InvalidToolName,
        ] {
            let m = code.message();
            assert!(!m.contains('{'), "{m} contains placeholder");
            assert!(!m.contains('/'), "{m} contains path separator");
        }
    }

    #[test]
    fn wire_codes_are_unique() {
        let all = [
            McpErrorCode::ParseError,
            McpErrorCode::InvalidRequest,
            McpErrorCode::MethodNotFound,
            McpErrorCode::InvalidParams,
            McpErrorCode::InternalError,
            McpErrorCode::UnknownTool,
            McpErrorCode::InvalidToolName,
            McpErrorCode::FlowMissing,
            McpErrorCode::AgentMissing,
            McpErrorCode::RunNotFound,
            McpErrorCode::LintFailed,
            McpErrorCode::TestsFailed,
            McpErrorCode::GitError,
            McpErrorCode::GhError,
            McpErrorCode::GhUnauthenticated,
            McpErrorCode::BranchDirty,
            McpErrorCode::ConversationInactive,
        ];
        let mut codes: Vec<&str> = all.iter().map(|c| c.wire_code()).collect();
        codes.sort();
        let len = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), len, "wire codes must be unique");
    }
}
