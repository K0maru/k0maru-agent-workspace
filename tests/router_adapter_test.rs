use assert_cmd::Command;
use k0maru_workspace::handle::TeardownAction;
use k0maru_workspace::orchestrator::{WorkspaceManager, WorktreeOrchestrator};
use k0maru_workspace::router::{
    AgentType, CallerInfo, MultiplexerType, RouterDispatchOptions, RouterDispatchReceipt,
    TargetInfo, TaskEnvelope, TaskPayload, WorkspaceRouterAdapter,
};
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

    std::fs::write(path.join("README.md"), "# Router Test Repo\n").unwrap();
    run_cmd(&["add", "README.md"]);
    run_cmd(&["commit", "-m", "Initial commit on main"]);

    dir
}

#[tokio::test]
async fn test_dispatch_envelope_end_to_end() {
    let repo = init_test_git_repo();
    let repo_root = repo.path();
    let repo_str = repo_root.to_str().unwrap();

    let envelope = TaskEnvelope {
        schema: Some("https://k0maru.dev/schemas/agent-task-v1.json".into()),
        id: "task_hotfix_01".into(),
        created_at: "2026-10-12T14:00:00Z".into(),
        caller: CallerInfo {
            workspace: "/caller/path".into(),
            agent: "router-master".into(),
            pane_id: Some("%1".into()),
        },
        target: TargetInfo {
            identifier: "k0maru-workspace".into(),
            workspace: Some(repo_str.into()),
            is_external: false,
        },
        intent: "fix_defect".into(),
        payload: TaskPayload {
            title: "Fix null pointer in memory cache".into(),
            severity: "critical".into(),
            reproduction: Some(serde_json::json!({
                "snippet": "cache.get(key).unwrap()",
                "error": "panicked at 'called `Option::unwrap()` on a `None` value'"
            })),
            expected: Some("Handle None gracefully with typed WorkspaceError".into()),
        },
    };

    let opts = RouterDispatchOptions {
        target_repo: Some(repo_root.to_path_buf()),
        base_branch: Some("main".into()),
        multiplexer: MultiplexerType::Tmux,
        agent: AgentType::Agy,
        custom_agent_cmd: None,
        shared_caches: Vec::new(),
        injected_configs: Vec::new(),
        execute_multiplexer: false,
    };

    let receipt = WorkspaceRouterAdapter::dispatch(&envelope, opts)
        .await
        .expect("dispatch");

    assert_eq!(receipt.envelope_id, "task_hotfix_01");
    assert_eq!(receipt.task_id, "task_hotfix_01");
    assert!(receipt.sandbox_handle.workspace_path.exists());
    assert_eq!(receipt.multiplexer_type, MultiplexerType::Tmux);
    assert!(receipt.multiplexer_command.is_some());

    let cmd = receipt.multiplexer_command.unwrap();
    assert!(cmd.contains("tmux split-window -c"));
    assert!(cmd.contains("agy"));

    // Teardown
    let orchestrator = WorktreeOrchestrator::new(repo_root);
    orchestrator
        .teardown("task_hotfix_01", TeardownAction::Abort)
        .await
        .expect("teardown");
}

#[test]
fn test_cli_route_subcommand() {
    let repo = init_test_git_repo();
    let repo_root = repo.path();
    let repo_str = repo_root.to_str().unwrap();

    let envelope_json = serde_json::json!({
        "id": "task_cli_route_42",
        "created_at": "2026-10-12T14:00:00Z",
        "caller": {
            "workspace": "/caller",
            "agent": "router"
        },
        "target": {
            "identifier": "target-repo",
            "workspace": repo_str,
            "is_external": false
        },
        "intent": "bug_fix",
        "payload": {
            "title": "CLI Route Integration",
            "severity": "high",
            "expected": "Worktree sandbox automatically provisioned"
        }
    });

    let envelope_file = repo_root.join("envelope.json");
    std::fs::write(
        &envelope_file,
        serde_json::to_string_pretty(&envelope_json).unwrap(),
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    let output = cmd
        .args([
            "route",
            "--envelope",
            envelope_file.to_str().unwrap(),
            "--multiplexer",
            "herdr",
            "--agent",
            "claude-code",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json_str = String::from_utf8(output).unwrap();
    let receipt: RouterDispatchReceipt = serde_json::from_str(&json_str).expect("parse receipt");

    assert_eq!(receipt.envelope_id, "task_cli_route_42");
    assert_eq!(receipt.multiplexer_type, MultiplexerType::Herdr);
    assert!(receipt.multiplexer_command.unwrap().contains("herdr split"));

    // Teardown
    let mut td_cmd = Command::cargo_bin("k0maru-workspace").unwrap();
    td_cmd
        .args([
            "teardown",
            "--task-id",
            "task_cli_route_42",
            "--repo",
            repo_str,
            "--action",
            "abort",
        ])
        .assert()
        .success();
}
