//! Notification routing: which actors learn about an event, and whether it
//! should wake them. The inbox is deliberately not a queue; the router only
//! decides recipients and the wake flag.

use serde::{Deserialize, Serialize};

use crate::event::{Event, EventKind};
use crate::ids::{ActorId, EventId, NotificationId, RunId};
use crate::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// Informational: something happened on a ticket you watch.
    Activity,
    Moved,
    Commented,
    Replied,
    Assigned,
    Mentioned,
    Unfrozen,
    /// Another actor asked the scheduler to wake you.
    Activated,
}

impl NotificationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            NotificationKind::Activity => "activity",
            NotificationKind::Moved => "moved",
            NotificationKind::Commented => "commented",
            NotificationKind::Replied => "replied",
            NotificationKind::Assigned => "assigned",
            NotificationKind::Mentioned => "mentioned",
            NotificationKind::Unfrozen => "unfrozen",
            NotificationKind::Activated => "activated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "activity" => Some(NotificationKind::Activity),
            "moved" => Some(NotificationKind::Moved),
            "commented" => Some(NotificationKind::Commented),
            "replied" => Some(NotificationKind::Replied),
            "assigned" => Some(NotificationKind::Assigned),
            "mentioned" => Some(NotificationKind::Mentioned),
            "unfrozen" => Some(NotificationKind::Unfrozen),
            "activated" => Some(NotificationKind::Activated),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationState {
    Pending,
    /// Created while the subject ticket was frozen. Released to Pending on unfreeze.
    Held,
    Seen,
    Handled,
    Dismissed,
}

impl NotificationState {
    pub fn as_str(&self) -> &'static str {
        match self {
            NotificationState::Pending => "pending",
            NotificationState::Held => "held",
            NotificationState::Seen => "seen",
            NotificationState::Handled => "handled",
            NotificationState::Dismissed => "dismissed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(NotificationState::Pending),
            "held" => Some(NotificationState::Held),
            "seen" => Some(NotificationState::Seen),
            "handled" => Some(NotificationState::Handled),
            "dismissed" => Some(NotificationState::Dismissed),
            _ => None,
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self, NotificationState::Pending | NotificationState::Seen)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    pub recipient_id: ActorId,
    pub event_id: EventId,
    pub kind: NotificationKind,
    pub wake: bool,
    pub state: NotificationState,
    pub created_at: Timestamp,
    pub handled_at: Option<Timestamp>,
    pub handled_in_run_id: Option<RunId>,
}

/// What the router produces. The store turns drafts into rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationDraft {
    pub recipient_id: ActorId,
    pub kind: NotificationKind,
    pub wake: bool,
    pub state: NotificationState,
}

/// Everything the router needs to know about the event's subject.
#[derive(Debug, Clone, Default)]
pub struct RoutingContext {
    pub assignee_id: Option<ActorId>,
    pub reporter_id: Option<ActorId>,
    /// Watchers other than reporter and assignee; the router unions them.
    pub watchers: Vec<ActorId>,
    /// Whether the subject ticket is frozen after this event was applied.
    pub frozen: bool,
}

/// Decide recipients for an event. Never notifies the actor who caused it,
/// collapses to one draft per recipient, and holds notifications on frozen
/// tickets except for the unfreeze itself.
pub fn route(event: &Event, ctx: &RoutingContext) -> Vec<NotificationDraft> {
    let actor = event.actor_id;
    let mut drafts: Vec<NotificationDraft> = Vec::new();

    let mut push = |recipient: ActorId, kind: NotificationKind, wake: bool| {
        if recipient == actor || recipient == ActorId::SYSTEM {
            return;
        }
        if let Some(existing) = drafts.iter_mut().find(|d| d.recipient_id == recipient) {
            existing.wake |= wake;
            if kind > existing.kind {
                existing.kind = kind;
            }
        } else {
            drafts.push(NotificationDraft {
                recipient_id: recipient,
                kind,
                wake,
                state: NotificationState::Pending,
            });
        }
    };

    let audience = |ctx: &RoutingContext| -> Vec<ActorId> {
        let mut all: Vec<ActorId> = Vec::new();
        if let Some(a) = ctx.assignee_id {
            all.push(a);
        }
        if let Some(r) = ctx.reporter_id {
            if !all.contains(&r) {
                all.push(r);
            }
        }
        for w in &ctx.watchers {
            if !all.contains(w) {
                all.push(*w);
            }
        }
        all
    };

    match &event.kind {
        EventKind::TicketCreated {
            assignee_id: Some(assignee),
            ..
        } => {
            push(*assignee, NotificationKind::Assigned, true);
        }
        EventKind::TicketCreated { .. } => {}
        EventKind::TicketAssigned { to: Some(to), .. } => {
            push(*to, NotificationKind::Assigned, true);
            for a in audience(ctx) {
                if a != *to {
                    push(a, NotificationKind::Activity, false);
                }
            }
        }
        EventKind::TicketAssigned { to: None, from, .. } => {
            if let Some(from) = from {
                push(*from, NotificationKind::Activity, false);
            }
            for a in audience(ctx) {
                push(a, NotificationKind::Activity, false);
            }
        }
        EventKind::TicketMoved { .. } => {
            for a in audience(ctx) {
                let is_assignee = ctx.assignee_id == Some(a);
                push(a, NotificationKind::Moved, is_assignee);
            }
        }
        EventKind::TicketUpdated { .. } => {
            for a in audience(ctx) {
                push(a, NotificationKind::Activity, false);
            }
        }
        EventKind::TicketCommented { .. } => {
            for a in audience(ctx) {
                let is_assignee = ctx.assignee_id == Some(a);
                push(a, NotificationKind::Commented, is_assignee);
            }
        }
        EventKind::TicketReplied {
            parent_author_id, ..
        } => {
            push(*parent_author_id, NotificationKind::Replied, true);
            for a in audience(ctx) {
                let is_assignee = ctx.assignee_id == Some(a);
                push(a, NotificationKind::Commented, is_assignee);
            }
        }
        EventKind::TicketFrozen { .. } => {
            for a in audience(ctx) {
                push(a, NotificationKind::Activity, false);
            }
        }
        EventKind::TicketUnfrozen { .. } => {
            if let Some(a) = ctx.assignee_id {
                push(a, NotificationKind::Unfrozen, true);
            }
        }
        EventKind::MentionCreated { mentioned_id, .. } => {
            push(*mentioned_id, NotificationKind::Mentioned, true);
        }
    }

    let hold = ctx.frozen && !matches!(event.kind, EventKind::TicketUnfrozen { .. });
    if hold {
        for d in &mut drafts {
            d.state = NotificationState::Held;
        }
    }
    drafts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ColumnId, CommentId, TicketId};
    use chrono::Utc;

    fn ev(actor: ActorId, kind: EventKind) -> Event {
        Event::new(actor, Utc::now(), kind)
    }

    #[test]
    fn assignment_wakes_the_assignee_only() {
        let human = ActorId::new();
        let agent = ActorId::new();
        let watcher = ActorId::new();
        let e = ev(
            human,
            EventKind::TicketAssigned {
                ticket_id: TicketId::new(),
                from: None,
                to: Some(agent),
            },
        );
        let ctx = RoutingContext {
            assignee_id: Some(agent),
            reporter_id: Some(human),
            watchers: vec![watcher],
            frozen: false,
        };
        let drafts = route(&e, &ctx);
        let to_agent = drafts.iter().find(|d| d.recipient_id == agent).unwrap();
        assert_eq!(to_agent.kind, NotificationKind::Assigned);
        assert!(to_agent.wake);
        let to_watcher = drafts.iter().find(|d| d.recipient_id == watcher).unwrap();
        assert!(!to_watcher.wake);
        assert!(
            drafts.iter().all(|d| d.recipient_id != human),
            "actor is never notified"
        );
    }

    #[test]
    fn reply_wakes_parent_author_and_collapses_per_recipient() {
        let author = ActorId::new();
        let parent_author = ActorId::new();
        let e = ev(
            author,
            EventKind::TicketReplied {
                ticket_id: TicketId::new(),
                comment_id: CommentId::new(),
                parent_comment_id: CommentId::new(),
                parent_author_id: parent_author,
            },
        );
        // parent author is also a watcher: must get exactly one draft, with wake.
        let ctx = RoutingContext {
            assignee_id: None,
            reporter_id: None,
            watchers: vec![parent_author],
            frozen: false,
        };
        let drafts = route(&e, &ctx);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].kind, NotificationKind::Replied);
        assert!(drafts[0].wake);
    }

    #[test]
    fn frozen_tickets_hold_everything_but_unfreeze() {
        let human = ActorId::new();
        let agent = ActorId::new();
        let ctx = RoutingContext {
            assignee_id: Some(agent),
            reporter_id: None,
            watchers: vec![],
            frozen: true,
        };
        let moved = ev(
            human,
            EventKind::TicketMoved {
                ticket_id: TicketId::new(),
                from: ColumnId::new(),
                to: ColumnId::new(),
            },
        );
        assert!(route(&moved, &ctx)
            .iter()
            .all(|d| d.state == NotificationState::Held));

        let ctx_after = RoutingContext {
            frozen: false,
            ..ctx.clone()
        };
        let unfrozen = ev(
            human,
            EventKind::TicketUnfrozen {
                ticket_id: TicketId::new(),
            },
        );
        let drafts = route(&unfrozen, &ctx_after);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].state, NotificationState::Pending);
        assert!(drafts[0].wake);
    }

    #[test]
    fn system_actor_is_never_a_recipient() {
        let e = ev(
            ActorId::new(),
            EventKind::MentionCreated {
                source: crate::event::MentionSource::TicketDescription {
                    ticket_id: TicketId::new(),
                },
                mentioned_id: ActorId::SYSTEM,
            },
        );
        assert!(route(&e, &RoutingContext::default()).is_empty());
    }
}
