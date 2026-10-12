use k0maru_workspace::error::WorkspaceError;
use k0maru_workspace::handle::{IsolationStrategy, TeardownAction, WorkspaceProvisionRequest};
use k0maru_workspace::orchestrator::{WorkspaceManager, WorktreeOrchestrator};
use std::process::Command;
use tempfile::TempDir;

fn init_test_git_repo() -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path();

    let run_cmd = |args: &[&str]| {
        let status = Command::new("git")
            .current_dir(path)
            .args(args)
            .status()
            .expect("run git command");
        assert!(status.success(), "git {:?} failed", args);
    };

    run_cmd(&["init", "-b", "main"]);
    run_cmd(&["config", "user.name", "Tester"]);
    run_cmd(&["config", "user.email", "tester@example.com"]);

    std::fs::write(path.join("README.md"), "# Lifecycle Test Repo\n").unwrap();
    run_cmd(&["add", "README.md"]);
    run_cmd(&["commit", "-m", "Initial commit on main"]);

    // Create dev branch
    run_cmd(&["checkout", "-b", "dev"]);
    std::fs::write(path.join("DEV.md"), "# Dev Branch\n").unwrap();
    run_cmd(&["add", "DEV.md"]);
    run_cmd(&["commit", "-m", "Add dev doc"]);

    dir
}

#[tokio::test]
async fn test_full_lifecycle_merge_to_dev() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());

    let req = WorkspaceProvisionRequest::new("task-feat-100", repo.path())
        .with_base_branch("dev")
        .with_strategy(IsolationStrategy::Worktree);

    let handle = orchestrator.provision(&req).await.expect("provision");
    assert_eq!(handle.task_id, "task-feat-100");
    assert_eq!(handle.branch_name, "feat/task-feat-100");
    assert!(handle.workspace_path.exists());

    // Agent works in isolated worktree
    let code_file = handle.workspace_path.join("feature.rs");
    std::fs::write(&code_file, "pub fn feat() -> bool { true }\n").unwrap();

    let status = Command::new("git")
        .current_dir(&handle.workspace_path)
        .args(["add", "feature.rs"])
        .status()
        .unwrap();
    assert!(status.success());

    let status = Command::new("git")
        .current_dir(&handle.workspace_path)
        .args(["commit", "-m", "feat(task-100): implement feature"])
        .status()
        .unwrap();
    assert!(status.success());

    // Teardown with Merge into dev
    let receipt = orchestrator
        .teardown(
            "task-feat-100",
            TeardownAction::Merge {
                target_branch: "dev".to_string(),
                squash: true,
            },
        )
        .await
        .expect("teardown merge");

    assert_eq!(receipt.action_taken, "merged");
    assert!(receipt.branch_deleted);
    assert!(!handle.workspace_path.exists());
    assert!(receipt.merge_commit.is_some());

    // Check file exists in dev
    assert!(repo.path().join("feature.rs").exists());
}

#[tokio::test]
async fn test_lifecycle_abort_with_dirty_tree() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());

    let req = WorkspaceProvisionRequest::new("task-dirty", repo.path()).with_base_branch("main");

    let handle = orchestrator.provision(&req).await.expect("provision");

    // Add uncommitted dirty file
    std::fs::write(handle.workspace_path.join("dirty.tmp"), "garbage").unwrap();

    let receipt = orchestrator
        .teardown("task-dirty", TeardownAction::Abort)
        .await
        .expect("abort");

    assert_eq!(receipt.action_taken, "aborted");
    assert!(receipt.branch_deleted);
    assert!(!handle.workspace_path.exists());
}

#[tokio::test]
async fn test_lifecycle_retain() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());

    let req = WorkspaceProvisionRequest::new("task-retain-01", repo.path());
    let handle = orchestrator.provision(&req).await.expect("provision");

    let receipt = orchestrator
        .teardown("task-retain-01", TeardownAction::Retain)
        .await
        .expect("retain");

    assert_eq!(receipt.action_taken, "retained");
    assert!(!receipt.branch_deleted);
    assert!(handle.workspace_path.exists());
}

#[tokio::test]
async fn test_duplicate_provision_error() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());

    let req = WorkspaceProvisionRequest::new("task-dup", repo.path());
    orchestrator.provision(&req).await.expect("first provision");

    let err = orchestrator.provision(&req).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::AlreadyExists { .. }));
}

#[tokio::test]
async fn test_invalid_task_id_rejection() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());

    let req_traversal = WorkspaceProvisionRequest::new("../../../etc/passwd", repo.path());
    let err = orchestrator.provision(&req_traversal).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::InvalidTaskId(_)));

    let req_empty = WorkspaceProvisionRequest::new("   ", repo.path());
    let err2 = orchestrator.provision(&req_empty).await.unwrap_err();
    assert!(matches!(err2, WorkspaceError::InvalidTaskId(_)));
}

#[tokio::test]
async fn test_not_found_teardown() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());

    let err = orchestrator
        .teardown("non-existent-task", TeardownAction::Abort)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::NotFound { .. }));
}

#[tokio::test]
async fn test_concurrent_independent_worktrees() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());

    let req1 = WorkspaceProvisionRequest::new("task-concurrent-1", repo.path());
    let req2 = WorkspaceProvisionRequest::new("task-concurrent-2", repo.path());
    let req3 = WorkspaceProvisionRequest::new("task-concurrent-3", repo.path());

    let h1 = orchestrator.provision(&req1).await.expect("p1");
    let h2 = orchestrator.provision(&req2).await.expect("p2");
    let h3 = orchestrator.provision(&req3).await.expect("p3");

    assert!(h1.workspace_path.exists());
    assert!(h2.workspace_path.exists());
    assert!(h3.workspace_path.exists());

    let sandboxes = orchestrator.list_sandboxes().await.expect("list");
    assert!(sandboxes
        .iter()
        .any(|s| s.task_id.as_deref() == Some("task-concurrent-1")));
    assert!(sandboxes
        .iter()
        .any(|s| s.task_id.as_deref() == Some("task-concurrent-2")));
    assert!(sandboxes
        .iter()
        .any(|s| s.task_id.as_deref() == Some("task-concurrent-3")));

    // Clean up
    orchestrator
        .teardown("task-concurrent-1", TeardownAction::Abort)
        .await
        .unwrap();
    orchestrator
        .teardown("task-concurrent-2", TeardownAction::Abort)
        .await
        .unwrap();
    orchestrator
        .teardown("task-concurrent-3", TeardownAction::Abort)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_prune_stale_sandboxes() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());

    let req = WorkspaceProvisionRequest::new("task-prune-me", repo.path());
    let handle = orchestrator.provision(&req).await.expect("provision");
    assert!(handle.workspace_path.exists());

    // Prune with 0s max age removes it
    let report = orchestrator.prune_stale(0).await.expect("prune");
    assert!(report
        .pruned_task_ids
        .contains(&"task-prune-me".to_string()));
    assert!(!handle.workspace_path.exists());
}
