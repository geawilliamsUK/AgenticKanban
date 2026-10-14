//! Running agents: the supervisor spawns and watches one Claude Code process
//! per run, the scheduler decides when an agent runs, the workspace manager
//! gives each agent its own git worktree, and the prompt builder composes
//! what the agent is told.
//!
//! This sits above the bridge and the runtime because it needs both.

pub mod guardrails;
pub mod prompts;
pub mod scheduler;
pub mod supervisor;
pub mod workspace;

use std::path::PathBuf;

pub use guardrails::GuardrailMonitor;
pub use scheduler::Scheduler;
pub use supervisor::{RunHandle, RunNotice, RunSpec, StopReason, Supervisor, SupervisorConfig};
pub use workspace::Workspace;

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("git: {0}")]
    Git(String),
    #[error(transparent)]
    Store(#[from] ak_store::StoreError),
    #[error(transparent)]
    Runtime(#[from] ak_runtime::RuntimeError),
    #[error("could not start Claude Code: {0}")]
    Spawn(String),
    #[error("{0}")]
    Other(String),
}

impl SupervisorError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
