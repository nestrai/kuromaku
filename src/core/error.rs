//! Engine-level validation error.
//!
//! `ValidationError` is the only error type the `core` module surfaces.
//! It is deliberately narrow: parsers (YAML, MD, future SDK adapters)
//! own format-specific errors (`serde_yaml::Error`, I/O, etc.) and map
//! `ValidationError` into their own error enum via a `From` impl.
//!
//! Keeping the engine error free of parser concerns is what lets `core`
//! depend only on `std`, `serde`, and `indexmap`. See issue #324.

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("validation error: {0}")]
pub struct ValidationError(pub String);

impl ValidationError {
    /// Build a [`ValidationError`] from anything that renders as a string.
    /// Mirrors the ergonomics callers had with the previous
    /// `ConfigError::Validation(String)` variant.
    pub fn new(msg: impl Into<String>) -> Self {
        ValidationError(msg.into())
    }
}
