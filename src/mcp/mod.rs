//! Model Context Protocol server (`kuro mcp`).
//!
//! Implements the stdio transport for MCP protocol version
//! [`protocol::MCP_PROTOCOL_VERSION`] (`2025-06-18`). External agents
//! (Codex, Cursor, Claude Code) connect by spawning `kuro mcp` and
//! exchanging NDJSON-framed JSON-RPC 2.0 messages over stdin/stdout.
//!
//! ## Architecture
//!
//! ```text
//! stdin (NDJSON) ──▶ server::run ──▶ dispatch ──▶ Tool (registry)
//!                                       │
//!                                       └────▶ Response ──▶ stdout
//! ```
//!
//! Modules:
//!
//! - [`protocol`] -- JSON-RPC + MCP wire types, NDJSON parsing.
//! - [`error`] -- stable error code catalog (team review #195).
//! - [`tools`] -- [`tools::Tool`] trait and [`tools::ToolRegistry`].
//! - [`server`] -- stdio loop and method dispatcher.
//!
//! ## Dependency rule
//!
//! Per the team review, this module depends only on the documented `pub`
//! API of `runner`, `resolver`, `config`, `messaging::router` and `stack`
//! -- never on private items or internals. The scaffold itself imports
//! only `koto_config` for project-config detection; tool implementations
//! land in follow-up issues (#196, #197, #198, #199).
//!
//! ## Logging
//!
//! All diagnostics route through `tracing` to **stderr**. Stdout is the
//! protocol channel and must never be touched by anything other than the
//! response writer.

use std::path::Path;
use std::sync::Arc;

use color_eyre::Result;
use color_eyre::eyre::eyre;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use self::session::SessionState;

pub mod discovery;
pub mod error;
pub mod execution;
pub mod messaging;
pub mod protocol;
pub mod server;
pub mod session;
pub mod tools;
pub mod umbrella;
pub mod workflow;

// Re-exports used inside the module.
pub(crate) use protocol::{Incoming, Request};

use crate::koto_config::{KOTO_DIR, KotoConfig};

/// Entry point wired to the `kuro mcp` clap subcommand.
///
/// Initialises tracing-to-stderr, performs a best-effort project-config
/// resolution (loads `.kuro/config.yaml` if present), constructs the
/// (currently empty) tool registry, and runs the stdio loop until stdin
/// reaches EOF.
///
/// `verbose` flips the default tracing level from `info` to `debug` for
/// the kuromaku target. `RUST_LOG` overrides both.
pub async fn run(verbose: bool) -> Result<()> {
    init_tracing(verbose)?;

    // Best-effort config detection. Missing `.kuro/` is not an error -- the
    // server still answers `initialize` and `tools/list`. Future tools that
    // need project config surface specific errors via the catalog
    // (`flow_missing`, `agent_missing`, etc.).
    match KotoConfig::load_optional(Path::new(".")) {
        Ok(Some(_)) => info!(dir = KOTO_DIR, "project config loaded"),
        Ok(None) => warn!(dir = KOTO_DIR, "no project config found"),
        Err(e) => warn!(error = %e, "project config invalid; continuing without it"),
    }

    // One `SessionState` per MCP process: scopes the active-run registry
    // to this stdio connection so a `send_message` call sees only flows
    // that were started by *this* client. `Arc` because the registry is
    // shared between `run_flow` (writer, on register/deregister) and
    // `send_message` (reader, on every call).
    let session = Arc::new(SessionState::new());

    let mut registry = tools::ToolRegistry::new();
    // Discovery tools (#197). `register` only fails on programmer error
    // (invalid name, duplicate, empty description); surface as eyre so a
    // broken build is loud, not silent.
    registry
        .register(Box::new(discovery::ListAgents))
        .map_err(|e| eyre!("register list_agents: {:?}", e))?;
    registry
        .register(Box::new(discovery::ListFlows))
        .map_err(|e| eyre!("register list_flows: {:?}", e))?;
    registry
        .register(Box::new(discovery::LoadAgent))
        .map_err(|e| eyre!("register load_agent: {:?}", e))?;
    // Flow execution tools (#198). Same loud-on-broken-build policy.
    registry
        .register(Box::new(execution::RunFlow::new(Arc::clone(&session))))
        .map_err(|e| eyre!("register run_flow: {:?}", e))?;
    registry
        .register(Box::new(execution::ShowOutput))
        .map_err(|e| eyre!("register show_output: {:?}", e))?;
    // Human-injection tool (#199). Reads the same session registry the
    // run_flow tool writes into, so it can find the active conversation.
    registry
        .register(Box::new(messaging::SendMessage::new(Arc::clone(&session))))
        .map_err(|e| eyre!("register send_message: {:?}", e))?;
    // Workflow tools (#196 split). `implement_issue` (#213), `review_pr`
    // (#214), `rework_pr` (#215) -- registration order is irrelevant because
    // the registry is a BTreeMap, but kept in issue order for readability.
    registry
        .register(Box::new(workflow::ImplementIssue))
        .map_err(|e| eyre!("register implement_issue: {:?}", e))?;
    registry
        .register(Box::new(workflow::ReviewPr))
        .map_err(|e| eyre!("register review_pr: {:?}", e))?;
    registry
        .register(Box::new(workflow::ReworkPr))
        .map_err(|e| eyre!("register rework_pr: {:?}", e))?;

    // Take ownership of the stdout fd BEFORE the runner can write to it.
    // Tools that call into `runner::execute_flow` indirectly invoke `ui::*`
    // which uses `println!` (process-wide stdout). Without this redirect
    // every UI line corrupts the JSON-RPC framing -- a real-world break for
    // Claude Code, Cursor and Codex. After redirect, `println!` lands on
    // stderr and the saved fd carries our protocol responses.
    let stdin = tokio::io::stdin();
    let protocol_writer = isolate_protocol_stdout()?;
    server::run(stdin, protocol_writer, registry)
        .await
        .map_err(|e| eyre!("mcp server io error: {e}"))?;

    Ok(())
}

/// Detach the protocol channel from the process-wide stdout. After this
/// runs, `println!`/`print!` (anywhere in the binary, including the runner)
/// goes to stderr; the returned writer points at the original stdout fd
/// the operating system handed us at exec time.
///
/// Implementation: dup fd 1, dup2 fd 2 onto fd 1, wrap the saved fd in a
/// `tokio::fs::File`. The writer is async because `server::run` expects an
/// `AsyncWrite`. On non-Unix platforms this is a no-op pass-through that
/// keeps the existing behaviour -- the redirect dance is Unix-specific
/// (Windows would need a different syscall and is not a kuro mcp target
/// today).
#[cfg(unix)]
fn isolate_protocol_stdout() -> Result<tokio::fs::File> {
    use std::os::unix::io::FromRawFd;

    // SAFETY: dup/dup2 are async-signal-safe and operate on POSIX fds we
    // know are open at process start (1 = stdout, 2 = stderr). We claim
    // the dup'd fd as an OwnedFd via from_raw_fd so it closes on drop.
    let saved_fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
    if saved_fd < 0 {
        return Err(eyre!(
            "dup(stdout) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let dup2_rc = unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) };
    if dup2_rc < 0 {
        // Best-effort cleanup of the saved fd before bubbling up.
        unsafe { libc::close(saved_fd) };
        return Err(eyre!(
            "dup2(stderr -> stdout) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: saved_fd is exclusively ours, opened just above.
    let file = unsafe { std::fs::File::from_raw_fd(saved_fd) };
    Ok(tokio::fs::File::from_std(file))
}

#[cfg(not(unix))]
fn isolate_protocol_stdout() -> Result<tokio::io::Stdout> {
    // Non-Unix: keep the legacy behaviour. The MCP server is Unix-targeted
    // for the foreseeable future; revisit when Windows support lands.
    Ok(tokio::io::stdout())
}

fn init_tracing(verbose: bool) -> Result<()> {
    let default = if verbose {
        "kuromaku=debug,info"
    } else {
        "info"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    // `try_init` so a re-entry from tests does not panic; `with_writer`
    // pins logs to stderr so stdout stays exclusively the protocol channel.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
    Ok(())
}
