//! The board tools an agent run can call. Every call executes as the run's
//! agent; the store enforces frozen tickets and versions regardless of what
//! the model asks for.
//!
//! Tickets are addressed by key (`AK-12`), columns by name or kind, actors by
//! handle, so the model never handles UUIDs.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use ak_core::board::{Column, ColumnKind, Priority, Ticket};
use ak_core::commands::{NewTicket, TicketPatch};
use ak_core::event::{Event, EventKind, MentionSource};
use ak_core::ids::{ActorId, ColumnId, CommentId, NotificationId, TicketId};
use ak_core::{Actor, ActorKind, Notification};
use ak_store::{Store, StoreError};

use crate::RunContext;

pub const INSTRUCTIONS: &str =
    "You are an agent on a kanban board. Start with inbox_list to see why you were woken. \
Tickets carry work: read them with ticket_get, comment on them, move them, and assign them. \
Mention a teammate with @handle in a comment to notify them. Before you finish, call inbox_list \
again, handle what is pending, then memory_update to record what you learned.";

pub struct Outcome {
    pub text: String,
    pub is_error: bool,
}

impl Outcome {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

// Inputs

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct Empty {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InboxHandleInput {
    /// Notification ids from inbox_list.
    pub notification_ids: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TicketKeyInput {
    /// The ticket key, e.g. AK-12.
    pub key: String,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct TicketListInput {
    /// Column name or kind (backlog, todo, in_progress, review, done).
    pub column: Option<String>,
    /// Assignee handle without the @, or "unassigned".
    pub assignee: Option<String>,
    pub label: Option<String>,
    /// Case-insensitive search in title and description.
    pub text: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TicketCreateInput {
    pub title: String,
    /// Markdown. @mention teammates to notify them.
    #[serde(default)]
    pub description: String,
    /// Column name or kind. Defaults to the To do column.
    pub column: Option<String>,
    /// Assignee handle without the @.
    pub assignee: Option<String>,
    /// low, medium, high or urgent. Defaults to medium.
    pub priority: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Key of a parent ticket.
    pub parent_key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TicketUpdateInput {
    pub key: String,
    /// The version you read, from ticket_get or ticket_list.
    pub version: i64,
    pub title: Option<String>,
    pub description: Option<String>,
    /// low, medium, high or urgent.
    pub priority: Option<String>,
    pub labels: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TicketMoveInput {
    pub key: String,
    /// The version you read.
    pub version: i64,
    /// Column name or kind.
    pub column: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TicketAssignInput {
    pub key: String,
    /// The version you read.
    pub version: i64,
    /// Assignee handle without the @, or "unassigned".
    pub assignee: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TicketCommentInput {
    pub key: String,
    /// Markdown. @mention teammates to notify them.
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TicketReplyInput {
    pub key: String,
    /// The id of the comment you are replying to, from ticket_get.
    pub comment_id: String,
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MemoryUpdateInput {
    /// The complete new memory file, markdown.
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentActivateInput {
    pub handle: String,
    pub reason: String,
}

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    schema: fn() -> Value,
}

fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| json!({"type": "object"}))
}

const SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "inbox_list",
        description: "Your pending notifications, newest first, with the ticket each one is about. Call this first, and again before you finish.",
        schema: schema::<Empty>,
    },
    ToolSpec {
        name: "inbox_handle",
        description: "Mark notifications as handled. Mutating a ticket handles its notifications automatically.",
        schema: schema::<InboxHandleInput>,
    },
    ToolSpec {
        name: "team_list",
        description: "Everyone on the board: handle, kind, what they own, and whether an agent is available.",
        schema: schema::<Empty>,
    },
    ToolSpec {
        name: "ticket_get",
        description: "A ticket in full: fields, description, comments with ids, and recent activity.",
        schema: schema::<TicketKeyInput>,
    },
    ToolSpec {
        name: "ticket_list",
        description: "Ticket summaries, optionally filtered by column, assignee, label or text.",
        schema: schema::<TicketListInput>,
    },
    ToolSpec {
        name: "ticket_create",
        description: "Create a ticket. You become its reporter.",
        schema: schema::<TicketCreateInput>,
    },
    ToolSpec {
        name: "ticket_update",
        description: "Change a ticket's title, description, priority or labels. Requires the version you read.",
        schema: schema::<TicketUpdateInput>,
    },
    ToolSpec {
        name: "ticket_move",
        description: "Move a ticket to another column. Requires the version you read.",
        schema: schema::<TicketMoveInput>,
    },
    ToolSpec {
        name: "ticket_assign",
        description: "Assign a ticket to a teammate, or unassign it. Requires the version you read.",
        schema: schema::<TicketAssignInput>,
    },
    ToolSpec {
        name: "ticket_comment",
        description: "Comment on a ticket. @mention teammates to notify them.",
        schema: schema::<TicketCommentInput>,
    },
    ToolSpec {
        name: "ticket_reply",
        description: "Reply to a specific comment. The comment's author is notified.",
        schema: schema::<TicketReplyInput>,
    },
    ToolSpec {
        name: "memory_read",
        description: "Your memory file, as it stands.",
        schema: schema::<Empty>,
    },
    ToolSpec {
        name: "memory_update",
        description: "Replace your memory file. Keep it short: what you learned about this project that your next run should know.",
        schema: schema::<MemoryUpdateInput>,
    },
    ToolSpec {
        name: "agent_activate",
        description: "Ask the scheduler to wake another agent with a reason. Prefer assigning or @mentioning on a ticket.",
        schema: schema::<AgentActivateInput>,
    },
];

/// The `tools/list` payload.
pub fn list() -> Vec<Value> {
    SPECS
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "inputSchema": (s.schema)(),
            })
        })
        .collect()
}

/// Runs one tool as the run's agent.
pub async fn call(ctx: &RunContext, name: &str, args: Value) -> Outcome {
    match run(ctx, name, args).await {
        Ok(text) => Outcome::ok(text),
        Err(ToolError::Message(m)) => Outcome::err(m),
        Err(ToolError::Store(e)) => Outcome::err(store_message(&e)),
    }
}

enum ToolError {
    Message(String),
    Store(StoreError),
}

impl From<StoreError> for ToolError {
    fn from(e: StoreError) -> Self {
        ToolError::Store(e)
    }
}

impl From<String> for ToolError {
    fn from(m: String) -> Self {
        ToolError::Message(m)
    }
}

fn store_message(e: &StoreError) -> String {
    match e {
        StoreError::Core(ak_core::CoreError::VersionConflict { expected, actual }) => format!(
            "version conflict: you read version {expected} but the ticket is at {actual}. \
             Call ticket_get, review what changed, and retry with the current version."
        ),
        StoreError::Core(ak_core::CoreError::TicketFrozen { key, reason }) => format!(
            "ticket {key} is frozen ({reason}). Only the human can unfreeze it; comment if you need to."
        ),
        other => other.to_string(),
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|e| ToolError::Message(format!("invalid arguments: {e}")))
}

async fn run(ctx: &RunContext, name: &str, args: Value) -> Result<String, ToolError> {
    let store = ctx.store();
    let me = ctx.agent.actor_id;
    match name {
        "inbox_list" => {
            let snap = Snapshot::load(store.as_ref()).await?;
            let items = store.notifications(me, true).await?;
            let events = store.events_since(0, 5000).await?;
            Ok(render_inbox(&items, &events, &snap))
        }
        "inbox_handle" => {
            let input: InboxHandleInput = parse_args(args)?;
            let ids: Vec<NotificationId> = input
                .notification_ids
                .iter()
                .filter_map(|s| s.parse().ok())
                .collect();
            let n = store
                .mark_notifications_handled(&ids, Some(ctx.run_id))
                .await?;
            Ok(format!("handled {n} notification(s)"))
        }
        "team_list" => {
            let actors = store.actors().await?;
            let agents = store.agents().await?;
            let mut lines = Vec::new();
            for a in actors {
                let line = match a.kind {
                    ActorKind::System => continue,
                    ActorKind::Human => {
                        format!("- @{} (human): the person running this board", a.handle)
                    }
                    ActorKind::Agent => {
                        let agent = agents.iter().find(|g| g.actor_id == a.id);
                        match agent {
                            Some(g) if g.is_retired() => continue,
                            Some(g) => format!(
                                "- @{} (agent, {}): {}",
                                a.handle,
                                g.state.label().to_lowercase(),
                                g.description
                            ),
                            None => format!("- @{} (agent)", a.handle),
                        }
                    }
                };
                lines.push(line);
            }
            Ok(lines.join("\n"))
        }
        "ticket_get" => {
            let input: TicketKeyInput = parse_args(args)?;
            let snap = Snapshot::load(store.as_ref()).await?;
            let t = find_ticket(store.as_ref(), &input.key).await?;
            let comments = store.comments(t.id).await?;
            let events = store.events_for_ticket(t.id).await?;
            Ok(render_ticket(&t, &comments, &events, &snap))
        }
        "ticket_list" => {
            let input: TicketListInput = parse_args(args.clone()).or_else(|_| {
                if args.is_null() {
                    Ok(TicketListInput::default())
                } else {
                    Err(ToolError::Message("invalid arguments".into()))
                }
            })?;
            let snap = Snapshot::load(store.as_ref()).await?;
            let column = match &input.column {
                Some(c) => Some(snap.column_named(c)?.id),
                None => None,
            };
            let assignee = match input.assignee.as_deref() {
                None => None,
                Some("unassigned") => Some(None),
                Some(h) => Some(Some(snap.actor_named(h)?.id)),
            };
            let text = input.text.as_deref().map(|s| s.to_lowercase());
            let label = input.label.as_deref().map(|s| s.to_lowercase());
            let mut lines = Vec::new();
            for t in store.tickets().await? {
                if column.is_some_and(|c| t.column_id != c) {
                    continue;
                }
                if let Some(a) = &assignee {
                    if t.assignee_id != *a {
                        continue;
                    }
                }
                if let Some(l) = &label {
                    if !t.labels.iter().any(|x| x == l) {
                        continue;
                    }
                }
                if let Some(q) = &text {
                    if !t.title.to_lowercase().contains(q)
                        && !t.description.to_lowercase().contains(q)
                    {
                        continue;
                    }
                }
                lines.push(summary_line(&t, &snap));
            }
            if lines.is_empty() {
                Ok("no tickets match".into())
            } else {
                Ok(lines.join("\n"))
            }
        }
        "ticket_create" => {
            let input: TicketCreateInput = parse_args(args)?;
            let snap = Snapshot::load(store.as_ref()).await?;
            let column_id = match &input.column {
                Some(c) => snap.column_named(c)?.id,
                None => snap.default_column()?.id,
            };
            let assignee_id = match &input.assignee {
                Some(h) => Some(snap.actor_named(h)?.id),
                None => None,
            };
            let priority = match input.priority.as_deref() {
                None => Priority::Medium,
                Some(p) => Priority::parse(&p.to_lowercase()).ok_or_else(|| {
                    format!("unknown priority {p:?}; use low, medium, high or urgent")
                })?,
            };
            let parent_id = match &input.parent_key {
                Some(k) => Some(find_ticket(store.as_ref(), k).await?.id),
                None => None,
            };
            let t = store
                .create_ticket(
                    NewTicket {
                        title: input.title,
                        description: input.description,
                        column_id,
                        assignee_id,
                        priority,
                        labels: input.labels,
                        parent_id,
                    },
                    me,
                )
                .await?;
            let snap = Snapshot::load(store.as_ref()).await?;
            Ok(format!("created {}", summary_line(&t, &snap)))
        }
        "ticket_update" => {
            let input: TicketUpdateInput = parse_args(args)?;
            let t = find_ticket(store.as_ref(), &input.key).await?;
            let priority = match input.priority.as_deref() {
                None => None,
                Some(p) => Some(
                    Priority::parse(&p.to_lowercase())
                        .ok_or_else(|| format!("unknown priority {p:?}"))?,
                ),
            };
            let updated = store
                .update_ticket(
                    t.id,
                    TicketPatch {
                        title: input.title,
                        description: input.description,
                        priority,
                        labels: input.labels,
                    },
                    input.version,
                    me,
                )
                .await?;
            auto_handle(ctx, store.as_ref(), updated.id).await?;
            let snap = Snapshot::load(store.as_ref()).await?;
            Ok(format!("updated {}", summary_line(&updated, &snap)))
        }
        "ticket_move" => {
            let input: TicketMoveInput = parse_args(args)?;
            let snap = Snapshot::load(store.as_ref()).await?;
            let t = find_ticket(store.as_ref(), &input.key).await?;
            let to = snap.column_named(&input.column)?.id;
            let moved = store.move_ticket(t.id, to, input.version, me).await?;
            auto_handle(ctx, store.as_ref(), moved.id).await?;
            Ok(format!("moved {}", summary_line(&moved, &snap)))
        }
        "ticket_assign" => {
            let input: TicketAssignInput = parse_args(args)?;
            let snap = Snapshot::load(store.as_ref()).await?;
            let t = find_ticket(store.as_ref(), &input.key).await?;
            let to = if input.assignee.trim().is_empty() || input.assignee == "unassigned" {
                None
            } else {
                Some(snap.actor_named(&input.assignee)?.id)
            };
            let assigned = store.assign_ticket(t.id, to, input.version, me).await?;
            auto_handle(ctx, store.as_ref(), assigned.id).await?;
            Ok(format!("assigned {}", summary_line(&assigned, &snap)))
        }
        "ticket_comment" => {
            let input: TicketCommentInput = parse_args(args)?;
            let t = find_ticket(store.as_ref(), &input.key).await?;
            let c = store.comment(t.id, input.body, None, me).await?;
            auto_handle(ctx, store.as_ref(), t.id).await?;
            Ok(format!("commented on {} (comment id {})", t.key, c.id))
        }
        "ticket_reply" => {
            let input: TicketReplyInput = parse_args(args)?;
            let t = find_ticket(store.as_ref(), &input.key).await?;
            let parent: CommentId = input
                .comment_id
                .parse()
                .map_err(|_| format!("invalid comment id {:?}", input.comment_id))?;
            let c = store.comment(t.id, input.body, Some(parent), me).await?;
            auto_handle(ctx, store.as_ref(), t.id).await?;
            Ok(format!("replied on {} (comment id {})", t.key, c.id))
        }
        "memory_read" => {
            let text = ctx
                .project
                .agents
                .read_memory(&ctx.agent.handle)
                .map_err(|e| e.to_string())?;
            if text.trim().is_empty() {
                Ok("(your memory is empty)".into())
            } else {
                Ok(text)
            }
        }
        "memory_update" => {
            let input: MemoryUpdateInput = parse_args(args)?;
            ctx.project
                .agents
                .write_memory(&ctx.agent.handle, &input.content)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "memory saved ({} characters)",
                input.content.chars().count()
            ))
        }
        "agent_activate" => {
            let input: AgentActivateInput = parse_args(args)?;
            let snap = Snapshot::load(store.as_ref()).await?;
            snap.actor_named(&input.handle)?;
            Err(ToolError::Message(
                "direct activation is not available yet; assign a ticket to them or @mention them in a comment instead".into(),
            ))
        }
        other => Err(ToolError::Message(format!("unknown tool {other}"))),
    }
}

/// After an agent mutates a ticket, its own open notifications about that
/// ticket are handled: the agent has clearly seen them.
async fn auto_handle(
    ctx: &RunContext,
    store: &dyn Store,
    ticket: TicketId,
) -> Result<(), ToolError> {
    let open = store.notifications(ctx.agent.actor_id, true).await?;
    if open.is_empty() {
        return Ok(());
    }
    let events = store.events_since(0, 5000).await?;
    let by_id: HashMap<_, &Event> = events.iter().map(|e| (e.id, e)).collect();
    let ids: Vec<NotificationId> = open
        .iter()
        .filter(|n| {
            by_id
                .get(&n.event_id)
                .and_then(|e| e.kind.ticket_id())
                .is_some_and(|t| t == ticket)
        })
        .map(|n| n.id)
        .collect();
    if !ids.is_empty() {
        store
            .mark_notifications_handled(&ids, Some(ctx.run_id))
            .await?;
    }
    Ok(())
}

async fn find_ticket(store: &dyn Store, key: &str) -> Result<Ticket, ToolError> {
    store
        .ticket_by_key(key)
        .await?
        .ok_or_else(|| ToolError::Message(format!("no ticket with key {key:?}")))
}

/// Lookups the renderers need, loaded once per call.
pub struct Snapshot {
    actors: Vec<Actor>,
    columns: Vec<Column>,
    tickets: HashMap<TicketId, Ticket>,
}

impl Snapshot {
    async fn load(store: &dyn Store) -> Result<Self, ToolError> {
        Ok(Self {
            actors: store.actors().await?,
            columns: store.columns().await?,
            tickets: store
                .tickets()
                .await?
                .into_iter()
                .map(|t| (t.id, t))
                .collect(),
        })
    }

    fn handle(&self, id: ActorId) -> String {
        self.actors
            .iter()
            .find(|a| a.id == id)
            .map(|a| format!("@{}", a.handle))
            .unwrap_or_else(|| "@unknown".into())
    }

    fn actor_named(&self, handle: &str) -> Result<&Actor, ToolError> {
        let h = handle.trim().trim_start_matches('@').to_lowercase();
        self.actors
            .iter()
            .find(|a| a.handle == h && a.kind != ActorKind::System)
            .ok_or_else(|| ToolError::Message(format!("no teammate with handle @{h}")))
    }

    fn column_name(&self, id: ColumnId) -> String {
        self.columns
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "?".into())
    }

    fn column_named(&self, name: &str) -> Result<&Column, ToolError> {
        let q = name.trim().to_lowercase();
        self.columns
            .iter()
            .find(|c| c.name.to_lowercase() == q || c.kind.as_str() == q.replace(' ', "_"))
            .ok_or_else(|| {
                let names: Vec<&str> = self.columns.iter().map(|c| c.name.as_str()).collect();
                ToolError::Message(format!(
                    "no column {name:?}; columns are: {}",
                    names.join(", ")
                ))
            })
    }

    fn default_column(&self) -> Result<&Column, ToolError> {
        self.columns
            .iter()
            .find(|c| c.kind == ColumnKind::Todo)
            .or_else(|| self.columns.first())
            .ok_or_else(|| ToolError::Message("the board has no columns".into()))
    }

    fn ticket_key(&self, id: TicketId) -> String {
        self.tickets
            .get(&id)
            .map(|t| t.key.clone())
            .unwrap_or_else(|| "?".into())
    }
}

fn summary_line(t: &Ticket, snap: &Snapshot) -> String {
    format!(
        "{} \"{}\" | {} | {} | {} | v{}{}{}",
        t.key,
        t.title,
        snap.column_name(t.column_id),
        t.assignee_id
            .map(|a| snap.handle(a))
            .unwrap_or_else(|| "unassigned".into()),
        t.priority.as_str(),
        t.version,
        if t.labels.is_empty() {
            String::new()
        } else {
            format!(" | #{}", t.labels.join(" #"))
        },
        if t.frozen { " | FROZEN" } else { "" },
    )
}

fn render_ticket(
    t: &Ticket,
    comments: &[ak_core::board::Comment],
    events: &[Event],
    snap: &Snapshot,
) -> String {
    let mut out = String::new();
    out.push_str(&summary_line(t, snap));
    out.push_str(&format!("\nreporter: {}", snap.handle(t.reporter_id)));
    if let Some(reason) = &t.freeze_reason {
        out.push_str(&format!("\nfrozen: {reason}"));
    }
    out.push_str("\n\n## Description\n\n");
    out.push_str(if t.description.trim().is_empty() {
        "(none)"
    } else {
        t.description.trim()
    });
    out.push_str(&format!("\n\n## Comments ({})\n", comments.len()));
    for c in comments {
        let reply = c
            .parent_comment_id
            .map(|p| format!(" (reply to {p})"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n[{}] {} at {}{}:\n{}\n",
            c.id,
            snap.handle(c.author_id),
            c.created_at.format("%Y-%m-%d %H:%M"),
            reply,
            c.body.trim()
        ));
    }
    out.push_str("\n## Recent activity\n");
    for e in events.iter().rev().take(15) {
        out.push_str(&format!(
            "- {} {}\n",
            e.occurred_at.format("%Y-%m-%d %H:%M"),
            describe(e, snap)
        ));
    }
    out
}

fn render_inbox(items: &[Notification], events: &[Event], snap: &Snapshot) -> String {
    if items.is_empty() {
        return "inbox empty: nothing needs your attention".into();
    }
    let by_id: HashMap<_, &Event> = events.iter().map(|e| (e.id, e)).collect();
    let mut out = format!("{} notification(s):\n", items.len());
    for n in items {
        let event = by_id.get(&n.event_id);
        let what = event
            .map(|e| describe(e, snap))
            .unwrap_or_else(|| "(event no longer available)".into());
        let ticket = event
            .and_then(|e| e.kind.ticket_id())
            .and_then(|id| snap.tickets.get(&id))
            .map(|t| format!("\n    ticket: {}", summary_line(t, snap)))
            .unwrap_or_default();
        out.push_str(&format!(
            "- id {} | {}{} | {}\n    {what}{ticket}\n",
            n.id,
            n.kind.as_str(),
            if n.wake { " (woke you)" } else { "" },
            n.created_at.format("%Y-%m-%d %H:%M"),
        ));
    }
    out.push_str(
        "\nUse ticket_get for the full ticket. Mutating a ticket handles its notifications; \
         inbox_handle handles the rest.",
    );
    out
}

pub fn describe(e: &Event, snap: &Snapshot) -> String {
    let who = snap.handle(e.actor_id);
    match &e.kind {
        EventKind::TicketCreated {
            ticket_id, title, ..
        } => {
            format!("{who} created {} \"{title}\"", snap.ticket_key(*ticket_id))
        }
        EventKind::TicketUpdated { ticket_id, changes } => {
            let fields: Vec<&str> = changes.iter().map(|c| c.field.as_str()).collect();
            format!(
                "{who} updated {} ({})",
                snap.ticket_key(*ticket_id),
                fields.join(", ")
            )
        }
        EventKind::TicketMoved {
            ticket_id,
            from,
            to,
        } => format!(
            "{who} moved {} from {} to {}",
            snap.ticket_key(*ticket_id),
            snap.column_name(*from),
            snap.column_name(*to)
        ),
        EventKind::TicketAssigned {
            ticket_id,
            to: Some(to),
            ..
        } => format!(
            "{who} assigned {} to {}",
            snap.ticket_key(*ticket_id),
            snap.handle(*to)
        ),
        EventKind::TicketAssigned {
            ticket_id,
            to: None,
            ..
        } => {
            format!("{who} unassigned {}", snap.ticket_key(*ticket_id))
        }
        EventKind::TicketCommented { ticket_id, .. } => {
            format!("{who} commented on {}", snap.ticket_key(*ticket_id))
        }
        EventKind::TicketReplied {
            ticket_id,
            parent_author_id,
            ..
        } => format!(
            "{who} replied to {} on {}",
            snap.handle(*parent_author_id),
            snap.ticket_key(*ticket_id)
        ),
        EventKind::TicketFrozen { ticket_id, reason } => {
            format!("{who} froze {}: {reason}", snap.ticket_key(*ticket_id))
        }
        EventKind::TicketUnfrozen { ticket_id } => {
            format!("{who} unfroze {}", snap.ticket_key(*ticket_id))
        }
        EventKind::MentionCreated {
            source,
            mentioned_id,
        } => {
            let place = match source {
                MentionSource::TicketDescription { .. } => "the description of",
                MentionSource::Comment { .. } => "a comment on",
            };
            format!(
                "{who} mentioned {} in {place} {}",
                snap.handle(*mentioned_id),
                snap.ticket_key(source.ticket_id())
            )
        }
    }
}

/// Lookups for callers outside this module, such as the stop hook.
pub async fn snapshot(store: &dyn Store) -> Result<Snapshot, String> {
    Snapshot::load(store).await.map_err(|e| match e {
        ToolError::Message(m) => m,
        ToolError::Store(s) => s.to_string(),
    })
}
