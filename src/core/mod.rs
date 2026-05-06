//! Format-agnostic engine core (issue #324).
//!
//! Houses the data shapes the engine actually executes against
//! ([`graph_flow`]) and the validators that decide whether a flow can
//! run ([`validator`]). Parsers (`config.rs`, `config_md.rs`, future SDK
//! adapters) deserialize INTO these types and call validators across
//! this boundary.
//!
//! Hard rule: this module must not depend on any parser. No `use
//! crate::config::*`, no `use crate::config_md::*`. The whole point of
//! the boundary is that the engine has no opinion about which on-disk
//! format produced a flow.
//!
//! Runtime extraction (`core::runtime`) is deferred to a follow-up
//! sub-issue under epic #323 -- it requires also lifting `Agent` and
//! `Backend` out of `config.rs` plus disentangling shared run scaffolding
//! from `runner/mod.rs`. See the design doc for #324 for the full
//! rationale.

pub mod error;
pub mod graph_flow;
pub mod validator;

pub use error::ValidationError;
pub use graph_flow::{GraphFlow, GraphState, SelectEntry, SelectReason, Version};
pub use validator::{validate_graph_flow, validate_graph_reachability};

// Note: `GraphValidationReport` lives in `core::validator` and is reachable
// at that path. It is not re-exported here because every current consumer
// (CLI, runner pre-flight) binds the value via inferred return type from
// `validate_graph_reachability` and never names the type. Add a `pub use`
// the day a consumer needs to name it.
