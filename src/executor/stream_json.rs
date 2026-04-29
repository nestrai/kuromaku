//! Parser for Claude CLI's `--output-format stream-json --verbose` output.
//!
//! The CLI emits one JSON object per line. Different event shapes appear over
//! the lifetime of a turn:
//!
//! * `system` (subtype = "init") -- session metadata, ignored.
//! * `assistant` -- assistant message; `message.content` is an array of
//!   blocks (text or tool_use). Ignored: the executor always runs with
//!   `--include-partial-messages`, so the same content already arrives via
//!   `stream_event` deltas. Honoring both events would duplicate text in
//!   the artifact and step buffer.
//! * `stream_event` -- low-level token-by-token events. We only care about
//!   `content_block_delta` with a `text_delta` (which carries the actual
//!   user-visible text chunk) and tool_use `content_block_start` events
//!   (so we can announce the tool name as it starts).
//! * `result` -- terminal event with the final assistant text in `result`,
//!   plus token usage and duration. We use it to set the canonical step
//!   output (matches what `--output-format text` would have produced).
//! * `user` -- tool result message coming back into the conversation; not
//!   useful for live display.
//!
//! The parser is intentionally lenient: unknown events return `None`, missing
//! fields return `None`. The CLI's output schema is not formally stable, so
//! we treat anything we don't recognize as a no-op rather than an error --
//! a parse failure should never crash the executor.

/// A piece of meaning extracted from a single stream-json line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fragment {
    /// User-visible assistant text. Append to artifact and buffer as-is.
    Text(String),
    /// Tool call announcement. Currently unused by the executor (markers
    /// were dropped from the artifact as noise), but the parser still emits
    /// it so future code paths (structured logs, telemetry) can use it.
    ToolUse { name: String },
    /// Final assistant output extracted from a `result` event. Used to set
    /// the canonical step output -- matches `--output-format text`.
    Result(String),
}

/// Parse a single NDJSON line. Returns `None` if the line is empty, not JSON,
/// or carries no user-relevant content. Multiple fragments per line are
/// possible (an `assistant` event can contain text and tool_use blocks),
/// hence `Vec<Fragment>` rather than `Option<Fragment>`.
pub fn parse_line(line: &str) -> Vec<Fragment> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match event_type {
        // `assistant` events are redundant with `stream_event` deltas when
        // `--include-partial-messages` is active (which the executor always
        // sets). Honoring both would append the same text and tool-use
        // markers twice -- once via deltas as they arrive, once via the
        // whole assistant message at the end. Ignore assistant events.
        "assistant" => Vec::new(),
        "stream_event" => parse_stream_event(&value),
        "result" => parse_result(&value),
        // "system", "user", and unknown events have no live-display meaning.
        _ => Vec::new(),
    }
}

/// Extract incremental deltas + tool_use starts from a `stream_event`.
fn parse_stream_event(value: &serde_json::Value) -> Vec<Fragment> {
    let event = match value.get("event") {
        Some(e) => e,
        None => return Vec::new(),
    };
    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match event_type {
        "content_block_delta" => {
            let delta = match event.get("delta") {
                Some(d) => d,
                None => return Vec::new(),
            };
            let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if delta_type == "text_delta"
                && let Some(text) = delta.get("text").and_then(|v| v.as_str())
                && !text.is_empty()
            {
                return vec![Fragment::Text(text.to_string())];
            }
            Vec::new()
        }
        "content_block_start" => {
            let block = match event.get("content_block") {
                Some(b) => b,
                None => return Vec::new(),
            };
            let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if block_type == "tool_use" {
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                return vec![Fragment::ToolUse { name }];
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// Extract the final assistant text from a `result` event.
fn parse_result(value: &serde_json::Value) -> Vec<Fragment> {
    // Standard shape: top-level `result` field carries the final text.
    if let Some(text) = value.get("result").and_then(|v| v.as_str())
        && !text.is_empty()
    {
        return vec![Fragment::Result(text.to_string())];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_line_yields_nothing() {
        assert!(parse_line("").is_empty());
        assert!(parse_line("   ").is_empty());
    }

    #[test]
    fn malformed_json_yields_nothing() {
        assert!(parse_line("not json at all").is_empty());
        assert!(parse_line("{").is_empty());
    }

    #[test]
    fn unknown_event_type_yields_nothing() {
        assert!(parse_line(r#"{"type":"system","subtype":"init"}"#).is_empty());
        assert!(parse_line(r#"{"type":"user","content":[]}"#).is_empty());
        assert!(parse_line(r#"{"type":"future_event_we_do_not_know"}"#).is_empty());
    }

    #[test]
    fn stream_event_text_delta_extracts_text() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}}}"#;
        assert_eq!(parse_line(line), vec![Fragment::Text("Hello ".to_string())]);
    }

    #[test]
    fn stream_event_non_text_delta_is_ignored() {
        // input_json_delta carries tool args, not user-visible text.
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}}"#;
        assert!(parse_line(line).is_empty());
    }

    #[test]
    fn stream_event_empty_text_delta_is_ignored() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}}"#;
        assert!(parse_line(line).is_empty());
    }

    #[test]
    fn stream_event_tool_use_start_emits_tool_fragment() {
        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"Bash","input":{}}}}"#;
        assert_eq!(
            parse_line(line),
            vec![Fragment::ToolUse {
                name: "Bash".to_string()
            }]
        );
    }

    #[test]
    fn stream_event_text_block_start_is_ignored() {
        // We only react to tool_use starts; text starts contribute nothing
        // until the deltas arrive.
        let line = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#;
        assert!(parse_line(line).is_empty());
    }

    #[test]
    fn assistant_text_block_is_ignored() {
        // With `--include-partial-messages` the same text arrives as
        // `stream_event` deltas. Honoring assistant events would duplicate.
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"answer here"}]}}"#;
        assert!(parse_line(line).is_empty());
    }

    #[test]
    fn assistant_tool_use_block_is_ignored() {
        // Tool-use starts come through `stream_event` content_block_start;
        // the assistant-event copy would emit a second marker.
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"tu_2","name":"Read","input":{"file_path":"x"}}]}}"#;
        assert!(parse_line(line).is_empty());
    }

    #[test]
    fn assistant_mixed_blocks_are_ignored() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"plan: "},{"type":"tool_use","name":"Bash","input":{}},{"type":"text","text":"done"}]}}"#;
        assert!(parse_line(line).is_empty());
    }

    #[test]
    fn assistant_without_content_array_is_ignored() {
        let line = r#"{"type":"assistant","message":{}}"#;
        assert!(parse_line(line).is_empty());
    }

    #[test]
    fn stream_event_and_assistant_for_same_text_yields_text_once() {
        // Regression: with --include-partial-messages, deltas and the final
        // assistant event both carry the same content. The parser must
        // surface the text exactly once (via deltas) so the artifact and
        // buffer don't contain duplicates.
        let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"message_once"}}}"#;
        let assistant = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"message_once"}]}}"#;

        let combined: String = [delta, assistant]
            .iter()
            .flat_map(|l| parse_line(l))
            .filter_map(|f| match f {
                Fragment::Text(t) => Some(t),
                _ => None,
            })
            .collect();

        assert_eq!(combined, "message_once");
    }

    #[test]
    fn result_event_extracts_final_text() {
        let line = r#"{"type":"result","subtype":"success","result":"final answer","total_cost_usd":0.01}"#;
        assert_eq!(
            parse_line(line),
            vec![Fragment::Result("final answer".to_string())]
        );
    }

    #[test]
    fn result_event_without_text_is_ignored() {
        let line = r#"{"type":"result","subtype":"error","total_cost_usd":0}"#;
        assert!(parse_line(line).is_empty());
    }
}
