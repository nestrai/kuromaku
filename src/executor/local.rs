use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::stream_json::{self, Fragment};
use super::{
    ExecutionHandle, ExecutionOutput, ExecutionStatus, ExecutionTask, Executor, ExecutorError,
    OutputFormat,
};

/// State tracked per running child process.
///
/// stdout/stderr are drained by background tasks at spawn time so the artifact
/// file can be tailed live (issue #16). The buffers hold the same content the
/// caller would have received from a single `wait_with_output()` call.
///
/// `result_override` is set by the stream-json reader when a `result` event
/// arrives. It carries the canonical assistant text (matching what
/// `--output-format text` would have produced). Used in preference to
/// accumulated deltas when present (issue #156).
struct RunningProcess {
    child: tokio::process::Child,
    stdout_buf: Arc<Mutex<String>>,
    stderr_buf: Arc<Mutex<String>>,
    result_override: Arc<Mutex<Option<String>>>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
}

/// Environment variables allowed to propagate from the parent process into
/// spawned child processes. Everything else is cleared so agent execution
/// is deterministic regardless of the user's shell environment, and
/// sensitive keys (API tokens, credentials) never leak into child
/// processes. Backend-specific vars should come from agent YAML config via
/// `task.env`, not from parent inheritance.
const ALLOWED_ENV_VARS: [&str; 14] = [
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "TERM",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
];

/// Executes steps locally as child processes.
///
/// Spawns commands via `sh -c` with a clean environment (allowlisted vars
/// only), streams stdout line-by-line to the configured artifact file (and
/// into a buffer returned at wait time), and surfaces stderr via the
/// buffer too.
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

        cmd.env_clear();
        for key in ALLOWED_ENV_VARS {
            if let Ok(value) = std::env::var(key) {
                cmd.env(key, value);
            }
        }
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
        let result_override: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let stdout_buf_clone = Arc::clone(&stdout_buf);
        let result_override_clone = Arc::clone(&result_override);
        let output_format = task.output_format;
        let stdout_reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            let mut file = stdout_file;
            // next_line() strips the line terminator. For Raw mode we re-add
            // `\n` to both the in-memory buffer and the file. For
            // ClaudeStreamJson we parse each line as NDJSON and only persist
            // the user-visible fragments (text + tool-use markers); the raw
            // JSON never lands in the artifact file.
            while let Ok(Some(line)) = reader.next_line().await {
                match output_format {
                    OutputFormat::Raw => {
                        write_raw_line(&line, &stdout_buf_clone, file.as_mut()).await;
                    }
                    OutputFormat::ClaudeStreamJson => {
                        let fragments = stream_json::parse_line(&line);
                        for fragment in fragments {
                            write_fragment(
                                fragment,
                                &stdout_buf_clone,
                                &result_override_clone,
                                file.as_mut(),
                            )
                            .await;
                        }
                    }
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
            result_override,
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

        // Prefer the canonical `result` text from a stream-json `result`
        // event when present (issue #156): it is exactly what
        // `--output-format text` would have returned, with no tool-use
        // markers or trailing whitespace from the live-display path. Fall
        // back to the accumulated text buffer otherwise (Raw mode, or
        // stream-json without a terminal `result` event).
        let stdout = match process.result_override.lock().await.clone() {
            Some(canonical) => canonical,
            None => process.stdout_buf.lock().await.clone(),
        };
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

/// Append a raw stdout line to the in-memory buffer and (if open) the
/// artifact file, restoring the trailing newline that `next_line()` strips.
/// Disk errors are swallowed: the canonical content lives in the buffer, and
/// breaking the loop on a transient write failure would lose subsequent
/// lines from both the file and the buffer for downstream consumers.
async fn write_raw_line(line: &str, buf: &Arc<Mutex<String>>, file: Option<&mut tokio::fs::File>) {
    {
        let mut b = buf.lock().await;
        b.push_str(line);
        b.push('\n');
    }
    if let Some(f) = file {
        let _ = f.write_all(line.as_bytes()).await;
        let _ = f.write_all(b"\n").await;
        let _ = f.flush().await;
    }
}

/// Apply a single stream-json [`Fragment`] to the artifact file and the
/// canonical buffer. Text fragments accumulate verbatim. Tool-use fragments
/// are dropped -- the artifact file is meant to show the agent's prose, not
/// internal tool plumbing. Result fragments populate `result_override` so
/// [`Executor::wait`] returns the canonical assistant text in preference to
/// accumulated deltas.
async fn write_fragment(
    fragment: Fragment,
    buf: &Arc<Mutex<String>>,
    result_override: &Arc<Mutex<Option<String>>>,
    file: Option<&mut tokio::fs::File>,
) {
    match fragment {
        Fragment::Text(text) => {
            {
                let mut b = buf.lock().await;
                b.push_str(&text);
            }
            if let Some(f) = file {
                let _ = f.write_all(text.as_bytes()).await;
                let _ = f.flush().await;
            }
        }
        Fragment::ToolUse { .. } => {
            // Dropped on purpose: the markers cluttered the live output
            // without adding value. The parser still emits the variant so
            // future code paths (e.g. structured logs) can use it.
        }
        Fragment::Result(text) => {
            let mut slot = result_override.lock().await;
            *slot = Some(text);
        }
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
            output_format: OutputFormat::Raw,
        };

        let handle = executor.spawn(task).await.unwrap();
        assert_eq!(handle.id, "test-echo");

        let output = executor.wait(&handle).await.unwrap();
        assert_eq!(output.stdout, "hello");
        assert_eq!(output.exit_code, 0);
    }

    #[tokio::test]
    async fn spawn_passes_task_env() {
        let executor = LocalExecutor::new();
        let mut env = HashMap::new();
        env.insert("KURO_TEST_VAR".to_string(), "works".to_string());

        let task = ExecutionTask {
            id: "test-env".to_string(),
            command: "echo $KURO_TEST_VAR".to_string(),
            env,
            stdout_file: None,
            output_format: OutputFormat::Raw,
        };

        let handle = executor.spawn(task).await.unwrap();
        let output = executor.wait(&handle).await.unwrap();
        assert_eq!(output.stdout, "works");
    }

    #[tokio::test]
    async fn spawn_does_not_inherit_parent_env() {
        // Set a var in this process that is NOT on the allowlist.
        // The child must not see it.
        // SAFETY: this test runs in isolation; no other threads depend on
        // this variable.
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-secret-leaked") };

        let executor = LocalExecutor::new();
        let task = ExecutionTask {
            id: "test-no-leak".to_string(),
            command: "echo ${ANTHROPIC_API_KEY:-clean}".to_string(),
            env: HashMap::new(),
            stdout_file: None,
            output_format: OutputFormat::Raw,
        };

        let handle = executor.spawn(task).await.unwrap();
        let output = executor.wait(&handle).await.unwrap();
        assert_eq!(output.stdout, "clean");

        // SAFETY: cleanup after test, same isolation assumption.
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    }

    #[tokio::test]
    async fn spawn_failed_command_returns_error() {
        let executor = LocalExecutor::new();
        let task = ExecutionTask {
            id: "test-fail".to_string(),
            command: "exit 42".to_string(),
            env: HashMap::new(),
            stdout_file: None,
            output_format: OutputFormat::Raw,
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
            output_format: OutputFormat::Raw,
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
            output_format: OutputFormat::Raw,
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
            output_format: OutputFormat::Raw,
        };

        let handle = executor.spawn(task).await.unwrap();
        let _ = executor.wait(&handle).await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!content.contains("stale"));
        assert!(content.contains("fresh"));
    }

    #[tokio::test]
    async fn claude_stream_json_writes_plain_text_to_artifact_live() {
        // Acceptance criteria (issue #156):
        //   1. Artifact file shows incremental output during the run.
        //   2. Final step output equals what plain --print would have
        //      returned (here: the `result` event's text).
        //   3. Tool call events are visible in the artifact file.
        //
        // We simulate the claude CLI by emitting hand-crafted stream-json
        // lines via printf, with a mid-stream sleep so we can observe the
        // file before the process exits.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.txt");

        // NDJSON sequence: a text delta, a tool_use start, another text
        // delta, then a result event. Each line is a single printf call so
        // it lands on the pipe at a separate moment, mimicking real
        // streaming.
        let cmd = r#"
printf '%s\n' '{"type":"system","subtype":"init"}'
printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello "}}}'
sleep 0.5
printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"Bash","input":{}}}}'
printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"world"}}}'
printf '%s\n' '{"type":"result","subtype":"success","result":"Hello world"}'
"#;

        let executor = LocalExecutor::new();
        let task = ExecutionTask {
            id: "test-stream-json".to_string(),
            command: cmd.to_string(),
            env: HashMap::new(),
            stdout_file: Some(path.clone()),
            output_format: OutputFormat::ClaudeStreamJson,
        };

        let handle = executor.spawn(task).await.unwrap();

        // Poll for the early "Hello " text to land in the file before the
        // process finishes. If stream-json parsing only ran at the end,
        // the file would stay empty for the full 500ms.
        let mut saw_first_early = false;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Ok(content) = tokio::fs::read_to_string(&path).await
                && content.contains("Hello ")
                && !content.contains("world")
            {
                saw_first_early = true;
                break;
            }
        }
        assert!(
            saw_first_early,
            "expected 'Hello ' to appear in {} before 'world'",
            path.display()
        );

        let output = executor.wait(&handle).await.unwrap();

        // Acceptance criterion 2: canonical step output comes from the
        // `result` event, not the raw NDJSON.
        assert_eq!(output.stdout, "Hello world");

        let final_content = tokio::fs::read_to_string(&path).await.unwrap();

        // Acceptance criterion 1+2: artifact contains the human-readable
        // text, not the raw JSON envelopes.
        assert!(final_content.contains("Hello "));
        assert!(final_content.contains("world"));
        assert!(
            !final_content.contains("content_block_delta"),
            "raw JSON must not leak into artifact:\n{final_content}"
        );
        assert!(
            !final_content.contains("text_delta"),
            "raw JSON must not leak into artifact:\n{final_content}"
        );

        // Tool-use markers are intentionally dropped: the artifact should
        // show the agent's prose, not internal tool plumbing.
        assert!(
            !final_content.contains("[tool: Bash]"),
            "tool marker leaked into artifact:\n{final_content}"
        );
        assert!(
            !final_content.contains("[tool:"),
            "tool marker leaked into artifact:\n{final_content}"
        );
    }

    #[tokio::test]
    async fn claude_stream_json_falls_back_to_accumulated_text_without_result_event() {
        // Robustness: if a run finishes without a `result` event (e.g.
        // older CLI version, or process killed before result), the buffer
        // must still hold the accumulated assistant text so downstream
        // agents see something useful.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.txt");

        let cmd = r#"
printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial "}}}'
printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}}'
"#;

        let executor = LocalExecutor::new();
        let task = ExecutionTask {
            id: "test-stream-json-noresult".to_string(),
            command: cmd.to_string(),
            env: HashMap::new(),
            stdout_file: Some(path.clone()),
            output_format: OutputFormat::ClaudeStreamJson,
        };

        let handle = executor.spawn(task).await.unwrap();
        let output = executor.wait(&handle).await.unwrap();
        assert_eq!(output.stdout, "partial answer");
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
            output_format: OutputFormat::Raw,
        };

        let err = executor.spawn(task).await.unwrap_err();
        assert!(matches!(err, ExecutorError::Spawn(_)));
    }
}
