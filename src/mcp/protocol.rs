//! JSON-RPC 2.0 + MCP wire types.
//!
//! Two layers:
//!
//! 1. JSON-RPC 2.0 envelope ([`Request`], [`Response`], [`ResponseError`],
//!    [`Notification`]). The id is preserved verbatim so clients that send
//!    string ids get string ids back -- the spec requires byte-for-byte echo.
//! 2. MCP method-specific payloads ([`InitializeParams`],
//!    [`InitializeResult`], [`ToolDescriptor`], [`ToolsListResult`],
//!    [`ToolsCallParams`], [`ToolsCallResult`]).
//!
//! Pinned MCP spec version: `2025-06-18`. The scaffold negotiates only
//! `tools` capability with `listChanged: false` -- no resources, prompts,
//! sampling, or notifications beyond the lifecycle pair. Future features are
//! added behind capabilities, never by breaking these payloads.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Pinned MCP protocol version. Incoming `initialize` requests that name a
/// different version still get this value in the response -- per the spec the
/// server tells the client which version it speaks; the client decides
/// whether to continue.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

// --- JSON-RPC 2.0 envelope ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    /// Echoed verbatim into the response. Spec preserves type (string,
    /// number, null) -- never reassign.
    pub id: Value,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: i32,
    pub message: String,
    /// Volatile substrings (paths, IDs, timestamps, PIDs) live here so the
    /// `message` itself stays a deterministic string -- per the team review's
    /// stable-error-catalog rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, error: ResponseError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// Either a request or a notification. JSON-RPC distinguishes by presence of
/// `id`, not by a tag, so we parse first and classify after.
#[derive(Debug)]
pub enum Incoming {
    Request(Request),
    Notification(Notification),
}

/// Parse a single JSON-RPC frame. Returns `Err` with the parsed `id` (if
/// present) so the dispatcher can answer with a matching `InvalidRequest`
/// error envelope. A frame without `id` and without `method` is treated as
/// `InvalidRequest` with a null id.
pub fn parse_incoming(line: &str) -> Result<Incoming, (Value, String)> {
    let value: Value = serde_json::from_str(line).map_err(|e| (Value::Null, e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| (Value::Null, "request must be a JSON object".to_string()))?;

    let id = obj.get("id").cloned();
    let method = obj
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| {
            (
                id.clone().unwrap_or(Value::Null),
                "missing method".to_string(),
            )
        })?
        .to_string();
    let params = obj.get("params").cloned();
    let jsonrpc = obj
        .get("jsonrpc")
        .and_then(|j| j.as_str())
        .unwrap_or("2.0")
        .to_string();

    match id {
        Some(id) => Ok(Incoming::Request(Request {
            jsonrpc,
            id,
            method,
            params,
        })),
        None => Ok(Incoming::Notification(Notification {
            jsonrpc,
            method,
            params,
        })),
    }
}

// --- MCP method payloads ---

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Version the client speaks. Recorded for logging; the server still
    /// responds with [`MCP_PROTOCOL_VERSION`].
    #[serde(default)]
    pub protocol_version: Option<String>,
    #[serde(default)]
    pub client_info: Option<ClientInfo>,
    #[serde(default)]
    pub capabilities: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientInfo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub protocol_version: &'static str,
    pub server_info: ServerInfo,
    pub capabilities: ServerCapabilities,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ServerCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    /// Whether the server emits `notifications/tools/list_changed`. Scaffold
    /// returns a static registry for now -- always false.
    pub list_changed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolsListResult {
    pub tools: Vec<ToolDescriptor>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolsCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Option<Value>,
}

/// MCP `tools/call` result. Follows the spec's content-block shape; the
/// scaffold only emits `text` blocks. Tools that return structured data wrap
/// their JSON in a single text block as a JSON string -- this matches what
/// existing MCP clients expect.
#[derive(Debug, Clone, Serialize)]
pub struct ToolsCallResult {
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "isError")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_preserves_string_id() {
        let line = r#"{"jsonrpc":"2.0","id":"abc","method":"tools/list"}"#;
        match parse_incoming(line).unwrap() {
            Incoming::Request(req) => {
                assert_eq!(req.id, Value::String("abc".into()));
                assert_eq!(req.method, "tools/list");
            }
            Incoming::Notification(_) => panic!("expected request"),
        }
    }

    #[test]
    fn parse_request_preserves_numeric_id() {
        let line = r#"{"jsonrpc":"2.0","id":42,"method":"initialize","params":{}}"#;
        match parse_incoming(line).unwrap() {
            Incoming::Request(req) => {
                assert_eq!(req.id, serde_json::json!(42));
            }
            Incoming::Notification(_) => panic!("expected request"),
        }
    }

    #[test]
    fn parse_notification_without_id() {
        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        match parse_incoming(line).unwrap() {
            Incoming::Notification(n) => assert_eq!(n.method, "notifications/initialized"),
            Incoming::Request(_) => panic!("expected notification"),
        }
    }

    #[test]
    fn parse_invalid_json_returns_null_id() {
        let err = parse_incoming("not json at all").unwrap_err();
        assert_eq!(err.0, Value::Null);
    }

    #[test]
    fn parse_missing_method_returns_id_for_error() {
        let line = r#"{"jsonrpc":"2.0","id":7}"#;
        let err = parse_incoming(line).unwrap_err();
        assert_eq!(err.0, serde_json::json!(7));
        assert!(err.1.contains("method"));
    }

    #[test]
    fn response_serialises_with_jsonrpc_field() {
        let resp = Response::ok(serde_json::json!(1), serde_json::json!({"ok": true}));
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""jsonrpc":"2.0""#));
        assert!(s.contains(r#""result""#));
        assert!(!s.contains(r#""error""#));
    }

    #[test]
    fn response_error_omits_data_when_absent() {
        let err = ResponseError {
            code: -32601,
            message: "method_not_found".to_string(),
            data: None,
        };
        let resp = Response::err(serde_json::json!(2), err);
        let s = serde_json::to_string(&resp).unwrap();
        assert!(!s.contains(r#""data""#));
        assert!(!s.contains(r#""result""#));
    }

    #[test]
    fn initialize_result_camel_case() {
        let r = InitializeResult {
            protocol_version: MCP_PROTOCOL_VERSION,
            server_info: ServerInfo {
                name: "kuromaku",
                version: "0",
            },
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: false,
                }),
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""protocolVersion""#));
        assert!(s.contains(r#""serverInfo""#));
        assert!(s.contains(r#""listChanged":false"#));
    }
}
