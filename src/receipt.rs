//! Verification Receipt Engine for K0maru Agent Workspace.
//!
//! Enforces the physical No-Receipt-No-Merge invariant before sandbox teardown
//! merges into target integration trunks (e.g. `dev`, `main`).

use crate::error::WorkspaceError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const ENV_K0MARU_RECEIPT_DIR: &str = "K0MARU_RECEIPT_DIR";
pub const DEFAULT_HOME_RECEIPTS_REL_PATH: &str = ".k0maru/receipts";
pub const LOCAL_WORKTREE_RECEIPTS_REL_PATH: &str = ".receipts";
pub const AGENT_RECEIPT_SCHEMA_V1: &str = "https://k0maru.dev/schemas/agent-receipt-v1.json";
pub const VERDICT_SUCCESS: &str = "success";
pub const SHA256_HEX_LENGTH: usize = 64;
pub const EXIT_CODE_VERIFICATION_FAILED: i32 = 1;

/// Artifacts payload inside `agent-receipt-v1.json` containing the cryptographic patch hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReceiptArtifacts {
    #[serde(default)]
    pub git_patch_sha256: Option<String>,
}

/// Minimal schema representation of `agent-receipt-v1.json` used for pre-merge verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinimalExecutionReceipt {
    #[serde(default, rename = "$schema")]
    pub schema: Option<String>,
    #[serde(default)]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    pub verdict: String,
    #[serde(default)]
    pub artifacts: Option<ReceiptArtifacts>,
}

impl MinimalExecutionReceipt {
    pub fn new_success(task_id: impl Into<String>, git_patch_sha256: impl Into<String>) -> Self {
        Self {
            schema: Some(AGENT_RECEIPT_SCHEMA_V1.to_string()),
            schema_version: Some("v1".to_string()),
            task_id: Some(task_id.into()),
            verdict: VERDICT_SUCCESS.to_string(),
            artifacts: Some(ReceiptArtifacts {
                git_patch_sha256: Some(git_patch_sha256.into()),
            }),
        }
    }
}

/// Computes the SHA-256 hex digest of a byte slice.
pub fn compute_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Resolves the global receipts directory.
///
/// Priority:
/// 1. `K0MARU_RECEIPT_DIR` environment variable.
/// 2. `~/.k0maru/receipts` fallback.
pub fn resolve_receipts_dir() -> PathBuf {
    if let Some(val) = std::env::var_os(ENV_K0MARU_RECEIPT_DIR) {
        if !val.is_empty() {
            return PathBuf::from(val);
        }
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(DEFAULT_HOME_RECEIPTS_REL_PATH);
    }
    PathBuf::from(DEFAULT_HOME_RECEIPTS_REL_PATH)
}

/// Locates the execution receipt file for a given task ID across standard lookup paths.
pub fn locate_receipt(repo_root: &Path, task_id: &str) -> Option<PathBuf> {
    let filename = format!("{}.json", task_id);
    let receipt_filename = format!("{}.receipt.json", task_id);

    let candidates = [
        resolve_receipts_dir().join(&filename),
        repo_root
            .join(".worktrees")
            .join(LOCAL_WORKTREE_RECEIPTS_REL_PATH)
            .join(&filename),
        repo_root
            .join(LOCAL_WORKTREE_RECEIPTS_REL_PATH)
            .join(&filename),
        repo_root.join(&receipt_filename),
        repo_root.join(&filename),
    ];

    candidates.into_iter().find(|candidate| candidate.is_file())
}

/// Verifies that a valid and authentic execution receipt exists and matches the actual git diff.
///
/// Returns `Ok(())` if verified, or `Err(WorkspaceError::VerificationReceiptRequired)` if:
/// - Receipt file is missing
/// - Receipt JSON is invalid
/// - Verdict is not "success"
/// - Git patch SHA-256 hash is missing or invalid
/// - Actual diff hash against `target_branch` (or `HEAD`) does not match
pub async fn verify_receipt_for_merge(
    repo_root: &Path,
    sandbox_path: &Path,
    task_id: &str,
    target_branch: &str,
) -> Result<(), WorkspaceError> {
    let receipt_path = locate_receipt(repo_root, task_id).ok_or_else(|| {
        WorkspaceError::VerificationReceiptRequired {
            reason: format!(
                "No verification receipt found for task '{}'. Run verification first or use --force to bypass.",
                task_id
            ),
        }
    })?;

    let content = tokio::fs::read_to_string(&receipt_path)
        .await
        .map_err(|e| WorkspaceError::VerificationReceiptRequired {
            reason: format!(
                "Failed to read receipt at '{}': {}",
                receipt_path.display(),
                e
            ),
        })?;

    let receipt: MinimalExecutionReceipt = serde_json::from_str(&content).map_err(|e| {
        WorkspaceError::VerificationReceiptRequired {
            reason: format!(
                "Failed to parse receipt JSON at '{}': {}",
                receipt_path.display(),
                e
            ),
        }
    })?;

    if receipt.verdict.to_lowercase() != VERDICT_SUCCESS {
        return Err(WorkspaceError::VerificationReceiptRequired {
            reason: format!(
                "Receipt verdict is '{}', required '{}'",
                receipt.verdict, VERDICT_SUCCESS
            ),
        });
    }

    let expected_sha = match receipt
        .artifacts
        .as_ref()
        .and_then(|a| a.git_patch_sha256.as_deref())
    {
        Some(sha) => {
            let trimmed = sha.trim().to_lowercase();
            if trimmed.len() != SHA256_HEX_LENGTH || !trimmed.chars().all(|c| c.is_ascii_hexdigit())
            {
                return Err(WorkspaceError::VerificationReceiptRequired {
                    reason: format!(
                        "Receipt artifacts contain invalid 'git_patch_sha256' (expected {}-char hex, got '{}')",
                        SHA256_HEX_LENGTH, sha
                    ),
                });
            }
            trimmed
        }
        None => {
            return Err(WorkspaceError::VerificationReceiptRequired {
                reason: "Receipt artifacts missing valid 'git_patch_sha256'".to_string(),
            });
        }
    };

    // 1. Check diff against target_branch
    let output_target = tokio::process::Command::new("git")
        .current_dir(sandbox_path)
        .args(["diff", target_branch])
        .output()
        .await
        .map_err(WorkspaceError::IoError)?;

    let actual_target_sha = if output_target.status.success() {
        compute_sha256(&output_target.stdout)
    } else {
        String::new()
    };

    if !actual_target_sha.is_empty() && actual_target_sha == expected_sha {
        return Ok(());
    }

    // 2. Fallback: check diff against HEAD
    let output_head = tokio::process::Command::new("git")
        .current_dir(sandbox_path)
        .args(["diff", "HEAD"])
        .output()
        .await
        .map_err(WorkspaceError::IoError)?;

    let actual_head_sha = if output_head.status.success() {
        compute_sha256(&output_head.stdout)
    } else {
        String::new()
    };

    if !actual_head_sha.is_empty() && actual_head_sha == expected_sha {
        return Ok(());
    }

    let reported_actual = if !actual_target_sha.is_empty() {
        actual_target_sha
    } else {
        actual_head_sha
    };

    Err(WorkspaceError::VerificationReceiptRequired {
        reason: format!(
            "Git patch SHA-256 mismatch: expected '{}', got '{}'",
            expected_sha, reported_actual
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_compute_sha256_known_values() {
        assert_eq!(
            compute_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            compute_sha256(b"hello world\n"),
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );
    }

    #[test]
    fn test_minimal_execution_receipt_serde() {
        let receipt = MinimalExecutionReceipt::new_success(
            "task-01",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        let json = serde_json::to_string_pretty(&receipt).expect("serialize");
        assert!(json.contains("https://k0maru.dev/schemas/agent-receipt-v1.json"));
        assert!(json.contains("task-01"));
        assert!(json.contains("success"));

        let deserialized: MinimalExecutionReceipt =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(receipt, deserialized);
    }

    #[test]
    fn test_locate_receipt_resolution_order() {
        let temp_dir = TempDir::new().unwrap();
        let repo_root = temp_dir.path();

        // 1. Initially none found
        assert_eq!(locate_receipt(repo_root, "task-test-loc"), None);

        // 2. Put receipt in repo_root/<task_id>.json
        let root_receipt = repo_root.join("task-test-loc.json");
        std::fs::write(&root_receipt, "{}").unwrap();
        assert_eq!(
            locate_receipt(repo_root, "task-test-loc"),
            Some(root_receipt)
        );

        // 3. Put receipt in repo_root/<task_id>.receipt.json (takes higher precedence)
        let named_receipt = repo_root.join("task-test-loc.receipt.json");
        std::fs::write(&named_receipt, "{}").unwrap();
        assert_eq!(
            locate_receipt(repo_root, "task-test-loc"),
            Some(named_receipt)
        );
    }
}
