//! The runtime: everything that touches the filesystem, processes or the
//! clock on behalf of the app. Milestone 1 ships the project registry and
//! config IO. The scheduler, supervisor and workspace manager follow.

pub mod agents;
pub mod analyst;
pub mod authoring;
pub mod claude;
pub mod config;
pub mod error;
pub mod instance;
pub mod paths;
pub mod registry;
pub mod snapshot;
pub mod utility;

pub use agents::{AgentFiles, AgentWatcher, ReconcileReport};
pub use claude::ClaudeProbe;
pub use error::RuntimeError;
pub use paths::HomePaths;
pub use registry::{OpenProject, ProjectRegistry};
pub use utility::{ClaudeCodeRunner, FakeRunner, UtilityOutput, UtilityRequest, UtilityRunner};

pub type Result<T> = std::result::Result<T, RuntimeError>;
