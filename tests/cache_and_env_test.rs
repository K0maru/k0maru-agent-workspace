use assert_cmd::Command;
use k0maru_workspace::handle::{
    CacheType, SandboxHandle, TeardownAction, WorkspaceProvisionRequest,
};
use k0maru_workspace::orchestrator::{WorkspaceManager, WorktreeOrchestrator};
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

    std::fs::write(path.join("README.md"), "# Cache Test Repo\n").unwrap();
    run_cmd(&["add", "README.md"]);
    run_cmd(&["commit", "-m", "Initial commit on main"]);

    dir
}

#[tokio::test]
async fn test_provision_with_mounted_caches_and_safe_teardown() {
    let repo = init_test_git_repo();
    let repo_root = repo.path();

    // 1. Create realistic cache directories in repo_root
    let root_target = repo_root.join("target");
    std::fs::create_dir_all(&root_target).unwrap();
    std::fs::write(root_target.join("artifact.bin"), "binary data").unwrap();

    let root_node = repo_root.join("node_modules");
    std::fs::create_dir_all(&root_node).unwrap();
    std::fs::write(root_node.join("lodash.js"), "module.exports = {}").unwrap();

    let root_venv = repo_root.join(".venv");
    std::fs::create_dir_all(&root_venv).unwrap();
    std::fs::write(root_venv.join("pyvenv.cfg"), "home = /usr/bin").unwrap();

    let orchestrator = WorktreeOrchestrator::new(repo_root);
    let req = WorkspaceProvisionRequest::new("task-cache-01", repo_root).with_caches(vec![
        CacheType::CargoTarget,
        CacheType::NodeModules,
        CacheType::PythonVenv,
    ]);

    // 2. Provision worktree
    let handle = orchestrator.provision(&req).await.expect("provision");
    assert_eq!(handle.mounted_caches.len(), 3);
    assert!(handle.workspace_path.join("target/artifact.bin").exists());
    assert!(handle
        .workspace_path
        .join("node_modules/lodash.js")
        .exists());
    assert!(handle.workspace_path.join(".venv/pyvenv.cfg").exists());

    // 3. Teardown with Abort
    let receipt = orchestrator
        .teardown("task-cache-01", TeardownAction::Abort)
        .await
        .expect("teardown");

    assert_eq!(receipt.unmounted_caches.len(), 3);
    assert!(!handle.workspace_path.exists());

    // 4. CRITICAL INVARIANT: The main repo caches MUST NOT be deleted or damaged!
    assert!(root_target.join("artifact.bin").exists());
    assert!(root_node.join("lodash.js").exists());
    assert!(root_venv.join("pyvenv.cfg").exists());
}

#[tokio::test]
async fn test_provision_with_injected_configs() {
    let repo = init_test_git_repo();
    let repo_root = repo.path();

    // Create configs in repo_root
    std::fs::write(repo_root.join(".env"), "SECRET_TOKEN=xyz123\n").unwrap();
    std::fs::write(repo_root.join(".mcp.json"), "{\"servers\": []}").unwrap();
    std::fs::write(repo_root.join("custom.conf"), "mode = production\n").unwrap();

    let orchestrator = WorktreeOrchestrator::new(repo_root);
    let req = WorkspaceProvisionRequest::new("task-inject-01", repo_root)
        .with_injected_configs(vec!["custom.conf".into()]);

    let handle = orchestrator.provision(&req).await.expect("provision");
    assert!(handle.injected_files.contains(&".env".to_string()));
    assert!(handle.injected_files.contains(&".mcp.json".to_string()));
    assert!(handle.injected_files.contains(&"custom.conf".to_string()));

    // Verify files copied to sandbox
    let sandbox_env = handle.workspace_path.join(".env");
    assert!(sandbox_env.exists());
    assert_eq!(
        std::fs::read_to_string(&sandbox_env).unwrap(),
        "SECRET_TOKEN=xyz123\n"
    );

    // Verify isolation: modifying sandbox .env does NOT affect root .env
    std::fs::write(&sandbox_env, "MUTATED_VALUE=hacked\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(repo_root.join(".env")).unwrap(),
        "SECRET_TOKEN=xyz123\n"
    );

    // Teardown
    orchestrator
        .teardown("task-inject-01", TeardownAction::Abort)
        .await
        .expect("teardown");
}

#[test]
fn test_cli_provision_with_cache_and_inject() {
    let repo = init_test_git_repo();
    let repo_root = repo.path();
    let repo_str = repo_root.to_str().unwrap();

    std::fs::create_dir_all(repo_root.join("target")).unwrap();
    std::fs::write(repo_root.join(".env"), "APP_ENV=test").unwrap();

    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let output = cmd
        .args([
            "provision",
            "--task-id",
            "task_cli_cache",
            "--repo",
            repo_str,
            "--cache",
            "cargo",
            "--inject",
            ".env",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json_str = String::from_utf8(output).unwrap();
    let handle: SandboxHandle = serde_json::from_str(&json_str).expect("deserialize handle");

    assert!(handle.mounted_caches.contains(&CacheType::CargoTarget));
    assert!(handle.injected_files.contains(&".env".to_string()));
    assert!(handle.workspace_path.join("target").exists());
    assert!(handle.workspace_path.join(".env").exists());

    // Teardown CLI
    let mut td_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    td_cmd
        .args([
            "teardown",
            "--task-id",
            "task_cli_cache",
            "--repo",
            repo_str,
            "--action",
            "abort",
            "--json",
        ])
        .assert()
        .success();

    // Verify root target/ still exists
    assert!(repo_root.join("target").exists());
}
