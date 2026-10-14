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
use ak_core::approval::{Approval, ApprovalState};
use ak_core::board::{Column, Comment, Ticket};
use ak_core::chat::{Chat, Message, Thread, ThreadHit};
use ak_core::commands::{NewTicket, TicketPatch};
use ak_core::config::ProjectPolicy;
use ak_core::guardrails::Trip;
use ak_core::ids::{
    ActorId, ApprovalId, CallId, ChatId, ColumnId, CommentId, NotificationId, RunId, ThreadId,
    TicketId,
};
use ak_core::run::{AgentRun, RunEvent};
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
    /// Every call for the agent that still awaits a decision, newest first.
    async fn pending_proposals(
        &self,
        agent_id: ActorId,
        kind: UtilityKind,
    ) -> Result<Vec<UtilityCall>>;
    /// Total recorded cost of calls started at or after `since`.
    async fn utility_cost_since(&self, since: Timestamp) -> Result<f64>;

    // Guardrails.
    /// Events on a thread, oldest first.
    async fn events_for_thread(&self, thread_id: ThreadId) -> Result<Vec<Event>>;
    /// Records a trip as a system event and notifies the human.
    async fn record_guardrail_trip(&self, trip: &Trip) -> Result<Event>;
    /// Trip events since a time, newest first.
    async fn guardrail_trips_since(&self, since: Timestamp) -> Result<Vec<Event>>;

    // Chats and threads.
    /// Every chat, for the human, who observes all of them.
    async fn chats(&self) -> Result<Vec<Chat>>;
    /// Chats the actor is a member of.
    async fn chats_for(&self, actor: ActorId) -> Result<Vec<Chat>>;
    async fn chat(&self, id: ChatId) -> Result<Chat>;
    async fn chat_members(&self, id: ChatId) -> Result<Vec<ActorId>>;
    /// The direct chat between two actors, created on first use.
    async fn direct_chat(&self, a: ActorId, b: ActorId, actor: ActorId) -> Result<Chat>;
    async fn create_group_chat(
        &self,
        name: String,
        members: Vec<ActorId>,
        actor: ActorId,
    ) -> Result<Chat>;
    async fn add_chat_member(&self, chat: ChatId, member: ActorId, actor: ActorId) -> Result<()>;
    /// Threads in a chat, most recent activity first.
    async fn threads(&self, chat: ChatId, include_archived: bool) -> Result<Vec<Thread>>;
    async fn thread(&self, id: ThreadId) -> Result<Thread>;
    /// Everyone who has posted in the thread.
    async fn thread_participants(&self, id: ThreadId) -> Result<Vec<ActorId>>;
    async fn messages(&self, thread: ThreadId) -> Result<Vec<Message>>;
    async fn create_thread(
        &self,
        chat: ChatId,
        title: String,
        body: String,
        actor: ActorId,
    ) -> Result<(Thread, Message)>;
    /// Posts in a thread. Reopens an archived thread.
    async fn post_message(&self, thread: ThreadId, body: String, actor: ActorId)
        -> Result<Message>;
    async fn set_thread_paused(
        &self,
        thread: ThreadId,
        paused: bool,
        actor: ActorId,
    ) -> Result<Thread>;
    /// Full-text search over titles and bodies. With an actor, scoped to the
    /// chats that actor is a member of.
    async fn search_threads(
        &self,
        actor: Option<ActorId>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ThreadHit>>;
    /// Archives threads with no activity since the given time. Returns how many.
    async fn archive_idle_threads(&self, idle_since: Timestamp) -> Result<u64>;

    // Used by the bridge on behalf of agent runs.

    async fn ticket_by_key(&self, key: &str) -> Result<Option<Ticket>>;
    /// Marks the given notifications Handled, recording the run. Returns the count.
    async fn mark_notifications_handled(
        &self,
        ids: &[NotificationId],
        run: Option<RunId>,
    ) -> Result<u64>;

    // Approvals raised by the PreToolUse hook.

    async fn insert_approval(&self, approval: &Approval) -> Result<()>;
    async fn approval(&self, id: ApprovalId) -> Result<Approval>;
    async fn pending_approvals(&self) -> Result<Vec<Approval>>;
    async fn decide_approval(
        &self,
        id: ApprovalId,
        state: ApprovalState,
        decided_by: Option<ActorId>,
    ) -> Result<Approval>;

    // Agent runs and transcripts.

    async fn insert_run(&self, run: &AgentRun) -> Result<()>;
    async fn update_run(&self, run: &AgentRun) -> Result<()>;
    async fn run(&self, id: RunId) -> Result<AgentRun>;
    /// Newest first.
    async fn runs_for_agent(&self, agent: ActorId, limit: u32) -> Result<Vec<AgentRun>>;
    async fn active_runs(&self) -> Result<Vec<AgentRun>>;
    async fn append_run_event(&self, event: &RunEvent) -> Result<()>;
    async fn run_events(&self, run: RunId) -> Result<Vec<RunEvent>>;
    /// Cost of runs started at or after `since`, for one agent or all.
    async fn run_cost_since(&self, since: Timestamp, agent: Option<ActorId>) -> Result<f64>;
    async fn set_agent_cooldown(&self, actor_id: ActorId, until: Option<Timestamp>) -> Result<()>;
}

pub type SharedStore = Arc<dyn Store>;
