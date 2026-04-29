use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;

use crate::config::Backend;

pub mod local;

// --- Errors ---

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("spawn failed: {0}")]
    Spawn(String),

    #[error("execution failed (exit {code}): {message}")]
    Failed { code: i32, message: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// --- Types ---

/// Describes what to execute.
///
/// `stdout_file`, when set, instructs the executor to stream the child's
/// stdout to that file line-by-line while the process runs, so a watcher
/// (`tail -f`) sees output as it is produced. The same content is also
/// captured in memory and returned via [`ExecutionOutput::stdout`] when
/// [`Executor::wait`] resolves.
pub struct ExecutionTask {
    pub id: String,
    pub command: String,
    pub env: HashMap<String, String>,
    pub stdout_file: Option<PathBuf>,
}

/// Handle to a running execution, used to poll/wait/stop.
#[derive(Debug)]
pub struct ExecutionHandle {
    pub id: String,
    pub metadata: HashMap<String, String>,
}

/// Current status of an execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionStatus {
    Running,
    Completed,
    Failed(String),
}

/// Output captured from a completed execution.
///
/// `stderr` is populated on success too -- shell-step execution surfaces it
/// to the user even when the command succeeds (issue #23 acceptance criteria).
/// LLM-backed callers can ignore it.
#[derive(Debug)]
pub struct ExecutionOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

// --- Trait ---

/// Trait for execution backends.
pub trait Executor: Send + Sync {
    fn spawn(
        &self,
        task: ExecutionTask,
    ) -> impl Future<Output = Result<ExecutionHandle, ExecutorError>> + Send;

    fn wait(
        &self,
        handle: &ExecutionHandle,
    ) -> impl Future<Output = Result<ExecutionOutput, ExecutorError>> + Send;

    fn status(
        &self,
        handle: &ExecutionHandle,
    ) -> impl Future<Output = Result<ExecutionStatus, ExecutorError>> + Send;

    fn stop(
        &self,
        handle: &ExecutionHandle,
    ) -> impl Future<Output = Result<(), ExecutorError>> + Send;
}

// --- Object-safe boxed variant ---

pub trait ExecutorBoxed: Send + Sync {
    fn spawn_boxed(
        &self,
        task: ExecutionTask,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionHandle, ExecutorError>> + Send + '_>>;

    fn wait_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionOutput, ExecutorError>> + Send + 'a>>;

    fn status_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionStatus, ExecutorError>> + Send + 'a>>;

    fn stop_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), ExecutorError>> + Send + 'a>>;
}

impl<T: Executor> ExecutorBoxed for T {
    fn spawn_boxed(
        &self,
        task: ExecutionTask,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionHandle, ExecutorError>> + Send + '_>>
    {
        Box::pin(self.spawn(task))
    }

    fn wait_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionOutput, ExecutorError>> + Send + 'a>>
    {
        Box::pin(self.wait(handle))
    }

    fn status_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<ExecutionStatus, ExecutorError>> + Send + 'a>>
    {
        Box::pin(self.status(handle))
    }

    fn stop_boxed<'a>(
        &'a self,
        handle: &'a ExecutionHandle,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), ExecutorError>> + Send + 'a>> {
        Box::pin(self.stop(handle))
    }
}

// --- Factory ---

/// Create the local executor. Only local execution is supported for now.
pub fn create_executor() -> Box<dyn ExecutorBoxed> {
    Box::new(local::LocalExecutor::new())
}

/// Build the CLI command string for a claude-cli backend.
pub fn build_claude_command(model: &str, system_prompt: Option<&str>, user_prompt: &str) -> String {
    let claude_bin = std::env::var("CLAUDE_CLI_PATH").unwrap_or_else(|_| "claude".to_string());

    let mut parts = vec![claude_bin];
    parts.push("--print".to_string());
    parts.push("--model".to_string());
    parts.push(shell_escape(model));
    parts.push("--output-format".to_string());
    parts.push("text".to_string());
    parts.push("--dangerously-skip-permissions".to_string());

    if let Some(system) = system_prompt {
        parts.push("--system-prompt".to_string());
        parts.push(shell_escape(system));
    }

    parts.push(shell_escape(user_prompt));

    parts.join(" ")
}

/// Build the CLI command string for a codex backend.
///
/// Uses `codex exec` in full-auto mode (no approval prompts, sandboxed).
/// Codex has no --system-prompt flag, so system and user prompts are combined.
pub fn build_codex_command(model: &str, system_prompt: Option<&str>, user_prompt: &str) -> String {
    let codex_bin = std::env::var("CODEX_CLI_PATH").unwrap_or_else(|_| "codex".to_string());

    // Codex CLI has no --system-prompt flag, so system and user prompts are
    // concatenated. The LLM cannot distinguish instructions from task content
    // if the user prompt starts instruction-like.
    let prompt = match system_prompt {
        Some(system) => format!("{system}\n\n{user_prompt}"),
        None => user_prompt.to_string(),
    };

    let mut parts = vec![codex_bin, "exec".to_string()];
    parts.push("--full-auto".to_string());
    // "default" means use Codex's built-in model, don't pass -m flag.
    // Any other value gets passed literally.
    if model != "default" {
        parts.push("-m".to_string());
        parts.push(shell_escape(model));
    }
    parts.push(shell_escape(&prompt));

    parts.join(" ")
}

/// Build the CLI command string for an ollama backend.
pub fn build_ollama_command(model: &str, prompt: &str) -> String {
    let ollama_bin = std::env::var("OLLAMA_PATH").unwrap_or_else(|_| "ollama".to_string());

    format!(
        "{} run {} {}",
        ollama_bin,
        shell_escape(model),
        shell_escape(prompt)
    )
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Determine whether a backend needs an executor (CLI-based) or is direct HTTP.
pub fn backend_needs_executor(backend: Backend) -> bool {
    matches!(
        backend,
        Backend::ClaudeCli | Backend::Codex | Backend::Ollama
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_plain_string() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn build_claude_command_basic() {
        let cmd = build_claude_command("claude-sonnet-4-5", None, "write tests");
        assert!(cmd.contains("claude"));
        assert!(cmd.contains("--model"));
        assert!(cmd.contains("--print"));
        assert!(!cmd.contains("--system-prompt"));
    }

    #[test]
    fn build_claude_command_with_system() {
        let cmd = build_claude_command("claude-sonnet-4-5", Some("You are a dev"), "write tests");
        assert!(cmd.contains("--system-prompt"));
        assert!(cmd.contains("You are a dev"));
    }

    #[test]
    fn build_ollama_command_basic() {
        let cmd = build_ollama_command("llama3", "hello world");
        assert!(cmd.contains("ollama"));
        assert!(cmd.contains("run"));
        assert!(cmd.contains("llama3"));
    }

    #[test]
    fn build_codex_command_basic() {
        let cmd = build_codex_command("o3", None, "write docs");
        assert!(cmd.contains("codex"));
        assert!(cmd.contains("exec"));
        assert!(cmd.contains("--full-auto"));
        assert!(cmd.contains("-m"));
        assert!(cmd.contains("write docs"));
        assert!(!cmd.contains("System:"));
    }

    #[test]
    fn build_codex_command_with_system() {
        let cmd = build_codex_command("o3", Some("You are a writer"), "write docs");
        assert!(cmd.contains("You are a writer"));
        assert!(cmd.contains("write docs"));
    }

    #[test]
    fn build_codex_command_default_model() {
        let cmd = build_codex_command("default", None, "write docs");
        assert!(cmd.contains("codex"));
        assert!(cmd.contains("exec"));
        assert!(cmd.contains("--full-auto"));
        assert!(!cmd.contains("-m"));
        assert!(cmd.contains("write docs"));
    }

    #[test]
    fn backend_needs_executor_classification() {
        assert!(backend_needs_executor(Backend::ClaudeCli));
        assert!(backend_needs_executor(Backend::Codex));
        assert!(backend_needs_executor(Backend::Ollama));
        assert!(!backend_needs_executor(Backend::Api));
    }

    #[test]
    fn create_executor_works() {
        let _ = create_executor();
    }
}
