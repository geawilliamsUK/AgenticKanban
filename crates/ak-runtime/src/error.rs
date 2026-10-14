use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("could not parse {path}: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("could not serialise config: {0}")]
    Serialize(String),

    #[error(transparent)]
    Store(#[from] ak_store::StoreError),

    #[error("{0} is not a git repository")]
    NotAGitRepository(PathBuf),

    #[error("{0} is already registered as project {1}")]
    AlreadyRegistered(PathBuf, String),

    #[error("project {0} is not registered")]
    UnknownProject(String),

    #[error("no home directory could be determined")]
    NoHomeDirectory,

    #[error("file watcher error: {0}")]
    Watch(String),

    #[error("{0}")]
    ClaudeNotFound(String),

    #[error("{0}")]
    Utility(String),

    #[error("{0}")]
    Snapshot(String),

    #[error("AgenticKanban is already running; close it first (lock: {0})")]
    AlreadyRunning(PathBuf),

    #[error("daily budget of ${limit:.2} reached (${spent:.2} spent today)")]
    BudgetExceeded { limit: f64, spent: f64 },

    #[error(transparent)]
    Core(#[from] ak_core::CoreError),
}

impl RuntimeError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
