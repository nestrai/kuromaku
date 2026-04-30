//! Inter-agent messaging primitives.
//!
//! Today this module exposes [`router::Router`], which orchestrates multiple
//! [`Transport`](crate::executor::transport::Transport) instances so agents
//! can talk to each other in a single conversation. Transport lives under
//! `executor` because it is the bidirectional cousin of one-shot execution;
//! the router lives here because it is a layer on top -- routing rules,
//! termination detection, and audit logging are conversation concerns, not
//! process-management concerns.

pub mod router;
