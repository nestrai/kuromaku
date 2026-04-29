use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::{
    ExecutionHandle, ExecutionOutput, ExecutionStatus, ExecutionTask, Executor, ExecutorError,
};

/// State tracked per running child process.
///
/// stdout/stderr are drained by background tasks at spawn time so the artifact
/// file can be tailed live (issue #16). The buffers hold the same content the
/// caller would have received from a single `wait_with_output()` call.
struct RunningProcess {
    child: tokio::process::Child,
    stdout_buf: Arc<Mutex<String>>,
    stderr_buf: Arc<Mutex<String>>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
}

/// Executes steps locally as child processes.
///
/// Spawns commands via `sh -c` inheriting the current environment, streams
/// stdout line-by-line to the configured artifact file (and into a buffer
/// returned at wait time), and surfaces stderr via the buffer too.
pub struct LocalExecutor {
    processes: Arc<Mutex<HashMap<String, RunningProcess>>>,
}

impl LocalExecutor {
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
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

        let mut child = cmd
            .spawn()
            .map_err(|e| ExecutorError::Spawn(format!("failed to spawn process: {e}")))?;

        let mut metadata = HashMap::new();
        if let Some(pid) = child.id() {
            metadata.insert("pid".to_string(), pid.to_string());
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ExecutorError::Spawn("failed to capture child stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ExecutorError::Spawn("failed to capture child stderr".to_string()))?;

        // Open the artifact file before spawning the reader so failure here
        // is reported synchronously to the caller. truncate(true) matches the
        // previous `std::fs::write` behavior of overwriting on each run.
        let stdout_file = match task.stdout_file.as_ref() {
            Some(path) => Some(
                tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(path)
                    .await
                    .map_err(|e| {
                        ExecutorError::Spawn(format!(
                            "failed to open output file {}: {e}",
                            path.display()
                        ))
                    })?,
            ),
            None => None,
        };

        let stdout_buf = Arc::new(Mutex::new(String::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

        let stdout_buf_clone = Arc::clone(&stdout_buf);
        let stdout_reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            let mut file = stdout_file;
            // next_line() strips the line terminator, so we re-add `\n` to
            // both the in-memory buffer and the file. Final `\n` is added
            // unconditionally; trim() in wait() canonicalizes the buffer.
            while let Ok(Some(line)) = reader.next_line().await {
                {
                    let mut buf = stdout_buf_clone.lock().await;
                    buf.push_str(&line);
                    buf.push('\n');
                }
                if let Some(f) = file.as_mut() {
                    // Best-effort writes: a disk error during stream should
                    // not poison the in-memory buffer or kill the process.
                    // The error becomes visible if the user inspects the
                    // truncated file; the canonical content is the buffer.
                    if f.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                    if f.write_all(b"\n").await.is_err() {
                        break;
                    }
                    let _ = f.flush().await;
                }
            }
            if let Some(mut f) = file {
                let _ = f.flush().await;
            }
        });

        let stderr_buf_clone = Arc::clone(&stderr_buf);
        let stderr_reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let mut buf = stderr_buf_clone.lock().await;
                buf.push_str(&line);
                buf.push('\n');
            }
        });

        let process = RunningProcess {
            child,
            stdout_buf,
            stderr_buf,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
        };

        self.processes.lock().await.insert(task.id.clone(), process);

        Ok(ExecutionHandle {
            id: task.id,
            metadata,
        })
    }

    async fn wait(&self, handle: &ExecutionHandle) -> Result<ExecutionOutput, ExecutorError> {
        let mut process = self
            .processes
            .lock()
            .await
            .remove(&handle.id)
            .ok_or_else(|| ExecutorError::Failed {
                code: -1,
                message: format!("process '{}' not found", handle.id),
            })?;

        let status = process
            .child
            .wait()
            .await
            .map_err(|e| ExecutorError::Failed {
                code: -1,
                message: format!("failed waiting for process: {e}"),
            })?;

        // Drain reader tasks so all output makes it into the buffers and the
        // artifact file before we read them. Without this the final lines
        // could race the wait() return and disappear from both.
        if let Some(handle) = process.stdout_reader.take() {
            let _ = handle.await;
        }
        if let Some(handle) = process.stderr_reader.take() {
            let _ = handle.await;
        }

        let stdout = process.stdout_buf.lock().await.clone();
        let stderr = process.stderr_buf.lock().await.clone();

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            return Err(ExecutorError::Failed {
                code,
                message: format!("process exited with code {code}: {stderr}"),
            });
        }

        Ok(ExecutionOutput {
            stdout: stdout.trim().to_string(),
            stderr,
            exit_code: status.code().unwrap_or(0),
        })
    }

    async fn status(&self, handle: &ExecutionHandle) -> Result<ExecutionStatus, ExecutorError> {
        let mut processes = self.processes.lock().await;
        if let Some(process) = processes.get_mut(&handle.id) {
            match process.child.try_wait() {
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
        if let Some(mut process) = self.processes.lock().await.remove(&handle.id) {
            process
                .child
                .kill()
                .await
                .map_err(|e| ExecutorError::Failed {
                    code: -1,
                    message: format!("failed to kill process: {e}"),
                })?;
            // Reader tasks exit naturally once the pipes close after kill,
            // but abort defensively in case kill() fails to propagate.
            if let Some(handle) = process.stdout_reader.take() {
                handle.abort();
            }
            if let Some(handle) = process.stderr_reader.take() {
                handle.abort();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

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
            stdout_file: None,
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
            stdout_file: None,
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
            stdout_file: None,
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
            stdout_file: None,
        };

        let handle = executor.spawn(task).await.unwrap();

        // Brief pause so the process is running
        tokio::time::sleep(Duration::from_millis(50)).await;

        let status = executor.status(&handle).await.unwrap();
        assert_eq!(status, ExecutionStatus::Running);

        executor.stop(&handle).await.unwrap();
    }

    #[tokio::test]
    async fn stdout_streams_to_file_during_execution() {
        // Acceptance criterion (issue #16): the artifact file must contain
        // partial output while the process is still running, so `tail -f`
        // works without waiting for completion.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.txt");

        let executor = LocalExecutor::new();
        // Print "first", flush, sleep 500ms, print "second", flush. We poll
        // the file in between to confirm the first line is visible before
        // the process exits.
        let task = ExecutionTask {
            id: "test-stream".to_string(),
            command: "echo first; sleep 0.5; echo second".to_string(),
            env: HashMap::new(),
            stdout_file: Some(path.clone()),
        };

        let handle = executor.spawn(task).await.unwrap();

        // Poll for the first line to land in the file before the process
        // finishes. If the executor still buffered until exit, the file
        // would stay empty for the full 500ms.
        let mut saw_first_early = false;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Ok(content) = tokio::fs::read_to_string(&path).await
                && content.contains("first")
                && !content.contains("second")
            {
                saw_first_early = true;
                break;
            }
        }
        assert!(
            saw_first_early,
            "expected 'first' to appear in {} before 'second'",
            path.display()
        );

        let output = executor.wait(&handle).await.unwrap();
        assert_eq!(output.stdout, "first\nsecond");

        let final_content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(final_content.contains("first"));
        assert!(final_content.contains("second"));
    }

    #[tokio::test]
    async fn stdout_file_truncates_on_reuse() {
        // Acceptance: re-running with the same path overwrites prior content
        // (matches old `std::fs::write` semantics, no append surprise).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.txt");
        tokio::fs::write(&path, "stale content that must vanish")
            .await
            .unwrap();

        let executor = LocalExecutor::new();
        let task = ExecutionTask {
            id: "test-truncate".to_string(),
            command: "echo fresh".to_string(),
            env: HashMap::new(),
            stdout_file: Some(path.clone()),
        };

        let handle = executor.spawn(task).await.unwrap();
        let _ = executor.wait(&handle).await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!content.contains("stale"));
        assert!(content.contains("fresh"));
    }

    #[tokio::test]
    async fn missing_output_directory_surfaces_error() {
        // Failure path: pointing stdout_file at a nonexistent directory must
        // fail at spawn, not silently swallow output.
        let executor = LocalExecutor::new();
        let task = ExecutionTask {
            id: "test-missing-dir".to_string(),
            command: "echo hi".to_string(),
            env: HashMap::new(),
            stdout_file: Some("/nonexistent/koto/dir/out.txt".into()),
        };

        let err = executor.spawn(task).await.unwrap_err();
        assert!(matches!(err, ExecutorError::Spawn(_)));
    }
}
