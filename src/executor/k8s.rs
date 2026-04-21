use super::{
    ExecutionHandle, ExecutionOutput, ExecutionStatus, ExecutionTask, Executor, ExecutorError,
    K8sConfig,
};

/// Executes stages as Kubernetes Jobs.
/// Currently a stub -- returns NotImplemented for all operations.
#[derive(Debug, Clone)]
pub struct KubernetesExecutor {
    config: K8sConfig,
}

impl KubernetesExecutor {
    pub fn new(config: K8sConfig) -> Self {
        Self { config }
    }
}

impl Executor for KubernetesExecutor {
    async fn spawn(&self, _task: ExecutionTask) -> Result<ExecutionHandle, ExecutorError> {
        Err(ExecutorError::NotImplemented(format!(
            "kubernetes executor for namespace '{}' coming soon",
            self.config.namespace
        )))
    }

    async fn wait(&self, _handle: &ExecutionHandle) -> Result<ExecutionOutput, ExecutorError> {
        Err(ExecutorError::NotImplemented(
            "kubernetes executor: wait not implemented".to_string(),
        ))
    }

    async fn status(&self, _handle: &ExecutionHandle) -> Result<ExecutionStatus, ExecutorError> {
        Err(ExecutorError::NotImplemented(
            "kubernetes executor: status not implemented".to_string(),
        ))
    }

    async fn stop(&self, _handle: &ExecutionHandle) -> Result<(), ExecutorError> {
        Err(ExecutorError::NotImplemented(
            "kubernetes executor: stop not implemented".to_string(),
        ))
    }
}
