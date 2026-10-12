use crate::error::WorkspaceError;
use crate::handle::{SandboxHandle, WorkspaceProvisionRequest};
use async_trait::async_trait;

#[async_trait]
pub trait WorkspaceManager: Send + Sync {
    async fn provision(&self, req: &WorkspaceProvisionRequest) -> Result<SandboxHandle, WorkspaceError>;
    async fn teardown(&self, task_id: &str) -> Result<(), WorkspaceError>;
    async fn prune_stale(&self, max_age_secs: u64) -> Result<usize, WorkspaceError>;
}
