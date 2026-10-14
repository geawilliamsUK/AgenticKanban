//! Pure domain logic for AgenticKanban.
//!
//! Nothing in this crate performs IO. It defines the entities, the events they
//! emit, the pure "decide" functions that turn a command into a new state plus
//! events, notification routing, and the configuration types. The store crate
//! persists what this crate decides; the runtime crate drives processes; the
//! app crate draws it.

pub mod actor;
pub mod agent;
pub mod board;
pub mod commands;
pub mod config;
pub mod error;
pub mod event;
pub mod ids;
pub mod mentions;
pub mod notification;
pub mod utility;

pub use actor::{Actor, ActorKind};
pub use agent::{Agent, AgentDefinition, AgentState, Budgets};
pub use board::{Column, ColumnKind, Comment, Priority, Ticket};
pub use error::CoreError;
pub use event::{Event, EventKind, FieldChange, MentionSource};
pub use ids::*;
pub use notification::{
    Notification, NotificationDraft, NotificationKind, NotificationState, RoutingContext,
};
pub use utility::{UtilityCall, UtilityKind, UtilityState};

/// Re-exports so downstream crates share one version of each.
pub use chrono;
pub use serde_norway;
pub use uuid;

pub type Timestamp = chrono::DateTime<chrono::Utc>;
