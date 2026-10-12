use assert_cmd::Command;
use k0maru_workspace::handle::{IsolationStrategy, TeardownAction, WorkspaceProvisionRequest};
use k0maru_workspace::orchestrator::{WorkspaceManager, WorktreeOrchestrator};
use k0maru_workspace::router::{
    CallerInfo, RouterDispatchOptions, RouterDispatchReceipt, TargetInfo, TaskEnvelope,
    TaskPayload, WorkspaceRouterAdapter,
};
use predicates::prelude::*;
use std::process::Command as StdCommand;
use tempfile::TempDir;

fn init_test_git_repo() -> TempDir {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path();

    let run_cmd = |args: &[&str]| {
        let status = StdCommand::new("git")
            .current_dir(path)
            .args(args)
            .status()
            .expect("run git command");
        assert!(status.success(), "git {:?} failed", args);
    };

    run_cmd(&["init", "-b", "main"]);
    run_cmd(&["config", "user.name", "Tester"]);
    run_cmd(&["config", "user.email", "tester@example.com"]);

    std::fs::write(path.join("README.md"), "# Isolation Test Repo\n").unwrap();
    run_cmd(&["add", "README.md"]);
    run_cmd(&["commit", "-m", "Initial commit on main"]);

    // Create dev branch
    run_cmd(&["checkout", "-b", "dev"]);
    std::fs::write(path.join("DEV.md"), "# Dev doc\n").unwrap();
    run_cmd(&["add", "DEV.md"]);
    run_cmd(&["commit", "-m", "Dev commit"]);
    run_cmd(&["checkout", "main"]);

    dir
}

#[tokio::test]
async fn test_inplace_lifecycle_repo_intact() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());

    let req = WorkspaceProvisionRequest::new("task-ip-intact", repo.path())
        .with_strategy(IsolationStrategy::InPlace);

    let handle = orchestrator
        .provision(&req)
        .await
        .expect("provision inplace");
    assert_eq!(handle.task_id, "task-ip-intact");
    assert_eq!(handle.strategy, IsolationStrategy::InPlace);
    assert_eq!(handle.workspace_path, repo.path());
    assert!(!handle.is_isolated);
    assert_eq!(handle.branch_name, "main");

    // Ensure no git worktree directory was created
    assert!(!orchestrator.task_worktree_path("task-ip-intact").exists());

    // Teardown with Abort
    let receipt = orchestrator
        .teardown("task-ip-intact", TeardownAction::Abort)
        .await
        .expect("teardown inplace");

    assert_eq!(receipt.task_id, "task-ip-intact");
    assert_eq!(
        receipt.action_taken,
        "in-place workspace completed without deletion"
    );
    assert!(!receipt.branch_deleted);
    assert_eq!(receipt.workspace_path, repo.path());

    // Invariants: repo_root and branches must remain 100% intact
    assert!(repo.path().exists());
    assert!(repo.path().join("README.md").exists());
    assert!(orchestrator.branch_exists("main").await);
    assert!(orchestrator.branch_exists("dev").await);
}

#[tokio::test]
async fn test_tempdir_lifecycle_cleanup() {
    let repo = init_test_git_repo();
    let orchestrator = WorktreeOrchestrator::new(repo.path());

    let req = WorkspaceProvisionRequest::new("task-tmp-clean", repo.path())
        .with_strategy(IsolationStrategy::TempDir);

    let handle = orchestrator
        .provision(&req)
        .await
        .expect("provision tempdir");
    assert_eq!(handle.task_id, "task-tmp-clean");
    assert_eq!(handle.strategy, IsolationStrategy::TempDir);
    assert!(handle.is_isolated);
    assert!(handle.workspace_path.exists());
    assert_ne!(handle.workspace_path, repo.path());

    let scratch_file = handle.workspace_path.join("scratch.rs");
    std::fs::write(&scratch_file, "fn scratch() {}").unwrap();
    assert!(scratch_file.exists());

    // Teardown with Abort
    let receipt = orchestrator
        .teardown("task-tmp-clean", TeardownAction::Abort)
        .await
        .expect("teardown tempdir");

    assert_eq!(receipt.task_id, "task-tmp-clean");
    assert_eq!(receipt.action_taken, "aborted");
    assert!(!handle.workspace_path.exists());
}

#[tokio::test]
async fn test_router_auto_deduction_read_only_tag() {
    let repo = init_test_git_repo();
    let repo_path = repo.path();

    let envelope = TaskEnvelope {
        schema: None,
        id: "task_ro_deduce".into(),
        created_at: "2026-10-12T15:00:00Z".into(),
        caller: CallerInfo {
            workspace: "/caller".into(),
            agent: "router-master".into(),
            pane_id: None,
        },
        target: TargetInfo {
            identifier: "k0maru-workspace".into(),
            workspace: Some(repo_path.to_str().unwrap().into()),
            is_external: false,
        },
        intent: "inspect".into(),
        payload: TaskPayload {
            title: "Codebase search".into(),
            severity: "info".into(),
            reproduction: None,
            expected: None,
        },
        tags: vec!["read_only".into()],
    };

    let opts = RouterDispatchOptions {
        target_repo: Some(repo_path.to_path_buf()),
        isolate_strategy: None, // Auto-deduce from tags
        ..Default::default()
    };

    let receipt = WorkspaceRouterAdapter::dispatch(&envelope, opts)
        .await
        .expect("dispatch read_only");

    assert_eq!(receipt.sandbox_handle.strategy, IsolationStrategy::InPlace);
    assert_eq!(receipt.sandbox_handle.workspace_path, repo_path);
    assert!(!receipt.sandbox_handle.is_isolated);

    let orchestrator = WorktreeOrchestrator::new(repo_path);
    let td_receipt = orchestrator
        .teardown("task_ro_deduce", TeardownAction::Abort)
        .await
        .expect("teardown read_only");

    assert_eq!(
        td_receipt.action_taken,
        "in-place workspace completed without deletion"
    );
    assert!(repo_path.exists());
    assert!(repo_path.join("README.md").exists());
}

#[test]
fn test_cli_provision_and_teardown_strategy_inplace() {
    let repo = init_test_git_repo();
    let repo_path = repo.path().to_str().unwrap();

    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let output = cmd
        .args([
            "provision",
            "--task-id",
            "task_cli_ip",
            "--repo",
            repo_path,
            "--strategy",
            "inplace",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json_str = String::from_utf8(output).unwrap();
    assert!(json_str.contains("\"strategy\": \"in_place\""));
    assert!(json_str.contains("\"is_isolated\": false"));

    // Teardown
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let td_output = cmd
        .args([
            "teardown",
            "--task-id",
            "task_cli_ip",
            "--repo",
            repo_path,
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let td_str = String::from_utf8(td_output).unwrap();
    assert!(td_str.contains("in-place workspace completed without deletion"));
    assert!(repo.path().join("README.md").exists());
}

#[test]
fn test_cli_route_strategy_inplace_and_auto_deduction() {
    let repo = init_test_git_repo();
    let repo_path = repo.path().to_str().unwrap();

    // 1. Route with read_only tag in envelope (auto-deduction)
    let envelope_json = serde_json::json!({
        "id": "task_cli_route_ro",
        "created_at": "2026-10-12T15:30:00Z",
        "caller": { "workspace": "/caller", "agent": "router" },
        "target": { "identifier": "k0maru-workspace", "workspace": repo_path },
        "intent": "query",
        "payload": { "title": "Check config", "severity": "low" },
        "tags": ["read_only"]
    });

    let envelope_file = repo.path().join("envelope_ro.json");
    std::fs::write(
        &envelope_file,
        serde_json::to_string(&envelope_json).unwrap(),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let output = cmd
        .args([
            "route",
            "--envelope",
            envelope_file.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let receipt: RouterDispatchReceipt = serde_json::from_slice(&output).unwrap();
    assert_eq!(receipt.sandbox_handle.strategy, IsolationStrategy::InPlace);
    assert!(!receipt.sandbox_handle.is_isolated);

    // Teardown
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    cmd.args([
        "teardown",
        "--task-id",
        "task_cli_route_ro",
        "--repo",
        repo_path,
    ])
    .assert()
    .success()
    .stdout(predicate::str::contains(
        "in-place workspace completed without deletion",
    ));

    // 2. Route with explicit --strategy inplace overriding empty tags
    let envelope_plain = serde_json::json!({
        "id": "task_cli_route_override",
        "created_at": "2026-10-12T15:30:00Z",
        "caller": { "workspace": "/caller", "agent": "router" },
        "target": { "identifier": "k0maru-workspace", "workspace": repo_path },
        "intent": "general",
        "payload": { "title": "Explicit InPlace", "severity": "info" },
        "tags": []
    });

    let envelope_plain_file = repo.path().join("envelope_plain.json");
    std::fs::write(
        &envelope_plain_file,
        serde_json::to_string(&envelope_plain).unwrap(),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let output = cmd
        .args([
            "route",
            "--envelope",
            envelope_plain_file.to_str().unwrap(),
            "--strategy",
            "inplace",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let receipt_ov: RouterDispatchReceipt = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        receipt_ov.sandbox_handle.strategy,
        IsolationStrategy::InPlace
    );

    // Teardown
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    cmd.args([
        "teardown",
        "--task-id",
        "task_cli_route_override",
        "--repo",
        repo_path,
    ])
    .assert()
    .success();
}

#[test]
fn test_cli_provision_and_teardown_strategy_tempdir() {
    let repo = init_test_git_repo();
    let repo_path = repo.path().to_str().unwrap();

    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let output = cmd
        .args([
            "provision",
            "--task-id",
            "task_cli_temp",
            "--repo",
            repo_path,
            "--strategy",
            "tempdir",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json_val: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json_val["strategy"], "temp_dir");
    assert_eq!(json_val["is_isolated"], true);

    let ws_path = std::path::PathBuf::from(json_val["workspace_path"].as_str().unwrap());
    assert!(ws_path.exists());

    // Teardown
    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    cmd.args([
        "teardown",
        "--task-id",
        "task_cli_temp",
        "--repo",
        repo_path,
        "--json",
    ])
    .assert()
    .success();

    assert!(!ws_path.exists());
}
