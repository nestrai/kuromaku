//! Broadcast message router for multi-agent conversations.
//!
//! The router owns N [`Transport`]s plus an optional human-input channel and
//! relays messages between them. It is the layer that turns "several agents
//! running concurrently" into "agents in a conversation": forwarding what
//! each agent says to the others, detecting when the conversation should
//! end, and logging the whole exchange for audit.
//!
//! ## Routing
//!
//! Every [`Fragment::Result`] (the canonical assistant text emitted at the
//! end of a turn) is broadcast to all other agents. Intermediate fragments
//! (text deltas, tool-use announcements) are logged but not routed --
//! forwarding partial deltas would inject half-finished sentences into the
//! receivers' input streams.
//!
//! No `@AgentName` directed messages in this phase (#169 scope). The
//! conversation is fully broadcast.
//!
//! ## Termination
//!
//! [`run`](Router::run) returns when one of these conditions hits, in
//! priority order:
//!
//! 1. **MaxTurns** -- total number of agent results reached the configured
//!    cap. Hard limit; always respected.
//! 2. **Convergence** -- every agent has produced at least one result AND
//!    every agent's most recent result text (trimmed) equals `DONE`. New
//!    human input resets the latest-result map so convergence is
//!    re-evaluated only after agents respond to the new input.
//! 3. **Timeout** -- no event arrived within `turn_timeout`. Acts as a
//!    wall-clock safety net; it is not a per-agent stopwatch.
//! 4. **AllAgentsClosed** -- every agent's transport reached EOF. Happens
//!    when the underlying CLIs all exited.
//! 5. **HumanClosed** -- the human-input channel was closed (sender
//!    dropped). Only relevant when a human channel was attached.
//!
//! ## Logging
//!
//! Every inbound fragment, every outbound message, every send failure, and
//! the final termination reason go through the [`Logger`] callback. The
//! callback is the only place audit data leaves the router; persisting that
//! audit trail to disk is #170 and is intentionally out of scope here.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::executor::stream_json::Fragment;
use crate::executor::transport::{Transport, TransportError, TransportEvent};

/// Stable identifier for an agent participating in the conversation.
///
/// Strings rather than indexes: log entries and config files reference
/// agents by name (`developer`, `reviewer`), and translating to and from
/// indexes at the boundary just adds noise.
pub type AgentId = String;

// --- Logging ---

/// Origin of a logged message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// One of the registered agents.
    Agent(AgentId),
    /// The human participant (text from the human-input channel).
    Human,
    /// The router itself (outbound deliveries, termination notices).
    Router,
}

/// Stable string form, distinct from `Debug`. Audit transcripts render the
/// source via `Display` so the on-disk identifier (`"user"` for the human,
/// agent id verbatim, `"router"` for the router) stays frozen even if the
/// in-source variant names change. Same rationale as
/// [`TerminationReason::Display`].
///
/// Acceptance (#171): human messages in the audit log identify their origin
/// as `"user"`. Centralised here so persistence consumers (#170 and later)
/// pick up a single canonical mapping.
impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Agent(id) => f.write_str(id),
            Source::Human => f.write_str("user"),
            Source::Router => f.write_str("router"),
        }
    }
}

/// Stable classification of an inbound message, decoupled from the
/// executor's wire format.
///
/// The router observes [`Fragment`]s coming off the transport, but audit
/// consumers (#170 persistence, future TUI views) should not be locked to
/// that schema. `Fragment` is the executor's on-the-wire representation
/// and is allowed to evolve; `MessageKind` is the messaging layer's stable
/// view of "what kind of thing was said".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageKind {
    /// Streaming partial text mid-turn. No turn boundary, not routed.
    Partial,
    /// Agent invoked a tool. `name` is the tool name as the agent reported
    /// it; preserved because audit views typically surface tool activity.
    ToolUse { name: String },
    /// Final, canonical text for a turn. The unit routing decisions key on
    /// (broadcast, convergence, max_turns). Also used for human messages,
    /// which are complete utterances rather than streaming deltas.
    Final,
}

/// Kind of logged event.
///
/// Separate from [`Source`] so a single entry says both "who" and "what":
/// `Source::Router + LogKind::Outbound { to: "reviewer", ... }` is a
/// router-issued delivery, while `Source::Agent("reviewer") +
/// LogKind::Inbound` is something the reviewer said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogKind {
    /// A message received from an agent or human, with its rendered text.
    /// `message` classifies it (partial, tool-use, final) so audit
    /// consumers can distinguish a final result from a streaming delta
    /// without re-parsing the text, and without depending on the
    /// executor's [`Fragment`] schema.
    Inbound {
        content: String,
        message: MessageKind,
    },
    /// Router delivered `content` to `to`.
    Outbound { to: AgentId, content: String },
    /// Router tried to deliver to `to` but the transport rejected the send.
    /// Logged so audit consumers see partial-broadcast situations; the
    /// router itself continues with the remaining agents.
    SendFailed { to: AgentId, error: String },
    /// Loop terminated for `reason`.
    Termination { reason: TerminationReason },
}

/// Single audit-log entry produced by the router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub from: Source,
    pub kind: LogKind,
}

/// Sink for log entries.
///
/// Arc rather than Box so the router can clone the handle into reader tasks
/// without giving up exclusive ownership in the main loop.
pub type Logger = Arc<dyn Fn(LogEntry) + Send + Sync>;

/// Discard-everything logger. Useful when the caller does not care about
/// audit output (tests, throwaway runs).
pub fn null_logger() -> Logger {
    Arc::new(|_| {})
}

// --- Configuration ---

#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// Hard cap on total agent results across the whole conversation.
    /// Hit before convergence checks, so a runaway loop cannot exceed it.
    pub max_turns: usize,
    /// Wall-clock idle timeout. Resets on every event (any fragment, any
    /// human input). 10 minutes is the issue's documented default.
    pub turn_timeout: Duration,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            max_turns: 20,
            turn_timeout: Duration::from_secs(600),
        }
    }
}

// --- Termination ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminationReason {
    /// Reached `RouterConfig::max_turns`.
    MaxTurns,
    /// All agents' most recent results trimmed to `DONE`.
    Convergence,
    /// No event for `RouterConfig::turn_timeout`.
    Timeout,
    /// Human input channel closed (sender dropped).
    HumanClosed,
    /// Every agent transport reached EOF.
    AllAgentsClosed,
}

/// Stable string form, distinct from `Debug`. Audit transcripts (#170) and
/// any future user-facing surfaces render the reason via `Display` so
/// renaming a variant in source does not silently mutate historical files
/// on disk. Variant rename = compile-time touch here; the on-disk strings
/// stay frozen.
impl std::fmt::Display for TerminationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            TerminationReason::MaxTurns => "max_turns",
            TerminationReason::Convergence => "convergence",
            TerminationReason::Timeout => "timeout",
            TerminationReason::HumanClosed => "human_closed",
            TerminationReason::AllAgentsClosed => "all_agents_closed",
        };
        f.write_str(s)
    }
}

// --- Router ---

/// Object-safe wrapper around [`Transport`].
///
/// The public [`Transport`] trait uses `impl Future` (return-position impl
/// trait), which makes it dyn-incompatible. The router needs to hold a
/// heterogeneous set of transports (mixed-backend flows: claude-cli alongside
/// api alongside ollama, see #170 and kuromaku's "backend-agnostic" principle
/// in CLAUDE.md), so we type-erase here with a `Pin<Box<dyn Future>>` shim.
///
/// This is internal: callers still interact with the [`Transport`] trait. The
/// blanket impl below makes any `T: Transport + Send + Sync + 'static` usable
/// as `Arc<dyn DynTransport>` without touching transport.rs.
trait DynTransport: Send + Sync {
    fn send<'a>(
        &'a self,
        message: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send + 'a>>;

    fn recv(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<TransportEvent>, TransportError>> + Send + '_>>;
}

impl<T: Transport + Send + Sync + 'static> DynTransport for T {
    fn send<'a>(
        &'a self,
        message: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send + 'a>> {
        Box::pin(<T as Transport>::send(self, message))
    }

    fn recv(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<TransportEvent>, TransportError>> + Send + '_>>
    {
        Box::pin(<T as Transport>::recv(self))
    }
}

struct Agent {
    id: AgentId,
    /// Arc<dyn> so the per-agent reader task can hold a handle while the main
    /// loop also reaches in to call `send`, and so the router can mix
    /// transports from different backends in one conversation. Concurrent
    /// send/recv on the same transport is part of the [`Transport`] contract.
    transport: Arc<dyn DynTransport>,
}

pub struct Router {
    agents: Vec<Agent>,
    human_input: Option<mpsc::Receiver<String>>,
    config: RouterConfig,
    logger: Logger,
}

impl Router {
    pub fn new(config: RouterConfig, logger: Logger) -> Self {
        Self {
            agents: Vec::new(),
            human_input: None,
            config,
            logger,
        }
    }

    /// Register an agent under `id`, taking ownership of its transport.
    ///
    /// The transport is type-erased internally so the router can hold agents
    /// backed by different [`Transport`] implementations in the same run.
    /// The caller surrenders ownership; transports cannot be inspected from
    /// outside the router afterwards.
    pub fn add_agent<T: Transport + Send + Sync + 'static>(
        &mut self,
        id: impl Into<AgentId>,
        transport: T,
    ) {
        self.agents.push(Agent {
            id: id.into(),
            transport: Arc::new(transport),
        });
    }

    /// Attach an mpsc receiver for human input. Each item sent on the
    /// channel is treated as a single human message and broadcast to all
    /// agents. Closing the sender terminates the loop with
    /// [`TerminationReason::HumanClosed`].
    pub fn set_human_input(&mut self, rx: mpsc::Receiver<String>) {
        self.human_input = Some(rx);
    }

    /// Drive the conversation until one termination condition fires.
    ///
    /// `initial_prompt`, when set, is broadcast to all agents before the
    /// main loop starts. Without it, the conversation has no seed and
    /// nothing happens until an agent emits something on its own (rare) or
    /// human input arrives.
    ///
    /// Consumes `self`: a router cannot be reused after run because reader
    /// tasks have already attached to the transports' recv halves.
    pub async fn run(self, initial_prompt: Option<&str>) -> TerminationReason {
        let Self {
            agents,
            mut human_input,
            config,
            logger,
        } = self;

        let agents = Arc::new(agents);
        let n_agents = agents.len();

        // Funnel for events from all agent reader tasks.
        let (events_tx, mut events_rx) = mpsc::channel::<(AgentId, TransportEvent)>(64);
        // Broadcast cancel signal so reader tasks can shut down promptly
        // when the main loop terminates without waiting for transport EOF.
        let (cancel_tx, _) = broadcast::channel::<()>(1);

        let mut reader_handles: Vec<JoinHandle<()>> = Vec::with_capacity(n_agents);
        for agent in agents.iter() {
            let id = agent.id.clone();
            let transport = Arc::clone(&agent.transport);
            let tx = events_tx.clone();
            let mut cancel_rx = cancel_tx.subscribe();
            reader_handles.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel_rx.recv() => break,
                        res = transport.recv() => match res {
                            Ok(Some(event)) => {
                                if tx.send((id.clone(), event)).await.is_err() {
                                    // Main loop dropped events_rx; stop
                                    // pulling from this transport.
                                    break;
                                }
                            }
                            // EOF or error: stop reading. The send-side of
                            // the channel will drop with this task, and the
                            // main loop will see AllAgentsClosed once every
                            // reader has dropped its clone.
                            Ok(None) | Err(_) => break,
                        }
                    }
                }
            }));
        }
        // Drop the original tx so events_rx closes once every reader task
        // has dropped its clone.
        drop(events_tx);

        // Initial seed.
        if let Some(prompt) = initial_prompt {
            broadcast(&agents, &logger, prompt, None).await;
        }

        let mut turn_count: usize = 0;
        let mut last_result: HashMap<AgentId, String> = HashMap::new();

        let reason = loop {
            let timer = tokio::time::sleep(config.turn_timeout);
            tokio::pin!(timer);

            tokio::select! {
                // `biased` so timeout is checked first when multiple
                // branches are ready. Without it, a never-resolving
                // human-input future could starve the timer.
                biased;
                _ = &mut timer => break TerminationReason::Timeout,
                event = events_rx.recv() => {
                    let (from, event) = match event {
                        Some(ev) => ev,
                        None => break TerminationReason::AllAgentsClosed,
                    };
                    log(&logger, LogEntry {
                        from: Source::Agent(from.clone()),
                        kind: LogKind::Inbound {
                            content: fragment_text(&event),
                            message: message_kind(&event),
                        },
                    });
                    if let Fragment::Result(text) = &event {
                        turn_count += 1;
                        last_result.insert(from.clone(), text.clone());

                        // Deliver the message to the other agents BEFORE
                        // evaluating termination. Otherwise the turn that
                        // ends the loop never reaches the other agents,
                        // which violates the broadcast invariant
                        // (acceptance: a message from A reaches B and C).
                        // This means broadcasts on the very last turn
                        // happen even though the loop exits afterwards;
                        // that is the correct trade-off for a "every
                        // message gets delivered" router.
                        broadcast(&agents, &logger, text, Some(&from)).await;

                        // max_turns is the hard cap; check it before
                        // convergence so it always dominates.
                        if turn_count >= config.max_turns {
                            break TerminationReason::MaxTurns;
                        }
                        if n_agents > 0
                            && last_result.len() == n_agents
                            && last_result.values().all(|t| t.trim() == "DONE")
                        {
                            break TerminationReason::Convergence;
                        }
                    }
                }
                human = async {
                    match &mut human_input {
                        Some(rx) => rx.recv().await,
                        // No human channel: never resolve. The other select
                        // branches drive progress.
                        None => std::future::pending::<Option<String>>().await,
                    }
                } => {
                    let content = match human {
                        Some(c) => c,
                        None => break TerminationReason::HumanClosed,
                    };
                    log(&logger, LogEntry {
                        from: Source::Human,
                        kind: LogKind::Inbound {
                            content: content.clone(),
                            // Human input is a complete utterance, so it
                            // classifies as Final. Source::Human already
                            // marks the origin; MessageKind only carries
                            // the shape of the message.
                            message: MessageKind::Final,
                        },
                    });
                    // New human input opens a fresh round: clear the
                    // last-result map so convergence requires every agent
                    // to respond again before terminating.
                    last_result.clear();
                    broadcast(&agents, &logger, &content, None).await;
                }
            }
        };

        log(
            &logger,
            LogEntry {
                from: Source::Router,
                kind: LogKind::Termination {
                    reason: reason.clone(),
                },
            },
        );

        // Tell readers to stop, then wait so we don't leak tasks.
        //
        // Drop `events_rx` first. A reader that already pulled an event out
        // of `transport.recv()` is now inside `tx.send(..).await` and no
        // longer in the surrounding `select!`, so the cancel signal cannot
        // reach it. If the events channel is full at that moment, the send
        // would block forever. Dropping the receiver closes the channel,
        // which makes the pending send return Err and the reader exits.
        drop(events_rx);
        let _ = cancel_tx.send(());
        for handle in reader_handles {
            let _ = handle.await;
        }

        reason
    }
}

// --- Helpers ---

/// Send `content` to every agent except `except` (when set), logging each
/// delivery. Continues past send failures so one closed transport cannot
/// abort the broadcast to the rest.
async fn broadcast(agents: &[Agent], logger: &Logger, content: &str, except: Option<&str>) {
    for agent in agents {
        if Some(agent.id.as_str()) == except {
            continue;
        }
        match agent.transport.send(content).await {
            Ok(()) => {
                log(
                    logger,
                    LogEntry {
                        from: Source::Router,
                        kind: LogKind::Outbound {
                            to: agent.id.clone(),
                            content: content.to_string(),
                        },
                    },
                );
            }
            Err(err) => {
                log(
                    logger,
                    LogEntry {
                        from: Source::Router,
                        kind: LogKind::SendFailed {
                            to: agent.id.clone(),
                            error: err.to_string(),
                        },
                    },
                );
            }
        }
    }
}

fn log(logger: &Logger, entry: LogEntry) {
    (logger)(entry);
}

/// Render a fragment as a human-readable string for log entries. The
/// canonical text for routing decisions still comes from `Fragment::Result`
/// directly -- this is just for audit-friendly display.
fn fragment_text(fragment: &Fragment) -> String {
    match fragment {
        Fragment::Text(s) => s.clone(),
        Fragment::ToolUse { name } => format!("[tool: {name}]"),
        Fragment::Result(s) => s.clone(),
    }
}

/// Project the executor's [`Fragment`] onto the messaging layer's stable
/// [`MessageKind`]. The router does this conversion at its boundary so
/// log consumers (and #170 persistence) never see `Fragment` directly.
fn message_kind(fragment: &Fragment) -> MessageKind {
    match fragment {
        Fragment::Text(_) => MessageKind::Partial,
        Fragment::ToolUse { name } => MessageKind::ToolUse { name: name.clone() },
        Fragment::Result(_) => MessageKind::Final,
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::transport::TransportError;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::Mutex as TokioMutex;

    /// Acceptance (#171): the audit log renders human input with
    /// `from: "user"`. The transcript and #170 persistence both go through
    /// `Source::Display`, so pinning the strings here pins the on-disk
    /// identifier across renames.
    #[test]
    fn source_display_is_stable() {
        assert_eq!(format!("{}", Source::Human), "user");
        assert_eq!(format!("{}", Source::Router), "router");
        assert_eq!(format!("{}", Source::Agent("Levi".into())), "Levi");
    }

    /// The audit transcript (#170) renders termination via Display, not
    /// Debug. Lock the strings down so a future variant rename triggers a
    /// failing test instead of silently mutating historical run files.
    #[test]
    fn termination_reason_display_is_stable() {
        assert_eq!(format!("{}", TerminationReason::MaxTurns), "max_turns");
        assert_eq!(format!("{}", TerminationReason::Convergence), "convergence");
        assert_eq!(format!("{}", TerminationReason::Timeout), "timeout");
        assert_eq!(
            format!("{}", TerminationReason::HumanClosed),
            "human_closed"
        );
        assert_eq!(
            format!("{}", TerminationReason::AllAgentsClosed),
            "all_agents_closed"
        );
    }

    /// Mock transport: send pushes onto an inspectable Vec, recv returns
    /// items the test pushed onto a channel ahead of time. Lets tests
    /// drive the router deterministically without spawning subprocesses.
    struct MockTransport {
        sent: Arc<StdMutex<Vec<String>>>,
        incoming: TokioMutex<mpsc::UnboundedReceiver<TransportEvent>>,
    }

    /// External handle so the test can inspect what was sent and inject
    /// what the agent will say next, even after the transport has been
    /// moved into the router.
    struct MockHandle {
        sent: Arc<StdMutex<Vec<String>>>,
        event_tx: mpsc::UnboundedSender<TransportEvent>,
    }

    impl MockHandle {
        fn push(&self, event: TransportEvent) {
            let _ = self.event_tx.send(event);
        }

        fn close(&self) {
            // No-op: dropping the sender is what closes the channel. The
            // helper exists for symmetry; tests typically rely on the
            // Drop on test exit, but holding the handle for inspection
            // keeps the channel open.
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    fn mock() -> (MockTransport, MockHandle) {
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        (
            MockTransport {
                sent: Arc::clone(&sent),
                incoming: TokioMutex::new(rx),
            },
            MockHandle { sent, event_tx: tx },
        )
    }

    impl Transport for MockTransport {
        async fn send(&self, message: &str) -> Result<(), TransportError> {
            // No await between lock and drop, so the std Mutex is safe to
            // hold across this expression.
            self.sent.lock().unwrap().push(message.to_string());
            Ok(())
        }

        async fn recv(&self) -> Result<Option<TransportEvent>, TransportError> {
            Ok(self.incoming.lock().await.recv().await)
        }
    }

    /// Logger that appends entries to a shared Vec the test can read after
    /// run completes.
    fn capturing_logger() -> (Logger, Arc<StdMutex<Vec<LogEntry>>>) {
        let entries: Arc<StdMutex<Vec<LogEntry>>> = Arc::new(StdMutex::new(Vec::new()));
        let entries_for_logger = Arc::clone(&entries);
        let logger: Logger = Arc::new(move |entry| {
            entries_for_logger.lock().unwrap().push(entry);
        });
        (logger, entries)
    }

    fn fast_config() -> RouterConfig {
        RouterConfig {
            max_turns: 20,
            // Short enough to keep the suite snappy, long enough that the
            // happy-path tests do not race the timeout under CI load.
            turn_timeout: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn broadcast_routes_message_to_all_other_agents() {
        // Acceptance: a Result from agent A reaches B and C, but not A
        // itself. This is the core routing invariant.
        let (mt_a, h_a) = mock();
        let (mt_b, h_b) = mock();
        let (mt_c, h_c) = mock();

        let (logger, _) = capturing_logger();
        let mut config = fast_config();
        // Stop after the single result so the loop terminates predictably.
        config.max_turns = 1;
        let mut router = Router::new(config, logger);
        router.add_agent("a", mt_a);
        router.add_agent("b", mt_b);
        router.add_agent("c", mt_c);

        // Agent A speaks; nothing from B and C, so they only ever receive.
        h_a.push(Fragment::Result("hello from A".to_string()));

        let reason = router.run(None).await;
        assert_eq!(reason, TerminationReason::MaxTurns);

        assert_eq!(
            h_a.sent(),
            Vec::<String>::new(),
            "sender must not receive its own message"
        );
        assert_eq!(h_b.sent(), vec!["hello from A".to_string()]);
        assert_eq!(h_c.sent(), vec!["hello from A".to_string()]);

        // Keep handles alive so transport recv channels stay open for the
        // duration of the run; we close implicitly here.
        h_a.close();
        h_b.close();
        h_c.close();
    }

    #[tokio::test]
    async fn max_turns_terminates_loop() {
        // Two agents, both keep emitting non-DONE results. Loop must stop
        // exactly when total result count reaches max_turns.
        let (mt_a, h_a) = mock();
        let (mt_b, h_b) = mock();

        let (logger, _) = capturing_logger();
        let config = RouterConfig {
            max_turns: 3,
            turn_timeout: Duration::from_secs(2),
        };
        let mut router = Router::new(config, logger);
        router.add_agent("a", mt_a);
        router.add_agent("b", mt_b);

        // Pre-load enough results to exceed the cap. Order does not matter
        // for the cap check.
        h_a.push(Fragment::Result("alpha".to_string()));
        h_b.push(Fragment::Result("beta".to_string()));
        h_a.push(Fragment::Result("gamma".to_string()));
        h_b.push(Fragment::Result("delta".to_string()));

        let reason = router.run(None).await;
        assert_eq!(reason, TerminationReason::MaxTurns);
    }

    #[tokio::test]
    async fn convergence_when_all_agents_emit_done() {
        // Three agents, each emits DONE as their result. Router must
        // detect convergence and terminate before max_turns.
        let (mt_a, h_a) = mock();
        let (mt_b, h_b) = mock();
        let (mt_c, h_c) = mock();

        let (logger, _) = capturing_logger();
        let config = RouterConfig {
            max_turns: 100,
            turn_timeout: Duration::from_secs(2),
        };
        let mut router = Router::new(config, logger);
        router.add_agent("a", mt_a);
        router.add_agent("b", mt_b);
        router.add_agent("c", mt_c);

        h_a.push(Fragment::Result("DONE".to_string()));
        h_b.push(Fragment::Result("DONE".to_string()));
        h_c.push(Fragment::Result("DONE".to_string()));

        let reason = router.run(None).await;
        assert_eq!(reason, TerminationReason::Convergence);
    }

    #[tokio::test]
    async fn convergence_requires_every_agent_to_speak() {
        // Two of three agents say DONE. Router must NOT converge -- it
        // waits for the third. We cap turns to force termination so the
        // test does not hang.
        let (mt_a, h_a) = mock();
        let (mt_b, h_b) = mock();
        let (mt_c, h_c) = mock();

        let (logger, _) = capturing_logger();
        let config = RouterConfig {
            // Two results from a/b, then we hit the cap before c speaks.
            max_turns: 2,
            turn_timeout: Duration::from_secs(2),
        };
        let mut router = Router::new(config, logger);
        router.add_agent("a", mt_a);
        router.add_agent("b", mt_b);
        router.add_agent("c", mt_c);

        h_a.push(Fragment::Result("DONE".to_string()));
        h_b.push(Fragment::Result("DONE".to_string()));
        // Note: h_c is silent.

        let reason = router.run(None).await;
        assert_eq!(
            reason,
            TerminationReason::MaxTurns,
            "convergence must wait for every agent to produce a result"
        );

        // Keep h_c alive so its transport recv stays open during the run.
        h_c.close();
    }

    #[tokio::test]
    async fn timeout_when_no_events_arrive() {
        // One agent, no events, no human input. Loop must trip the
        // turn_timeout safety net.
        let (mt_a, h_a) = mock();

        let (logger, _) = capturing_logger();
        let config = RouterConfig {
            max_turns: 100,
            turn_timeout: Duration::from_millis(50),
        };
        let mut router = Router::new(config, logger);
        router.add_agent("a", mt_a);

        let start = std::time::Instant::now();
        let reason = router.run(None).await;
        let elapsed = start.elapsed();

        assert_eq!(reason, TerminationReason::Timeout);
        // Sanity: we waited at least the timeout, but not absurdly longer.
        assert!(elapsed >= Duration::from_millis(50));
        assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");

        h_a.close();
    }

    #[tokio::test]
    async fn logger_receives_all_messages() {
        // Acceptance: every inbound fragment, every outbound delivery, and
        // the termination notice must surface through the logger.
        let (mt_a, h_a) = mock();
        let (mt_b, h_b) = mock();

        let (logger, entries) = capturing_logger();
        let config = RouterConfig {
            max_turns: 1,
            turn_timeout: Duration::from_secs(2),
        };
        let mut router = Router::new(config, logger);
        router.add_agent("a", mt_a);
        router.add_agent("b", mt_b);

        h_a.push(Fragment::Result("hello".to_string()));

        let reason = router.run(Some("kickoff")).await;
        assert_eq!(reason, TerminationReason::MaxTurns);

        let log = entries.lock().unwrap().clone();

        // Initial broadcast: outbound to a and b.
        assert!(log.contains(&LogEntry {
            from: Source::Router,
            kind: LogKind::Outbound {
                to: "a".into(),
                content: "kickoff".into(),
            },
        }));
        assert!(log.contains(&LogEntry {
            from: Source::Router,
            kind: LogKind::Outbound {
                to: "b".into(),
                content: "kickoff".into(),
            },
        }));

        // Agent A's inbound result.
        assert!(log.contains(&LogEntry {
            from: Source::Agent("a".into()),
            kind: LogKind::Inbound {
                content: "hello".into(),
                message: MessageKind::Final,
            },
        }));

        // Termination entry.
        assert!(log.iter().any(|e| matches!(
            e.kind,
            LogKind::Termination {
                reason: TerminationReason::MaxTurns
            }
        )));

        h_a.close();
        h_b.close();
    }

    #[tokio::test]
    async fn human_input_is_broadcast_to_all_agents() {
        // Acceptance scope-adjacent: the router must accept human input
        // via mpsc and forward it to every agent. The human-injection UI
        // itself is #168b; this test verifies just the routing hook.
        let (mt_a, h_a) = mock();
        let (mt_b, h_b) = mock();

        let (logger, entries) = capturing_logger();
        let config = RouterConfig {
            max_turns: 100,
            turn_timeout: Duration::from_millis(200),
        };
        let mut router = Router::new(config, logger);
        router.add_agent("a", mt_a);
        router.add_agent("b", mt_b);

        let (human_tx, human_rx) = mpsc::channel::<String>(4);
        router.set_human_input(human_rx);

        // Send human input, then drop the sender so the router exits
        // with HumanClosed instead of hanging on the timeout.
        tokio::spawn(async move {
            human_tx.send("focus on tests".to_string()).await.unwrap();
            // Give the router a moment to receive and broadcast before
            // we close the channel. Without this the close can race
            // ahead of the message in some runtime schedules.
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(human_tx);
        });

        let reason = router.run(None).await;
        assert_eq!(reason, TerminationReason::HumanClosed);

        // Both agents received the human message.
        assert_eq!(h_a.sent(), vec!["focus on tests".to_string()]);
        assert_eq!(h_b.sent(), vec!["focus on tests".to_string()]);

        // Human input was logged with Source::Human.
        let log = entries.lock().unwrap().clone();
        assert!(
            log.iter().any(|e| e.from == Source::Human),
            "human input must be logged with Source::Human; got {log:#?}"
        );
    }

    #[tokio::test]
    async fn human_input_resets_latest_result_map() {
        // Verifies the documented "human input clears convergence state"
        // behavior. We pre-load a DONE for one agent so the latest-result
        // map starts non-empty, send human input (which should clear it),
        // and then have only one of two agents say DONE -- if the map
        // had not been cleared, that single DONE plus the leftover entry
        // for the other agent would falsely converge.
        let (mt_a, h_a) = mock();
        let (mt_b, h_b) = mock();

        let (logger, _) = capturing_logger();
        let config = RouterConfig {
            max_turns: 100,
            // Short, so the test stops promptly after the partial round
            // without max_turns kicking in.
            turn_timeout: Duration::from_millis(150),
        };
        let mut router = Router::new(config, logger);
        router.add_agent("a", mt_a);
        router.add_agent("b", mt_b);

        let (human_tx, human_rx) = mpsc::channel::<String>(4);
        router.set_human_input(human_rx);

        // Step 1: A says DONE. Step 2: human input arrives (clears the
        // map). Step 3: only B says DONE. With the clear, last_result
        // contains only {B: DONE} (size 1, not 2 agents) -> no
        // convergence, loop hits timeout instead.
        let h_a_send = h_a.event_tx.clone();
        let h_b_send = h_b.event_tx.clone();
        tokio::spawn(async move {
            let _ = h_a_send.send(Fragment::Result("DONE".into()));
            tokio::time::sleep(Duration::from_millis(20)).await;
            human_tx.send("new topic".to_string()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = h_b_send.send(Fragment::Result("DONE".into()));
            // Keep human_tx alive (no drop) so HumanClosed does not
            // race the timeout we want to observe.
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(human_tx);
        });

        let reason = router.run(None).await;
        assert_eq!(
            reason,
            TerminationReason::Timeout,
            "human input must clear the latest-result map; without that, \
             the leftover DONE from A plus B's DONE would falsely converge"
        );

        // Sanity: A heard the human message and B's eventual DONE; B
        // heard A's initial DONE and the human message.
        assert!(h_a.sent().contains(&"new topic".to_string()));
        assert!(h_b.sent().contains(&"new topic".to_string()));
    }

    #[tokio::test]
    async fn fragments_other_than_result_do_not_count_as_turns() {
        // Text deltas and tool-use announcements get logged but must not
        // increment the turn counter or trigger broadcast. With max_turns
        // = 1 and a stream of non-Result fragments, the router stays in
        // the loop until it hits the safety-net timeout.
        let (mt_a, h_a) = mock();

        let (logger, _) = capturing_logger();
        let config = RouterConfig {
            max_turns: 1,
            turn_timeout: Duration::from_millis(100),
        };
        let mut router = Router::new(config, logger);
        router.add_agent("a", mt_a);

        h_a.push(Fragment::Text("partial 1".to_string()));
        h_a.push(Fragment::Text("partial 2".to_string()));
        h_a.push(Fragment::ToolUse {
            name: "Read".to_string(),
        });

        let reason = router.run(None).await;
        assert_eq!(
            reason,
            TerminationReason::Timeout,
            "non-Result fragments must not advance turn counter"
        );

        h_a.close();
    }

    #[tokio::test]
    async fn null_logger_does_not_panic() {
        // A no-op logger is the simplest exit path; just make sure the
        // helper compiles and the router can use it end-to-end.
        let (mt_a, h_a) = mock();

        let mut router = Router::new(
            RouterConfig {
                max_turns: 1,
                turn_timeout: Duration::from_millis(200),
            },
            null_logger(),
        );
        router.add_agent("a", mt_a);

        h_a.push(Fragment::Result("ok".into()));

        let reason = router.run(None).await;
        assert_eq!(reason, TerminationReason::MaxTurns);
        h_a.close();
    }
}
