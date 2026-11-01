//! What a run token resolves to.

use std::path::PathBuf;
use std::sync::Arc;

use ak_core::agent::Agent;
use ak_core::config::PermissionProfile;
use ak_core::ids::RunId;
use ak_runtime::OpenProject;

pub struct RunContext {
    pub project: Arc<OpenProject>,
    /// Who acts. For a Desk session this stands for the human.
    pub agent: Agent,
    pub run_id: RunId,
    pub profile: PermissionProfile,
    /// Set for a Desk session: its memory file, outside the agents
    /// directory so it is never taken for an agent.
    pub desk_memory: Option<PathBuf>,
    /// Set for a Desk session: the project's settings file, which its team
    /// tools may change.
    pub policy_file: Option<PathBuf>,
}

impl RunContext {
    pub fn store(&self) -> Arc<dyn ak_store::Store> {
        self.project.store()
    }

    /// A Desk session: the human's own conversation, acting as the human.
    /// It is never held at stop and never cut off for repeating itself.
    pub fn is_desk(&self) -> bool {
        self.desk_memory.is_some()
    }
}
