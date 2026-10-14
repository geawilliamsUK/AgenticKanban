//! The append-only event log. Every mutation emits one or more events in the
//! same transaction. Events are the activity timeline, the audit trail and the
//! input to notification routing and guardrails.

use serde::{Deserialize, Serialize};

use crate::ids::{ActorId, ColumnId, CommentId, EventId, TicketId};
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
}

impl Subject {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Subject::Ticket(_) => "ticket",
        }
    }

    pub fn id_string(&self) -> String {
        match self {
            Subject::Ticket(id) => id.to_string(),
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
}

impl MentionSource {
    pub fn ticket_id(&self) -> TicketId {
        match self {
            MentionSource::TicketDescription { ticket_id } => *ticket_id,
            MentionSource::Comment { ticket_id, .. } => *ticket_id,
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
            EventKind::MentionCreated { source, .. } => Subject::Ticket(source.ticket_id()),
        }
    }

    pub fn ticket_id(&self) -> Option<TicketId> {
        match self.subject() {
            Subject::Ticket(id) => Some(id),
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
}
