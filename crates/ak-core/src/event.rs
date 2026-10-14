//! The append-only event log. Every mutation emits one or more events in the
//! same transaction. Events are the activity timeline, the audit trail and the
//! input to notification routing and guardrails.

use serde::{Deserialize, Serialize};

use crate::chat::ChatKind;
use crate::ids::{ActorId, ChatId, ColumnId, CommentId, EventId, MessageId, ThreadId, TicketId};
use crate::Timestamp;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    /// Monotonic within a project. Zero until persisted.
    pub seq: i64,
    pub occurred_at: Timestamp,
    pub actor_id: ActorId,
    pub kind: EventKind,
}

impl Event {
    pub fn new(actor_id: ActorId, occurred_at: Timestamp, kind: EventKind) -> Self {
        Self {
            id: EventId::new(),
            seq: 0,
            occurred_at,
            actor_id,
            kind,
        }
    }

    pub fn subject(&self) -> Subject {
        self.kind.subject()
    }
}

/// What an event is about, for indexing and for timelines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Subject {
    Ticket(TicketId),
    Thread(ThreadId),
    Chat(ChatId),
}

impl Subject {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Subject::Ticket(_) => "ticket",
            Subject::Thread(_) => "thread",
            Subject::Chat(_) => "chat",
        }
    }

    pub fn id_string(&self) -> String {
        match self {
            Subject::Ticket(id) => id.to_string(),
            Subject::Thread(id) => id.to_string(),
            Subject::Chat(id) => id.to_string(),
        }
    }
}

/// A single field change, recorded on `ticket.updated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldChange {
    pub field: String,
    pub from: serde_json::Value,
    pub to: serde_json::Value,
}

/// Where a mention was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum MentionSource {
    TicketDescription {
        ticket_id: TicketId,
    },
    Comment {
        ticket_id: TicketId,
        comment_id: CommentId,
    },
    Message {
        thread_id: ThreadId,
        message_id: MessageId,
    },
}

impl MentionSource {
    pub fn ticket_id(&self) -> Option<TicketId> {
        match self {
            MentionSource::TicketDescription { ticket_id } => Some(*ticket_id),
            MentionSource::Comment { ticket_id, .. } => Some(*ticket_id),
            MentionSource::Message { .. } => None,
        }
    }

    pub fn thread_id(&self) -> Option<ThreadId> {
        match self {
            MentionSource::Message { thread_id, .. } => Some(*thread_id),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    TicketCreated {
        ticket_id: TicketId,
        key: String,
        title: String,
        column_id: ColumnId,
        assignee_id: Option<ActorId>,
    },
    TicketUpdated {
        ticket_id: TicketId,
        changes: Vec<FieldChange>,
    },
    TicketMoved {
        ticket_id: TicketId,
        from: ColumnId,
        to: ColumnId,
    },
    TicketAssigned {
        ticket_id: TicketId,
        from: Option<ActorId>,
        to: Option<ActorId>,
    },
    TicketCommented {
        ticket_id: TicketId,
        comment_id: CommentId,
    },
    TicketReplied {
        ticket_id: TicketId,
        comment_id: CommentId,
        parent_comment_id: CommentId,
        parent_author_id: ActorId,
    },
    TicketFrozen {
        ticket_id: TicketId,
        reason: String,
    },
    TicketUnfrozen {
        ticket_id: TicketId,
    },
    MentionCreated {
        source: MentionSource,
        mentioned_id: ActorId,
    },
    ChatCreated {
        chat_id: ChatId,
        kind: ChatKind,
        name: Option<String>,
        member_ids: Vec<ActorId>,
    },
    ChatMemberAdded {
        chat_id: ChatId,
        member_id: ActorId,
    },
    ThreadCreated {
        chat_id: ChatId,
        thread_id: ThreadId,
        title: String,
    },
    /// Titles are repeated on thread events so timelines need no lookup;
    /// threads are never renamed.
    ThreadMessagePosted {
        chat_id: ChatId,
        thread_id: ThreadId,
        message_id: MessageId,
        title: String,
    },
    ThreadPaused {
        chat_id: ChatId,
        thread_id: ThreadId,
        title: String,
    },
    ThreadUnpaused {
        chat_id: ChatId,
        thread_id: ThreadId,
        title: String,
    },
}

impl EventKind {
    /// Dotted name used in storage and logs, e.g. `ticket.moved`.
    pub fn name(&self) -> &'static str {
        match self {
            EventKind::TicketCreated { .. } => "ticket.created",
            EventKind::TicketUpdated { .. } => "ticket.updated",
            EventKind::TicketMoved { .. } => "ticket.moved",
            EventKind::TicketAssigned { .. } => "ticket.assigned",
            EventKind::TicketCommented { .. } => "ticket.commented",
            EventKind::TicketReplied { .. } => "ticket.replied",
            EventKind::TicketFrozen { .. } => "ticket.frozen",
            EventKind::TicketUnfrozen { .. } => "ticket.unfrozen",
            EventKind::MentionCreated { .. } => "mention.created",
            EventKind::ChatCreated { .. } => "chat.created",
            EventKind::ChatMemberAdded { .. } => "chat.member_added",
            EventKind::ThreadCreated { .. } => "thread.created",
            EventKind::ThreadMessagePosted { .. } => "thread.message_posted",
            EventKind::ThreadPaused { .. } => "thread.paused",
            EventKind::ThreadUnpaused { .. } => "thread.unpaused",
        }
    }

    pub fn subject(&self) -> Subject {
        match self {
            EventKind::TicketCreated { ticket_id, .. }
            | EventKind::TicketUpdated { ticket_id, .. }
            | EventKind::TicketMoved { ticket_id, .. }
            | EventKind::TicketAssigned { ticket_id, .. }
            | EventKind::TicketCommented { ticket_id, .. }
            | EventKind::TicketReplied { ticket_id, .. }
            | EventKind::TicketFrozen { ticket_id, .. }
            | EventKind::TicketUnfrozen { ticket_id } => Subject::Ticket(*ticket_id),
            EventKind::MentionCreated { source, .. } => match source {
                MentionSource::Message { thread_id, .. } => Subject::Thread(*thread_id),
                other => Subject::Ticket(other.ticket_id().expect("ticket mention")),
            },
            EventKind::ChatCreated { chat_id, .. } | EventKind::ChatMemberAdded { chat_id, .. } => {
                Subject::Chat(*chat_id)
            }
            EventKind::ThreadCreated { thread_id, .. }
            | EventKind::ThreadMessagePosted { thread_id, .. }
            | EventKind::ThreadPaused { thread_id, .. }
            | EventKind::ThreadUnpaused { thread_id, .. } => Subject::Thread(*thread_id),
        }
    }

    pub fn ticket_id(&self) -> Option<TicketId> {
        match self.subject() {
            Subject::Ticket(id) => Some(id),
            _ => None,
        }
    }

    pub fn thread_id(&self) -> Option<ThreadId> {
        match self.subject() {
            Subject::Thread(id) => Some(id),
            _ => None,
        }
    }

    pub fn chat_id(&self) -> Option<ChatId> {
        match self {
            EventKind::ChatCreated { chat_id, .. }
            | EventKind::ChatMemberAdded { chat_id, .. }
            | EventKind::ThreadCreated { chat_id, .. }
            | EventKind::ThreadMessagePosted { chat_id, .. }
            | EventKind::ThreadPaused { chat_id, .. }
            | EventKind::ThreadUnpaused { chat_id, .. } => Some(*chat_id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kinds_round_trip_as_tagged_json() {
        let kind = EventKind::TicketMoved {
            ticket_id: TicketId::new(),
            from: ColumnId::new(),
            to: ColumnId::new(),
        };
        let json = serde_json::to_string(&kind).unwrap();
        assert!(json.contains("\"type\":\"ticket_moved\""));
        let back: EventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, back);
    }

    #[test]
    fn subjects_follow_the_kind() {
        let thread_id = ThreadId::new();
        let posted = EventKind::ThreadMessagePosted {
            chat_id: ChatId::new(),
            thread_id,
            message_id: MessageId::new(),
            title: "t".into(),
        };
        assert_eq!(posted.subject(), Subject::Thread(thread_id));
        assert_eq!(posted.thread_id(), Some(thread_id));
        assert_eq!(posted.ticket_id(), None);
        let mention = EventKind::MentionCreated {
            source: MentionSource::Message {
                thread_id,
                message_id: MessageId::new(),
            },
            mentioned_id: ActorId::new(),
        };
        assert_eq!(mention.subject().kind_str(), "thread");
    }
}
