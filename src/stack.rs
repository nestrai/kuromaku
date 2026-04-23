use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum StackError {
    #[error("failed to read stack: {0}")]
    Read(std::io::Error),

    #[error("failed to write stack: {0}")]
    Write(std::io::Error),

    #[error("failed to create stack directory: {0}")]
    CreateDir(std::io::Error),

    #[error("failed to serialize stack: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepOutput {
    pub step_id: String,
    pub agent_id: String,
    pub model: String,
    pub prompt: String,
    pub response: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedOutput {
    pub cache_key: String,
    pub step_output: StepOutput,
}

/// Ensure the stack directory exists.
pub fn ensure_dir(stack_path: &Path) -> Result<(), StackError> {
    if !stack_path.exists() {
        std::fs::create_dir_all(stack_path).map_err(StackError::CreateDir)?;
    }
    Ok(())
}

/// Write a step result to `<stack_path>/<step_id>.json`.
pub fn write_step(stack_path: &Path, output: &StepOutput) -> Result<(), StackError> {
    ensure_dir(stack_path)?;
    let file_path = stack_path.join(format!("{}.json", output.step_id));
    let json = serde_json::to_string_pretty(output)?;
    std::fs::write(&file_path, json).map_err(StackError::Write)
}

/// Read a prior step output from `<stack_path>/<step_id>.json`.
pub fn read_step(stack_path: &Path, step_id: &str) -> Result<StepOutput, StackError> {
    let file_path = stack_path.join(format!("{step_id}.json"));
    let contents = std::fs::read_to_string(&file_path).map_err(StackError::Read)?;
    let output: StepOutput = serde_json::from_str(&contents)?;
    Ok(output)
}

/// Compute cache key from model, backend, and assembled prompts.
/// Uses length-prefix framing to prevent hash collisions.
pub fn cache_key(model: &str, backend: &str, system_prompt: &str, user_content: &str) -> String {
    let mut hasher = Sha256::new();

    // Length-prefix each input to prevent collision attacks
    hasher.update((model.len() as u64).to_le_bytes());
    hasher.update(model.as_bytes());

    hasher.update((backend.len() as u64).to_le_bytes());
    hasher.update(backend.as_bytes());

    hasher.update((system_prompt.len() as u64).to_le_bytes());
    hasher.update(system_prompt.as_bytes());

    hasher.update((user_content.len() as u64).to_le_bytes());
    hasher.update(user_content.as_bytes());

    format!("{:x}", hasher.finalize())
}

/// Read cached output from `<stack_path>/cache/<key>.json`.
/// Returns None if cache miss (file doesn't exist or read fails).
pub fn read_cache(stack_path: &Path, key: &str) -> Option<StepOutput> {
    let cache_path = stack_path.join("cache").join(format!("{key}.json"));
    let contents = std::fs::read_to_string(&cache_path).ok()?;
    let cached: CachedOutput = serde_json::from_str(&contents).ok()?;
    Some(cached.step_output)
}

/// Write cached output to `<stack_path>/cache/<key>.json`.
pub fn write_cache(stack_path: &Path, key: &str, output: &StepOutput) -> Result<(), StackError> {
    let cache_dir = stack_path.join("cache");
    ensure_dir(&cache_dir)?;

    let cached = CachedOutput {
        cache_key: key.to_string(),
        step_output: output.clone(),
    };

    let cache_path = cache_dir.join(format!("{key}.json"));
    let json = serde_json::to_string_pretty(&cached)?;
    std::fs::write(&cache_path, json).map_err(StackError::Write)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_output() -> StepOutput {
        StepOutput {
            step_id: "design".to_string(),
            agent_id: "architect".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            prompt: "Design the system".to_string(),
            response: "Here is the design...".to_string(),
            timestamp: "2026-04-20T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let stack_path = dir.path().join("stack");

        let output = sample_output();
        write_step(&stack_path, &output).unwrap();

        let loaded = read_step(&stack_path, "design").unwrap();
        assert_eq!(loaded.step_id, "design");
        assert_eq!(loaded.agent_id, "architect");
        assert_eq!(loaded.response, "Here is the design...");
    }

    #[test]
    fn read_nonexistent_step_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_step(dir.path(), "ghost").unwrap_err();
        assert!(matches!(err, StackError::Read(_)));
    }

    #[test]
    fn write_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let stack_path = dir.path().join("nested").join("stack");

        let output = sample_output();
        write_step(&stack_path, &output).unwrap();
        assert!(stack_path.join("design.json").exists());
    }

    #[test]
    fn serialization_format() {
        let output = sample_output();
        let json = serde_json::to_string_pretty(&output).unwrap();
        assert!(json.contains("\"step_id\": \"design\""));
        assert!(json.contains("\"response\": \"Here is the design...\""));
    }

    #[test]
    fn cache_key_deterministic() {
        let key1 = cache_key("sonnet", "api", "system", "user");
        let key2 = cache_key("sonnet", "api", "system", "user");
        assert_eq!(key1, key2);
    }

    #[test]
    fn cache_key_different_inputs() {
        let key1 = cache_key("sonnet", "api", "system", "user");
        let key2 = cache_key("sonnet", "api", "system-changed", "user");
        assert_ne!(key1, key2);
    }

    #[test]
    fn cache_key_collision_resistance() {
        // Without length prefixing, "ab" + "cd" would equal "a" + "bcd"
        let key1 = cache_key("ab", "cd", "", "");
        let key2 = cache_key("a", "bcd", "", "");
        assert_ne!(key1, key2);
    }

    #[test]
    fn write_and_read_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let stack_path = dir.path().join("stack");
        let output = sample_output();
        let key = "abc123";

        write_cache(&stack_path, key, &output).unwrap();

        let loaded = read_cache(&stack_path, key).unwrap();
        assert_eq!(loaded.step_id, "design");
        assert_eq!(loaded.response, "Here is the design...");
    }

    #[test]
    fn read_cache_miss_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_cache(dir.path(), "nonexistent");
        assert!(result.is_none());
    }

    #[test]
    fn write_cache_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let stack_path = dir.path().join("nested").join("stack");
        let output = sample_output();

        write_cache(&stack_path, "key", &output).unwrap();
        assert!(stack_path.join("cache").join("key.json").exists());
    }
}
