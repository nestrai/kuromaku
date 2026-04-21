use std::collections::HashMap;

use super::{
    DeployTarget, ExecutionHandle, ExecutionOutput, ExecutionStatus, ExecutionTask, Executor,
    ExecutorError,
};

/// Executes stages locally via tmux sessions.
#[derive(Debug, Clone)]
pub struct LocalExecutor {
    poll_interval: std::time::Duration,
}

impl LocalExecutor {
    pub fn new() -> Self {
        Self {
            poll_interval: std::time::Duration::from_secs(2),
        }
    }

    #[cfg(test)]
    pub fn with_poll_interval(poll_interval: std::time::Duration) -> Self {
        Self { poll_interval }
    }

    fn session_name(task_id: &str) -> String {
        // Sanitize for tmux session naming
        task_id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }
}

impl Executor for LocalExecutor {
    async fn spawn(&self, task: ExecutionTask) -> Result<ExecutionHandle, ExecutorError> {
        let session = Self::session_name(&task.id);
        let output_file = std::env::temp_dir().join(format!("{session}.out"));

        // Build the tmux command that redirects output to a file
        let tmux_cmd = format!("{} > '{}' 2>&1", task.command, output_file.display());

        eprintln!("      tmux session: {session}");
        eprintln!("      attach: tmux attach -t {session}");

        // Kill any existing session with this name (ignore errors)
        let _ = tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", &session])
            .output()
            .await;

        // Build env args for tmux
        let mut cmd = tokio::process::Command::new("tmux");
        cmd.args(["new-session", "-d", "-s", &session]);

        // Set environment variables in the tmux session
        for (key, value) in &task.env {
            cmd.env(key, value);
        }

        cmd.arg(&tmux_cmd);

        let create_output = cmd.output().await?;

        if !create_output.status.success() {
            let stderr = String::from_utf8_lossy(&create_output.stderr).to_string();
            return Err(ExecutorError::Tmux(format!(
                "failed to create tmux session '{session}': {stderr}"
            )));
        }

        let mut metadata = HashMap::new();
        metadata.insert("session".to_string(), session);
        metadata.insert("output_file".to_string(), output_file.display().to_string());

        Ok(ExecutionHandle {
            id: task.id,
            target: DeployTarget::Local,
            metadata,
        })
    }

    async fn wait(&self, handle: &ExecutionHandle) -> Result<ExecutionOutput, ExecutorError> {
        let session = handle
            .metadata
            .get("session")
            .ok_or_else(|| ExecutorError::Tmux("missing session in handle metadata".to_string()))?;
        let output_file = handle.metadata.get("output_file").ok_or_else(|| {
            ExecutorError::Tmux("missing output_file in handle metadata".to_string())
        })?;

        // Poll until the tmux session ends
        loop {
            tokio::time::sleep(self.poll_interval).await;

            let check = tokio::process::Command::new("tmux")
                .args(["has-session", "-t", session])
                .output()
                .await?;

            if !check.status.success() {
                break;
            }
        }

        // Read the output file
        let stdout = tokio::fs::read_to_string(output_file).await.map_err(|e| {
            ExecutorError::Tmux(format!(
                "failed to read output from session '{session}': {e}"
            ))
        })?;

        // Clean up temp file
        let _ = tokio::fs::remove_file(output_file).await;

        Ok(ExecutionOutput {
            stdout: stdout.trim().to_string(),
            exit_code: 0,
        })
    }

    async fn status(&self, handle: &ExecutionHandle) -> Result<ExecutionStatus, ExecutorError> {
        let session = handle
            .metadata
            .get("session")
            .ok_or_else(|| ExecutorError::Tmux("missing session in handle metadata".to_string()))?;

        let check = tokio::process::Command::new("tmux")
            .args(["has-session", "-t", session])
            .output()
            .await?;

        if check.status.success() {
            Ok(ExecutionStatus::Running)
        } else {
            Ok(ExecutionStatus::Completed)
        }
    }

    async fn stop(&self, handle: &ExecutionHandle) -> Result<(), ExecutorError> {
        let session = handle
            .metadata
            .get("session")
            .ok_or_else(|| ExecutorError::Tmux("missing session in handle metadata".to_string()))?;

        let output = tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", session])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            return Err(ExecutorError::Tmux(format!(
                "failed to kill session '{session}': {stderr}"
            )));
        }

        // Clean up output file if it exists
        if let Some(output_file) = handle.metadata.get("output_file") {
            let _ = tokio::fs::remove_file(output_file).await;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_name_sanitizes() {
        assert_eq!(
            LocalExecutor::session_name("koto-planning-design"),
            "koto-planning-design"
        );
        assert_eq!(
            LocalExecutor::session_name("my flow/stage"),
            "my-flow-stage"
        );
        assert_eq!(LocalExecutor::session_name("a.b.c"), "a-b-c");
    }

    #[test]
    fn new_creates_with_defaults() {
        let executor = LocalExecutor::new();
        assert_eq!(executor.poll_interval, std::time::Duration::from_secs(2));
    }

    #[test]
    fn custom_poll_interval() {
        let executor = LocalExecutor::with_poll_interval(std::time::Duration::from_millis(100));
        assert_eq!(
            executor.poll_interval,
            std::time::Duration::from_millis(100)
        );
    }
}
