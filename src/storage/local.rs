//! Filesystem-backed [`Storage`] (issue #162).
//!
//! Maps keys to paths under a configurable root. The root is whatever the
//! caller resolved (typically `~/.koto/stacks`); keys carry the
//! `<project>/<run-id>/...` segments. This matches the cloud bucket model
//! (root = bucket, key = everything else) so a future S3 backend slots in
//! without callers having to re-shape their keys.
//!
//! All ops are async via `tokio::fs`. Cheap on local FS, but lets the rest
//! of the codebase use a single async API regardless of backend.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::{Storage, StorageError, StorageKey};

/// Filesystem storage rooted at a single base directory. Keys map directly
/// to paths under that root (with `/` -> the platform separator handled by
/// `PathBuf::join`).
#[derive(Debug, Clone)]
pub struct LocalStorage {
    root: PathBuf,
}

impl LocalStorage {
    /// New backend rooted at `root`. The directory is created on first
    /// `put_*` call; existence at construction is not required so callers
    /// can construct a Storage before the directory exists on disk.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve a key to its filesystem path under this root. `pub` because
    /// the runner uses this to hand a streaming target path to the local
    /// executor (streaming is out-of-scope for the trait per #162). Cloud
    /// backends do not implement this -- the runner only invokes it after
    /// downcasting to `LocalStorage`.
    pub fn local_path(&self, key: &StorageKey) -> PathBuf {
        let mut p = self.root.clone();
        for seg in key.as_str().split('/') {
            p.push(seg);
        }
        p
    }

    /// Borrow the configured root.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl Storage for LocalStorage {
    fn backend_label(&self) -> &'static str {
        "local"
    }

    async fn put_bytes(&self, key: &StorageKey, bytes: &[u8]) -> Result<(), StorageError> {
        let path = self.local_path(key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, bytes).await?;
        Ok(())
    }

    async fn get_bytes(&self, key: &StorageKey) -> Result<Vec<u8>, StorageError> {
        let path = self.local_path(key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(StorageError::NotFound(key.to_string()))
            }
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    async fn list_prefix(&self, prefix: &StorageKey) -> Result<Vec<StorageKey>, StorageError> {
        let dir = self.local_path(prefix);
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StorageError::Io(e)),
        };

        let mut keys = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // Skip non-UTF-8 entries -- keys are UTF-8 by construction, so a
            // lossy filename would never round-trip through `StorageKey::new`
            // anyway. Surfacing this as an error would also be defensible;
            // chose silent skip because it matches the "list what you can
            // address" semantics of S3 listings on non-UTF-8 keys.
            if name_str.contains('\u{FFFD}') {
                continue;
            }
            let joined = format!("{}/{name_str}", prefix.as_str());
            // joined is built from a validated prefix and a single filename
            // segment that came from the OS; re-validate so any oddities
            // (a filename with embedded `/`, which is impossible on Unix but
            // defensive on Windows / future migration) become InvalidKey.
            keys.push(StorageKey::new(joined)?);
        }
        Ok(keys)
    }

    async fn exists(&self, key: &StorageKey) -> Result<bool, StorageError> {
        match tokio::fs::metadata(self.local_path(key)).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(StorageError::Io(e)),
        }
    }

    async fn delete(&self, key: &StorageKey) -> Result<(), StorageError> {
        match tokio::fs::remove_file(self.local_path(key)).await {
            Ok(()) => Ok(()),
            // Idempotent: deleting an absent key is fine.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StorageError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(root: &Path) -> LocalStorage {
        LocalStorage::new(root)
    }

    #[tokio::test]
    async fn put_and_get_bytes_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let key = StorageKey::new("a/b/c.bin").unwrap();
        s.put_bytes(&key, b"hello").await.unwrap();
        assert_eq!(s.get_bytes(&key).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn put_string_creates_parents() {
        // The local FS demands parent directories; the trait promises put_*
        // creates them transparently.
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let key = StorageKey::new("deep/nested/path/file.txt").unwrap();
        s.put_string(&key, "body").await.unwrap();
        assert_eq!(s.get_string(&key).await.unwrap(), "body");
    }

    #[tokio::test]
    async fn get_missing_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let err = s.get_bytes(&StorageKey::new("missing").unwrap()).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn exists_reports_truth() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let key = StorageKey::new("check.txt").unwrap();
        assert!(!s.exists(&key).await.unwrap());
        s.put_string(&key, "x").await.unwrap();
        assert!(s.exists(&key).await.unwrap());
    }

    #[tokio::test]
    async fn list_prefix_returns_immediate_children() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.put_string(&StorageKey::new("run/steps/01.md").unwrap(), "a").await.unwrap();
        s.put_string(&StorageKey::new("run/steps/02.md").unwrap(), "b").await.unwrap();
        s.put_string(&StorageKey::new("run/manifest.yaml").unwrap(), "m").await.unwrap();

        let mut listed = s.list_prefix(&StorageKey::new("run/steps").unwrap()).await.unwrap();
        listed.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(
            listed.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            vec!["run/steps/01.md", "run/steps/02.md"]
        );
    }

    #[tokio::test]
    async fn list_prefix_unknown_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let listed = s.list_prefix(&StorageKey::new("ghost").unwrap()).await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let key = StorageKey::new("once.txt").unwrap();
        s.put_string(&key, "x").await.unwrap();
        s.delete(&key).await.unwrap();
        // Second delete must succeed even though the key is gone.
        s.delete(&key).await.unwrap();
        assert!(!s.exists(&key).await.unwrap());
    }

    #[tokio::test]
    async fn local_path_resolves_under_root() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let key = StorageKey::new("project/run/steps/01.md").unwrap();
        let p = s.local_path(&key);
        assert!(p.starts_with(dir.path()));
        assert!(p.ends_with("project/run/steps/01.md"));
    }
}
