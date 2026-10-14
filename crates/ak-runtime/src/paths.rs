//! Where the app keeps its own state. Everything lives under one home
//! directory, `~/.agentic-kanban` by default, overridable with the
//! `AGENTIC_KANBAN_HOME` environment variable.
//!
//! ```text
//! ~/.agentic-kanban/
//!   config.toml                               global settings and the project list
//!   projects/<id>/project.toml                project policy
//!   projects/<id>/agents/<handle>/agent.md    agent definition
//!   projects/<id>/agents/<handle>/memory.md   agent memory
//!   projects/<id>/board.db                    that project's store
//!   projects/<id>/runs/<run>/                 per-run files
//!   calls/<id>/                               utility call directories
//! ```

use std::path::{Path, PathBuf};

use ak_core::ids::{ProjectId, RunId};

use crate::error::RuntimeError;

pub const ENV_HOME_OVERRIDE: &str = "AGENTIC_KANBAN_HOME";
pub const HOME_DIR_NAME: &str = ".agentic-kanban";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomePaths {
    root: PathBuf,
}

impl HomePaths {
    /// Resolves the home directory from the environment override or the
    /// user's home directory. Does not create anything.
    pub fn resolve() -> Result<Self, RuntimeError> {
        if let Some(over) = std::env::var_os(ENV_HOME_OVERRIDE) {
            return Ok(Self::at(PathBuf::from(over)));
        }
        let base = directories::BaseDirs::new().ok_or(RuntimeError::NoHomeDirectory)?;
        Ok(Self::at(base.home_dir().join(HOME_DIR_NAME)))
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    pub fn project_dir(&self, id: ProjectId) -> PathBuf {
        self.projects_dir().join(id.to_string())
    }

    pub fn project_policy_file(&self, id: ProjectId) -> PathBuf {
        self.project_dir(id).join("project.toml")
    }

    pub fn project_db_file(&self, id: ProjectId) -> PathBuf {
        self.project_dir(id).join("board.db")
    }

    pub fn agents_dir(&self, id: ProjectId) -> PathBuf {
        self.project_dir(id).join("agents")
    }

    pub fn agent_dir(&self, id: ProjectId, handle: &str) -> PathBuf {
        self.agents_dir(id).join(handle)
    }

    pub fn runs_dir(&self, id: ProjectId) -> PathBuf {
        self.project_dir(id).join("runs")
    }

    pub fn run_dir(&self, id: ProjectId, run: RunId) -> PathBuf {
        self.runs_dir(id).join(run.to_string())
    }

    pub fn calls_dir(&self) -> PathBuf {
        self.root.join("calls")
    }
}

/// The directory a repository snapshot lives in, relative to the repo root.
pub const REPO_SNAPSHOT_DIR: &str = ".agentic";
