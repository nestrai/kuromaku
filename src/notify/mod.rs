//! Sinks for posting step output to external systems.
//!
//! Today only `github` exists, but the module is structured so additional
//! transports (Slack, generic webhook) can be added without touching the
//! runner.
pub mod github;
