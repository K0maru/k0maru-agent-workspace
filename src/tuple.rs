//! Linda Tuple Space Engine and Tuple-Driven Lifecycle for K0maru Agent Workspace.
//!
//! Provides associative shared-memory primitives (`out`, `in`, `rd`, `list`)
//! operating on local filesystem buses with POSIX-atomic file locks and zero daemons.
//! Connects `k0maru-agent-workspace` with `k0maru-agent-router` for autonomous
//! task claiming, isolated worktree provisioning, and receipt-verified settlement.

use crate::error::WorkspaceError;
use crate::handle::{
    CacheType, IsolationStrategy, SandboxHandle, TeardownAction, TeardownReceipt,
    WorkspaceProvisionRequest,
};
use crate::orchestrator::{WorkspaceManager, WorktreeOrchestrator};
use crate::receipt::locate_receipt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Environment variable controlling global tuple space storage directory.
pub const ENV_K0MARU_TUPLE_DIR: &str = "K0MARU_TUPLE_DIR";

/// Default subfolder name for tuple space storage.
pub const DEFAULT_K0MARU_DIR_REL: &str = ".k0maru";
pub const DEFAULT_TUPLES_DIR_REL: &str = "tuples";

/// Standard Linda Tuple kinds for agent coordination.
pub const TUPLE_KIND_TASK_READY: &str = "TASK_READY";
pub const TUPLE_KIND_SANDBOX_READY: &str = "SANDBOX_READY";
pub const TUPLE_KIND_TASK_VERIFIED: &str = "TASK_VERIFIED";
pub const TUPLE_KIND_TASK_COMPLETED: &str = "TASK_COMPLETED";
pub const TUPLE_KIND_TASK_BLOCKED: &str = "TASK_BLOCKED";
pub const TUPLE_KIND_TASK_ABORTED: &str = "TASK_ABORTED";
pub const TUPLE_KIND_TASK_FAILED: &str = "TASK_FAILED";

/// Tuple ID and file naming constants.
pub const TUPLE_PREFIX: &str = "tup_";
pub const TUPLE_CLAIM_EXT: &str = "claimed";
pub const TUPLE_TMP_EXT: &str = "tmp";
pub const DEFAULT_SETTLE_TARGET_BRANCH: &str = "dev";
pub const DEFAULT_FALLBACK_DOMAIN: &str = "WORKSPACE";

/// Linda Tuple representing an associative unit of shared state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tuple {
    /// Unique tuple identifier (e.g., `tup_9f82a1b4`).
    pub id: String,
    /// Tuple classification or event kind (e.g. "TASK_READY", "SANDBOX_READY", "TASK_COMPLETED").
    pub kind: String,
    /// Target repository or domain identifier (e.g. "WORKSPACE", "AUTH-SRV").
    pub domain: String,
    /// Associated task identifier.
    pub task_id: String,
    /// Arbitrary payload, such as a K-DSL wire string or JSON object.
    pub payload: String,
    /// Creation timestamp in RFC-3339 format.
    pub created_at: String,
    /// Optional time-to-live in seconds before tuple becomes stale/expired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u64>,
}

impl Tuple {
    /// Creates a new Tuple with an auto-generated unique ID and current UTC timestamp.
    pub fn new(
        kind: impl Into<String>,
        domain: impl Into<String>,
        task_id: impl Into<String>,
        payload: impl Into<String>,
    ) -> Self {
        Self {
            id: format!("{}{}", TUPLE_PREFIX, uuid::Uuid::new_v4().simple()),
            kind: kind.into(),
            domain: domain.into(),
            task_id: task_id.into(),
            payload: payload.into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            ttl_secs: None,
        }
    }

    /// Sets explicit tuple identifier.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Sets optional time-to-live in seconds.
    pub fn with_ttl(mut self, ttl_secs: u64) -> Self {
        self.ttl_secs = Some(ttl_secs);
        self
    }

    /// Sets explicit creation timestamp (RFC-3339).
    pub fn with_created_at(mut self, created_at: impl Into<String>) -> Self {
        self.created_at = created_at.into();
        self
    }

    /// Checks if the tuple is expired according to its `ttl_secs`.
    pub fn is_expired(&self) -> bool {
        if let Some(ttl) = self.ttl_secs {
            if let Ok(created) = chrono::DateTime::parse_from_rfc3339(&self.created_at) {
                let now = chrono::Utc::now();
                let expires_at = created + chrono::Duration::seconds(ttl as i64);
                return now > expires_at;
            }
        }
        false
    }
}

/// Associative query pattern for matching Tuples in the space.
///
/// Any `None` field acts as an associative wildcard matching any value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TuplePattern {
    /// Optional exact match on `kind`.
    pub kind: Option<String>,
    /// Optional exact match on `domain`.
    pub domain: Option<String>,
    /// Optional exact match on `task_id`.
    pub task_id: Option<String>,
}

impl TuplePattern {
    /// Creates a new wildcard pattern matching all tuples.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new wildcard pattern matching all tuples.
    pub fn any() -> Self {
        Self::default()
    }

    /// Match a specific kind.
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    /// Match a specific domain.
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Match a specific task_id.
    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    /// Returns `true` if the tuple satisfies the query pattern.
    pub fn matches(&self, tuple: &Tuple) -> bool {
        if let Some(ref kind) = self.kind {
            if &tuple.kind != kind {
                return false;
            }
        }
        if let Some(ref domain) = self.domain {
            if &tuple.domain != domain {
                return false;
            }
        }
        if let Some(ref task_id) = self.task_id {
            if &tuple.task_id != task_id {
                return false;
            }
        }
        true
    }
}

/// Resolves the canonical tuple space storage directory based on standard precedence:
/// 1. Priority 1: Explicit override path if supplied
/// 2. Priority 2: Environment variable `K0MARU_TUPLE_DIR`
/// 3. Priority 3: Default home directory `~/.k0maru/tuples` (fallback to `.k0maru/tuples`)
pub fn resolve_tuples_dir(override_path: Option<&Path>) -> PathBuf {
    if let Some(path) = override_path {
        return path.to_path_buf();
    }

    if let Ok(env_val) = std::env::var(ENV_K0MARU_TUPLE_DIR) {
        let trimmed = env_val.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        PathBuf::from(home)
            .join(DEFAULT_K0MARU_DIR_REL)
            .join(DEFAULT_TUPLES_DIR_REL)
    } else {
        PathBuf::from(DEFAULT_K0MARU_DIR_REL).join(DEFAULT_TUPLES_DIR_REL)
    }
}

/// Linda Tuple Space Engine providing atomic shared-memory operations.
#[derive(Debug, Clone)]
pub struct TupleSpaceEngine {
    tuple_dir: PathBuf,
}

impl TupleSpaceEngine {
    /// Creates a new `TupleSpaceEngine` backed by the given directory.
    pub fn new(tuple_dir: impl Into<PathBuf>) -> Self {
        Self {
            tuple_dir: tuple_dir.into(),
        }
    }

    /// Creates a new `TupleSpaceEngine` using the default global directory.
    pub fn with_default_dir() -> Self {
        Self::new(resolve_tuples_dir(None))
    }

    /// Returns a reference to the underlying tuple space directory.
    pub fn tuple_dir(&self) -> &Path {
        &self.tuple_dir
    }

    /// Writes a tuple atomically into the space (`out` primitive).
    pub fn out(&self, mut tuple: Tuple) -> Result<Tuple, WorkspaceError> {
        if tuple.id.trim().is_empty() {
            tuple.id = format!("{}{}", TUPLE_PREFIX, uuid::Uuid::new_v4().simple());
        }
        if tuple.created_at.trim().is_empty() {
            tuple.created_at = chrono::Utc::now().to_rfc3339();
        }

        // Validate tuple ID to prevent directory traversal
        if tuple.id.contains('/') || tuple.id.contains('\\') || tuple.id.contains("..") {
            return Err(WorkspaceError::TupleSpaceError(format!(
                "Invalid tuple ID '{}': must not contain path traversal characters",
                tuple.id
            )));
        }

        fs::create_dir_all(&self.tuple_dir).map_err(|e| {
            WorkspaceError::TupleSpaceError(format!(
                "Failed to create tuple directory '{}': {e}",
                self.tuple_dir.display()
            ))
        })?;

        let target_file = self.tuple_dir.join(format!("{}.json", tuple.id));
        let tmp_file = self.tuple_dir.join(format!(
            "{}.{}.{}",
            tuple.id,
            TUPLE_TMP_EXT,
            uuid::Uuid::new_v4().simple()
        ));

        let serialized = serde_json::to_string_pretty(&tuple).map_err(|e| {
            WorkspaceError::TupleSpaceError(format!("Failed to serialize tuple: {e}"))
        })?;

        fs::write(&tmp_file, serialized).map_err(|e| {
            WorkspaceError::TupleSpaceError(format!(
                "Failed to write temporary tuple file '{}': {e}",
                tmp_file.display()
            ))
        })?;

        fs::rename(&tmp_file, &target_file).map_err(|e| {
            let _ = fs::remove_file(&tmp_file);
            WorkspaceError::TupleSpaceError(format!(
                "Failed to atomically persist tuple '{}': {e}",
                target_file.display()
            ))
        })?;

        Ok(tuple)
    }

    /// Non-destructively reads the first matching tuple (`rd` primitive).
    pub fn rd(&self, pattern: &TuplePattern) -> Result<Option<Tuple>, WorkspaceError> {
        let candidates = self.scan_candidates()?;
        for path in candidates {
            if let Some(tuple) = self.read_tuple_from_file(&path)? {
                if !tuple.is_expired() && pattern.matches(&tuple) {
                    return Ok(Some(tuple));
                }
            }
        }
        Ok(None)
    }

    /// Atomically finds, claims, and removes a matching tuple (`in` primitive).
    pub fn take(&self, pattern: &TuplePattern) -> Result<Option<Tuple>, WorkspaceError> {
        let candidates = self.scan_candidates()?;
        for path in candidates {
            if let Some(tuple) = self.read_tuple_from_file(&path)? {
                if tuple.is_expired() {
                    continue;
                }
                if pattern.matches(&tuple) {
                    // Attempt atomic claim via rename
                    let claim_file = path.with_extension(format!(
                        "{}.{}",
                        TUPLE_CLAIM_EXT,
                        uuid::Uuid::new_v4().simple()
                    ));
                    if fs::rename(&path, &claim_file).is_ok() {
                        let content = fs::read_to_string(&claim_file).map_err(|e| {
                            WorkspaceError::TupleSpaceError(format!(
                                "Failed to read claimed tuple: {e}"
                            ))
                        })?;
                        let _ = fs::remove_file(&claim_file);
                        let claimed_tuple: Tuple = serde_json::from_str(&content).map_err(|e| {
                            WorkspaceError::TupleSpaceError(format!(
                                "Failed to parse claimed tuple: {e}"
                            ))
                        })?;
                        return Ok(Some(claimed_tuple));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Alias for `take` to match formal Linda `in` primitive.
    pub fn r#in(&self, pattern: &TuplePattern) -> Result<Option<Tuple>, WorkspaceError> {
        self.take(pattern)
    }

    /// Lists all tuples matching the given pattern without removing them.
    pub fn list(&self, pattern: &TuplePattern) -> Result<Vec<Tuple>, WorkspaceError> {
        let candidates = self.scan_candidates()?;
        let mut results = Vec::new();
        for path in candidates {
            if let Some(tuple) = self.read_tuple_from_file(&path)? {
                if !tuple.is_expired() && pattern.matches(&tuple) {
                    results.push(tuple);
                }
            }
        }
        Ok(results)
    }

    /// Scans candidate `.json` tuple files in the directory.
    fn scan_candidates(&self) -> Result<Vec<PathBuf>, WorkspaceError> {
        if !self.tuple_dir.is_dir() {
            return Ok(Vec::new());
        }

        let read_dir = fs::read_dir(&self.tuple_dir).map_err(|e| {
            WorkspaceError::TupleSpaceError(format!(
                "Failed to read tuple directory '{}': {e}",
                self.tuple_dir.display()
            ))
        })?;

        let mut entries = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_file() {
                let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if fname.ends_with(".json")
                    && !fname.contains(".tmp.")
                    && !fname.contains(".claimed.")
                {
                    entries.push(path);
                }
            }
        }
        entries.sort();
        Ok(entries)
    }

    /// Reads and deserializes a tuple from a file.
    fn read_tuple_from_file(&self, path: &Path) -> Result<Option<Tuple>, WorkspaceError> {
        match fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str::<Tuple>(&content) {
                Ok(tuple) => Ok(Some(tuple)),
                Err(_) => Ok(None),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WorkspaceError::TupleSpaceError(format!(
                "Failed to read tuple file '{}': {e}",
                path.display()
            ))),
        }
    }
}

/// Options for claiming a task tuple and provisioning a sandbox.
#[derive(Debug, Clone)]
pub struct TupleClaimOptions {
    pub domain: String,
    pub task_id: Option<String>,
    pub repo_root: PathBuf,
    pub base_branch: Option<String>,
    pub strategy: IsolationStrategy,
    pub caches: Vec<CacheType>,
    pub injected_configs: Vec<String>,
    pub tuple_dir: Option<PathBuf>,
}

/// Receipt returned upon successfully claiming a task and provisioning a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TupleClaimReceipt {
    pub claimed_tuple: Tuple,
    pub sandbox: SandboxHandle,
    pub emitted_tuple: Tuple,
}

/// Claims a `TASK_READY` tuple, provisions an isolated sandbox, and emits `SANDBOX_READY`.
pub async fn tuple_claim(opts: TupleClaimOptions) -> Result<TupleClaimReceipt, WorkspaceError> {
    let engine = TupleSpaceEngine::new(resolve_tuples_dir(opts.tuple_dir.as_deref()));

    let mut pattern = TuplePattern::new()
        .with_kind(TUPLE_KIND_TASK_READY)
        .with_domain(&opts.domain);
    if let Some(ref tid) = opts.task_id {
        pattern = pattern.with_task_id(tid);
    }

    let claimed_tuple = engine
        .take(&pattern)?
        .ok_or_else(|| WorkspaceError::NotFound {
            task_id: format!("No ready task tuple found in domain '{}'", opts.domain),
        })?;

    // Resolve task ID from tuple or payload
    let resolved_task_id = if !claimed_tuple.task_id.trim().is_empty() {
        claimed_tuple.task_id.clone()
    } else if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&claimed_tuple.payload) {
        parsed
            .get("task_id")
            .and_then(|v| v.as_str())
            .unwrap_or("task-default")
            .to_string()
    } else {
        format!("task_{}", uuid::Uuid::new_v4().simple())
    };

    let orchestrator = WorktreeOrchestrator::new(&opts.repo_root);
    let mut req = WorkspaceProvisionRequest::new(&resolved_task_id, &opts.repo_root)
        .with_strategy(opts.strategy)
        .with_caches(opts.caches)
        .with_injected_configs(opts.injected_configs);

    if let Some(ref base) = opts.base_branch {
        req = req.with_base_branch(base);
    }

    let sandbox_handle = orchestrator.provision(&req).await?;

    // Construct SANDBOX_READY payload
    let sandbox_payload = serde_json::json!({
        "task_id": sandbox_handle.task_id,
        "domain": claimed_tuple.domain,
        "workspace_path": sandbox_handle.workspace_path.to_string_lossy(),
        "branch_name": sandbox_handle.branch_name,
        "strategy": format!("{:?}", sandbox_handle.strategy),
        "is_isolated": sandbox_handle.is_isolated,
        "claimed_tuple_id": claimed_tuple.id,
        "claimed_at": chrono::Utc::now().to_rfc3339()
    });

    let emitted = engine.out(Tuple::new(
        TUPLE_KIND_SANDBOX_READY,
        &claimed_tuple.domain,
        &sandbox_handle.task_id,
        sandbox_payload.to_string(),
    ))?;

    Ok(TupleClaimReceipt {
        claimed_tuple,
        sandbox: sandbox_handle,
        emitted_tuple: emitted,
    })
}

/// Options for settling a task tuple (verifying receipt, merging, and teardown).
#[derive(Debug, Clone)]
pub struct TupleSettleOptions {
    pub task_id: String,
    pub repo_root: PathBuf,
    pub target_branch: String,
    pub squash: bool,
    pub force: bool,
    pub tuple_dir: Option<PathBuf>,
}

/// Receipt returned upon settling a task sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TupleSettleReceipt {
    pub task_id: String,
    pub action: String,
    pub target_branch: Option<String>,
    pub teardown_receipt: TeardownReceipt,
    pub emitted_tuple: Tuple,
}

/// Settles a task sandbox with fail-safe abort on `TASK_BLOCKED` or verified merge on `TASK_VERIFIED`.
pub async fn tuple_settle(opts: TupleSettleOptions) -> Result<TupleSettleReceipt, WorkspaceError> {
    let engine = TupleSpaceEngine::new(resolve_tuples_dir(opts.tuple_dir.as_deref()));
    let orchestrator = WorktreeOrchestrator::new(&opts.repo_root);

    // 1. Fail-safe abort on TASK_BLOCKED
    let blocked_pattern = TuplePattern::new()
        .with_kind(TUPLE_KIND_TASK_BLOCKED)
        .with_task_id(&opts.task_id);

    if let Some(blocked_tuple) = engine.take(&blocked_pattern)? {
        let teardown_receipt = orchestrator
            .teardown(&opts.task_id, TeardownAction::Abort)
            .await?;

        let abort_payload = serde_json::json!({
            "status": "ABORTED",
            "reason": "Task was marked as TASK_BLOCKED; fail-safe teardown aborted workspace cleanly",
            "blocked_tuple_id": blocked_tuple.id,
            "aborted_at": chrono::Utc::now().to_rfc3339()
        });

        let emitted = engine.out(Tuple::new(
            TUPLE_KIND_TASK_ABORTED,
            &blocked_tuple.domain,
            &opts.task_id,
            abort_payload.to_string(),
        ))?;

        return Ok(TupleSettleReceipt {
            task_id: opts.task_id,
            action: "aborted".to_string(),
            target_branch: None,
            teardown_receipt,
            emitted_tuple: emitted,
        });
    }

    // 2. Look for TASK_VERIFIED tuple
    let verified_pattern = TuplePattern::new()
        .with_kind(TUPLE_KIND_TASK_VERIFIED)
        .with_task_id(&opts.task_id);

    let verified_tuple_opt = engine.take(&verified_pattern)?;

    // If not force, ensure either TASK_VERIFIED tuple was present or execution receipt exists on disk
    if !opts.force
        && verified_tuple_opt.is_none()
        && locate_receipt(&opts.repo_root, &opts.task_id).is_none()
    {
        return Err(WorkspaceError::VerificationReceiptRequired {
            reason: format!(
                "No TASK_VERIFIED tuple or valid execution receipt found for task '{}'. Run verification first or use --force to bypass.",
                opts.task_id
            ),
        });
    }

    let domain = verified_tuple_opt
        .as_ref()
        .map(|t| t.domain.clone())
        .unwrap_or_else(|| DEFAULT_FALLBACK_DOMAIN.to_string());

    // 3. Execute teardown merge (enforces No-Receipt-No-Merge physically in orchestrator unless force)
    let teardown_receipt = orchestrator
        .teardown(
            &opts.task_id,
            TeardownAction::Merge {
                target_branch: opts.target_branch.clone(),
                squash: opts.squash,
                force: opts.force,
            },
        )
        .await?;

    // Get current HEAD commit hash
    let commit_hash = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&opts.repo_root)
        .output()
        .await
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let complete_payload = serde_json::json!({
        "status": "PASS",
        "merged_to": opts.target_branch,
        "commit": commit_hash,
        "squash": opts.squash,
        "verified_tuple_id": verified_tuple_opt.map(|t| t.id),
        "completed_at": chrono::Utc::now().to_rfc3339()
    });

    let emitted = engine.out(Tuple::new(
        TUPLE_KIND_TASK_COMPLETED,
        domain,
        &opts.task_id,
        complete_payload.to_string(),
    ))?;

    Ok(TupleSettleReceipt {
        task_id: opts.task_id,
        action: "merged".to_string(),
        target_branch: Some(opts.target_branch),
        teardown_receipt,
        emitted_tuple: emitted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_tuple_creation_and_matching() {
        let tuple = Tuple::new("TASK_READY", "AUTH-SRV", "task-01", "{\"op\":\"patch\"}");
        assert_eq!(tuple.kind, "TASK_READY");
        assert_eq!(tuple.domain, "AUTH-SRV");
        assert_eq!(tuple.task_id, "task-01");
        assert!(!tuple.is_expired());

        let pattern = TuplePattern::new()
            .with_kind("TASK_READY")
            .with_domain("AUTH-SRV");
        assert!(pattern.matches(&tuple));

        let non_matching = TuplePattern::new().with_domain("OTHER-SRV");
        assert!(!non_matching.matches(&tuple));
    }

    #[test]
    fn test_tuple_space_engine_primitives() {
        let dir = tempdir().unwrap();
        let engine = TupleSpaceEngine::new(dir.path());

        // out primitive
        let t1 = Tuple::new("TASK_READY", "WORKSPACE", "task-100", "payload-100");
        let persisted = engine.out(t1.clone()).unwrap();
        assert_eq!(persisted.task_id, "task-100");

        // rd primitive (non-destructive)
        let pat = TuplePattern::new().with_task_id("task-100");
        let read = engine.rd(&pat).unwrap();
        assert!(read.is_some());
        assert_eq!(read.unwrap().task_id, "task-100");

        // list primitive
        let all = engine.list(&TuplePattern::any()).unwrap();
        assert_eq!(all.len(), 1);

        // in / take primitive (destructive claim)
        let taken = engine.take(&pat).unwrap();
        assert!(taken.is_some());
        assert_eq!(taken.unwrap().task_id, "task-100");

        // Verify it is gone
        let empty = engine.rd(&pat).unwrap();
        assert!(empty.is_none());
    }

    #[test]
    fn test_tuple_expiration() {
        let tuple = Tuple::new("TASK_READY", "DOM", "t-exp", "{}")
            .with_created_at("2020-01-01T00:00:00Z")
            .with_ttl(60);
        assert!(tuple.is_expired());
    }
}
