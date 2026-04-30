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

use color_eyre::Result;
use color_eyre::eyre::eyre;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

pub mod error;
pub mod protocol;
pub mod server;
pub mod tools;

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

    let registry = tools::ToolRegistry::new();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    server::run(stdin, stdout, registry)
        .await
        .map_err(|e| eyre!("mcp server io error: {e}"))?;

    Ok(())
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
