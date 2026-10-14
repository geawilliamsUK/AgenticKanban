//! Persistence for one project.
//!
//! `Store` is the only way the rest of the app touches state. Its command
//! methods are transactional: load, run the pure decide function from
//! `ak_core::commands`, write, append events, route notifications, commit, and
//! only then publish the events to subscribers. Queries are plain reads.
//!
//! v1 ships `SqliteStore`. Nothing above this trait may depend on SQLite.

pub mod error;
pub mod sqlite;

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast;

use ak_core::agent::{Agent, AgentState};
use ak_core::board::{Column, Comment, Ticket};
use ak_core::commands::{NewTicket, TicketPatch};
use ak_core::config::ProjectPolicy;
use ak_core::ids::{ActorId, CallId, ColumnId, CommentId, NotificationId, RunId, TicketId};
use ak_core::utility::{UtilityCall, UtilityKind};
use ak_core::{Actor, Event, Notification, NotificationState, Timestamp};

pub use error::StoreError;
pub use sqlite::SqliteStore;

pub type Result<T> = std::result::Result<T, StoreError>;

/// Who the human on this machine is, for bootstrapping a fresh project.
#[derive(Debug, Clone)]
pub struct HumanIdentity {
    pub handle: String,
    pub display_name: String,
}

impl Default for HumanIdentity {
    fn default() -> Self {
        Self {
            handle: "me".into(),
            display_name: "Me".into(),
        }
    }
}

#[async_trait]
pub trait Store: Send + Sync {
    /// Events are published here after their transaction commits.
    fn subscribe(&self) -> broadcast::Receiver<Arc<Event>>;

    /// Idempotent. Creates the system and human actors and the columns from
    /// the policy when the database is new. Returns the human actor.
    async fn bootstrap(&self, policy: &ProjectPolicy, human: &HumanIdentity) -> Result<Actor>;

    // Queries

    async fn actors(&self) -> Result<Vec<Actor>>;
    async fn actor(&self, id: ActorId) -> Result<Actor>;
    async fn actor_by_handle(&self, handle: &str) -> Result<Option<Actor>>;
    async fn human(&self) -> Result<Actor>;
    async fn columns(&self) -> Result<Vec<Column>>;
    async fn tickets(&self) -> Result<Vec<Ticket>>;
    async fn ticket(&self, id: TicketId) -> Result<Ticket>;
    async fn comments(&self, ticket_id: TicketId) -> Result<Vec<Comment>>;
    async fn watchers(&self, ticket_id: TicketId) -> Result<Vec<ActorId>>;
    async fn events_for_ticket(&self, ticket_id: TicketId) -> Result<Vec<Event>>;
    /// Events with `seq` greater than `after_seq`, oldest first.
    async fn events_since(&self, after_seq: i64, limit: u32) -> Result<Vec<Event>>;
    async fn notifications(&self, recipient: ActorId, open_only: bool)
        -> Result<Vec<Notification>>;

    // Commands

    async fn upsert_actor(&self, actor: &Actor) -> Result<()>;
    async fn create_ticket(&self, input: NewTicket, actor: ActorId) -> Result<Ticket>;
    async fn update_ticket(
        &self,
        id: TicketId,
        patch: TicketPatch,
        expected_version: i64,
        actor: ActorId,
    ) -> Result<Ticket>;
    async fn move_ticket(
        &self,
        id: TicketId,
        to: ColumnId,
        expected_version: i64,
        actor: ActorId,
    ) -> Result<Ticket>;
    async fn assign_ticket(
        &self,
        id: TicketId,
        to: Option<ActorId>,
        expected_version: i64,
        actor: ActorId,
    ) -> Result<Ticket>;
    async fn comment(
        &self,
        ticket_id: TicketId,
        body: String,
        parent: Option<CommentId>,
        actor: ActorId,
    ) -> Result<Comment>;
    async fn freeze_ticket(&self, id: TicketId, reason: String, actor: ActorId) -> Result<Ticket>;
    async fn unfreeze_ticket(&self, id: TicketId, actor: ActorId) -> Result<Ticket>;
    async fn set_notification_state(
        &self,
        id: NotificationId,
        state: NotificationState,
    ) -> Result<()>;
    /// Marks every Pending notification for the recipient as Seen. Returns the count.
    async fn mark_notifications_seen(&self, recipient: ActorId) -> Result<u64>;

    // Agents. The definition file is the source of truth; these rows cache it
    // plus runtime state, and the runtime's reconciliation keeps them aligned.

    async fn agents(&self) -> Result<Vec<Agent>>;
    async fn agent(&self, actor_id: ActorId) -> Result<Agent>;
    async fn agent_by_handle(&self, handle: &str) -> Result<Option<Agent>>;
    /// Inserts or updates the agent row and its actor row together.
    async fn upsert_agent(&self, agent: &Agent) -> Result<()>;
    async fn set_agent_missing(&self, actor_id: ActorId, since: Option<Timestamp>) -> Result<()>;
    async fn set_agent_state(
        &self,
        actor_id: ActorId,
        state: AgentState,
        current_run_id: Option<RunId>,
    ) -> Result<()>;

    // Utility calls: Refine and Integrate records.

    async fn insert_utility_call(&self, call: &UtilityCall) -> Result<()>;
    async fn update_utility_call(&self, call: &UtilityCall) -> Result<()>;
    async fn utility_call(&self, id: CallId) -> Result<UtilityCall>;
    /// The newest call for the agent that still awaits a decision.
    async fn pending_proposal(
        &self,
        agent_id: ActorId,
        kind: UtilityKind,
    ) -> Result<Option<UtilityCall>>;
    /// Total recorded cost of calls started at or after `since`.
    async fn utility_cost_since(&self, since: Timestamp) -> Result<f64>;
}

pub type SharedStore = Arc<dyn Store>;
