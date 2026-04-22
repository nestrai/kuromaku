use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::{
    ExecutionHandle, ExecutionOutput, ExecutionStatus, ExecutionTask, Executor, ExecutorError,
};

/// Executes steps locally as child processes.
///
/// Spawns commands via `sh -c` inheriting the current environment,
/// captures stdout+stderr, and returns the output when done.
pub struct LocalExecutor {
    children: Arc<Mutex<HashMap<String, tokio::process::Child>>>,
}

impl LocalExecutor {
    pub fn new() -> Self {
        Self {
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Executor for LocalExecutor {
    async fn spawn(&self, task: ExecutionTask) -> Result<ExecutionHandle, ExecutorError> {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(&task.command);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        for (key, value) in &task.env {
            cmd.env(key, value);
        }

        let child = cmd
            .spawn()
            .map_err(|e| ExecutorError::Spawn(format!("failed to spawn process: {e}")))?;

        let mut metadata = HashMap::new();
        if let Some(pid) = child.id() {
            metadata.insert("pid".to_string(), pid.to_string());
        }

        self.children.lock().await.insert(task.id.clone(), child);

        Ok(ExecutionHandle {
            id: task.id,
            metadata,
        })
    }

    async fn wait(&self, handle: &ExecutionHandle) -> Result<ExecutionOutput, ExecutorError> {
        let child = self
            .children
            .lock()
            .await
            .remove(&handle.id)
            .ok_or_else(|| ExecutorError::Failed {
                code: -1,
                message: format!("process '{}' not found", handle.id),
            })?;

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| ExecutorError::Failed {
                code: -1,
                message: format!("failed waiting for process: {e}"),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            let code = output.status.code().unwrap_or(-1);
            return Err(ExecutorError::Failed {
                code,
                message: format!("process exited with code {code}: {stderr}"),
            });
        }

        Ok(ExecutionOutput {
            stdout: stdout.trim().to_string(),
            exit_code: output.status.code().unwrap_or(0),
        })
    }

    async fn status(&self, handle: &ExecutionHandle) -> Result<ExecutionStatus, ExecutorError> {
        let mut children = self.children.lock().await;
        if let Some(child) = children.get_mut(&handle.id) {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        Ok(ExecutionStatus::Completed)
                    } else {
                        Ok(ExecutionStatus::Failed(format!(
                            "exit code: {}",
                            status.code().unwrap_or(-1)
                        )))
                    }
                }
                Ok(None) => Ok(ExecutionStatus::Running),
                Err(e) => Err(ExecutorError::Failed {
                    code: -1,
                    message: format!("failed to check process status: {e}"),
                }),
            }
        } else {
            // Process not in map means it was already waited on
            Ok(ExecutionStatus::Completed)
        }
    }

    async fn stop(&self, handle: &ExecutionHandle) -> Result<(), ExecutorError> {
        if let Some(mut child) = self.children.lock().await.remove(&handle.id) {
            child.kill().await.map_err(|e| ExecutorError::Failed {
                code: -1,
                message: format!("failed to kill process: {e}"),
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_executor() {
        let _ = LocalExecutor::new();
    }

    #[tokio::test]
    async fn spawn_and_wait_echo() {
        let executor = LocalExecutor::new();
        let task = ExecutionTask {
            id: "test-echo".to_string(),
            command: "echo hello".to_string(),
            env: HashMap::new(),
        };

        let handle = executor.spawn(task).await.unwrap();
        assert_eq!(handle.id, "test-echo");

        let output = executor.wait(&handle).await.unwrap();
        assert_eq!(output.stdout, "hello");
        assert_eq!(output.exit_code, 0);
    }

    #[tokio::test]
    async fn spawn_inherits_env() {
        let executor = LocalExecutor::new();
        let mut env = HashMap::new();
        env.insert("KOTO_TEST_VAR".to_string(), "works".to_string());

        let task = ExecutionTask {
            id: "test-env".to_string(),
            command: "echo $KOTO_TEST_VAR".to_string(),
            env,
        };

        let handle = executor.spawn(task).await.unwrap();
        let output = executor.wait(&handle).await.unwrap();
        assert_eq!(output.stdout, "works");
    }

    #[tokio::test]
    async fn spawn_failed_command_returns_error() {
        let executor = LocalExecutor::new();
        let task = ExecutionTask {
            id: "test-fail".to_string(),
            command: "exit 42".to_string(),
            env: HashMap::new(),
        };

        let handle = executor.spawn(task).await.unwrap();
        let err = executor.wait(&handle).await.unwrap_err();
        assert!(matches!(err, ExecutorError::Failed { code: 42, .. }));
    }

    #[tokio::test]
    async fn stop_kills_process() {
        let executor = LocalExecutor::new();
        let task = ExecutionTask {
            id: "test-stop".to_string(),
            command: "sleep 60".to_string(),
            env: HashMap::new(),
        };

        let handle = executor.spawn(task).await.unwrap();

        // Brief pause so the process is running
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let status = executor.status(&handle).await.unwrap();
        assert_eq!(status, ExecutionStatus::Running);

        executor.stop(&handle).await.unwrap();
    }
}
