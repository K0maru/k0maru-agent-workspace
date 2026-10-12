use crate::env::EnvironmentManager;
use crate::error::WorkspaceError;
use crate::handle::{
    IsolationStrategy, PruneReport, SandboxHandle, TeardownAction, TeardownReceipt,
    WorkspaceProvisionRequest, WorktreeInfo,
};
use async_trait::async_trait;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime};
use tokio::fs;
use tokio::process::Command;

static TASK_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]+$").expect("valid regex"));

#[async_trait]
pub trait WorkspaceManager: Send + Sync {
    async fn provision(
        &self,
        req: &WorkspaceProvisionRequest,
    ) -> Result<SandboxHandle, WorkspaceError>;
    async fn teardown(
        &self,
        task_id: &str,
        action: TeardownAction,
    ) -> Result<TeardownReceipt, WorkspaceError>;
    async fn list_sandboxes(&self) -> Result<Vec<WorktreeInfo>, WorkspaceError>;
    async fn prune_stale(&self, max_age_secs: u64) -> Result<PruneReport, WorkspaceError>;
}

#[derive(Debug, Clone)]
pub struct WorktreeOrchestrator {
    pub repo_root: PathBuf,
}

impl WorktreeOrchestrator {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    pub fn validate_task_id(task_id: &str) -> Result<(), WorkspaceError> {
        let trimmed = task_id.trim();
        if trimmed.is_empty() || !TASK_ID_RE.is_match(trimmed) || trimmed.contains("..") {
            return Err(WorkspaceError::InvalidTaskId(task_id.to_string()));
        }
        Ok(())
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.repo_root.join(".worktrees")
    }

    pub fn task_worktree_path(&self, task_id: &str) -> PathBuf {
        self.worktrees_dir().join(task_id)
    }

    pub fn branch_name_for_task(task_id: &str) -> String {
        let suffix = task_id
            .strip_prefix("task-")
            .or_else(|| task_id.strip_prefix("task_"))
            .unwrap_or(task_id);
        format!("feat/task-{}", suffix)
    }

    pub fn extract_task_id(path: &Path, branch: Option<&str>) -> Option<String> {
        let mut found_worktrees = false;
        for comp in path.components() {
            if found_worktrees {
                return comp.as_os_str().to_str().map(|s| s.to_string());
            }
            if comp.as_os_str() == ".worktrees" {
                found_worktrees = true;
            }
        }
        branch.and_then(|b| b.strip_prefix("feat/task-").map(|s| s.to_string()))
    }

    async fn run_git_in(cwd: &Path, args: &[&str]) -> Result<String, WorkspaceError> {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .await
            .map_err(WorkspaceError::IoError)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(WorkspaceError::GitError(stderr));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn run_git(&self, args: &[&str]) -> Result<String, WorkspaceError> {
        Self::run_git_in(&self.repo_root, args).await
    }

    pub async fn branch_exists(&self, branch_name: &str) -> bool {
        let ref_path = format!("refs/heads/{}", branch_name);
        Self::run_git_in(
            &self.repo_root,
            &["rev-parse", "--verify", "--quiet", &ref_path],
        )
        .await
        .is_ok()
    }

    pub fn metadata_dir(&self) -> PathBuf {
        self.worktrees_dir().join(".metadata")
    }

    pub fn metadata_path(&self, task_id: &str) -> PathBuf {
        self.metadata_dir().join(format!("{}.json", task_id))
    }

    pub async fn save_metadata(&self, handle: &SandboxHandle) -> Result<(), WorkspaceError> {
        let dir = self.metadata_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir).await?;
        }
        let json = serde_json::to_string_pretty(handle).map_err(|e| {
            WorkspaceError::GitError(format!("Failed to serialize metadata: {}", e))
        })?;
        fs::write(self.metadata_path(&handle.task_id), json).await?;
        Ok(())
    }

    pub async fn load_metadata(&self, task_id: &str) -> Option<SandboxHandle> {
        let path = self.metadata_path(task_id);
        if !path.exists() {
            return None;
        }
        let content = fs::read_to_string(path).await.ok()?;
        serde_json::from_str(&content).ok()
    }

    pub async fn remove_metadata(&self, task_id: &str) {
        let path = self.metadata_path(task_id);
        if path.exists() {
            let _ = fs::remove_file(path).await;
        }
    }

    pub fn find_os_temp_dir(task_id: &str) -> Option<PathBuf> {
        let prefix = format!("k0maru-temp-{}-", task_id);
        let os_temp = std::env::temp_dir();
        if let Ok(entries) = std::fs::read_dir(&os_temp) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(&prefix) {
                        return Some(entry.path());
                    }
                }
            }
        }
        None
    }
}

#[async_trait]
impl WorkspaceManager for WorktreeOrchestrator {
    async fn provision(
        &self,
        req: &WorkspaceProvisionRequest,
    ) -> Result<SandboxHandle, WorkspaceError> {
        Self::validate_task_id(&req.task_id)?;

        if self.load_metadata(&req.task_id).await.is_some() {
            return Err(WorkspaceError::AlreadyExists {
                task_id: req.task_id.clone(),
                path: format!("Task '{}' already active", req.task_id),
            });
        }

        match req.isolate_strategy {
            IsolationStrategy::InPlace => {
                let branch_name = match self.run_git(&["rev-parse", "--abbrev-ref", "HEAD"]).await {
                    Ok(b) if !b.is_empty() => b,
                    _ => req
                        .base_branch
                        .clone()
                        .unwrap_or_else(|| "HEAD".to_string()),
                };

                let handle = SandboxHandle {
                    task_id: req.task_id.clone(),
                    workspace_path: self.repo_root.clone(),
                    branch_name,
                    base_branch: req.base_branch.clone(),
                    is_isolated: false,
                    created_at: chrono::Utc::now().to_rfc3339(),
                    mounted_caches: Vec::new(),
                    injected_files: Vec::new(),
                    strategy: IsolationStrategy::InPlace,
                };

                self.save_metadata(&handle).await?;
                Ok(handle)
            }
            IsolationStrategy::TempDir => {
                if Self::find_os_temp_dir(&req.task_id).is_some() {
                    return Err(WorkspaceError::AlreadyExists {
                        task_id: req.task_id.clone(),
                        path: format!("Temp directory for task '{}' already exists", req.task_id),
                    });
                }

                #[allow(deprecated)]
                let temp_path = tempfile::Builder::new()
                    .prefix(&format!("k0maru-temp-{}-", req.task_id))
                    .tempdir()
                    .map_err(WorkspaceError::IoError)?
                    .into_path();

                let mounted_caches = match EnvironmentManager::mount_caches(
                    &self.repo_root,
                    &temp_path,
                    &req.shared_caches,
                )
                .await
                {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = EnvironmentManager::cleanup_mounts(&temp_path);
                        if temp_path.exists() {
                            let _ = fs::remove_dir_all(&temp_path).await;
                        }
                        return Err(e);
                    }
                };

                let injected_files = EnvironmentManager::inject_configs(
                    &self.repo_root,
                    &temp_path,
                    &req.injected_configs,
                )
                .await
                .unwrap_or_default();

                let branch_name = Self::branch_name_for_task(&req.task_id);

                let handle = SandboxHandle {
                    task_id: req.task_id.clone(),
                    workspace_path: temp_path,
                    branch_name,
                    base_branch: req.base_branch.clone(),
                    is_isolated: true,
                    created_at: chrono::Utc::now().to_rfc3339(),
                    mounted_caches,
                    injected_files,
                    strategy: IsolationStrategy::TempDir,
                };

                self.save_metadata(&handle).await?;
                Ok(handle)
            }
            IsolationStrategy::Worktree => {
                let worktrees_dir = self.worktrees_dir();
                let target_path = self.task_worktree_path(&req.task_id);
                let branch_name = Self::branch_name_for_task(&req.task_id);

                if target_path.exists() {
                    return Err(WorkspaceError::AlreadyExists {
                        task_id: req.task_id.clone(),
                        path: target_path.display().to_string(),
                    });
                }

                if self.branch_exists(&branch_name).await {
                    return Err(WorkspaceError::AlreadyExists {
                        task_id: req.task_id.clone(),
                        path: format!("branch '{}' already exists", branch_name),
                    });
                }

                if !worktrees_dir.exists() {
                    fs::create_dir_all(&worktrees_dir).await?;
                }

                let base_ref = req.base_branch.as_deref().unwrap_or("HEAD");
                let path_str = target_path.to_string_lossy().to_string();

                let add_res = self
                    .run_git(&["worktree", "add", "-b", &branch_name, &path_str, base_ref])
                    .await;

                if let Err(e) = add_res {
                    // Atomic rollback
                    let _ = EnvironmentManager::cleanup_mounts(&target_path);
                    let _ = self
                        .run_git(&["worktree", "remove", "--force", &path_str])
                        .await;
                    let _ = self.run_git(&["branch", "-D", &branch_name]).await;
                    if target_path.exists() {
                        let _ = fs::remove_dir_all(&target_path).await;
                    }
                    return Err(e);
                }

                let mounted_caches = match EnvironmentManager::mount_caches(
                    &self.repo_root,
                    &target_path,
                    &req.shared_caches,
                )
                .await
                {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = EnvironmentManager::cleanup_mounts(&target_path);
                        let _ = self
                            .run_git(&["worktree", "remove", "--force", &path_str])
                            .await;
                        let _ = self.run_git(&["branch", "-D", &branch_name]).await;
                        if target_path.exists() {
                            let _ = fs::remove_dir_all(&target_path).await;
                        }
                        return Err(e);
                    }
                };

                let injected_files = EnvironmentManager::inject_configs(
                    &self.repo_root,
                    &target_path,
                    &req.injected_configs,
                )
                .await
                .unwrap_or_default();

                let handle = SandboxHandle {
                    task_id: req.task_id.clone(),
                    workspace_path: target_path,
                    branch_name,
                    base_branch: req.base_branch.clone(),
                    is_isolated: true,
                    created_at: chrono::Utc::now().to_rfc3339(),
                    mounted_caches,
                    injected_files,
                    strategy: IsolationStrategy::Worktree,
                };

                self.save_metadata(&handle).await?;
                Ok(handle)
            }
        }
    }

    async fn list_sandboxes(&self) -> Result<Vec<WorktreeInfo>, WorkspaceError> {
        let stdout = self.run_git(&["worktree", "list", "--porcelain"]).await?;
        let mut worktrees = Vec::new();

        let mut cur_path: Option<PathBuf> = None;
        let mut cur_head = String::new();
        let mut cur_branch_opt: Option<String> = None;
        let mut is_bare = false;
        let mut is_locked = false;

        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                if let Some(path) = cur_path.take() {
                    let task_id = Self::extract_task_id(&path, cur_branch_opt.as_deref());
                    worktrees.push(WorktreeInfo {
                        task_id,
                        worktree_path: path,
                        head_commit: cur_head.clone(),
                        branch: cur_branch_opt.take(),
                        is_bare,
                        is_locked,
                    });
                }
                cur_head.clear();
                cur_branch_opt = None;
                is_bare = false;
                is_locked = false;
                continue;
            }

            if let Some(p) = line.strip_prefix("worktree ") {
                cur_path = Some(PathBuf::from(p));
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                cur_head = h.to_string();
            } else if let Some(b) = line.strip_prefix("branch ") {
                let branch_short = b.strip_prefix("refs/heads/").unwrap_or(b);
                cur_branch_opt = Some(branch_short.to_string());
            } else if line == "bare" {
                is_bare = true;
            } else if line.starts_with("locked") {
                is_locked = true;
            }
        }

        if let Some(path) = cur_path.take() {
            let task_id = Self::extract_task_id(&path, cur_branch_opt.as_deref());
            worktrees.push(WorktreeInfo {
                task_id,
                worktree_path: path,
                head_commit: cur_head,
                branch: cur_branch_opt,
                is_bare,
                is_locked,
            });
        }

        Ok(worktrees)
    }

    async fn teardown(
        &self,
        task_id: &str,
        action: TeardownAction,
    ) -> Result<TeardownReceipt, WorkspaceError> {
        Self::validate_task_id(task_id)?;
        let start_time = Instant::now();

        // 1. Check if metadata was recorded for this task
        let meta_opt = self.load_metadata(task_id).await;

        // 2. Identify strategy and path
        let (strategy, ws_path) = if let Some(meta) = &meta_opt {
            (meta.strategy, meta.workspace_path.clone())
        } else if let Some(temp_path) = Self::find_os_temp_dir(task_id) {
            (IsolationStrategy::TempDir, temp_path)
        } else {
            let target_path = self.task_worktree_path(task_id);
            let branch_name = Self::branch_name_for_task(task_id);
            if !target_path.exists() && !self.branch_exists(&branch_name).await {
                return Err(WorkspaceError::NotFound {
                    task_id: task_id.to_string(),
                });
            }
            (IsolationStrategy::Worktree, target_path)
        };

        match strategy {
            IsolationStrategy::InPlace => {
                // Safe no-op! MUST NEVER delete repo_root or delete branches.
                self.remove_metadata(task_id).await;
                Ok(TeardownReceipt {
                    task_id: task_id.to_string(),
                    action_taken: "in-place workspace completed without deletion".to_string(),
                    workspace_path: self.repo_root.clone(),
                    branch_deleted: false,
                    merge_commit: None,
                    duration_ms: start_time.elapsed().as_millis() as u64,
                    unmounted_caches: Vec::new(),
                })
            }
            IsolationStrategy::TempDir => {
                let unmounted_caches =
                    EnvironmentManager::cleanup_mounts(&ws_path).unwrap_or_default();

                match action {
                    TeardownAction::Retain => Ok(TeardownReceipt {
                        task_id: task_id.to_string(),
                        action_taken: "retained".to_string(),
                        workspace_path: ws_path,
                        branch_deleted: false,
                        merge_commit: None,
                        duration_ms: start_time.elapsed().as_millis() as u64,
                        unmounted_caches: Vec::new(),
                    }),
                    TeardownAction::Abort | TeardownAction::Merge { .. } => {
                        if ws_path.exists() {
                            let _ = fs::remove_dir_all(&ws_path).await;
                        }
                        self.remove_metadata(task_id).await;
                        Ok(TeardownReceipt {
                            task_id: task_id.to_string(),
                            action_taken: "aborted".to_string(),
                            workspace_path: ws_path,
                            branch_deleted: false,
                            merge_commit: None,
                            duration_ms: start_time.elapsed().as_millis() as u64,
                            unmounted_caches,
                        })
                    }
                }
            }
            IsolationStrategy::Worktree => {
                let target_path = ws_path;
                let branch_name = Self::branch_name_for_task(task_id);
                let path_str = target_path.to_string_lossy().to_string();

                match action {
                    TeardownAction::Abort => {
                        let unmounted_caches =
                            EnvironmentManager::cleanup_mounts(&target_path).unwrap_or_default();
                        if target_path.exists() {
                            let _ = self
                                .run_git(&["worktree", "remove", "--force", &path_str])
                                .await;
                            if target_path.exists() {
                                let _ = fs::remove_dir_all(&target_path).await;
                            }
                        }
                        let branch_deleted = if self.branch_exists(&branch_name).await {
                            self.run_git(&["branch", "-D", &branch_name]).await.is_ok()
                        } else {
                            false
                        };

                        self.remove_metadata(task_id).await;

                        Ok(TeardownReceipt {
                            task_id: task_id.to_string(),
                            action_taken: "aborted".to_string(),
                            workspace_path: target_path,
                            branch_deleted,
                            merge_commit: None,
                            duration_ms: start_time.elapsed().as_millis() as u64,
                            unmounted_caches,
                        })
                    }
                    TeardownAction::Merge {
                        target_branch,
                        squash,
                    } => {
                        if !self.branch_exists(&target_branch).await {
                            return Err(WorkspaceError::BranchError(format!(
                                "Target branch '{}' does not exist",
                                target_branch
                            )));
                        }

                        // Switch repo_root to target_branch
                        self.run_git(&["checkout", &target_branch]).await?;

                        let merge_res = if squash {
                            let res = self.run_git(&["merge", "--squash", &branch_name]).await;
                            if res.is_ok() {
                                let commit_msg =
                                    format!("feat({}): squash merge task {}", task_id, task_id);
                                self.run_git(&["commit", "-m", &commit_msg]).await
                            } else {
                                res
                            }
                        } else {
                            let commit_msg =
                                format!("Merge branch '{}' into {}", branch_name, target_branch);
                            self.run_git(&["merge", "--no-ff", "-m", &commit_msg, &branch_name])
                                .await
                        };

                        if let Err(e) = merge_res {
                            let _ = self.run_git(&["merge", "--abort"]).await;
                            return Err(WorkspaceError::MergeConflict {
                                task_id: task_id.to_string(),
                                details: e.to_string(),
                            });
                        }

                        let merge_commit = self.run_git(&["rev-parse", "HEAD"]).await.ok();

                        // Clean up mounts, worktree, and ephemeral branch
                        let unmounted_caches =
                            EnvironmentManager::cleanup_mounts(&target_path).unwrap_or_default();
                        if target_path.exists() {
                            let _ = self
                                .run_git(&["worktree", "remove", "--force", &path_str])
                                .await;
                            if target_path.exists() {
                                let _ = fs::remove_dir_all(&target_path).await;
                            }
                        }
                        let branch_deleted =
                            self.run_git(&["branch", "-D", &branch_name]).await.is_ok();

                        self.remove_metadata(task_id).await;

                        Ok(TeardownReceipt {
                            task_id: task_id.to_string(),
                            action_taken: "merged".to_string(),
                            workspace_path: target_path,
                            branch_deleted,
                            merge_commit,
                            duration_ms: start_time.elapsed().as_millis() as u64,
                            unmounted_caches,
                        })
                    }
                    TeardownAction::Retain => Ok(TeardownReceipt {
                        task_id: task_id.to_string(),
                        action_taken: "retained".to_string(),
                        workspace_path: target_path,
                        branch_deleted: false,
                        merge_commit: None,
                        duration_ms: start_time.elapsed().as_millis() as u64,
                        unmounted_caches: Vec::new(),
                    }),
                }
            }
        }
    }

    async fn prune_stale(&self, max_age_secs: u64) -> Result<PruneReport, WorkspaceError> {
        let mut report = PruneReport::default();
        let _ = self.run_git(&["worktree", "prune"]).await;

        let worktrees_dir = self.worktrees_dir();
        if !worktrees_dir.exists() {
            return Ok(report);
        }

        let mut read_dir = fs::read_dir(&worktrees_dir).await?;
        let max_age = Duration::from_secs(max_age_secs);

        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let task_id = match entry.file_name().to_str() {
                Some(name) => name.to_string(),
                None => continue,
            };

            if task_id.starts_with('.') {
                continue;
            }

            let is_stale = match entry.metadata().await {
                Ok(metadata) => match metadata.modified() {
                    Ok(mtime) => match SystemTime::now().duration_since(mtime) {
                        Ok(age) => age >= max_age,
                        Err(_) => true,
                    },
                    Err(_) => false,
                },
                Err(_) => false,
            };

            if is_stale {
                match self.teardown(&task_id, TeardownAction::Abort).await {
                    Ok(receipt) => {
                        report.pruned_task_ids.push(task_id);
                        report.pruned_paths.push(receipt.workspace_path);
                        if receipt.branch_deleted {
                            report
                                .pruned_branches
                                .push(Self::branch_name_for_task(&receipt.task_id));
                        }
                    }
                    Err(e) => {
                        report
                            .errors
                            .push(format!("Failed pruning task {}: {}", task_id, e));
                    }
                }
            }
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup_git_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        Command::new("git")
            .current_dir(path)
            .args(["init", "-b", "main"])
            .output()
            .await
            .unwrap();

        Command::new("git")
            .current_dir(path)
            .args(["config", "user.name", "Tester"])
            .output()
            .await
            .unwrap();

        Command::new("git")
            .current_dir(path)
            .args(["config", "user.email", "tester@example.com"])
            .output()
            .await
            .unwrap();

        let readme = path.join("README.md");
        fs::write(&readme, "# Test Repo\n").await.unwrap();

        Command::new("git")
            .current_dir(path)
            .args(["add", "."])
            .output()
            .await
            .unwrap();

        Command::new("git")
            .current_dir(path)
            .args(["commit", "-m", "initial commit"])
            .output()
            .await
            .unwrap();

        dir
    }

    #[tokio::test]
    async fn test_validate_task_id() {
        assert!(WorktreeOrchestrator::validate_task_id("task-123").is_ok());
        assert!(WorktreeOrchestrator::validate_task_id("fix_42").is_ok());
        assert!(WorktreeOrchestrator::validate_task_id("").is_err());
        assert!(WorktreeOrchestrator::validate_task_id("../escape").is_err());
        assert!(WorktreeOrchestrator::validate_task_id("bad/slash").is_err());
        assert!(WorktreeOrchestrator::validate_task_id("bad*char").is_err());
    }

    #[tokio::test]
    async fn test_provision_and_list() {
        let repo = setup_git_repo().await;
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-01", repo.path()).with_base_branch("main");

        let handle = orchestrator.provision(&req).await.expect("provision");
        assert_eq!(handle.task_id, "task-01");
        assert_eq!(handle.branch_name, "feat/task-01");
        assert!(handle.workspace_path.exists());

        let sandboxes = orchestrator.list_sandboxes().await.expect("list");
        assert!(sandboxes
            .iter()
            .any(|s| s.task_id.as_deref() == Some("task-01")));

        // Double provision must fail with AlreadyExists
        let err = orchestrator.provision(&req).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::AlreadyExists { .. }));
    }

    #[tokio::test]
    async fn test_teardown_abort() {
        let repo = setup_git_repo().await;
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-abort", repo.path());
        let handle = orchestrator.provision(&req).await.expect("provision");

        // Write a dirty file in worktree
        let dirty_file = handle.workspace_path.join("dirty.txt");
        fs::write(&dirty_file, "uncommitted changes").await.unwrap();

        let receipt = orchestrator
            .teardown("task-abort", TeardownAction::Abort)
            .await
            .expect("teardown abort");

        assert_eq!(receipt.action_taken, "aborted");
        assert!(receipt.branch_deleted);
        assert!(!handle.workspace_path.exists());
    }

    #[tokio::test]
    async fn test_teardown_merge() {
        let repo = setup_git_repo().await;
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req =
            WorkspaceProvisionRequest::new("task-merge", repo.path()).with_base_branch("main");
        let handle = orchestrator.provision(&req).await.expect("provision");

        // Create a new file in worktree and commit it
        let new_file = handle.workspace_path.join("feature.txt");
        fs::write(&new_file, "implemented feature").await.unwrap();

        Command::new("git")
            .current_dir(&handle.workspace_path)
            .args(["add", "."])
            .output()
            .await
            .unwrap();

        Command::new("git")
            .current_dir(&handle.workspace_path)
            .args(["commit", "-m", "add feature"])
            .output()
            .await
            .unwrap();

        let receipt = orchestrator
            .teardown(
                "task-merge",
                TeardownAction::Merge {
                    target_branch: "main".into(),
                    squash: false,
                },
            )
            .await
            .expect("teardown merge");

        assert_eq!(receipt.action_taken, "merged");
        assert!(receipt.merge_commit.is_some());
        assert!(!handle.workspace_path.exists());

        // Verify file is now in main repository
        let merged_file = repo.path().join("feature.txt");
        assert!(merged_file.exists());
    }

    #[tokio::test]
    async fn test_teardown_retain() {
        let repo = setup_git_repo().await;
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-retain", repo.path());
        let handle = orchestrator.provision(&req).await.expect("provision");

        let receipt = orchestrator
            .teardown("task-retain", TeardownAction::Retain)
            .await
            .expect("teardown retain");

        assert_eq!(receipt.action_taken, "retained");
        assert!(!receipt.branch_deleted);
        assert!(handle.workspace_path.exists());
    }

    #[tokio::test]
    async fn test_prune_stale() {
        let repo = setup_git_repo().await;
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-stale", repo.path());
        let handle = orchestrator.provision(&req).await.expect("provision");
        assert!(handle.workspace_path.exists());

        // Prune with 0 max_age_secs to force pruning
        let report = orchestrator.prune_stale(0).await.expect("prune");
        assert!(report.pruned_task_ids.contains(&"task-stale".to_string()));
        assert!(!handle.workspace_path.exists());
    }

    #[tokio::test]
    async fn test_provision_and_teardown_inplace() {
        let repo = setup_git_repo().await;
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-inplace-01", repo.path())
            .with_strategy(IsolationStrategy::InPlace);

        let handle = orchestrator
            .provision(&req)
            .await
            .expect("provision inplace");
        assert_eq!(handle.task_id, "task-inplace-01");
        assert_eq!(handle.strategy, IsolationStrategy::InPlace);
        assert_eq!(handle.workspace_path, repo.path());
        assert!(!handle.is_isolated);
        assert_eq!(handle.branch_name, "main");
        // Must not create git worktree
        assert!(!orchestrator.task_worktree_path("task-inplace-01").exists());

        // Double provision must fail
        let dup_err = orchestrator.provision(&req).await.unwrap_err();
        assert!(matches!(dup_err, WorkspaceError::AlreadyExists { .. }));

        // Teardown must be safe no-op and never delete repo_root or branch
        let receipt = orchestrator
            .teardown("task-inplace-01", TeardownAction::Abort)
            .await
            .expect("teardown inplace");

        assert_eq!(receipt.task_id, "task-inplace-01");
        assert_eq!(
            receipt.action_taken,
            "in-place workspace completed without deletion"
        );
        assert!(!receipt.branch_deleted);
        assert_eq!(receipt.workspace_path, repo.path());
        assert!(repo.path().exists());
        assert!(repo.path().join("README.md").exists());
        assert!(orchestrator.branch_exists("main").await);

        // Teardown again should be NotFound
        let not_found_err = orchestrator
            .teardown("task-inplace-01", TeardownAction::Abort)
            .await
            .unwrap_err();
        assert!(matches!(not_found_err, WorkspaceError::NotFound { .. }));
    }

    #[tokio::test]
    async fn test_provision_and_teardown_tempdir() {
        let repo = setup_git_repo().await;
        let orchestrator = WorktreeOrchestrator::new(repo.path());

        let req = WorkspaceProvisionRequest::new("task-temp-01", repo.path())
            .with_strategy(IsolationStrategy::TempDir);

        let handle = orchestrator
            .provision(&req)
            .await
            .expect("provision tempdir");
        assert_eq!(handle.task_id, "task-temp-01");
        assert_eq!(handle.strategy, IsolationStrategy::TempDir);
        assert!(handle.is_isolated);
        assert!(handle.workspace_path.exists());

        let temp_file = handle.workspace_path.join("scratch.txt");
        fs::write(&temp_file, "hello temp").await.unwrap();
        assert!(temp_file.exists());

        // Teardown must remove temp directory
        let receipt = orchestrator
            .teardown("task-temp-01", TeardownAction::Abort)
            .await
            .expect("teardown tempdir");

        assert_eq!(receipt.task_id, "task-temp-01");
        assert_eq!(receipt.action_taken, "aborted");
        assert!(!handle.workspace_path.exists());

        // Teardown again should be NotFound
        let not_found_err = orchestrator
            .teardown("task-temp-01", TeardownAction::Abort)
            .await
            .unwrap_err();
        assert!(matches!(not_found_err, WorkspaceError::NotFound { .. }));
    }
}
