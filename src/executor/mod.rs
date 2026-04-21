use std::collections::HashMap;
use std::future::Future;
use std::path::Path;

use serde::Deserialize;

use crate::config::Backend;

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

// --- Executor config files (.koto/executors/<name>.yaml) ---

/// Parsed executor configuration from `.koto/executors/<name>.yaml`.
/// The `kind` field determines which executor implementation to use.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ExecutorConfig {
    Local {
        #[serde(default = "default_runtime")]
        runtime: String,
    },
    Ssh {
        host: String,
        user: Option<String>,
        key_file: Option<String>,
    },
    Kubernetes {
        namespace: String,
        image: String,
        service_account: Option<String>,
        resources: Option<K8sResources>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct K8sResources {
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

fn default_runtime() -> String {
    "tmux".to_string()
}

/// Load an executor config from `.koto/executors/<name>.yaml`.
fn load_executor_config(koto_dir: &Path, name: &str) -> Result<ExecutorConfig, color_eyre::Report> {
    let path = koto_dir.join("executors").join(format!("{name}.yaml"));
    if !path.exists() {
        return Err(color_eyre::eyre::eyre!(
            "executor '{}' not found at {}\n\nhint: create {} with a 'kind' field (local, ssh, kubernetes)",
            name,
            path.display(),
            path.display()
        ));
    }
    let contents = std::fs::read_to_string(&path)?;
    let config: ExecutorConfig = serde_yaml::from_str(&contents)
        .map_err(|e| color_eyre::eyre::eyre!("failed to parse {}: {e}", path.display()))?;
    Ok(config)
}

// --- Factory ---

/// Resolve the executor from CLI flag, env var, or default.
/// Resolution order: --executor flag > KOTO_EXECUTOR env var > default "local".
/// "local" is built-in (no file needed). Other names load from `.koto/executors/<name>.yaml`.
pub fn resolve_executor_target(
    cli_flag: Option<&str>,
    koto_dir: &Path,
) -> Result<DeployTarget, color_eyre::Report> {
    let name = cli_flag
        .map(|s| s.to_string())
        .or_else(|| std::env::var("KOTO_EXECUTOR").ok())
        .unwrap_or_else(|| "local".to_string());

    // "local" is the built-in default -- no file needed
    if name == "local" {
        return Ok(DeployTarget::Local);
    }

    let config = load_executor_config(koto_dir, &name)?;
    match config {
        ExecutorConfig::Local { .. } => Ok(DeployTarget::Local),
        ExecutorConfig::Ssh {
            host,
            user,
            key_file,
        } => Ok(DeployTarget::Ssh(SshConfig {
            host,
            user,
            key_file,
        })),
        ExecutorConfig::Kubernetes {
            namespace,
            image,
            service_account,
            ..
        } => Ok(DeployTarget::Kubernetes(K8sConfig {
            namespace,
            image,
            service_account,
        })),
    }
}

/// Create executor for a specific deploy target.
pub fn create_executor(target: &DeployTarget) -> Box<dyn ExecutorBoxed> {
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
    fn create_executor_local() {
        let _ = create_executor(&DeployTarget::Local);
    }

    #[test]
    fn resolve_executor_defaults_to_local() {
        let dir = tempfile::tempdir().unwrap();
        let target = resolve_executor_target(None, dir.path()).unwrap();
        assert_eq!(target, DeployTarget::Local);
    }

    #[test]
    fn resolve_executor_local_flag() {
        let dir = tempfile::tempdir().unwrap();
        let target = resolve_executor_target(Some("local"), dir.path()).unwrap();
        assert_eq!(target, DeployTarget::Local);
    }

    #[test]
    fn resolve_executor_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_executor_target(Some("magic"), dir.path()).unwrap_err();
        assert!(err.to_string().contains("executor 'magic' not found"));
    }

    #[test]
    fn resolve_executor_ssh_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let executors_dir = dir.path().join("executors");
        std::fs::create_dir_all(&executors_dir).unwrap();
        let config = "kind: ssh\nhost: dev-server\nuser: deploy\n";
        std::fs::write(executors_dir.join("remote.yaml"), config).unwrap();
        let target = resolve_executor_target(Some("remote"), dir.path()).unwrap();
        assert_eq!(
            target,
            DeployTarget::Ssh(SshConfig {
                host: "dev-server".to_string(),
                user: Some("deploy".to_string()),
                key_file: None,
            })
        );
    }

    #[test]
    fn resolve_executor_k8s_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let executors_dir = dir.path().join("executors");
        std::fs::create_dir_all(&executors_dir).unwrap();
        let config = "kind: kubernetes\nnamespace: koto-agents\nimage: ghcr.io/org/agent:latest\n";
        std::fs::write(executors_dir.join("k8s.yaml"), config).unwrap();
        let target = resolve_executor_target(Some("k8s"), dir.path()).unwrap();
        assert_eq!(
            target,
            DeployTarget::Kubernetes(K8sConfig {
                namespace: "koto-agents".to_string(),
                image: "ghcr.io/org/agent:latest".to_string(),
                service_account: None,
            })
        );
    }

    #[test]
    fn resolve_executor_local_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let executors_dir = dir.path().join("executors");
        std::fs::create_dir_all(&executors_dir).unwrap();
        let config = "kind: local\nruntime: tmux\n";
        std::fs::write(executors_dir.join("dev.yaml"), config).unwrap();
        let target = resolve_executor_target(Some("dev"), dir.path()).unwrap();
        assert_eq!(target, DeployTarget::Local);
    }
}
