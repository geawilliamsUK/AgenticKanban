//! What a run token resolves to.

use std::sync::Arc;

use ak_core::agent::Agent;
use ak_core::config::PermissionProfile;
use ak_core::ids::RunId;
use ak_runtime::OpenProject;

pub struct RunContext {
    pub project: Arc<OpenProject>,
    pub agent: Agent,
    pub run_id: RunId,
    pub profile: PermissionProfile,
}

impl RunContext {
    pub fn store(&self) -> Arc<dyn ak_store::Store> {
        self.project.store()
    }
}
