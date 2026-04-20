use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("failed to read state: {0}")]
    Read(std::io::Error),

    #[error("failed to write state: {0}")]
    Write(std::io::Error),

    #[error("failed to create state directory: {0}")]
    CreateDir(std::io::Error),

    #[error("failed to serialize state: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageOutput {
    pub stage_id: String,
    pub agent_id: String,
    pub model: String,
    pub prompt: String,
    pub response: String,
    pub timestamp: String,
}

/// Ensure the state directory exists.
pub fn ensure_dir(state_path: &Path) -> Result<(), StateError> {
    if !state_path.exists() {
        std::fs::create_dir_all(state_path).map_err(StateError::CreateDir)?;
    }
    Ok(())
}

/// Write a stage result to `<state_path>/<stage_id>.json`.
pub fn write_stage(state_path: &Path, output: &StageOutput) -> Result<(), StateError> {
    ensure_dir(state_path)?;
    let file_path = state_path.join(format!("{}.json", output.stage_id));
    let json = serde_json::to_string_pretty(output)?;
    std::fs::write(&file_path, json).map_err(StateError::Write)
}

/// Read a prior stage output from `<state_path>/<stage_id>.json`.
pub fn read_stage(state_path: &Path, stage_id: &str) -> Result<StageOutput, StateError> {
    let file_path = state_path.join(format!("{stage_id}.json"));
    let contents = std::fs::read_to_string(&file_path).map_err(StateError::Read)?;
    let output: StageOutput = serde_json::from_str(&contents)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_output() -> StageOutput {
        StageOutput {
            stage_id: "design".to_string(),
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
        let state_path = dir.path().join("state");

        let output = sample_output();
        write_stage(&state_path, &output).unwrap();

        let loaded = read_stage(&state_path, "design").unwrap();
        assert_eq!(loaded.stage_id, "design");
        assert_eq!(loaded.agent_id, "architect");
        assert_eq!(loaded.response, "Here is the design...");
    }

    #[test]
    fn read_nonexistent_stage_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_stage(dir.path(), "ghost").unwrap_err();
        assert!(matches!(err, StateError::Read(_)));
    }

    #[test]
    fn write_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("nested").join("state");

        let output = sample_output();
        write_stage(&state_path, &output).unwrap();
        assert!(state_path.join("design.json").exists());
    }

    #[test]
    fn serialization_format() {
        let output = sample_output();
        let json = serde_json::to_string_pretty(&output).unwrap();
        assert!(json.contains("\"stage_id\": \"design\""));
        assert!(json.contains("\"response\": \"Here is the design...\""));
    }
}
