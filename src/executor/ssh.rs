use super::{
    ExecutionHandle, ExecutionOutput, ExecutionStatus, ExecutionTask, Executor, ExecutorError,
    SshConfig,
};

/// Executes stages on remote hosts via SSH.
/// Currently a stub -- returns NotImplemented for all operations.
#[derive(Debug, Clone)]
pub struct SshExecutor {
    config: SshConfig,
}

impl SshExecutor {
    pub fn new(config: SshConfig) -> Self {
        Self { config }
    }
}

impl Executor for SshExecutor {
    async fn spawn(&self, _task: ExecutionTask) -> Result<ExecutionHandle, ExecutorError> {
        Err(ExecutorError::NotImplemented(format!(
            "ssh executor for host '{}' coming soon",
            self.config.host
        )))
    }

    async fn wait(&self, _handle: &ExecutionHandle) -> Result<ExecutionOutput, ExecutorError> {
        Err(ExecutorError::NotImplemented(
            "ssh executor: wait not implemented".to_string(),
        ))
    }

    async fn status(&self, _handle: &ExecutionHandle) -> Result<ExecutionStatus, ExecutorError> {
        Err(ExecutorError::NotImplemented(
            "ssh executor: status not implemented".to_string(),
        ))
    }

    async fn stop(&self, _handle: &ExecutionHandle) -> Result<(), ExecutorError> {
        Err(ExecutorError::NotImplemented(
            "ssh executor: stop not implemented".to_string(),
        ))
    }
}
