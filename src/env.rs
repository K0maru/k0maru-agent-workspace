use crate::error::WorkspaceError;
use crate::handle::CacheType;
use std::path::{Path, PathBuf};
use tokio::fs;

pub struct EnvironmentManager;

impl EnvironmentManager {
    /// Mounts shared caches (e.g. Cargo target, node_modules, .venv) from repo_root into sandbox_path.
    pub async fn mount_caches(
        repo_root: &Path,
        sandbox_path: &Path,
        caches: &[CacheType],
    ) -> Result<Vec<CacheType>, WorkspaceError> {
        let mut mounted = Vec::new();

        for cache in caches {
            match cache {
                CacheType::CargoTarget => {
                    let source_target = repo_root.join("target");
                    if !source_target.exists() {
                        fs::create_dir_all(&source_target).await?;
                    }
                    let dest_target = sandbox_path.join("target");
                    if !dest_target.exists() {
                        Self::create_symlink(&source_target, &dest_target).await?;
                        mounted.push(CacheType::CargoTarget);
                    }
                }
                CacheType::NodeModules => {
                    let source_modules = repo_root.join("node_modules");
                    if source_modules.exists() {
                        let dest_modules = sandbox_path.join("node_modules");
                        if !dest_modules.exists() {
                            Self::create_symlink(&source_modules, &dest_modules).await?;
                            mounted.push(CacheType::NodeModules);
                        }
                    }
                }
                CacheType::PythonVenv => {
                    let source_venv = if repo_root.join(".venv").exists() {
                        repo_root.join(".venv")
                    } else if repo_root.join("venv").exists() {
                        repo_root.join("venv")
                    } else {
                        PathBuf::new()
                    };

                    if !source_venv.as_os_str().is_empty() {
                        let dest_venv = sandbox_path.join(".venv");
                        if !dest_venv.exists() {
                            Self::create_symlink(&source_venv, &dest_venv).await?;
                            mounted.push(CacheType::PythonVenv);
                        }
                    }
                }
            }
        }

        Ok(mounted)
    }

    /// Injects configuration files (e.g. .env, .mcp.json) into the sandbox.
    /// Files are copied rather than symlinked to avoid cross-sandbox mutation of developer secrets.
    pub async fn inject_configs(
        repo_root: &Path,
        sandbox_path: &Path,
        custom_files: &[String],
    ) -> Result<Vec<String>, WorkspaceError> {
        let mut injected = Vec::new();

        let standard_candidates = [".env", ".env.local", ".mcp.json", "mcp-servers.json"];

        let mut all_files = Vec::new();
        for f in standard_candidates {
            all_files.push(f.to_string());
        }
        for f in custom_files {
            if !all_files.contains(f) {
                all_files.push(f.clone());
            }
        }

        for rel_file in all_files {
            let src = repo_root.join(&rel_file);
            if src.exists() && src.is_file() {
                let dest = sandbox_path.join(&rel_file);
                if let Some(parent) = dest.parent() {
                    if !parent.exists() {
                        fs::create_dir_all(parent).await?;
                    }
                }
                fs::copy(&src, &dest).await?;
                injected.push(rel_file);
            }
        }

        Ok(injected)
    }

    /// Safely unlinks mounted symlinks in sandbox_path before worktree teardown,
    /// guaranteeing that worktree deletion NEVER removes the shared cache in repo_root.
    pub fn cleanup_mounts(sandbox_path: &Path) -> Result<Vec<CacheType>, WorkspaceError> {
        let mut unmounted = Vec::new();

        let entries = [
            ("target", CacheType::CargoTarget),
            ("node_modules", CacheType::NodeModules),
            (".venv", CacheType::PythonVenv),
        ];

        for (name, cache_type) in entries {
            let path = sandbox_path.join(name);
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                if meta.file_type().is_symlink() {
                    #[cfg(unix)]
                    {
                        let _ = std::fs::remove_file(&path);
                    }
                    #[cfg(windows)]
                    {
                        if meta.is_dir() {
                            let _ = std::fs::remove_dir(&path);
                        } else {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                    unmounted.push(cache_type);
                }
            }
        }

        Ok(unmounted)
    }

    #[cfg(unix)]
    async fn create_symlink(src: &Path, dst: &Path) -> Result<(), WorkspaceError> {
        std::os::unix::fs::symlink(src, dst).map_err(WorkspaceError::IoError)
    }

    #[cfg(windows)]
    async fn create_symlink(src: &Path, dst: &Path) -> Result<(), WorkspaceError> {
        if src.is_dir() {
            std::os::windows::fs::symlink_dir(src, dst).map_err(WorkspaceError::IoError)
        } else {
            std::os::windows::fs::symlink_file(src, dst).map_err(WorkspaceError::IoError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_mount_caches_and_safe_cleanup() {
        let root_dir = TempDir::new().unwrap();
        let sandbox_dir = TempDir::new().unwrap();

        let repo_root = root_dir.path();
        let sandbox_path = sandbox_dir.path();

        // 1. Create a fake target/ and node_modules/ in repo_root
        let root_target = repo_root.join("target");
        fs::create_dir(&root_target).await.unwrap();
        fs::write(root_target.join("CACHEDIR.TAG"), "cache")
            .await
            .unwrap();

        let root_modules = repo_root.join("node_modules");
        fs::create_dir(&root_modules).await.unwrap();
        fs::write(root_modules.join("package.json"), "{}")
            .await
            .unwrap();

        let caches = vec![CacheType::CargoTarget, CacheType::NodeModules];
        let mounted = EnvironmentManager::mount_caches(repo_root, sandbox_path, &caches)
            .await
            .unwrap();

        assert_eq!(mounted.len(), 2);
        assert!(sandbox_path.join("target").exists());
        assert!(sandbox_path.join("node_modules").exists());

        // 2. Safely cleanup mounts
        let unmounted = EnvironmentManager::cleanup_mounts(sandbox_path).unwrap();
        assert_eq!(unmounted.len(), 2);

        // Crucial invariant: repo_root cache MUST NOT be deleted!
        assert!(root_target.exists());
        assert!(root_target.join("CACHEDIR.TAG").exists());
        assert!(root_modules.exists());
    }

    #[tokio::test]
    async fn test_inject_configs() {
        let root_dir = TempDir::new().unwrap();
        let sandbox_dir = TempDir::new().unwrap();

        let repo_root = root_dir.path();
        let sandbox_path = sandbox_dir.path();

        fs::write(repo_root.join(".env"), "SECRET=k0maru\n")
            .await
            .unwrap();
        fs::write(repo_root.join(".mcp.json"), "{\"mcp\": true}")
            .await
            .unwrap();

        let injected = EnvironmentManager::inject_configs(repo_root, sandbox_path, &[])
            .await
            .unwrap();

        assert!(injected.contains(&".env".to_string()));
        assert!(injected.contains(&".mcp.json".to_string()));

        assert!(sandbox_path.join(".env").exists());
        assert_eq!(
            fs::read_to_string(sandbox_path.join(".env")).await.unwrap(),
            "SECRET=k0maru\n"
        );
    }
}
