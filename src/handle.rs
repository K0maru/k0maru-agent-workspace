use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceProvisionRequest {
    pub task_id: String,
    pub repo_root: PathBuf,
    pub base_branch: Option<String>,
    pub isolate_strategy: IsolationStrategy,
    pub shared_caches: Vec<CacheType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IsolationStrategy {
    Worktree,
    TempDir,
    InPlace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheType {
    CargoTarget,
    NodeModules,
    PythonVenv,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxHandle {
    pub task_id: String,
    pub workspace_path: PathBuf,
    pub branch_name: String,
    pub is_isolated: bool,
    pub created_at: String,
}
