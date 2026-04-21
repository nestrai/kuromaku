use std::collections::HashMap;
use std::future::Future;

use crate::config::{Backend, FlowConfig};

pub mod k8s;
pub mod local;
pub mod ssh;

// --- Errors ---

#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("spawn failed: {0}")]
    Spawn(String),

    #[error("execution failed (exit {code}): {message}")]
    Failed { code: i32, message: String },

    #[error("tmux error: {0}")]
    Tmux(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not implemented: {0}")]
    NotImplemented(String),
}

// --- Types ---

/// Describes what to execute.
pub struct ExecutionTask {
    pub id: String,
    pub command: String,
    pub env: HashMap<String, String>,
}

/// Handle to a running execution, used to poll/wait/stop.
pub struct ExecutionHandle {
    pub id: String,
    pub target: DeployTarget,
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
pub struct ExecutionOutput {
    pub stdout: String,
    pub exit_code: i32,
}

/// Where a stage gets deployed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployTarget {
    Local,
    Ssh(SshConfig),
    Kubernetes(K8sConfig),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshConfig {
    pub host: String,
    pub user: Option<String>,
    pub key_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct K8sConfig {
    pub namespace: String,
    pub image: String,
    pub service_account: Option<String>,
}

// --- Trait ---

/// Where and how stages get deployed.
pub trait Executor: Send + Sync {
    /// Spawn a stage execution. Returns a handle to track it.
    fn spawn(
        &self,
        task: ExecutionTask,
    ) -> impl Future<Output = Result<ExecutionHandle, ExecutorError>> + Send;

    /// Wait for a spawned execution to complete and return its output.
    fn wait(
        &self,
        handle: &ExecutionHandle,
    ) -> impl Future<Output = Result<ExecutionOutput, ExecutorError>> + Send;

    /// Check the current status of an execution.
    fn status(
        &self,
        handle: &ExecutionHandle,
    ) -> impl Future<Output = Result<ExecutionStatus, ExecutorError>> + Send;

    /// Stop/kill a running execution.
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

/// Create the appropriate executor based on config.
/// Defaults to local (tmux) if no deploy config is specified.
pub fn create_executor(config: &FlowConfig) -> Box<dyn ExecutorBoxed> {
    // For now, always return local executor. Deploy target config will come later.
    let _ = config;
    Box::new(local::LocalExecutor::new())
}

/// Create executor for a specific deploy target.
pub fn create_executor_for_target(target: &DeployTarget) -> Box<dyn ExecutorBoxed> {
    match target {
        DeployTarget::Local => Box::new(local::LocalExecutor::new()),
        DeployTarget::Ssh(config) => Box::new(ssh::SshExecutor::new(config.clone())),
        DeployTarget::Kubernetes(config) => Box::new(k8s::KubernetesExecutor::new(config.clone())),
    }
}

/// Build the CLI command string for a claude-cli stage.
pub fn build_claude_command(model: &str, system_prompt: Option<&str>, user_prompt: &str) -> String {
    let claude_bin = std::env::var("CLAUDE_CLI_PATH").unwrap_or_else(|_| "claude".to_string());

    let mut parts = vec![claude_bin];
    parts.push("--model".to_string());
    parts.push(shell_escape(model));
    parts.push("--output-format".to_string());
    parts.push("text".to_string());

    if let Some(system) = system_prompt {
        parts.push("--system-prompt".to_string());
        parts.push(shell_escape(system));
    }

    parts.push("--prompt".to_string());
    parts.push(shell_escape(user_prompt));

    parts.join(" ")
}

/// Build the CLI command string for an ollama stage.
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
    matches!(backend, Backend::ClaudeCli | Backend::Ollama)
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
        assert!(cmd.contains("--prompt"));
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
    fn backend_needs_executor_classification() {
        assert!(backend_needs_executor(Backend::ClaudeCli));
        assert!(backend_needs_executor(Backend::Ollama));
        assert!(!backend_needs_executor(Backend::Api));
    }

    #[test]
    fn deploy_target_local_default() {
        let config = FlowConfig {
            version: "1".to_string(),
            name: "test".to_string(),
            defaults: crate::config::Defaults {
                model: "m".to_string(),
                backend: Backend::ClaudeCli,
            },
            agents: vec![],
            stages: vec![],
            state: crate::config::StateConfig {
                backend: "local".to_string(),
                path: ".koto/state".to_string(),
            },
        };
        // Should not panic -- returns local executor
        let _ = create_executor(&config);
    }
}
