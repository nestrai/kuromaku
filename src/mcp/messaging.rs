//! `send_message` tool (#199): inject a human-style message into a live
//! conversation step.
//!
//! ## Why this exists
//!
//! When an MCP client (Claude Code, Cursor, Codex) drives a flow through
//! [`super::execution::RunFlow`], the human running the client has no
//! stdin into the running flow -- the binary's stdout is occupied by the
//! JSON-RPC protocol channel. `send_message` is the seam: any time a
//! conversation step is live, the client can post a message that lands in
//! the router exactly like a stdin-typed line, including the audit-log
//! entry attributed to `user`.
//!
//! ## v1 scope (per team review on #199)
//!
//! - **Broadcast only.** No `to` parameter. The router's
//!   [`crate::messaging::router::Router::set_human_input`] takes a single
//!   receiver and broadcasts to every participant. Per-recipient targeting
//!   is router-side work; deferred until the router supports it.
//! - **Single live conversation.** If zero or more than one conversation
//!   is currently active across the registered runs, the tool returns
//!   `conversation_inactive`. Multi-conversation routing is the same
//!   future work as `to`.
//! - **Session-scoped.** The tool only sees runs registered by *this* MCP
//!   session via [`super::session::SessionState`]. A run started by a
//!   different client (or by `kuro run` directly) is invisible.
//!
//! ## Error mapping
//!
//! - empty / whitespace `text`               -> `invalid_params`
//! - zero live conversations                 -> `conversation_inactive`
//! - more than one live conversation         -> `conversation_inactive`
//!   (with `count` in `details`, so the client can render a useful hint)
//! - router channel already closed           -> `conversation_inactive`
//!   (the conversation just terminated; from the client's perspective the
//!   live state is gone, which is the same wire-level signal)

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::error::{McpError, McpErrorCode};
use super::session::SessionState;
use super::tools::Tool;

#[derive(Deserialize)]
struct SendMessageArgs {
    text: String,
}

/// `send_message` -- inject a human turn into the active conversation step.
///
/// Holds an `Arc<SessionState>` so it observes the same registry that
/// `run_flow` writes into. The tool itself is stateless -- the registry
/// is the only shared mutable state.
pub struct SendMessage {
    session: Arc<SessionState>,
}

impl SendMessage {
    pub fn new(session: Arc<SessionState>) -> Self {
        Self { session }
    }
}

#[async_trait]
impl Tool for SendMessage {
    fn name(&self) -> &'static str {
        "send_message"
    }

    fn description(&self) -> &'static str {
        "Inject a human-style message into the conversation step that is currently running. \
         Use this when a flow is in a multi-agent conversation (kuro's `kind: conversation` \
         step) and you want to add a user turn from outside -- exactly as if you had typed \
         the line on stdin. The message is broadcast to every participant; targeting one \
         participant is not yet supported. Returns `conversation_inactive` if no flow \
         started by this MCP session is currently in a conversation step. \
         Example: send_message {\"text\":\"focus on the parser bug, ignore the docs\"}."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The human turn to inject. Must be non-empty. Whitespace-only \
                                    strings are rejected with invalid_params."
                }
            },
            "required": ["text"]
        })
    }

    async fn call(&self, arguments: Value) -> Result<Value, McpError> {
        let parsed: SendMessageArgs = serde_json::from_value(arguments).map_err(|e| {
            McpError::with_details(
                McpErrorCode::InvalidParams,
                json!({"reason": format!("arguments: {e}")}),
            )
        })?;
        let trimmed = parsed.text.trim();
        if trimmed.is_empty() {
            return Err(McpError::with_details(
                McpErrorCode::InvalidParams,
                json!({"reason": "text must not be empty"}),
            ));
        }

        let live = self.session.live_routers();
        match live.len() {
            0 => Err(McpError::with_details(
                McpErrorCode::ConversationInactive,
                json!({
                    "reason": "no flow started by this MCP session is currently in a conversation step",
                    "count": 0,
                }),
            )),
            1 => {
                // We pulled the single accessor as an owned value, so a
                // `Closed` error here means the conversation terminated
                // between our snapshot and the send. Surface that as
                // `conversation_inactive` -- the client's wire-level model
                // is "alive or not", not "alive a moment ago".
                let accessor = live.into_iter().next().expect("len == 1");
                accessor
                    .inject_human_message(parsed.text.clone())
                    .await
                    .map_err(|_| {
                        McpError::with_details(
                            McpErrorCode::ConversationInactive,
                            json!({"reason": "router closed during send"}),
                        )
                    })?;
                Ok(json!({"delivered": true}))
            }
            n => Err(McpError::with_details(
                McpErrorCode::ConversationInactive,
                json!({
                    "reason": "more than one conversation is currently live; v1 send_message only \
                               supports a single active conversation per MCP session",
                    "count": n,
                }),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner;

    fn fresh_session() -> Arc<SessionState> {
        Arc::new(SessionState::new())
    }

    #[tokio::test]
    async fn rejects_empty_text() {
        let tool = SendMessage::new(fresh_session());
        let err = tool.call(json!({"text": ""})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn rejects_whitespace_only_text() {
        let tool = SendMessage::new(fresh_session());
        let err = tool.call(json!({"text": "   \t\n"})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn rejects_missing_text_field() {
        let tool = SendMessage::new(fresh_session());
        let err = tool.call(json!({})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InvalidParams);
    }

    #[tokio::test]
    async fn returns_inactive_when_no_runs_registered() {
        let tool = SendMessage::new(fresh_session());
        let err = tool.call(json!({"text": "hi"})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::ConversationInactive);
        let details = err.details.unwrap();
        assert_eq!(details["count"], 0);
    }

    #[tokio::test]
    async fn returns_inactive_when_run_has_no_published_router() {
        // Registered run, but the conversation step has not started yet
        // (or has already finished). `live_routers` filters it out.
        let session = fresh_session();
        let _slot = session.register(runner::test_support::fresh_active_router());
        let tool = SendMessage::new(session);
        let err = tool.call(json!({"text": "hi"})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::ConversationInactive);
    }

    #[tokio::test]
    async fn returns_inactive_when_more_than_one_conversation_is_live() {
        let session = fresh_session();
        let (ar1, _acc1) = runner::test_support::active_router_with_published();
        let (ar2, _acc2) = runner::test_support::active_router_with_published();
        let _s1 = session.register(ar1);
        let _s2 = session.register(ar2);
        let tool = SendMessage::new(session);
        let err = tool.call(json!({"text": "hi"})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::ConversationInactive);
        let details = err.details.unwrap();
        assert_eq!(details["count"], 2);
    }

    #[tokio::test]
    async fn returns_inactive_when_router_channel_already_closed() {
        // `active_router_with_published` drops the receiver inside, so the
        // accessor's send fails with `Closed`. From the client's view the
        // conversation is gone, which is the same wire signal as "no live
        // run".
        let session = fresh_session();
        let (ar, _acc) = runner::test_support::active_router_with_published();
        let _slot = session.register(ar);
        let tool = SendMessage::new(session);
        let err = tool.call(json!({"text": "hi"})).await.unwrap_err();
        assert_eq!(err.code, McpErrorCode::ConversationInactive);
    }

    #[tokio::test]
    async fn descriptors_round_trip_through_registry() {
        // The registry's name validator and required-fields check are the
        // contract surface for tool discovery; verify `send_message` lands
        // there cleanly.
        let mut reg = super::super::tools::ToolRegistry::new();
        reg.register(Box::new(SendMessage::new(fresh_session())))
            .unwrap();
        let names: Vec<String> = reg.descriptors().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["send_message"]);
    }
}
