//! NDJSON audit log for conversation steps (issue #172).
//!
//! The router's [`Logger`](super::router::Logger) callback fires for every
//! inbound fragment, every outbound delivery, every send failure, and the
//! final termination notice. This module turns that stream into the
//! on-disk audit format documented in #172: one JSON object per line at
//! `<run-dir>/messages/<step-id>.ndjson`.
//!
//! ## Why NDJSON
//!
//! Streaming format. The conversation can run for minutes; users want to
//! `tail -f` the file while it grows. NDJSON appends one record at a time
//! with no enclosing container to update, so a partial file is still a
//! valid prefix of a complete file. YAML would require rewriting the whole
//! document on every append.
//!
//! ## Schema
//!
//! Each line is a [`Message`]:
//!
//! ```json
//! {"id":"01HZX...","ts":"2026-04-30T10:00:00Z","from":"Noah",
//!  "to":null,"kind":"message","content":"...","turn":1,"refs":[]}
//! ```
//!
//! The schema is the contract with `kuro show` and any future audit
//! tooling. Field names and the `kind` vocabulary are stable; renaming a
//! variant requires a schema bump.
//!
//! ## Mapping from router events
//!
//! Not every router log entry becomes a message. The router's vocabulary
//! is finer-grained than the audit schema; the audit log only records
//! events that a reader of the conversation transcript would care about:
//!
//! | Router event                | Audit message                                      |
//! |-----------------------------|----------------------------------------------------|
//! | `Inbound{Final}` (agent)    | `kind=message, from=agent_id, to=null`             |
//! | `Inbound{Final}` (human)    | `kind=message, from="user", to=null`               |
//! | `Inbound{ToolUse{name}}`    | `kind=advisory, content="[tool: <name>]"`          |
//! | `Inbound{Partial}`          | skipped (the matching Final carries the same text) |
//! | `Outbound`                  | skipped (duplicates the inbound that triggered it) |
//! | `SendFailed{to,error}`      | `kind=system, from="system", to=<agent>`           |
//! | `Termination{reason}`       | `kind=system, from="system", to=null`              |
//!
//! ## Turn semantics
//!
//! The issue's example log has two agents on `turn:1` and a subsequent
//! human message on `turn:2`. We follow that: the turn starts at 1, and
//! every human-input event bumps it before that human message is written.
//! Agent finals stay on the current turn -- a "turn" here is a round of
//! discourse triggered by either the initial seed or a human nudge, not a
//! single agent emission.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use super::router::{LogEntry, LogKind, MessageKind as RouterMessageKind, Source};
use crate::stack::MESSAGES_SUBDIR;

/// Audit-layer classification. Mirrors the `kind` enum in the issue spec
/// and is intentionally narrower than [`RouterMessageKind`] -- it is the
/// stable vocabulary visible to consumers of the on-disk log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// A participant said something (agent text, human text). The default
    /// kind for normal conversation flow.
    Message,
    /// A non-message advisory: tool usage, status update. Logged so audit
    /// consumers see what the agent did, not just what it said.
    Advisory,
    /// Reserved for future use (e.g. an agent escalates to the human).
    /// Defined in the schema so we don't break consumers when the runner
    /// starts emitting these.
    #[allow(dead_code)]
    Escalation,
    /// Router-internal events: send failures, termination notices.
    System,
}

/// Sentinel `from` value for router-internal entries (send failures,
/// termination). Distinct from [`Source::Router`] in the in-memory log;
/// the on-disk schema uses `"system"` as documented in the issue example
/// (`kind: message | advisory | escalation | system`).
const SYSTEM_FROM: &str = "system";

/// One audit-log entry.
///
/// Field order matches the issue's documented schema so a `serde_json`
/// roundtrip produces stable line layouts. The `Option<String>` for `to`
/// serialises as `null` for broadcast messages (the schema requirement).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    /// ULID -- 26 chars, lexicographically sortable by time, globally
    /// unique without coordination.
    pub id: String,
    /// ISO-8601 UTC timestamp, second precision with `Z` suffix.
    pub ts: String,
    /// Origin: agent name, `"user"`, or `"system"`.
    pub from: String,
    /// Destination: agent name, or `null` for broadcast.
    pub to: Option<String>,
    pub kind: MessageKind,
    pub content: String,
    /// Conversation turn number; see [module docs](self).
    pub turn: u32,
    /// Optional file paths, issue numbers, or other references. Serialised
    /// as `[]` rather than omitted so consumers can index without checking
    /// for the field's presence.
    #[serde(default)]
    pub refs: Vec<String>,
}

/// Generate a fresh ULID. Wrapper kept so the rest of the module does not
/// reach into the [`ulid`] crate directly -- if we ever swap the
/// implementation (e.g. for a deterministic test ULID), this is the seam.
pub fn ulid() -> String {
    ulid::Ulid::new().to_string()
}

/// Filename for a step's audit log: `<step-id>.ndjson`.
pub fn message_log_filename(step_id: &str) -> String {
    format!("{step_id}.ndjson")
}

/// Path to a step's audit log under a run directory.
pub fn message_log_path(run_path: &Path, step_id: &str) -> PathBuf {
    run_path
        .join(MESSAGES_SUBDIR)
        .join(message_log_filename(step_id))
}

/// Append-only NDJSON writer for a single conversation step.
///
/// Owns the file handle, the turn counter, and a message-count tally. The
/// router's logger callback is `Send + Sync`, so internal state is wrapped
/// in `Mutex` -- logger calls are infrequent (one per fragment) and the
/// lock cost is negligible compared to the I/O it protects.
pub struct MessageLogWriter {
    file: Mutex<File>,
    /// Current conversation turn. Bumped before logging each human input
    /// so the human message and any subsequent agent replies share the new
    /// turn number.
    turn: Mutex<u32>,
    /// Number of messages successfully appended. Surfaced in the manifest
    /// summary so the audit count matches reality (a write failure does
    /// NOT increment this).
    count: Mutex<u32>,
}

impl MessageLogWriter {
    /// Open `path` for write, truncating any prior content. The parent
    /// directory is created if missing -- callers normally rely on
    /// `stack::init_run_layout` having done that already, but the
    /// idempotent `create_dir_all` saves a forgotten init from costing the
    /// audit log.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        Ok(Self {
            file: Mutex::new(file),
            turn: Mutex::new(1),
            count: Mutex::new(0),
        })
    }

    /// Append a freshly-built message. Assigns a new ULID, stamps the
    /// current UTC time at second precision, and uses the writer's
    /// current turn counter. The line is flushed so a `tail -f` reader
    /// sees it immediately.
    fn append(
        &self,
        from: &str,
        to: Option<String>,
        kind: MessageKind,
        content: String,
        refs: Vec<String>,
    ) -> std::io::Result<()> {
        let turn = *self.turn.lock().expect("turn mutex poisoned");
        let msg = Message {
            id: ulid(),
            ts: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            from: from.to_string(),
            to,
            kind,
            content,
            turn,
            refs,
        };
        let line = serde_json::to_string(&msg).map_err(std::io::Error::other)?;
        let mut f = self.file.lock().expect("file mutex poisoned");
        writeln!(f, "{line}")?;
        f.flush()?;
        // Only count successfully written messages: a partial write would
        // otherwise inflate the manifest summary.
        *self.count.lock().expect("count mutex poisoned") += 1;
        Ok(())
    }

    /// Total messages successfully written so far.
    pub fn message_count(&self) -> u32 {
        *self.count.lock().expect("count mutex poisoned")
    }

    /// Current turn number. Useful primarily for tests; production callers
    /// don't need to read it because [`record`](Self::record) handles
    /// turn advancement internally.
    #[cfg(test)]
    pub fn current_turn(&self) -> u32 {
        *self.turn.lock().expect("turn mutex poisoned")
    }

    /// Translate one router log entry into an audit message and append it.
    ///
    /// Returns `Ok(true)` when a line was written, `Ok(false)` when the
    /// entry was deliberately dropped (partials, outbound deliveries).
    /// Errors are I/O failures; callers should log them and continue --
    /// the conversation does not abort just because the audit file could
    /// not be flushed.
    pub fn record(&self, entry: &LogEntry) -> std::io::Result<bool> {
        match &entry.kind {
            LogKind::Inbound { content, message } => match message {
                RouterMessageKind::Partial => Ok(false),
                RouterMessageKind::Final => {
                    // Human input opens a fresh round: bump the turn
                    // before logging so the human message and subsequent
                    // agent replies share the new turn number, matching
                    // the issue's example (`Noah/Levi turn:1`, `user
                    // turn:2`).
                    if matches!(entry.from, Source::Human) {
                        *self.turn.lock().expect("turn mutex poisoned") += 1;
                    }
                    let from = entry.from.to_string();
                    self.append(
                        &from,
                        None,
                        MessageKind::Message,
                        content.clone(),
                        Vec::new(),
                    )?;
                    Ok(true)
                }
                RouterMessageKind::ToolUse { name } => {
                    let from = entry.from.to_string();
                    let content = format!("[tool: {name}]");
                    self.append(&from, None, MessageKind::Advisory, content, Vec::new())?;
                    Ok(true)
                }
            },
            LogKind::Outbound { .. } => Ok(false),
            LogKind::SendFailed { to, error } => {
                let content = format!("failed to deliver to {to}: {error}");
                self.append(
                    SYSTEM_FROM,
                    Some(to.clone()),
                    MessageKind::System,
                    content,
                    Vec::new(),
                )?;
                Ok(true)
            }
            LogKind::Termination { reason } => {
                let content = format!("terminated: {reason}");
                self.append(SYSTEM_FROM, None, MessageKind::System, content, Vec::new())?;
                Ok(true)
            }
        }
    }
}

/// Read an NDJSON message log into a `Vec<Message>`.
///
/// Used by `kuro show` and tests. Empty / whitespace-only lines are
/// tolerated (a half-flushed last line is normal during streaming reads),
/// but a line that parses as anything other than a [`Message`] is a hard
/// error: silently dropping malformed entries would mask schema drift.
pub fn read_message_log(path: &Path) -> std::io::Result<Vec<Message>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Message = serde_json::from_str(&line)
            .map_err(|e| std::io::Error::other(format!("{}:{}: {e}", path.display(), i + 1)))?;
        messages.push(msg);
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::router::{MessageKind as RouterMessageKind, TerminationReason};
    use tempfile::TempDir;

    fn writer(dir: &TempDir, step_id: &str) -> (MessageLogWriter, PathBuf) {
        std::fs::create_dir_all(dir.path().join(MESSAGES_SUBDIR)).unwrap();
        let path = message_log_path(dir.path(), step_id);
        let w = MessageLogWriter::create(&path).unwrap();
        (w, path)
    }

    /// Acceptance: ULIDs are 26 chars and lexicographically sortable by
    /// time. Two ULIDs minted milliseconds apart must compare in
    /// generation order.
    #[test]
    fn ulid_is_26_chars_and_time_sortable() {
        let a = ulid();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = ulid();
        assert_eq!(a.len(), 26);
        assert_eq!(b.len(), 26);
        assert!(a < b, "ULIDs must sort by time: {a} >= {b}");
    }

    /// Acceptance: file location follows the run-dir layout from #164.
    /// Path is `<run>/messages/<step-id>.ndjson`.
    #[test]
    fn message_log_path_uses_messages_subdir() {
        let path = message_log_path(Path::new("/runs/01HZ"), "design");
        assert_eq!(
            path,
            Path::new("/runs/01HZ")
                .join(MESSAGES_SUBDIR)
                .join("design.ndjson")
        );
    }

    /// Final from an agent becomes a `kind=message` line with broadcast
    /// destination (`to=null`).
    #[test]
    fn agent_final_becomes_message_with_null_to() {
        let dir = TempDir::new().unwrap();
        let (w, path) = writer(&dir, "step");

        let entry = LogEntry {
            from: Source::Agent("Noah".into()),
            kind: LogKind::Inbound {
                content: "I propose option A.".into(),
                message: RouterMessageKind::Final,
            },
        };
        assert!(w.record(&entry).unwrap());

        let msgs = read_message_log(&path).unwrap();
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!(m.from, "Noah");
        assert_eq!(m.to, None);
        assert_eq!(m.kind, MessageKind::Message);
        assert_eq!(m.content, "I propose option A.");
        assert_eq!(m.turn, 1);
        assert_eq!(m.id.len(), 26);
        assert!(m.ts.ends_with('Z'), "ts must be UTC: {}", m.ts);
    }

    /// Acceptance #171 carryover: human input is logged as `from="user"`.
    /// The mapping flows through `Source::Display`, so a regression there
    /// would surface here too.
    #[test]
    fn human_final_from_is_user() {
        let dir = TempDir::new().unwrap();
        let (w, path) = writer(&dir, "step");

        let entry = LogEntry {
            from: Source::Human,
            kind: LogKind::Inbound {
                content: "focus on tests".into(),
                message: RouterMessageKind::Final,
            },
        };
        assert!(w.record(&entry).unwrap());

        let msgs = read_message_log(&path).unwrap();
        assert_eq!(msgs[0].from, "user");
        assert_eq!(msgs[0].kind, MessageKind::Message);
    }

    /// Tool-use becomes an `advisory` so audit consumers can show "agent
    /// X used tool Y" without re-parsing free-form text.
    #[test]
    fn tool_use_becomes_advisory() {
        let dir = TempDir::new().unwrap();
        let (w, path) = writer(&dir, "step");

        let entry = LogEntry {
            from: Source::Agent("Levi".into()),
            kind: LogKind::Inbound {
                content: String::new(),
                message: RouterMessageKind::ToolUse {
                    name: "read_file".into(),
                },
            },
        };
        assert!(w.record(&entry).unwrap());

        let msgs = read_message_log(&path).unwrap();
        assert_eq!(msgs[0].kind, MessageKind::Advisory);
        assert_eq!(msgs[0].content, "[tool: read_file]");
        assert_eq!(msgs[0].from, "Levi");
    }

    /// Streaming partials and outbound deliveries are skipped: their text
    /// is already covered by the corresponding Final / inbound, and
    /// including them would inflate the message count.
    #[test]
    fn partial_and_outbound_are_skipped() {
        let dir = TempDir::new().unwrap();
        let (w, path) = writer(&dir, "step");

        let partial = LogEntry {
            from: Source::Agent("Noah".into()),
            kind: LogKind::Inbound {
                content: "thinking...".into(),
                message: RouterMessageKind::Partial,
            },
        };
        let outbound = LogEntry {
            from: Source::Router,
            kind: LogKind::Outbound {
                to: "Levi".into(),
                content: "hi".into(),
            },
        };

        assert!(!w.record(&partial).unwrap(), "partial must be skipped");
        assert!(!w.record(&outbound).unwrap(), "outbound must be skipped");

        let msgs = read_message_log(&path).unwrap();
        assert!(msgs.is_empty(), "no audit lines for partial/outbound");
        assert_eq!(w.message_count(), 0);
    }

    /// Send-failure is a system event addressed to the agent that could
    /// not be reached.
    #[test]
    fn send_failed_becomes_system_with_to_field() {
        let dir = TempDir::new().unwrap();
        let (w, path) = writer(&dir, "step");

        let entry = LogEntry {
            from: Source::Router,
            kind: LogKind::SendFailed {
                to: "Mika".into(),
                error: "transport closed".into(),
            },
        };
        assert!(w.record(&entry).unwrap());

        let msgs = read_message_log(&path).unwrap();
        assert_eq!(msgs[0].kind, MessageKind::System);
        assert_eq!(msgs[0].from, "system");
        assert_eq!(msgs[0].to.as_deref(), Some("Mika"));
        assert!(msgs[0].content.contains("transport closed"));
    }

    /// Termination produces a final `system` line so a reader knows the
    /// log is complete and which reason ended it.
    #[test]
    fn termination_becomes_system_with_reason_in_content() {
        let dir = TempDir::new().unwrap();
        let (w, path) = writer(&dir, "step");

        let entry = LogEntry {
            from: Source::Router,
            kind: LogKind::Termination {
                reason: TerminationReason::Convergence,
            },
        };
        assert!(w.record(&entry).unwrap());

        let msgs = read_message_log(&path).unwrap();
        assert_eq!(msgs[0].kind, MessageKind::System);
        assert_eq!(msgs[0].from, "system");
        assert_eq!(msgs[0].to, None);
        // Reason rendered via Display, the stable on-disk string.
        assert!(
            msgs[0].content.contains("convergence"),
            "got: {}",
            msgs[0].content
        );
    }

    /// Acceptance: turn semantics from the issue's example. Two agent
    /// finals share `turn=1`; a subsequent human input bumps to `turn=2`,
    /// and a following agent final stays on `turn=2`.
    #[test]
    fn turn_advances_only_on_human_input() {
        let dir = TempDir::new().unwrap();
        let (w, path) = writer(&dir, "step");

        let noah = LogEntry {
            from: Source::Agent("Noah".into()),
            kind: LogKind::Inbound {
                content: "A".into(),
                message: RouterMessageKind::Final,
            },
        };
        let levi = LogEntry {
            from: Source::Agent("Levi".into()),
            kind: LogKind::Inbound {
                content: "B".into(),
                message: RouterMessageKind::Final,
            },
        };
        let user = LogEntry {
            from: Source::Human,
            kind: LogKind::Inbound {
                content: "next topic".into(),
                message: RouterMessageKind::Final,
            },
        };
        let noah2 = LogEntry {
            from: Source::Agent("Noah".into()),
            kind: LogKind::Inbound {
                content: "C".into(),
                message: RouterMessageKind::Final,
            },
        };

        for e in [&noah, &levi, &user, &noah2] {
            w.record(e).unwrap();
        }

        let msgs = read_message_log(&path).unwrap();
        assert_eq!(
            msgs.iter().map(|m| m.turn).collect::<Vec<_>>(),
            vec![1, 1, 2, 2]
        );
    }

    /// Acceptance: messages are NDJSON -- one JSON object per line. The
    /// raw file must parse line-by-line; concatenating into a single JSON
    /// document would not be valid.
    #[test]
    fn file_is_ndjson_one_object_per_line() {
        let dir = TempDir::new().unwrap();
        let (w, path) = writer(&dir, "step");

        for content in ["one", "two", "three"] {
            let entry = LogEntry {
                from: Source::Agent("Noah".into()),
                kind: LogKind::Inbound {
                    content: content.into(),
                    message: RouterMessageKind::Final,
                },
            };
            w.record(&entry).unwrap();
        }

        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            // Each line must be a self-contained JSON object.
            let _: Message = serde_json::from_str(line).unwrap();
        }
        assert_eq!(w.message_count(), 3);
    }

    /// The reader must round-trip the writer's output byte-for-byte at
    /// the message level. Pins the schema: a field rename in either
    /// direction breaks this test.
    #[test]
    fn read_message_log_roundtrips_writer_output() {
        let dir = TempDir::new().unwrap();
        let (w, path) = writer(&dir, "step");

        for entry in [
            LogEntry {
                from: Source::Agent("Noah".into()),
                kind: LogKind::Inbound {
                    content: "hello".into(),
                    message: RouterMessageKind::Final,
                },
            },
            LogEntry {
                from: Source::Human,
                kind: LogKind::Inbound {
                    content: "user nudge".into(),
                    message: RouterMessageKind::Final,
                },
            },
            LogEntry {
                from: Source::Agent("Levi".into()),
                kind: LogKind::Inbound {
                    content: String::new(),
                    message: RouterMessageKind::ToolUse {
                        name: "Read".into(),
                    },
                },
            },
        ] {
            w.record(&entry).unwrap();
        }

        let msgs = read_message_log(&path).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].from, "Noah");
        assert_eq!(msgs[0].turn, 1);
        assert_eq!(msgs[1].from, "user");
        assert_eq!(msgs[1].turn, 2);
        assert_eq!(msgs[2].kind, MessageKind::Advisory);
    }

    /// A line that fails to parse must surface as an error rather than
    /// being silently dropped -- otherwise schema drift in the writer
    /// would never be noticed by the reader.
    #[test]
    fn read_message_log_errors_on_malformed_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.ndjson");
        std::fs::write(&path, "{ this is not json }\n").unwrap();
        let err = read_message_log(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
    }

    /// Ignoring blank lines lets a `tail -f` reader cope with a truncated
    /// final line during streaming reads. A trailing newline is the
    /// common case; verify it does not produce a phantom empty message.
    #[test]
    fn read_message_log_tolerates_blank_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("with-blanks.ndjson");
        let m = Message {
            id: ulid(),
            ts: "2026-04-30T10:00:00Z".into(),
            from: "Noah".into(),
            to: None,
            kind: MessageKind::Message,
            content: "hi".into(),
            turn: 1,
            refs: vec![],
        };
        let body = format!("\n{}\n   \n", serde_json::to_string(&m).unwrap());
        std::fs::write(&path, body).unwrap();
        let msgs = read_message_log(&path).unwrap();
        assert_eq!(msgs.len(), 1);
    }

    /// Schema sanity: the JSON `kind` discriminant uses snake_case
    /// strings (`message`, `advisory`, `system`), not the Rust variant
    /// names. Pinning prevents an accidental serde rename from breaking
    /// historical files on disk.
    #[test]
    fn kind_serialises_as_snake_case() {
        let m = Message {
            id: "01HZ".into(),
            ts: "2026-04-30T10:00:00Z".into(),
            from: "Noah".into(),
            to: None,
            kind: MessageKind::Message,
            content: "hi".into(),
            turn: 1,
            refs: vec![],
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"kind\":\"message\""), "got: {json}");

        let mut adv = m.clone();
        adv.kind = MessageKind::Advisory;
        assert!(
            serde_json::to_string(&adv)
                .unwrap()
                .contains("\"kind\":\"advisory\"")
        );

        let mut sys = m.clone();
        sys.kind = MessageKind::System;
        assert!(
            serde_json::to_string(&sys)
                .unwrap()
                .contains("\"kind\":\"system\"")
        );
    }

    /// Refs default to an empty array on read. Keeps consumers from
    /// having to special-case missing fields when the writer chose to
    /// omit them.
    #[test]
    fn refs_defaults_to_empty_when_missing() {
        let line = r#"{"id":"01H","ts":"2026-04-30T10:00:00Z","from":"Noah","to":null,"kind":"message","content":"hi","turn":1}"#;
        let m: Message = serde_json::from_str(line).unwrap();
        assert!(m.refs.is_empty());
    }

    /// Initial turn is 1, matching the issue's example. A reader that
    /// has not yet seen a human-input event must see `turn=1` on every
    /// recorded line.
    #[test]
    fn initial_turn_is_one() {
        let dir = TempDir::new().unwrap();
        let (w, _) = writer(&dir, "step");
        assert_eq!(w.current_turn(), 1);
    }
}
