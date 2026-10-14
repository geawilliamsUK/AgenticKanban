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
use ak_core::chat::{Chat, ChatKind, Thread};
use ak_core::commands::{NewTicket, TicketPatch};
use ak_core::event::{Event, EventKind, MentionSource};
use ak_core::ids::{ActorId, ChatId, ColumnId, CommentId, NotificationId, ThreadId, TicketId};
use ak_core::{Actor, ActorKind, Notification};
use ak_store::{Store, StoreError};

use crate::RunContext;

pub const INSTRUCTIONS: &str =
    "You are an agent on a kanban board. Start with inbox_list to see why you were woken. \
Tickets carry work: read them with ticket_get, comment on them, move them, and assign them. \
Mention a teammate with @handle in a comment to notify them. Before you finish, call inbox_list \
again, handle what is pending, then memory_update to record what you learned. Chats are for \
conversation, not work: tickets carry work and decisions. A direct thread (chat_direct, then \
thread_create) is for a short clarification with one teammate; a group thread is for a question to \
the team. Keep messages short. The human sees every chat and may answer.";

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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChatDirectInput {
    /// The teammate's handle without the @.
    pub handle: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChatCreateGroupInput {
    pub name: String,
    /// Handles without the @. You are always a member.
    pub members: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThreadListInput {
    /// From chat_list.
    pub chat_id: String,
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThreadSearchInput {
    /// Words to look for in thread titles and messages.
    pub query: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThreadReadInput {
    /// From thread_list, thread_search or your inbox.
    pub thread_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThreadCreateInput {
    pub chat_id: String,
    /// What the thread is about, in a few words.
    pub title: String,
    /// The first message, markdown. @mention teammates to notify them.
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThreadPostInput {
    pub thread_id: String,
    /// Markdown. @mention teammates to notify them.
    pub body: String,
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
        name: "chat_list",
        description: "Chats you are in: your direct chats and group chats, with their ids.",
        schema: schema::<Empty>,
    },
    ToolSpec {
        name: "chat_direct",
        description: "Your direct chat with a teammate, created if needed. A message there wakes them.",
        schema: schema::<ChatDirectInput>,
    },
    ToolSpec {
        name: "chat_create_group",
        description: "A named group chat with teammates. Posts there do not wake anyone; @mentions do.",
        schema: schema::<ChatCreateGroupInput>,
    },
    ToolSpec {
        name: "thread_list",
        description: "Threads in a chat, most recent activity first.",
        schema: schema::<ThreadListInput>,
    },
    ToolSpec {
        name: "thread_search",
        description: "Full-text search over thread titles and messages in your chats.",
        schema: schema::<ThreadSearchInput>,
    },
    ToolSpec {
        name: "thread_read",
        description: "Every message in a thread.",
        schema: schema::<ThreadReadInput>,
    },
    ToolSpec {
        name: "thread_create",
        description: "Start a thread in a chat with a title and a first message. Every message lives in a thread.",
        schema: schema::<ThreadCreateInput>,
    },
    ToolSpec {
        name: "thread_post",
        description: "Post in an existing thread. Rejected while the thread is paused. Posting handles your notifications about the thread.",
        schema: schema::<ThreadPostInput>,
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
    serde_json::from_value(args).map_err(|e| ToolError::Message(format!(
        "invalid arguments: {e}. Check the tool's input schema; a ticket change needs the version from ticket_get."
    )))
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
        "chat_list" => {
            let snap = Snapshot::load(store.as_ref()).await?;
            let chats = store.chats_for(me).await?;
            if chats.is_empty() {
                return Ok("no chats yet: chat_direct opens one with a teammate".into());
            }
            let mut lines = Vec::new();
            for c in &chats {
                let members = store.chat_members(c.id).await?;
                lines.push(format!(
                    "- chat {} | {}",
                    c.id,
                    describe_chat(c, &members, me, &snap)
                ));
            }
            Ok(lines.join("\n"))
        }
        "chat_direct" => {
            let input: ChatDirectInput = parse_args(args)?;
            let snap = Snapshot::load(store.as_ref()).await?;
            let other = snap.actor_named(&input.handle)?;
            let chat = store.direct_chat(me, other.id, me).await?;
            Ok(format!(
                "chat {} (direct with @{}). Use thread_create to start a thread there.",
                chat.id, other.handle
            ))
        }
        "chat_create_group" => {
            let input: ChatCreateGroupInput = parse_args(args)?;
            let snap = Snapshot::load(store.as_ref()).await?;
            let mut members = Vec::new();
            for h in &input.members {
                members.push(snap.actor_named(h)?.id);
            }
            let chat = store.create_group_chat(input.name, members, me).await?;
            Ok(format!(
                "chat {} (group \"{}\")",
                chat.id,
                chat.name.unwrap_or_default()
            ))
        }
        "thread_list" => {
            let input: ThreadListInput = parse_args(args)?;
            let chat_id = parse_chat_id(&input.chat_id)?;
            member_of(store.as_ref(), chat_id, me).await?;
            let threads = store.threads(chat_id, input.include_archived).await?;
            if threads.is_empty() {
                return Ok("no threads in this chat".into());
            }
            Ok(threads
                .iter()
                .map(thread_line)
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "thread_search" => {
            let input: ThreadSearchInput = parse_args(args)?;
            let snap = Snapshot::load(store.as_ref()).await?;
            let hits = store.search_threads(Some(me), &input.query, 20).await?;
            if hits.is_empty() {
                return Ok("no threads match".into());
            }
            Ok(hits
                .iter()
                .map(|h| {
                    format!(
                        "- thread {} | \"{}\" | in {} | {}",
                        h.thread.id,
                        h.thread.title,
                        snap.chat_label(h.thread.chat_id),
                        h.snippet
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "thread_read" => {
            let input: ThreadReadInput = parse_args(args)?;
            let thread_id = parse_thread_id(&input.thread_id)?;
            let thread = store.thread(thread_id).await?;
            member_of(store.as_ref(), thread.chat_id, me).await?;
            let snap = Snapshot::load(store.as_ref()).await?;
            let messages = store.messages(thread_id).await?;
            let mut out = format!(
                "thread {} \"{}\" in {}{}{}\n",
                thread.id,
                thread.title,
                snap.chat_label(thread.chat_id),
                if thread.paused {
                    " | PAUSED: agents cannot post"
                } else {
                    ""
                },
                if thread.archived { " | archived" } else { "" }
            );
            for m in &messages {
                out.push_str(&format!(
                    "\n{} ({}):\n{}\n",
                    snap.handle(m.author_id),
                    m.created_at.format("%Y-%m-%d %H:%M"),
                    m.body
                ));
            }
            Ok(out)
        }
        "thread_create" => {
            let input: ThreadCreateInput = parse_args(args)?;
            let chat_id = parse_chat_id(&input.chat_id)?;
            let (thread, _) = store
                .create_thread(chat_id, input.title, input.body, me)
                .await?;
            Ok(format!("created thread {} \"{}\"", thread.id, thread.title))
        }
        "thread_post" => {
            let input: ThreadPostInput = parse_args(args)?;
            let thread_id = parse_thread_id(&input.thread_id)?;
            let message = store.post_message(thread_id, input.body, me).await?;
            auto_handle_thread(ctx, store.as_ref(), thread_id).await?;
            Ok(format!(
                "posted in thread {} (message id {})",
                thread_id, message.id
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

fn parse_chat_id(s: &str) -> Result<ChatId, ToolError> {
    s.trim()
        .parse()
        .map_err(|_| ToolError::Message(format!("not a chat id: {s:?}; use the id from chat_list")))
}

fn parse_thread_id(s: &str) -> Result<ThreadId, ToolError> {
    s.trim().parse().map_err(|_| {
        ToolError::Message(format!(
            "not a thread id: {s:?}; use the id from thread_list or your inbox"
        ))
    })
}

/// Agents read and write only the chats they belong to.
async fn member_of(store: &dyn Store, chat: ChatId, me: ActorId) -> Result<(), ToolError> {
    if store.chat_members(chat).await?.contains(&me) {
        Ok(())
    } else {
        Err(ToolError::Message(
            "you are not a member of that chat".into(),
        ))
    }
}

fn thread_line(t: &Thread) -> String {
    format!(
        "- thread {} | \"{}\" | last activity {}{}{}",
        t.id,
        t.title,
        t.last_activity_at.format("%Y-%m-%d %H:%M"),
        if t.paused { " | paused" } else { "" },
        if t.archived { " | archived" } else { "" }
    )
}

fn describe_chat(c: &Chat, members: &[ActorId], me: ActorId, snap: &Snapshot) -> String {
    match c.kind {
        ChatKind::Direct => {
            let other = members
                .iter()
                .find(|m| **m != me)
                .map(|m| snap.handle(*m))
                .unwrap_or_else(|| "?".into());
            format!("direct with {other}")
        }
        ChatKind::Group => format!(
            "group \"{}\": {}",
            c.name.clone().unwrap_or_default(),
            members
                .iter()
                .map(|m| snap.handle(*m))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// After an agent posts in a thread, its own open notifications about that
/// thread are handled.
async fn auto_handle_thread(
    ctx: &RunContext,
    store: &dyn Store,
    thread: ThreadId,
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
                .and_then(|e| e.kind.thread_id())
                .is_some_and(|t| t == thread)
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
    chats: HashMap<ChatId, Chat>,
    threads: HashMap<ThreadId, Thread>,
}

impl Snapshot {
    async fn load(store: &dyn Store) -> Result<Self, ToolError> {
        let chats = store.chats().await?;
        let mut threads = HashMap::new();
        for c in &chats {
            for t in store.threads(c.id, true).await? {
                threads.insert(t.id, t);
            }
        }
        Ok(Self {
            actors: store.actors().await?,
            columns: store.columns().await?,
            tickets: store
                .tickets()
                .await?
                .into_iter()
                .map(|t| (t.id, t))
                .collect(),
            chats: chats.into_iter().map(|c| (c.id, c)).collect(),
            threads,
        })
    }

    fn chat_label(&self, id: ChatId) -> String {
        match self.chats.get(&id) {
            Some(c) if c.kind == ChatKind::Group => {
                format!("group \"{}\"", c.name.clone().unwrap_or_default())
            }
            Some(_) => "a direct chat".into(),
            None => "a chat".into(),
        }
    }

    fn thread_title(&self, id: ThreadId) -> String {
        self.threads
            .get(&id)
            .map(|t| t.title.clone())
            .unwrap_or_else(|| "?".into())
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
        let thread = event
            .and_then(|e| e.kind.thread_id())
            .and_then(|id| snap.threads.get(&id))
            .map(|t| {
                format!(
                    "\n    thread: {} \"{}\" in {}",
                    t.id,
                    t.title,
                    snap.chat_label(t.chat_id)
                )
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "- id {} | {}{} | {}\n    {what}{ticket}{thread}\n",
            n.id,
            n.kind.as_str(),
            if n.wake { " (woke you)" } else { "" },
            n.created_at.format("%Y-%m-%d %H:%M"),
        ));
    }
    out.push_str(
        "\nUse ticket_get for the full ticket, thread_read for a thread. Mutating a ticket or \
         posting in a thread handles its notifications; inbox_handle handles the rest.",
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
        } => match source {
            MentionSource::TicketDescription { ticket_id } => format!(
                "{who} mentioned {} in the description of {}",
                snap.handle(*mentioned_id),
                snap.ticket_key(*ticket_id)
            ),
            MentionSource::Comment { ticket_id, .. } => format!(
                "{who} mentioned {} in a comment on {}",
                snap.handle(*mentioned_id),
                snap.ticket_key(*ticket_id)
            ),
            MentionSource::Message { thread_id, .. } => format!(
                "{who} mentioned {} in thread \"{}\"",
                snap.handle(*mentioned_id),
                snap.thread_title(*thread_id)
            ),
        },
        EventKind::ChatCreated {
            kind,
            name,
            member_ids,
            ..
        } => {
            let others: Vec<String> = member_ids
                .iter()
                .filter(|m| **m != e.actor_id)
                .map(|m| snap.handle(*m))
                .collect();
            match kind {
                ChatKind::Direct => {
                    format!("{who} opened a direct chat with {}", others.join(", "))
                }
                ChatKind::Group => format!(
                    "{who} created group chat \"{}\" with {}",
                    name.clone().unwrap_or_default(),
                    others.join(", ")
                ),
            }
        }
        EventKind::ChatMemberAdded { chat_id, member_id } => format!(
            "{who} added {} to {}",
            snap.handle(*member_id),
            snap.chat_label(*chat_id)
        ),
        EventKind::ThreadCreated { chat_id, title, .. } => {
            format!(
                "{who} started thread \"{title}\" in {}",
                snap.chat_label(*chat_id)
            )
        }
        EventKind::ThreadMessagePosted { chat_id, title, .. } => {
            format!(
                "{who} posted in thread \"{title}\" ({})",
                snap.chat_label(*chat_id)
            )
        }
        EventKind::ThreadPaused { title, .. } => format!("{who} paused thread \"{title}\""),
        EventKind::ThreadUnpaused { title, .. } => format!("{who} unpaused thread \"{title}\""),
    }
}

/// Lookups for callers outside this module, such as the stop hook.
pub async fn snapshot(store: &dyn Store) -> Result<Snapshot, String> {
    Snapshot::load(store).await.map_err(|e| match e {
        ToolError::Message(m) => m,
        ToolError::Store(s) => s.to_string(),
    })
}
