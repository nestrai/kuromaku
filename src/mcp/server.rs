//! Stdio JSON-RPC loop and method dispatcher.
//!
//! ## Lifecycle
//!
//! 1. Read NDJSON frames from stdin.
//! 2. Parse JSON-RPC; classify request vs notification.
//! 3. Dispatch on `method`. Requests get a response on stdout; notifications
//!    are processed silently.
//! 4. On stdin EOF, return -- the binary exits cleanly. (Per team review,
//!    SIGTERM is intentionally not handled in the scaffold.)
//!
//! ## Stdio discipline
//!
//! Stdio is the protocol channel. Nothing else may write to stdout. All
//! diagnostics route through `tracing` to stderr. The `eprintln!` is only
//! used as a final-fallback in `flush()` failures so the user still sees
//! something if tracing is misconfigured.
//!
//! ## Methods handled in the scaffold
//!
//! - `initialize` -- handshake, returns server info + capabilities + pinned
//!   protocol version.
//! - `notifications/initialized` -- ack from client, no-op.
//! - `tools/list` -- registry descriptors (initially empty).
//! - `tools/call` -- dispatch via registry; returns `unknown_tool` when the
//!   name is not registered.
//! - anything else -- `method_not_found`.

use std::sync::Arc;

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::error::{McpError, McpErrorCode};
use super::protocol::{
    ContentBlock, InitializeParams, InitializeResult, MCP_PROTOCOL_VERSION, Response,
    ServerCapabilities, ServerInfo, ToolsCallParams, ToolsCallResult, ToolsCapability,
    ToolsListResult, parse_incoming,
};
use super::tools::ToolRegistry;
use super::{Incoming, Request};

/// Default server identification reported in `initialize`. Pulled from the
/// crate's `Cargo.toml` so a release bumps the wire version automatically.
pub const SERVER_NAME: &str = "kuromaku";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the stdio server loop until stdin reaches EOF.
///
/// Generic over the reader and writer so tests can drive the dispatcher
/// without spawning the binary. The `kuro mcp` subcommand wires up
/// `tokio::io::stdin()` / `tokio::io::stdout()`.
pub async fn run<R, W>(reader: R, writer: W, registry: ToolRegistry) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let registry = Arc::new(registry);
    let writer = Arc::new(Mutex::new(writer));
    let mut lines = BufReader::new(reader).lines();

    info!(
        protocol = MCP_PROTOCOL_VERSION,
        server = SERVER_NAME,
        version = SERVER_VERSION,
        tools = registry.len(),
        "mcp server ready"
    );

    // `FuturesUnordered` lets a long-running tool call (notably `run_flow`,
    // which awaits the entire flow) overlap with later frames coming in --
    // `send_message` depends on this, otherwise it could never reach the
    // dispatcher while a flow is in flight. We avoid `tokio::spawn` here
    // because that would force a `'static` bound on the writer and break
    // the in-memory test harness; polling cooperatively in the same task
    // gives concurrency without that constraint, and stdout framing stays
    // correct because `write_response` serialises per-frame writes via
    // the writer mutex.
    let mut in_flight: FuturesUnordered<DispatchFuture<'_>> = FuturesUnordered::new();

    loop {
        tokio::select! {
            biased;
            // Drain completed dispatches as they finish so the FuturesUnordered
            // does not grow unbounded under steady traffic.
            Some(_) = in_flight.next(), if !in_flight.is_empty() => {}
            line = lines.next_line() => {
                match line? {
                    None => break,
                    Some(line) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        debug!(bytes = line.len(), "frame in");
                        let registry = Arc::clone(&registry);
                        let writer = Arc::clone(&writer);
                        in_flight.push(Box::pin(async move {
                            if let Some(response) = handle_line(&line, registry).await
                                && let Err(e) = write_response(&writer, &response).await
                            {
                                warn!(error = %e, "write response failed");
                            }
                        }));
                    }
                }
            }
        }
    }

    // Wait for any in-flight dispatches before returning so a final frame
    // is not silently dropped on stdin EOF.
    while in_flight.next().await.is_some() {}

    info!("stdin EOF, mcp server shutting down");
    Ok(())
}

/// Boxed dispatch future. Lifetime `'a` keeps the future tied to the local
/// `run` call so non-`'static` writers (like `&mut Vec<u8>` in tests) stay
/// valid for the duration. No `Send` bound -- `FuturesUnordered` polls the
/// futures on the same task, so they never cross thread boundaries.
type DispatchFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>;

/// Handle a single NDJSON line. Returns `None` for notifications (no reply
/// on the wire) and `Some(Response)` for requests including parse failures.
async fn handle_line(line: &str, registry: Arc<ToolRegistry>) -> Option<Response> {
    match parse_incoming(line) {
        Err((id, msg)) => {
            warn!(error = %msg, "parse error");
            Some(Response::err(
                id,
                McpError::with_details(
                    McpErrorCode::ParseError,
                    serde_json::json!({"reason": msg}),
                )
                .into_response_error(),
            ))
        }
        Ok(Incoming::Notification(n)) => {
            debug!(method = %n.method, "notification");
            // Lifecycle ack from the client; nothing to do for the scaffold.
            // Future notifications (cancelled, progress) get explicit arms.
            None
        }
        Ok(Incoming::Request(req)) => Some(dispatch(req, registry).await),
    }
}

/// Method dispatch for requests. One arm per supported method; everything
/// else returns the JSON-RPC `method_not_found` envelope so clients can
/// detect unsupported features without a custom code path.
async fn dispatch(req: Request, registry: Arc<ToolRegistry>) -> Response {
    let id = req.id.clone();
    debug!(method = %req.method, "dispatch");

    let result: Result<Value, McpError> = match req.method.as_str() {
        "initialize" => handle_initialize(req.params),
        "tools/list" => handle_tools_list(&registry),
        "tools/call" => handle_tools_call(req.params, &registry).await,
        "ping" => Ok(serde_json::json!({})),
        _ => Err(McpError::with_details(
            McpErrorCode::MethodNotFound,
            serde_json::json!({"method": req.method}),
        )),
    };

    match result {
        Ok(value) => Response::ok(id, value),
        Err(err) => Response::err(id, err.into_response_error()),
    }
}

fn handle_initialize(params: Option<Value>) -> Result<Value, McpError> {
    // Params are optional in practice -- some clients send `{}` or omit the
    // field. Decode best-effort; treat decode failure as InvalidParams so
    // clients learn early instead of getting a half-initialized session.
    if let Some(p) = params {
        let parsed: Result<InitializeParams, _> = serde_json::from_value(p);
        match parsed {
            Ok(p) => {
                let client = p
                    .client_info
                    .as_ref()
                    .and_then(|c| c.name.as_deref())
                    .unwrap_or("unknown");
                let client_proto = p.protocol_version.as_deref().unwrap_or("unset");
                info!(
                    client = client,
                    client_protocol = client_proto,
                    "initialize"
                );
            }
            Err(e) => {
                return Err(McpError::with_details(
                    McpErrorCode::InvalidParams,
                    serde_json::json!({"reason": e.to_string()}),
                ));
            }
        }
    } else {
        info!(client = "unknown", client_protocol = "unset", "initialize");
    }

    let result = InitializeResult {
        protocol_version: MCP_PROTOCOL_VERSION,
        server_info: ServerInfo {
            name: SERVER_NAME,
            version: SERVER_VERSION,
        },
        capabilities: ServerCapabilities {
            tools: Some(ToolsCapability {
                list_changed: false,
            }),
        },
    };
    serde_json::to_value(result).map_err(|e| {
        McpError::with_details(
            McpErrorCode::InternalError,
            serde_json::json!({"reason": e.to_string()}),
        )
    })
}

fn handle_tools_list(registry: &ToolRegistry) -> Result<Value, McpError> {
    let result = ToolsListResult {
        tools: registry.descriptors(),
    };
    serde_json::to_value(result).map_err(|e| {
        McpError::with_details(
            McpErrorCode::InternalError,
            serde_json::json!({"reason": e.to_string()}),
        )
    })
}

async fn handle_tools_call(
    params: Option<Value>,
    registry: &ToolRegistry,
) -> Result<Value, McpError> {
    let params: ToolsCallParams = match params {
        Some(p) => serde_json::from_value(p).map_err(|e| {
            McpError::with_details(
                McpErrorCode::InvalidParams,
                serde_json::json!({"reason": e.to_string()}),
            )
        })?,
        None => {
            return Err(McpError::with_details(
                McpErrorCode::InvalidParams,
                serde_json::json!({"reason": "missing params"}),
            ));
        }
    };

    let tool = registry.get(&params.name).ok_or_else(|| {
        McpError::with_details(
            McpErrorCode::UnknownTool,
            serde_json::json!({"name": params.name}),
        )
    })?;

    let arguments = params.arguments.unwrap_or_else(|| serde_json::json!({}));
    let value = tool.call(arguments).await?;

    // Wrap the tool's structured output in a single text block so MCP
    // clients can render it. Tools that prefer their own content shape will
    // override this once the registry grows past the scaffold (#196+).
    let text = serde_json::to_string(&value).map_err(|e| {
        McpError::with_details(
            McpErrorCode::InternalError,
            serde_json::json!({"reason": e.to_string()}),
        )
    })?;
    let result = ToolsCallResult {
        content: vec![ContentBlock::Text { text }],
        is_error: None,
    };
    serde_json::to_value(result).map_err(|e| {
        McpError::with_details(
            McpErrorCode::InternalError,
            serde_json::json!({"reason": e.to_string()}),
        )
    })
}

/// Serialise a response and write it as one NDJSON line. Holds the writer
/// lock for the duration of one frame so concurrent dispatches (future
/// async tools) can't interleave bytes on stdout.
async fn write_response<W>(writer: &Arc<Mutex<W>>, response: &Response) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut buf = serde_json::to_vec(response).map_err(std::io::Error::other)?;
    buf.push(b'\n');
    let mut guard = writer.lock().await;
    guard.write_all(&buf).await?;
    guard.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn run_one(line: &str, reg: ToolRegistry) -> Option<Response> {
        handle_line(line, Arc::new(reg)).await
    }

    #[tokio::test]
    async fn initialize_returns_pinned_protocol_version() {
        let reg = ToolRegistry::new();
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = run_one(line, reg).await.unwrap();
        assert_eq!(resp.id, json!(1));
        let result = resp.result.unwrap();
        assert_eq!(result["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    }

    #[tokio::test]
    async fn tools_list_empty_registry() {
        let line = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        let resp = run_one(line, ToolRegistry::new()).await.unwrap();
        assert_eq!(resp.result.unwrap()["tools"], json!([]));
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_returns_catalog_code() {
        let line = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#;
        let resp = run_one(line, ToolRegistry::new()).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32000);
        assert_eq!(err.message, "unknown tool");
        let data = err.data.unwrap();
        assert_eq!(data["code"], "unknown_tool");
        assert_eq!(data["details"]["name"], "nope");
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let line = r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#;
        let resp = run_one(line, ToolRegistry::new()).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "method not found");
    }

    #[tokio::test]
    async fn invalid_json_returns_parse_error_with_null_id() {
        let resp = run_one("garbage", ToolRegistry::new()).await.unwrap();
        assert_eq!(resp.id, Value::Null);
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32700);
    }

    #[tokio::test]
    async fn notification_returns_no_response() {
        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(run_one(line, ToolRegistry::new()).await.is_none());
    }

    #[tokio::test]
    async fn tools_call_missing_params_is_invalid_params() {
        let line = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call"}"#;
        let resp = run_one(line, ToolRegistry::new()).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "invalid params");
    }

    #[tokio::test]
    async fn run_processes_two_frames_then_exits_on_eof() {
        // End-to-end through the public `run` entry: pipe two NDJSON lines
        // through an in-memory reader, collect bytes from the writer, parse
        // them as the dispatcher would have on real stdio.
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n\
                      {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
        let reader = std::io::Cursor::new(input.to_vec());
        let mut output: Vec<u8> = Vec::new();
        run(reader, &mut output, ToolRegistry::new()).await.unwrap();

        let text = String::from_utf8(output).unwrap();
        let mut lines = text.lines();
        let first: Response = serde_json::from_str(lines.next().unwrap()).unwrap();
        let second: Response = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert!(lines.next().is_none(), "exactly two frames out");
        assert_eq!(first.id, json!(1));
        assert_eq!(
            first.result.unwrap()["protocolVersion"],
            MCP_PROTOCOL_VERSION
        );
        assert_eq!(second.id, json!(2));
        assert_eq!(second.result.unwrap()["tools"], json!([]));
    }

    #[tokio::test]
    async fn run_skips_blank_lines() {
        // Some MCP clients send empty lines as keep-alives. The loop must
        // tolerate them without producing a parse-error frame.
        let input = b"\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n\n";
        let reader = std::io::Cursor::new(input.to_vec());
        let mut output: Vec<u8> = Vec::new();
        run(reader, &mut output, ToolRegistry::new()).await.unwrap();

        let text = String::from_utf8(output).unwrap();
        let frames: Vec<&str> = text.lines().collect();
        assert_eq!(frames.len(), 1, "blank lines must not produce frames");
    }
}
