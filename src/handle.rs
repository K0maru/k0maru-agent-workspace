use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceProvisionRequest {
    pub task_id: String,
    pub repo_root: PathBuf,
    pub base_branch: Option<String>,
    pub isolate_strategy: IsolationStrategy,
    pub shared_caches: Vec<CacheType>,
    pub injected_configs: Vec<String>,
}

impl WorkspaceProvisionRequest {
    pub fn new(task_id: impl Into<String>, repo_root: impl Into<PathBuf>) -> Self {
        Self {
            task_id: task_id.into(),
            repo_root: repo_root.into(),
            base_branch: None,
            isolate_strategy: IsolationStrategy::Worktree,
            shared_caches: Vec::new(),
            injected_configs: Vec::new(),
        }
    }

    pub fn with_base_branch(mut self, base: impl Into<String>) -> Self {
        self.base_branch = Some(base.into());
        self
    }

    pub fn with_strategy(mut self, strategy: IsolationStrategy) -> Self {
        self.isolate_strategy = strategy;
        self
    }

    pub fn with_caches(mut self, caches: Vec<CacheType>) -> Self {
        self.shared_caches = caches;
        self
    }

    pub fn with_injected_configs(mut self, configs: Vec<String>) -> Self {
        self.injected_configs = configs;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IsolationStrategy {
    #[default]
    Worktree,
    TempDir,
    InPlace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheType {
    CargoTarget,
    NodeModules,
    PythonVenv,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxHandle {
    pub task_id: String,
    pub workspace_path: PathBuf,
    pub branch_name: String,
    pub base_branch: Option<String>,
    pub is_isolated: bool,
    pub created_at: String,
    #[serde(default)]
    pub mounted_caches: Vec<CacheType>,
    #[serde(default)]
    pub injected_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TeardownAction {
    Abort,
    Merge {
        target_branch: String,
        #[serde(default)]
        squash: bool,
    },
    Retain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeardownReceipt {
    pub task_id: String,
    pub action_taken: String,
    pub workspace_path: PathBuf,
    pub branch_deleted: bool,
    pub merge_commit: Option<String>,
    pub duration_ms: u64,
    #[serde(default)]
    pub unmounted_caches: Vec<CacheType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub task_id: Option<String>,
    pub worktree_path: PathBuf,
    pub head_commit: String,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PruneReport {
    pub pruned_task_ids: Vec<String>,
    pub pruned_paths: Vec<PathBuf>,
    pub pruned_branches: Vec<String>,
    pub errors: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workspace_provision_request_builder() {
        let req = WorkspaceProvisionRequest::new("task-42", "/tmp/repo")
            .with_base_branch("main")
            .with_strategy(IsolationStrategy::Worktree)
            .with_caches(vec![CacheType::CargoTarget])
            .with_injected_configs(vec![".env".into()]);

        assert_eq!(req.task_id, "task-42");
        assert_eq!(req.base_branch.as_deref(), Some("main"));
        assert_eq!(req.isolate_strategy, IsolationStrategy::Worktree);
        assert_eq!(req.shared_caches, vec![CacheType::CargoTarget]);
        assert_eq!(req.injected_configs, vec![".env".to_string()]);

        let json = serde_json::to_string(&req).expect("serialize");
        let deserialized: WorkspaceProvisionRequest =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(req, deserialized);
    }

    #[test]
    fn test_teardown_action_serde() {
        let abort = TeardownAction::Abort;
        let json_abort = serde_json::to_string(&abort).unwrap();
        assert!(json_abort.contains("\"abort\""));
        let back_abort: TeardownAction = serde_json::from_str(&json_abort).unwrap();
        assert_eq!(abort, back_abort);

        let merge = TeardownAction::Merge {
            target_branch: "dev".into(),
            squash: true,
        };
        let json_merge = serde_json::to_string(&merge).unwrap();
        let back_merge: TeardownAction = serde_json::from_str(&json_merge).unwrap();
        assert_eq!(merge, back_merge);
    }

    #[test]
    fn test_sandbox_handle_and_receipt_serde() {
        let handle = SandboxHandle {
            task_id: "task-01".into(),
            workspace_path: PathBuf::from("/tmp/repo/.worktrees/task-01"),
            branch_name: "feat/task-01".into(),
            base_branch: Some("dev".into()),
            is_isolated: true,
            created_at: "2026-10-12T00:00:00Z".into(),
            mounted_caches: vec![CacheType::CargoTarget],
            injected_files: vec![".env".into()],
        };

        let json = serde_json::to_string(&handle).unwrap();
        let deserialized: SandboxHandle = serde_json::from_str(&json).unwrap();
        assert_eq!(handle, deserialized);

        let receipt = TeardownReceipt {
            task_id: "task-01".into(),
            action_taken: "merged".into(),
            workspace_path: PathBuf::from("/tmp/repo/.worktrees/task-01"),
            branch_deleted: true,
            merge_commit: Some("abcdef123".into()),
            duration_ms: 120,
            unmounted_caches: vec![CacheType::CargoTarget],
        };
        let receipt_json = serde_json::to_string(&receipt).unwrap();
        let deserialized_receipt: TeardownReceipt = serde_json::from_str(&receipt_json).unwrap();
        assert_eq!(receipt, deserialized_receipt);
    }
}
