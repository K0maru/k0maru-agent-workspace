use thiserror::Error;

#[derive(Error, Debug)]
pub enum WorkspaceError {
    #[error("Git command failed: {0}")]
    GitError(String),
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Workspace already exists for task: {0}")]
    AlreadyExists(String),
    #[error("Workspace not found for task: {0}")]
    NotFound(String),
    #[error("Invalid path: {0}")]
    InvalidPath(String),
}
