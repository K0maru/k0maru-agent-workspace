use thiserror::Error;

#[derive(Error, Debug)]
pub enum WorkspaceError {
    #[error("Git error: {0}")]
    GitError(String),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Workspace already exists for task '{task_id}' at {path}")]
    AlreadyExists { task_id: String, path: String },

    #[error("Workspace not found for task '{task_id}'")]
    NotFound { task_id: String },

    #[error("Invalid task ID '{0}': must contain only alphanumeric characters, dashes, and underscores without path traversal")]
    InvalidTaskId(String),

    #[error("Invalid path: {0}")]
    InvalidPath(String),

    #[error("Merge conflict in task '{task_id}': {details}")]
    MergeConflict { task_id: String, details: String },

    #[error("Branch error: {0}")]
    BranchError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_formatting() {
        let err = WorkspaceError::AlreadyExists {
            task_id: "task-01".into(),
            path: "/path/to/worktree".into(),
        };
        assert_eq!(
            err.to_string(),
            "Workspace already exists for task 'task-01' at /path/to/worktree"
        );

        let err2 = WorkspaceError::InvalidTaskId("../malicious".into());
        assert!(err2.to_string().contains("Invalid task ID"));

        let err3 = WorkspaceError::NotFound {
            task_id: "missing".into(),
        };
        assert_eq!(err3.to_string(), "Workspace not found for task 'missing'");
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let ws_err: WorkspaceError = io_err.into();
        assert!(matches!(ws_err, WorkspaceError::IoError(_)));
    }
}
