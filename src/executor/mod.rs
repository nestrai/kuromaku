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

// --- Deploy config file (.koto/deploy.yaml) ---

/// Operator-level deploy configuration, separate from flow definitions.
/// Lives at `.koto/deploy.yaml`, potentially gitignored.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DeployConfig {
    pub ssh: Option<DeploySshConfig>,
    pub kubernetes: Option<DeployK8sConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeploySshConfig {
    pub host: String,
    pub user: Option<String>,
    pub key_file: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeployK8sConfig {
    pub namespace: String,
    pub image: String,
    pub service_account: Option<String>,
}

/// Load `.koto/deploy.yaml` if it exists. Returns None if file is absent.
fn load_deploy_config(koto_dir: &Path) -> Result<Option<DeployConfig>, ExecutorError> {
    let path = koto_dir.join("deploy.yaml");
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(&path)?;
    let config: DeployConfig = serde_yaml::from_str(&contents)
        .map_err(|e| ExecutorError::Spawn(format!("failed to parse {}: {e}", path.display())))?;
    Ok(Some(config))
}

// --- Factory ---

/// Resolve the deploy target from CLI flag, env var, or default.
/// Resolution order: CLI flag > KOTO_DEPLOY env var > default (local).
/// For ssh/kubernetes targets, connection details come from `.koto/deploy.yaml`.
pub fn resolve_deploy_target(
    cli_flag: Option<&str>,
    koto_dir: &Path,
) -> Result<DeployTarget, color_eyre::Report> {
    let target_name = cli_flag
        .map(|s| s.to_string())
        .or_else(|| std::env::var("KOTO_DEPLOY").ok())
        .unwrap_or_else(|| "local".to_string());

    match target_name.as_str() {
        "local" => Ok(DeployTarget::Local),
        "ssh" => {
            let deploy_config = load_deploy_config(koto_dir)?.ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "deploy target 'ssh' requires .koto/deploy.yaml with ssh config"
                )
            })?;
            let ssh = deploy_config.ssh.ok_or_else(|| {
                color_eyre::eyre::eyre!(".koto/deploy.yaml exists but has no 'ssh' section")
            })?;
            Ok(DeployTarget::Ssh(SshConfig {
                host: ssh.host,
                user: ssh.user,
                key_file: ssh.key_file,
            }))
        }
        "kubernetes" | "k8s" => {
            let deploy_config = load_deploy_config(koto_dir)?.ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "deploy target 'kubernetes' requires .koto/deploy.yaml with kubernetes config"
                )
            })?;
            let k8s = deploy_config.kubernetes.ok_or_else(|| {
                color_eyre::eyre::eyre!(".koto/deploy.yaml exists but has no 'kubernetes' section")
            })?;
            Ok(DeployTarget::Kubernetes(K8sConfig {
                namespace: k8s.namespace,
                image: k8s.image,
                service_account: k8s.service_account,
            }))
        }
        other => Err(color_eyre::eyre::eyre!(
            "unknown deploy target '{other}'\n\nvalid targets: local, ssh, kubernetes (k8s)"
        )),
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
    fn resolve_deploy_target_defaults_to_local() {
        let dir = tempfile::tempdir().unwrap();
        let target = resolve_deploy_target(None, dir.path()).unwrap();
        assert_eq!(target, DeployTarget::Local);
    }

    #[test]
    fn resolve_deploy_target_cli_flag() {
        let dir = tempfile::tempdir().unwrap();
        let target = resolve_deploy_target(Some("local"), dir.path()).unwrap();
        assert_eq!(target, DeployTarget::Local);
    }

    #[test]
    fn resolve_deploy_target_unknown_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_deploy_target(Some("magic"), dir.path()).unwrap_err();
        assert!(err.to_string().contains("unknown deploy target 'magic'"));
    }

    #[test]
    fn resolve_deploy_target_ssh_needs_config() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_deploy_target(Some("ssh"), dir.path()).unwrap_err();
        assert!(err.to_string().contains("deploy.yaml"));
    }

    #[test]
    fn resolve_deploy_target_ssh_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_content = "ssh:\n  host: dev-server\n  user: deploy\n";
        std::fs::write(dir.path().join("deploy.yaml"), config_content).unwrap();
        let target = resolve_deploy_target(Some("ssh"), dir.path()).unwrap();
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
    fn resolve_deploy_target_k8s_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_content =
            "kubernetes:\n  namespace: koto-agents\n  image: ghcr.io/org/agent:latest\n";
        std::fs::write(dir.path().join("deploy.yaml"), config_content).unwrap();
        let target = resolve_deploy_target(Some("k8s"), dir.path()).unwrap();
        assert_eq!(
            target,
            DeployTarget::Kubernetes(K8sConfig {
                namespace: "koto-agents".to_string(),
                image: "ghcr.io/org/agent:latest".to_string(),
                service_account: None,
            })
        );
    }
}
