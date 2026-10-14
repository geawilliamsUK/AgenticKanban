//! Pure "decide" functions. Each takes the current state and a command and
//! returns the new state plus the events to append. The store wraps a decide
//! call in a transaction: load, decide, write, append events, commit.
//!
//! Rules enforced here apply no matter who is calling: frozen tickets reject
//! agent mutations, versions must match, titles cannot be empty.

use serde_json::json;

use crate::actor::Actor;
use crate::board::{Comment, Priority, Ticket};
use crate::chat::{Chat, ChatKind, Message, Thread};
use crate::error::CoreError;
use crate::event::{EventKind, FieldChange, MentionSource};
use crate::ids::{ActorId, ChatId, ColumnId, CommentId, MessageId, ThreadId, TicketId};
use crate::mentions;
use crate::Timestamp;

/// The result of a decide function: the new value, the events to append, and
/// any `@handles` found in text the caller must resolve to actors.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome<T> {
    pub value: T,
    pub events: Vec<EventKind>,
    pub mention_handles: Vec<String>,
}

impl<T> Outcome<T> {
    fn new(value: T, events: Vec<EventKind>) -> Self {
        Self {
            value,
            events,
            mention_handles: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct NewTicket {
    pub title: String,
    pub description: String,
    pub column_id: ColumnId,
    pub assignee_id: Option<ActorId>,
    pub priority: Priority,
    pub labels: Vec<String>,
    pub parent_id: Option<TicketId>,
}

pub fn create_ticket(
    input: NewTicket,
    key: String,
    position: i64,
    reporter: &Actor,
    now: Timestamp,
) -> Result<Outcome<Ticket>, CoreError> {
    let title = input.title.trim().to_string();
    if title.is_empty() {
        return Err(CoreError::validation("title cannot be empty"));
    }
    let ticket = Ticket {
        id: TicketId::new(),
        key,
        title,
        description: input.description,
        column_id: input.column_id,
        position,
        assignee_id: input.assignee_id,
        reporter_id: reporter.id,
        priority: input.priority,
        labels: normalise_labels(input.labels),
        parent_id: input.parent_id,
        frozen: false,
        freeze_reason: None,
        version: 1,
        created_at: now,
        updated_at: now,
    };
    let events = vec![EventKind::TicketCreated {
        ticket_id: ticket.id,
        key: ticket.key.clone(),
        title: ticket.title.clone(),
        column_id: ticket.column_id,
        assignee_id: ticket.assignee_id,
    }];
    let mention_handles = mentions::extract_handles(&ticket.description);
    Ok(Outcome {
        value: ticket,
        events,
        mention_handles,
    })
}

#[derive(Debug, Clone, Default)]
pub struct TicketPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    pub priority: Option<Priority>,
    pub labels: Option<Vec<String>>,
}

pub fn update_ticket(
    ticket: &Ticket,
    patch: TicketPatch,
    expected_version: i64,
    actor: &Actor,
    now: Timestamp,
) -> Result<Outcome<Ticket>, CoreError> {
    check_mutable(ticket, actor, expected_version)?;
    let mut next = ticket.clone();
    let mut changes = Vec::new();
    let mut mention_handles = Vec::new();

    if let Some(title) = patch.title {
        let title = title.trim().to_string();
        if title.is_empty() {
            return Err(CoreError::validation("title cannot be empty"));
        }
        if title != next.title {
            changes.push(FieldChange {
                field: "title".into(),
                from: json!(next.title),
                to: json!(title),
            });
            next.title = title;
        }
    }
    if let Some(description) = patch.description {
        if description != next.description {
            changes.push(FieldChange {
                field: "description".into(),
                from: json!(next.description),
                to: json!(description),
            });
            // Only handles new to the description count as fresh mentions.
            let before = mentions::extract_handles(&next.description);
            mention_handles = mentions::extract_handles(&description)
                .into_iter()
                .filter(|h| !before.contains(h))
                .collect();
            next.description = description;
        }
    }
    if let Some(priority) = patch.priority {
        if priority != next.priority {
            changes.push(FieldChange {
                field: "priority".into(),
                from: json!(next.priority),
                to: json!(priority),
            });
            next.priority = priority;
        }
    }
    if let Some(labels) = patch.labels {
        let labels = normalise_labels(labels);
        if labels != next.labels {
            changes.push(FieldChange {
                field: "labels".into(),
                from: json!(next.labels),
                to: json!(labels),
            });
            next.labels = labels;
        }
    }

    if changes.is_empty() {
        return Ok(Outcome::new(next, Vec::new()));
    }
    bump(&mut next, now);
    let events = vec![EventKind::TicketUpdated {
        ticket_id: next.id,
        changes,
    }];
    Ok(Outcome {
        value: next,
        events,
        mention_handles,
    })
}

pub fn move_ticket(
    ticket: &Ticket,
    to: ColumnId,
    new_position: i64,
    expected_version: i64,
    actor: &Actor,
    now: Timestamp,
) -> Result<Outcome<Ticket>, CoreError> {
    check_mutable(ticket, actor, expected_version)?;
    if ticket.column_id == to {
        return Ok(Outcome::new(ticket.clone(), Vec::new()));
    }
    let mut next = ticket.clone();
    let from = next.column_id;
    next.column_id = to;
    next.position = new_position;
    bump(&mut next, now);
    let events = vec![EventKind::TicketMoved {
        ticket_id: next.id,
        from,
        to,
    }];
    Ok(Outcome::new(next, events))
}

pub fn assign_ticket(
    ticket: &Ticket,
    to: Option<ActorId>,
    expected_version: i64,
    actor: &Actor,
    now: Timestamp,
) -> Result<Outcome<Ticket>, CoreError> {
    check_mutable(ticket, actor, expected_version)?;
    if ticket.assignee_id == to {
        return Ok(Outcome::new(ticket.clone(), Vec::new()));
    }
    let mut next = ticket.clone();
    let from = next.assignee_id;
    next.assignee_id = to;
    bump(&mut next, now);
    let events = vec![EventKind::TicketAssigned {
        ticket_id: next.id,
        from,
        to,
    }];
    Ok(Outcome::new(next, events))
}

/// Comment on a ticket, or reply when `parent` is given. Comments do not bump
/// the ticket version; they are additive and never conflict.
pub fn comment(
    ticket: &Ticket,
    body: String,
    parent: Option<&Comment>,
    author: &Actor,
    now: Timestamp,
) -> Result<Outcome<Comment>, CoreError> {
    check_not_frozen_for_agent(ticket, author)?;
    let body = body.trim().to_string();
    if body.is_empty() {
        return Err(CoreError::validation("comment cannot be empty"));
    }
    if let Some(p) = parent {
        if p.ticket_id != ticket.id {
            return Err(CoreError::validation(
                "parent comment belongs to another ticket",
            ));
        }
    }
    let comment = Comment {
        id: CommentId::new(),
        ticket_id: ticket.id,
        author_id: author.id,
        body,
        parent_comment_id: parent.map(|p| p.id),
        created_at: now,
    };
    let events = vec![match parent {
        Some(p) => EventKind::TicketReplied {
            ticket_id: ticket.id,
            comment_id: comment.id,
            parent_comment_id: p.id,
            parent_author_id: p.author_id,
        },
        None => EventKind::TicketCommented {
            ticket_id: ticket.id,
            comment_id: comment.id,
        },
    }];
    let mention_handles = mentions::extract_handles(&comment.body);
    Ok(Outcome {
        value: comment,
        events,
        mention_handles,
    })
}

/// Humans and the system may freeze. Agents may not.
pub fn freeze_ticket(
    ticket: &Ticket,
    reason: String,
    actor: &Actor,
    now: Timestamp,
) -> Result<Outcome<Ticket>, CoreError> {
    if actor.is_agent() {
        return Err(CoreError::NotPermitted(
            "agents cannot freeze tickets".into(),
        ));
    }
    let reason = reason.trim().to_string();
    if reason.is_empty() {
        return Err(CoreError::validation("freeze reason cannot be empty"));
    }
    if ticket.frozen {
        return Ok(Outcome::new(ticket.clone(), Vec::new()));
    }
    let mut next = ticket.clone();
    next.frozen = true;
    next.freeze_reason = Some(reason.clone());
    bump(&mut next, now);
    let events = vec![EventKind::TicketFrozen {
        ticket_id: next.id,
        reason,
    }];
    Ok(Outcome::new(next, events))
}

/// Only the human may unfreeze.
pub fn unfreeze_ticket(
    ticket: &Ticket,
    actor: &Actor,
    now: Timestamp,
) -> Result<Outcome<Ticket>, CoreError> {
    if !actor.is_human() {
        return Err(CoreError::NotPermitted(
            "only the human can unfreeze a ticket".into(),
        ));
    }
    if !ticket.frozen {
        return Ok(Outcome::new(ticket.clone(), Vec::new()));
    }
    let mut next = ticket.clone();
    next.frozen = false;
    next.freeze_reason = None;
    bump(&mut next, now);
    let events = vec![EventKind::TicketUnfrozen { ticket_id: next.id }];
    Ok(Outcome::new(next, events))
}

/// Build the mention events for resolved handles. The store resolves handles
/// to actors because only it knows the project's roster.
pub fn mention_events(source: MentionSource, mentioned: &[ActorId]) -> Vec<EventKind> {
    mentioned
        .iter()
        .map(|id| EventKind::MentionCreated {
            source,
            mentioned_id: *id,
        })
        .collect()
}

fn check_mutable(ticket: &Ticket, actor: &Actor, expected_version: i64) -> Result<(), CoreError> {
    check_not_frozen_for_agent(ticket, actor)?;
    if ticket.version != expected_version {
        return Err(CoreError::VersionConflict {
            expected: expected_version,
            actual: ticket.version,
        });
    }
    Ok(())
}

fn check_not_frozen_for_agent(ticket: &Ticket, actor: &Actor) -> Result<(), CoreError> {
    if ticket.frozen && actor.is_agent() {
        return Err(CoreError::TicketFrozen {
            key: ticket.key.clone(),
            reason: ticket.freeze_reason.clone().unwrap_or_default(),
        });
    }
    Ok(())
}

fn bump(ticket: &mut Ticket, now: Timestamp) {
    ticket.version += 1;
    ticket.updated_at = now;
}

fn normalise_labels(labels: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in labels {
        let l = l.trim().to_lowercase();
        if !l.is_empty() && !out.contains(&l) {
            out.push(l);
        }
    }
    out
}

// Chats and threads

/// Members may act in a chat; the human may act in every chat.
fn chat_access(members: &[ActorId], actor: &Actor) -> Result<(), CoreError> {
    if actor.is_human() || members.contains(&actor.id) {
        Ok(())
    } else {
        Err(CoreError::NotPermitted(format!(
            "@{} is not a member of this chat",
            actor.handle
        )))
    }
}

pub fn create_direct_chat(
    a: &Actor,
    b: &Actor,
    by: &Actor,
    now: Timestamp,
) -> Result<Outcome<Chat>, CoreError> {
    if a.id == b.id {
        return Err(CoreError::validation(
            "a direct chat needs two different members",
        ));
    }
    if a.id == ActorId::SYSTEM || b.id == ActorId::SYSTEM {
        return Err(CoreError::validation("the system actor cannot chat"));
    }
    if !by.is_human() && by.id != a.id && by.id != b.id {
        return Err(CoreError::NotPermitted(
            "only a member or the human can open a direct chat".into(),
        ));
    }
    let chat = Chat {
        id: ChatId::new(),
        kind: ChatKind::Direct,
        name: None,
        created_by: by.id,
        created_at: now,
    };
    let events = vec![EventKind::ChatCreated {
        chat_id: chat.id,
        kind: ChatKind::Direct,
        name: None,
        member_ids: vec![a.id, b.id],
    }];
    Ok(Outcome::new(chat, events))
}

/// A group chat with the given members. An agent creating one is always a
/// member; the human never is, being an observer of every chat.
pub fn create_group_chat(
    name: String,
    members: Vec<ActorId>,
    by: &Actor,
    now: Timestamp,
) -> Result<Outcome<(Chat, Vec<ActorId>)>, CoreError> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(CoreError::validation("a group chat needs a name"));
    }
    let mut ids: Vec<ActorId> = Vec::new();
    for m in members {
        if m != ActorId::SYSTEM && !ids.contains(&m) {
            ids.push(m);
        }
    }
    if by.is_agent() && !ids.contains(&by.id) {
        ids.push(by.id);
    }
    if ids.is_empty() {
        return Err(CoreError::validation(
            "a group chat needs at least one member",
        ));
    }
    let chat = Chat {
        id: ChatId::new(),
        kind: ChatKind::Group,
        name: Some(name.clone()),
        created_by: by.id,
        created_at: now,
    };
    let events = vec![EventKind::ChatCreated {
        chat_id: chat.id,
        kind: ChatKind::Group,
        name: Some(name),
        member_ids: ids.clone(),
    }];
    Ok(Outcome::new((chat, ids), events))
}

pub fn add_chat_member(
    chat: &Chat,
    members: &[ActorId],
    member: &Actor,
    by: &Actor,
    _now: Timestamp,
) -> Result<Outcome<()>, CoreError> {
    if chat.kind != ChatKind::Group {
        return Err(CoreError::validation(
            "a direct chat has exactly two members",
        ));
    }
    chat_access(members, by)?;
    if member.id == ActorId::SYSTEM {
        return Err(CoreError::validation("the system actor cannot chat"));
    }
    if members.contains(&member.id) {
        return Err(CoreError::validation(format!(
            "@{} is already a member",
            member.handle
        )));
    }
    Ok(Outcome::new(
        (),
        vec![EventKind::ChatMemberAdded {
            chat_id: chat.id,
            member_id: member.id,
        }],
    ))
}

/// A thread always starts with a message; there is no posting outside one.
pub fn create_thread(
    chat: &Chat,
    members: &[ActorId],
    title: String,
    body: String,
    author: &Actor,
    now: Timestamp,
) -> Result<Outcome<(Thread, Message)>, CoreError> {
    chat_access(members, author)?;
    let title = title.trim().to_string();
    if title.is_empty() {
        return Err(CoreError::validation("a thread needs a title"));
    }
    let body = body.trim().to_string();
    if body.is_empty() {
        return Err(CoreError::validation("the first message cannot be empty"));
    }
    let thread = Thread {
        id: ThreadId::new(),
        chat_id: chat.id,
        title,
        created_by: author.id,
        created_at: now,
        last_activity_at: now,
        paused: false,
        archived: false,
    };
    let message = Message {
        id: MessageId::new(),
        thread_id: thread.id,
        author_id: author.id,
        body,
        created_at: now,
    };
    let mention_handles = mentions::extract_handles(&message.body);
    let events = vec![
        EventKind::ThreadCreated {
            chat_id: chat.id,
            thread_id: thread.id,
            title: thread.title.clone(),
        },
        EventKind::ThreadMessagePosted {
            chat_id: chat.id,
            thread_id: thread.id,
            message_id: message.id,
            title: thread.title.clone(),
        },
    ];
    Ok(Outcome {
        value: (thread, message),
        events,
        mention_handles,
    })
}

/// Agents cannot post in a paused thread; the human can. Posting in an
/// archived thread reopens it, which the store applies.
pub fn post_message(
    chat: &Chat,
    members: &[ActorId],
    thread: &Thread,
    body: String,
    author: &Actor,
    now: Timestamp,
) -> Result<Outcome<Message>, CoreError> {
    chat_access(members, author)?;
    if thread.chat_id != chat.id {
        return Err(CoreError::validation("thread belongs to another chat"));
    }
    if thread.paused && author.is_agent() {
        return Err(CoreError::NotPermitted(
            "this thread is paused; only the human can post in it or unpause it".into(),
        ));
    }
    let body = body.trim().to_string();
    if body.is_empty() {
        return Err(CoreError::validation("message cannot be empty"));
    }
    let message = Message {
        id: MessageId::new(),
        thread_id: thread.id,
        author_id: author.id,
        body,
        created_at: now,
    };
    let mention_handles = mentions::extract_handles(&message.body);
    let events = vec![EventKind::ThreadMessagePosted {
        chat_id: chat.id,
        thread_id: thread.id,
        message_id: message.id,
        title: thread.title.clone(),
    }];
    Ok(Outcome {
        value: message,
        events,
        mention_handles,
    })
}

/// Humans and the system pause and unpause threads. Agents may not.
pub fn set_thread_paused(
    thread: &Thread,
    paused: bool,
    actor: &Actor,
    _now: Timestamp,
) -> Result<Outcome<Thread>, CoreError> {
    if actor.is_agent() {
        return Err(CoreError::NotPermitted(
            "agents cannot pause or unpause threads".into(),
        ));
    }
    if thread.paused == paused {
        return Err(CoreError::validation(if paused {
            "thread is already paused"
        } else {
            "thread is not paused"
        }));
    }
    let mut updated = thread.clone();
    updated.paused = paused;
    let kind = if paused {
        EventKind::ThreadPaused {
            chat_id: thread.chat_id,
            thread_id: thread.id,
            title: thread.title.clone(),
        }
    } else {
        EventKind::ThreadUnpaused {
            chat_id: thread.chat_id,
            thread_id: thread.id,
            title: thread.title.clone(),
        }
    };
    Ok(Outcome::new(updated, vec![kind]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::ActorKind;
    use chrono::Utc;

    fn human() -> Actor {
        Actor::human("me", "Me")
    }

    fn agent() -> Actor {
        Actor {
            id: ActorId::new(),
            kind: ActorKind::Agent,
            handle: "dba".into(),
            display_name: "DBA".into(),
        }
    }

    fn ticket() -> Ticket {
        let h = human();
        create_ticket(
            NewTicket {
                title: "Fix login".into(),
                column_id: ColumnId::new(),
                ..Default::default()
            },
            "AK-1".into(),
            0,
            &h,
            Utc::now(),
        )
        .unwrap()
        .value
    }

    #[test]
    fn create_requires_title() {
        let err = create_ticket(
            NewTicket {
                title: "   ".into(),
                ..Default::default()
            },
            "AK-1".into(),
            0,
            &human(),
            Utc::now(),
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[test]
    fn create_extracts_mentions_from_description() {
        let out = create_ticket(
            NewTicket {
                title: "Schema change".into(),
                description: "needs @dba and @backend".into(),
                ..Default::default()
            },
            "AK-2".into(),
            0,
            &human(),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(out.mention_handles, vec!["dba", "backend"]);
    }

    #[test]
    fn version_mismatch_is_a_conflict() {
        let t = ticket();
        let err = move_ticket(&t, ColumnId::new(), 0, 99, &human(), Utc::now()).unwrap_err();
        assert_eq!(
            err,
            CoreError::VersionConflict {
                expected: 99,
                actual: 1
            }
        );
    }

    #[test]
    fn move_bumps_version_and_emits_event() {
        let t = ticket();
        let to = ColumnId::new();
        let out = move_ticket(&t, to, 5, t.version, &human(), Utc::now()).unwrap();
        assert_eq!(out.value.version, 2);
        assert_eq!(out.value.column_id, to);
        assert_eq!(out.value.position, 5);
        assert!(matches!(out.events[0], EventKind::TicketMoved { .. }));
    }

    #[test]
    fn moving_to_same_column_is_a_no_op() {
        let t = ticket();
        let out = move_ticket(&t, t.column_id, 0, t.version, &human(), Utc::now()).unwrap();
        assert!(out.events.is_empty());
        assert_eq!(out.value.version, t.version);
    }

    #[test]
    fn frozen_rejects_agents_but_not_humans() {
        let t = ticket();
        let frozen = freeze_ticket(&t, "ping-pong".into(), &human(), Utc::now())
            .unwrap()
            .value;
        assert!(frozen.frozen);

        let err = comment(&frozen, "hi".into(), None, &agent(), Utc::now()).unwrap_err();
        assert!(matches!(err, CoreError::TicketFrozen { .. }));
        assert!(comment(&frozen, "hi".into(), None, &human(), Utc::now()).is_ok());

        let err = assign_ticket(&frozen, None, frozen.version, &agent(), Utc::now()).unwrap_err();
        assert!(matches!(err, CoreError::TicketFrozen { .. }));
    }

    #[test]
    fn only_human_unfreezes_and_agents_cannot_freeze() {
        let t = ticket();
        assert!(matches!(
            freeze_ticket(&t, "x".into(), &agent(), Utc::now()).unwrap_err(),
            CoreError::NotPermitted(_)
        ));
        let frozen = freeze_ticket(&t, "x".into(), &Actor::system(), Utc::now())
            .unwrap()
            .value;
        assert!(matches!(
            unfreeze_ticket(&frozen, &Actor::system(), Utc::now()).unwrap_err(),
            CoreError::NotPermitted(_)
        ));
        let out = unfreeze_ticket(&frozen, &human(), Utc::now()).unwrap();
        assert!(!out.value.frozen);
        assert!(matches!(out.events[0], EventKind::TicketUnfrozen { .. }));
    }

    #[test]
    fn update_records_only_real_changes_and_new_mentions() {
        let t = ticket();
        let out = update_ticket(
            &t,
            TicketPatch {
                title: Some(t.title.clone()),
                description: Some("cc @dba".into()),
                priority: Some(Priority::High),
                labels: Some(vec!["Bug".into(), "bug".into(), " ".into()]),
            },
            t.version,
            &human(),
            Utc::now(),
        )
        .unwrap();
        match &out.events[0] {
            EventKind::TicketUpdated { changes, .. } => {
                let fields: Vec<&str> = changes.iter().map(|c| c.field.as_str()).collect();
                assert_eq!(fields, vec!["description", "priority", "labels"]);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(out.value.labels, vec!["bug"]);
        assert_eq!(out.mention_handles, vec!["dba"]);

        // Editing the description again without new handles yields no mentions.
        let again = update_ticket(
            &out.value,
            TicketPatch {
                description: Some("cc @dba, more detail".into()),
                ..Default::default()
            },
            out.value.version,
            &human(),
            Utc::now(),
        )
        .unwrap();
        assert!(again.mention_handles.is_empty());
    }

    #[test]
    fn reply_targets_parent_author() {
        let t = ticket();
        let a = agent();
        let first = comment(&t, "question?".into(), None, &a, Utc::now())
            .unwrap()
            .value;
        let out = comment(&t, "answer".into(), Some(&first), &human(), Utc::now()).unwrap();
        match out.events[0] {
            EventKind::TicketReplied {
                parent_author_id,
                parent_comment_id,
                ..
            } => {
                assert_eq!(parent_author_id, a.id);
                assert_eq!(parent_comment_id, first.id);
            }
            _ => panic!("expected reply event"),
        }
    }
}
