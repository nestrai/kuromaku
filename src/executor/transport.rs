//! Bidirectional transport for inter-agent messaging.
//!
//! The [`Executor`](super::Executor) trait drives one-shot, output-only runs:
//! spawn, drain stdout, wait for exit. The router (issue #166) needs more --
//! it must inject user messages into a running agent's stdin while concurrently
//! reading the agent's stream-json output. This module provides that shape.
//!
//! [`Transport`] abstracts the bidirectional channel so future implementations
//! (file-based, socket, MCP) can drop in without router changes. The first
//! implementation, [`StreamJsonTransport`], uses Claude CLI's
//! `--input-format stream-json --output-format stream-json` mode.
//!
//! Parsing is delegated to [`super::stream_json::parse_line`]. There is exactly
//! one stream-json parser in the crate; transport reuses it rather than
//! duplicating the logic.

use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use super::stream_json::{self, Fragment};

// --- Errors ---

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("spawn failed: {0}")]
    Spawn(String),

    #[error("send failed: {0}")]
    Send(String),

    #[error("recv failed: {0}")]
    Recv(String),

    #[error("close failed: {0}")]
    Close(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// --- Types ---

/// A piece of meaning received from the remote agent.
///
/// Aliased to [`Fragment`] -- the parser already enumerates the events the
/// router cares about (text, tool use, terminal result), and a separate enum
/// would just shadow it. Split this if a future transport needs to carry
/// events that don't map onto Fragment.
pub type TransportEvent = Fragment;

// --- Trait ---

/// Bidirectional channel to a running agent process.
///
/// `send` and `recv` are intentionally separate concerns -- the router writes
/// when it has a message to deliver and reads in its own loop. `&self` (with
/// internal locking) so the same transport can be shared across the router's
/// send and receive tasks without ownership gymnastics.
pub trait Transport: Send + Sync {
    /// Deliver a message to the agent. Returns when the bytes have been
    /// flushed to the child's stdin pipe (not when the agent has consumed
    /// them).
    fn send(&self, message: &str) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Pull the next event from the agent. Returns `Ok(None)` on stdout EOF
    /// (the child has closed its output, e.g. after a `result` event or
    /// process exit). Subsequent calls after `None` continue to return
    /// `None`.
    fn recv(&self) -> impl Future<Output = Result<Option<TransportEvent>, TransportError>> + Send;
}

// --- StreamJsonTransport ---

/// Transport implementation backed by a child process using Claude CLI's
/// stream-json input/output protocol.
///
/// On construction the child's stdout is drained by a background task that
/// parses each NDJSON line into [`Fragment`] values and forwards them through
/// an mpsc channel. `recv()` reads from that channel; `send()` writes a
/// `{"type":"user","message":...}` envelope to the child's stdin. Stderr is
/// drained into a buffer so a crashed child has its diagnostics available
/// after `close()`.
///
/// Lifecycle:
/// * `spawn` -- starts the child, wires up reader tasks, returns the transport.
/// * `send` / `recv` -- normal operation; safe to interleave.
/// * `close` -- drops stdin (EOF for the child), waits for the child to exit,
///   awaits the reader tasks. Returns the exit status and captured stderr.
pub struct StreamJsonTransport {
    /// Locked because `AsyncWriteExt::write_all` needs `&mut`. The send path
    /// is serialized; concurrent `send` calls queue on the lock rather than
    /// interleaving partial JSON lines on the wire.
    stdin: Mutex<Option<ChildStdin>>,
    /// Reader-side end of the parsed-fragment channel. `&mut` is needed for
    /// `recv().await`, hence the Mutex. Locking is fine because there is
    /// only ever one logical consumer (the router's read loop).
    rx: Mutex<mpsc::UnboundedReceiver<TransportEvent>>,
    /// The child process. Owned so we can wait/kill on close.
    child: Mutex<Option<Child>>,
    /// Background readers for stdout (parses fragments, feeds `rx`) and
    /// stderr (drains into `stderr_buf`). Joined on close.
    stdout_reader: Mutex<Option<JoinHandle<()>>>,
    stderr_reader: Mutex<Option<JoinHandle<()>>>,
    stderr_buf: Arc<Mutex<String>>,
}

/// Output from a finished transport run.
#[derive(Debug)]
pub struct TransportOutput {
    pub exit_code: i32,
    pub stderr: String,
}

impl StreamJsonTransport {
    /// Spawn `command` as a child with stdin/stdout/stderr piped, and start
    /// background tasks that parse stdout into [`TransportEvent`]s and drain
    /// stderr into a buffer.
    ///
    /// The caller is responsible for configuring `command` (binary, args, env)
    /// -- e.g. for Claude CLI, set `--input-format stream-json
    /// --output-format stream-json --verbose --include-partial-messages`.
    /// Tests use a plain shell script.
    pub async fn spawn(mut command: tokio::process::Command) -> Result<Self, TransportError> {
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|e| TransportError::Spawn(format!("failed to spawn process: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Spawn("failed to capture child stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Spawn("failed to capture child stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| TransportError::Spawn("failed to capture child stderr".to_string()))?;

        let (tx, rx) = mpsc::unbounded_channel::<TransportEvent>();

        // stdout reader: parse each NDJSON line via the shared parser and
        // forward fragments. EOF closes the channel, which surfaces as
        // `recv() -> None` to the caller.
        let stdout_reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                for fragment in stream_json::parse_line(&line) {
                    // Receiver dropped means caller is gone; stop reading.
                    if tx.send(fragment).is_err() {
                        return;
                    }
                }
            }
            // tx dropped here -> rx returns None on next recv.
        });

        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf_clone = Arc::clone(&stderr_buf);
        let stderr_reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let mut buf = stderr_buf_clone.lock().await;
                buf.push_str(&line);
                buf.push('\n');
            }
        });

        Ok(Self {
            stdin: Mutex::new(Some(stdin)),
            rx: Mutex::new(rx),
            child: Mutex::new(Some(child)),
            stdout_reader: Mutex::new(Some(stdout_reader)),
            stderr_reader: Mutex::new(Some(stderr_reader)),
            stderr_buf,
        })
    }

    /// Close stdin (EOF for the child), wait for the child to exit, await the
    /// reader tasks. Returns exit status and captured stderr.
    ///
    /// Consumes `self` so the transport cannot be used after close.
    pub async fn close(self) -> Result<TransportOutput, TransportError> {
        // Dropping stdin sends EOF to the child. Some agents (Claude CLI in
        // stream-json mode) terminate naturally on stdin close; others run
        // until they emit a result event. We don't kill -- the caller
        // composes timeouts on top if needed.
        {
            let mut stdin_slot = self.stdin.lock().await;
            stdin_slot.take();
        }

        let exit_code = {
            let mut child_slot = self.child.lock().await;
            match child_slot.take() {
                Some(mut child) => {
                    let status = child.wait().await.map_err(|e| {
                        TransportError::Close(format!("failed waiting for child: {e}"))
                    })?;
                    status.code().unwrap_or(-1)
                }
                None => -1,
            }
        };

        // Drain reader tasks so all stderr/stdout is captured before we read.
        if let Some(handle) = self.stdout_reader.lock().await.take() {
            let _ = handle.await;
        }
        if let Some(handle) = self.stderr_reader.lock().await.take() {
            let _ = handle.await;
        }

        let stderr = self.stderr_buf.lock().await.clone();

        Ok(TransportOutput { exit_code, stderr })
    }
}

impl Transport for StreamJsonTransport {
    async fn send(&self, message: &str) -> Result<(), TransportError> {
        // Stream-json user message envelope. serde_json handles escaping for
        // quotes, backslashes, control chars -- a hand-built string would
        // break on any apostrophe in the message.
        let envelope = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": message,
            }
        });
        let mut line = serde_json::to_string(&envelope)
            .map_err(|e| TransportError::Send(format!("failed to encode message: {e}")))?;
        line.push('\n');

        let mut stdin_slot = self.stdin.lock().await;
        let stdin = stdin_slot
            .as_mut()
            .ok_or_else(|| TransportError::Send("transport stdin already closed".to_string()))?;

        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| TransportError::Send(format!("write failed: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| TransportError::Send(format!("flush failed: {e}")))?;
        Ok(())
    }

    async fn recv(&self) -> Result<Option<TransportEvent>, TransportError> {
        let mut rx = self.rx.lock().await;
        Ok(rx.recv().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Helper: build a tokio Command for a shell script.
    fn sh(script: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[tokio::test]
    async fn send_then_recv_round_trips_through_child() {
        // Acceptance: send a message, the child sees it on stdin, and the
        // transport surfaces a fragment back via recv.
        //
        // The script reads one line of stdin, then emits a stream-json
        // result event so the test can assert on the parsed fragment.
        let script = r#"
read -r line
printf '%s\n' '{"type":"result","subtype":"success","result":"ack"}'
"#;
        let transport = StreamJsonTransport::spawn(sh(script)).await.unwrap();

        transport.send("ping").await.unwrap();

        // First recv should yield the result fragment.
        let event = transport.recv().await.unwrap();
        assert_eq!(event, Some(Fragment::Result("ack".to_string())));

        // After the child exits, stdout closes and recv returns None.
        let after_eof = tokio::time::timeout(Duration::from_secs(2), transport.recv())
            .await
            .expect("recv should not hang past child exit")
            .unwrap();
        assert_eq!(after_eof, None);

        let out = transport.close().await.unwrap();
        assert_eq!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn send_serializes_message_as_stream_json_user_envelope() {
        // The protocol sent on stdin must be a proper stream-json user
        // message line. We capture the script's stdin to a tempfile and
        // verify the JSON shape -- if this drifts, the router will not be
        // able to talk to Claude CLI in --input-format stream-json mode.
        //
        // Capture via tempfile (not echo-back) so the test stays portable
        // across shells without depending on python/awk for JSON escaping.
        let tmp = tempfile::TempDir::new().unwrap();
        let captured = tmp.path().join("stdin.txt");
        let captured_str = captured.display().to_string();

        let script = format!(
            r#"
read -r line
printf '%s' "$line" > '{captured_str}'
printf '%s\n' '{{"type":"result","subtype":"success","result":"ok"}}'
"#
        );
        let transport = StreamJsonTransport::spawn(sh(&script)).await.unwrap();

        transport.send("hello \"world\"").await.unwrap();

        // Wait for the result event so we know the script finished writing.
        let event = transport.recv().await.unwrap();
        assert_eq!(event, Some(Fragment::Result("ok".to_string())));

        let captured_line = tokio::fs::read_to_string(&captured).await.unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&captured_line).expect("stdin line must be valid JSON");
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["role"], "user");
        assert_eq!(parsed["message"]["content"], "hello \"world\"");

        let _ = transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn recv_returns_none_on_eof() {
        // When the child exits without further output, recv must yield None
        // promptly (not hang). This is how the router detects end-of-turn.
        let transport = StreamJsonTransport::spawn(sh("exit 0")).await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(2), transport.recv())
            .await
            .expect("recv should not hang past child exit")
            .unwrap();
        assert_eq!(event, None);

        let out = transport.close().await.unwrap();
        assert_eq!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn recv_yields_multiple_fragments_in_order() {
        // The parser can produce multiple fragments per turn (text deltas
        // followed by a result). Transport must surface them in arrival
        // order, one per recv call.
        let script = r#"
printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"part1 "}}}'
printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"part2"}}}'
printf '%s\n' '{"type":"result","subtype":"success","result":"part1 part2"}'
"#;
        let transport = StreamJsonTransport::spawn(sh(script)).await.unwrap();

        let mut collected = Vec::new();
        while let Some(event) = transport.recv().await.unwrap() {
            collected.push(event);
        }

        assert_eq!(
            collected,
            vec![
                Fragment::Text("part1 ".to_string()),
                Fragment::Text("part2".to_string()),
                Fragment::Result("part1 part2".to_string()),
            ]
        );

        let _ = transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn send_after_close_returns_error() {
        // Defensive: once close has dropped stdin, further sends must fail
        // cleanly rather than panicking.
        let transport = StreamJsonTransport::spawn(sh("exit 0")).await.unwrap();

        // Drain stdout to None so close completes cleanly.
        let _ = transport.recv().await.unwrap();

        // Take stdin manually to simulate a half-closed state without
        // consuming the transport (which `close` would do).
        {
            let mut stdin_slot = transport.stdin.lock().await;
            stdin_slot.take();
        }

        let err = transport.send("late").await.unwrap_err();
        assert!(matches!(err, TransportError::Send(_)));

        let _ = transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn close_captures_stderr() {
        // If the agent crashes or warns on stderr, that diagnostic must be
        // available after close -- otherwise the router has no way to tell
        // the user why the agent stopped.
        let script = r#"
printf 'something went wrong\n' >&2
exit 3
"#;
        let transport = StreamJsonTransport::spawn(sh(script)).await.unwrap();

        // Drain stdout (none expected, but recv should reach EOF).
        let _ = transport.recv().await.unwrap();

        let out = transport.close().await.unwrap();
        assert_eq!(out.exit_code, 3);
        assert!(
            out.stderr.contains("something went wrong"),
            "expected stderr to be captured, got: {:?}",
            out.stderr
        );
    }

    #[tokio::test]
    async fn malformed_stdout_lines_are_skipped() {
        // The shared parser already returns no fragments for non-JSON or
        // unknown events. Transport must not crash on those lines.
        let script = r#"
printf '%s\n' 'not json at all'
printf '%s\n' '{"type":"system","subtype":"init"}'
printf '%s\n' '{"type":"result","subtype":"success","result":"ok"}'
"#;
        let transport = StreamJsonTransport::spawn(sh(script)).await.unwrap();

        let event = transport.recv().await.unwrap();
        assert_eq!(event, Some(Fragment::Result("ok".to_string())));

        let after = transport.recv().await.unwrap();
        assert_eq!(after, None);

        let _ = transport.close().await.unwrap();
    }
}
