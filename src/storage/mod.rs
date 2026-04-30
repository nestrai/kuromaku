//! Storage abstraction for stack and run state (issue #162).
//!
//! Stack and run code (manifests, per-step content/meta files, audit text,
//! future inter-agent messages) writes through the [`Storage`] trait instead
//! of calling `std::fs::*` directly. This is the precondition for hosted
//! koto (#37), the MCP server (#100), and any cloud backend (S3/GCS/Azure)
//! that lands later.
//!
//! Today the only impl is [`local::LocalStorage`]. The trait surface
//! intentionally stays small and key-based so cloud backends slot in without
//! rewriting callers.
//!
//! # Keys
//!
//! Operations use [`StorageKey`] -- forward-slash separated, no leading
//! slash, no `.` / `..` / empty segments, no backslashes. This is the same
//! shape S3 / GCS / Azure Blob accept, so a cloud backend can pass the key
//! through unchanged.
//!
//! # Out of scope
//!
//! - Streaming reads/writes. Current callers are whole-file. The runner's
//!   live-tail stdout streaming continues to write through a `&Path` that
//!   the local executor receives directly; see [`local::LocalStorage::local_path`]
//!   for the escape hatch that the runner uses to materialise the parent
//!   directory and resolve the streaming target.
//! - Atomic multi-key transactions.

pub mod local;

use async_trait::async_trait;

/// Errors surfaced by [`Storage`] operations.
///
/// `NotFound` is distinguished from generic I/O so callers can treat it as a
/// non-fatal "no such key" without string-matching.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("invalid storage key: {0}")]
    InvalidKey(String),

    #[error("storage I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("key not found: {0}")]
    NotFound(String),

    #[error("invalid UTF-8 in stored value at {key}: {source}")]
    InvalidUtf8 {
        key: String,
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error("operation '{op}' not supported by backend '{backend}'")]
    Unsupported { op: &'static str, backend: &'static str },
}

/// A storage key. Forward-slash separated, no leading slash, no parent
/// traversal, no empty segments, no backslashes.
///
/// Constructed via [`StorageKey::new`] which validates eagerly so backends
/// never have to re-check.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StorageKey(String);

impl StorageKey {
    /// Validate and wrap a key string. Returns `InvalidKey` if any segment
    /// is empty, `.`, or `..`, if the key starts with `/`, or if it contains
    /// a backslash (Windows path separator -- rejected to keep keys portable).
    pub fn new(s: impl Into<String>) -> Result<Self, StorageError> {
        let s = s.into();
        if s.is_empty() {
            return Err(StorageError::InvalidKey("key must not be empty".to_string()));
        }
        if s.starts_with('/') {
            return Err(StorageError::InvalidKey(format!(
                "key must not start with '/': {s}"
            )));
        }
        if s.contains('\\') {
            return Err(StorageError::InvalidKey(format!(
                "key must not contain backslash: {s}"
            )));
        }
        for seg in s.split('/') {
            if seg.is_empty() {
                return Err(StorageError::InvalidKey(format!(
                    "key contains empty segment: {s}"
                )));
            }
            if seg == "." || seg == ".." {
                return Err(StorageError::InvalidKey(format!(
                    "key contains parent traversal segment '{seg}': {s}"
                )));
            }
        }
        Ok(Self(s))
    }

    /// Borrow the underlying key as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Append a sub-path to this key, validating the full result.
    pub fn join(&self, rest: &str) -> Result<Self, StorageError> {
        let trimmed = rest.trim_start_matches('/');
        Self::new(format!("{}/{trimmed}", self.0))
    }
}

impl std::fmt::Display for StorageKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The storage abstraction. All stack/run reads and writes go through here.
///
/// Async by design so cloud backends can issue HTTP requests without blocking
/// the runtime. Local backends are also async (using `tokio::fs`) so callers
/// don't need to know which backend they're talking to.
#[async_trait]
pub trait Storage: Send + Sync + std::fmt::Debug {
    /// Stable label for audit/log output (`"local"`, `"s3"`, ...).
    fn backend_label(&self) -> &'static str;

    /// Write raw bytes. Overwrites any existing value at this key. Creates
    /// any intermediate "directories" the backend needs (no-op for cloud).
    async fn put_bytes(&self, key: &StorageKey, bytes: &[u8]) -> Result<(), StorageError>;

    /// Read raw bytes. Returns `NotFound` if the key has no value.
    async fn get_bytes(&self, key: &StorageKey) -> Result<Vec<u8>, StorageError>;

    /// Convenience: write a string. Default impl delegates to `put_bytes`.
    async fn put_string(&self, key: &StorageKey, s: &str) -> Result<(), StorageError> {
        self.put_bytes(key, s.as_bytes()).await
    }

    /// Convenience: read a string. Default impl delegates to `get_bytes` and
    /// surfaces non-UTF-8 content as `InvalidUtf8` (rather than a panic or
    /// silent lossy conversion).
    async fn get_string(&self, key: &StorageKey) -> Result<String, StorageError> {
        let bytes = self.get_bytes(key).await?;
        String::from_utf8(bytes).map_err(|e| StorageError::InvalidUtf8 {
            key: key.to_string(),
            source: e,
        })
    }

    /// List every key under `prefix` (one level only -- non-recursive). The
    /// returned keys are full keys (i.e. they include the prefix), not
    /// relative names. Empty result for an unknown prefix.
    async fn list_prefix(&self, prefix: &StorageKey) -> Result<Vec<StorageKey>, StorageError>;

    /// True iff a value exists at this key.
    async fn exists(&self, key: &StorageKey) -> Result<bool, StorageError>;

    /// Remove the value at this key. No-op on already-absent keys (idempotent).
    /// Reserved for the GDPR right-to-deletion path (separate issue); no
    /// production caller invokes this yet.
    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_accepts_simple_path() {
        let k = StorageKey::new("foo/bar/baz.md").unwrap();
        assert_eq!(k.as_str(), "foo/bar/baz.md");
    }

    #[test]
    fn key_rejects_empty() {
        let err = StorageKey::new("").unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }

    #[test]
    fn key_rejects_leading_slash() {
        let err = StorageKey::new("/foo").unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }

    #[test]
    fn key_rejects_parent_traversal() {
        let err = StorageKey::new("foo/../bar").unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
        let err = StorageKey::new("..").unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }

    #[test]
    fn key_rejects_dot_segment() {
        let err = StorageKey::new("foo/./bar").unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }

    #[test]
    fn key_rejects_empty_segment() {
        let err = StorageKey::new("foo//bar").unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }

    #[test]
    fn key_rejects_backslash() {
        let err = StorageKey::new("foo\\bar").unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }

    #[test]
    fn key_join_appends_segment() {
        let base = StorageKey::new("run-id").unwrap();
        let joined = base.join("steps/01-design.md").unwrap();
        assert_eq!(joined.as_str(), "run-id/steps/01-design.md");
    }

    #[test]
    fn key_join_validates_result() {
        let base = StorageKey::new("run-id").unwrap();
        // ".." is still rejected when joined.
        let err = base.join("../escape").unwrap_err();
        assert!(matches!(err, StorageError::InvalidKey(_)));
    }
}
