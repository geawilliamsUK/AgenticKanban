//! Notification routing: which actors learn about an event, and whether it
//! should wake them. The inbox is deliberately not a queue; the router only
//! decides recipients and the wake flag.

use serde::{Deserialize, Serialize};

use crate::chat::ChatKind;
use crate::event::{Event, EventKind};
use crate::ids::{ActorId, EventId, NotificationId, RunId};
use crate::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    /// Informational: something happened on a ticket or thread you follow.
    Activity,
    Moved,
    Commented,
    Replied,
    /// A message in a thread you are in, or a direct message.
    Messaged,
    Assigned,
    Mentioned,
    Unfrozen,
    /// A guardrail fired. Only the human receives these.
    Guardrail,
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
            NotificationKind::Messaged => "messaged",
            NotificationKind::Assigned => "assigned",
            NotificationKind::Mentioned => "mentioned",
            NotificationKind::Unfrozen => "unfrozen",
            NotificationKind::Guardrail => "guardrail",
            NotificationKind::Activated => "activated",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "activity" => Some(NotificationKind::Activity),
            "moved" => Some(NotificationKind::Moved),
            "commented" => Some(NotificationKind::Commented),
            "replied" => Some(NotificationKind::Replied),
            "messaged" => Some(NotificationKind::Messaged),
            "assigned" => Some(NotificationKind::Assigned),
            "mentioned" => Some(NotificationKind::Mentioned),
            "unfrozen" => Some(NotificationKind::Unfrozen),
            "guardrail" => Some(NotificationKind::Guardrail),
            "activated" => Some(NotificationKind::Activated),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationState {
    Pending,
    /// Created while the subject was frozen or paused. Released to Pending
    /// on unfreeze or unpause.
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

/// The chat an event happened in, for thread events.
#[derive(Debug, Clone, Default)]
pub struct ChatRouting {
    pub direct: bool,
    pub members: Vec<ActorId>,
    /// Everyone who has posted in the thread so far.
    pub participants: Vec<ActorId>,
    /// Informed of direct messages without being woken: the human, when
    /// not a member of the chat.
    pub observers: Vec<ActorId>,
}

/// Everything the router needs to know about the event's subject.
#[derive(Debug, Clone, Default)]
pub struct RoutingContext {
    pub assignee_id: Option<ActorId>,
    pub reporter_id: Option<ActorId>,
    /// Watchers other than reporter and assignee; the router unions them.
    pub watchers: Vec<ActorId>,
    /// Whether the subject is frozen (ticket) or paused (thread) after this
    /// event was applied.
    pub frozen: bool,
    pub chat: Option<ChatRouting>,
    /// A human's message in a thread wakes the agents in it.
    pub actor_is_human: bool,
    /// Who receives guardrail trips.
    pub human_id: Option<ActorId>,
}

/// Decide recipients for an event. Never notifies the actor who caused it,
/// collapses to one draft per recipient, and holds notifications on frozen
/// tickets and paused threads except for the unfreeze or unpause itself.
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
        // A direct chat is silent until a message arrives; a group is announced.
        EventKind::ChatCreated {
            kind: ChatKind::Group,
            member_ids,
            ..
        } => {
            for m in member_ids {
                push(*m, NotificationKind::Activity, false);
            }
        }
        EventKind::ChatCreated { .. } => {}
        EventKind::ChatMemberAdded { member_id, .. } => {
            push(*member_id, NotificationKind::Activity, false);
        }
        EventKind::ThreadCreated { .. } => {
            // The first message routes on its own; a new group thread is
            // announced to the members without waking them.
            if let Some(c) = &ctx.chat {
                if !c.direct {
                    for m in &c.members {
                        push(*m, NotificationKind::Activity, false);
                    }
                }
            }
        }
        EventKind::ThreadMessagePosted { .. } => {
            if let Some(c) = &ctx.chat {
                if c.direct {
                    for m in &c.members {
                        push(*m, NotificationKind::Messaged, true);
                    }
                    for o in &c.observers {
                        push(*o, NotificationKind::Messaged, false);
                    }
                } else {
                    for p in &c.participants {
                        push(*p, NotificationKind::Messaged, ctx.actor_is_human);
                    }
                }
            }
        }
        EventKind::ThreadPaused { .. } => {
            if let Some(c) = &ctx.chat {
                for p in &c.participants {
                    push(*p, NotificationKind::Activity, false);
                }
            }
        }
        EventKind::ThreadUnpaused { .. } => {
            if let Some(c) = &ctx.chat {
                for p in &c.participants {
                    push(*p, NotificationKind::Unfrozen, true);
                }
            }
        }
        EventKind::GuardrailTripped { .. } => {
            if let Some(h) = ctx.human_id {
                push(h, NotificationKind::Guardrail, false);
            }
        }
    }

    let hold = ctx.frozen
        && !matches!(
            event.kind,
            EventKind::TicketUnfrozen { .. } | EventKind::ThreadUnpaused { .. }
        );
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
    use crate::ids::{ChatId, ColumnId, CommentId, MessageId, ThreadId, TicketId};
    use chrono::Utc;

    fn ev(actor: ActorId, kind: EventKind) -> Event {
        Event::new(actor, Utc::now(), kind)
    }

    fn posted() -> EventKind {
        EventKind::ThreadMessagePosted {
            chat_id: ChatId::new(),
            thread_id: ThreadId::new(),
            message_id: MessageId::new(),
            title: "t".into(),
        }
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
            ..Default::default()
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
            watchers: vec![parent_author],
            ..Default::default()
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
            frozen: true,
            ..Default::default()
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

    #[test]
    fn direct_messages_wake_the_other_member_and_inform_the_observer() {
        let a = ActorId::new();
        let b = ActorId::new();
        let human = ActorId::new();
        let ctx = RoutingContext {
            chat: Some(ChatRouting {
                direct: true,
                members: vec![a, b],
                participants: vec![a],
                observers: vec![human],
            }),
            ..Default::default()
        };
        let drafts = route(&ev(a, posted()), &ctx);
        assert_eq!(drafts.len(), 2);
        let to_b = drafts.iter().find(|d| d.recipient_id == b).unwrap();
        assert_eq!(to_b.kind, NotificationKind::Messaged);
        assert!(to_b.wake);
        let to_human = drafts.iter().find(|d| d.recipient_id == human).unwrap();
        assert!(!to_human.wake);
    }

    #[test]
    fn group_posts_reach_participants_and_wake_only_for_the_human() {
        let a = ActorId::new();
        let b = ActorId::new();
        let c = ActorId::new();
        let human = ActorId::new();
        let ctx = RoutingContext {
            chat: Some(ChatRouting {
                direct: false,
                members: vec![a, b, c],
                participants: vec![a, b],
                observers: vec![],
            }),
            ..Default::default()
        };
        let drafts = route(&ev(a, posted()), &ctx);
        assert_eq!(drafts.len(), 1, "only the other participant");
        assert_eq!(drafts[0].recipient_id, b);
        assert!(!drafts[0].wake);

        let ctx_human = RoutingContext {
            actor_is_human: true,
            ..ctx.clone()
        };
        let drafts = route(&ev(human, posted()), &ctx_human);
        assert_eq!(drafts.len(), 2);
        assert!(drafts.iter().all(|d| d.wake), "the human's post wakes them");

        // A new group thread is announced to members without waking them.
        let created = ev(
            a,
            EventKind::ThreadCreated {
                chat_id: ChatId::new(),
                thread_id: ThreadId::new(),
                title: "t".into(),
            },
        );
        let drafts = route(&created, &ctx);
        assert_eq!(drafts.len(), 2);
        assert!(drafts.iter().all(|d| !d.wake));
    }

    #[test]
    fn paused_threads_hold_until_unpaused() {
        let a = ActorId::new();
        let b = ActorId::new();
        let human = ActorId::new();
        let chat = ChatRouting {
            direct: true,
            members: vec![a, b],
            participants: vec![a, b],
            observers: vec![human],
        };
        let ctx = RoutingContext {
            frozen: true,
            chat: Some(chat.clone()),
            actor_is_human: true,
            ..Default::default()
        };
        let drafts = route(&ev(human, posted()), &ctx);
        assert!(drafts.iter().all(|d| d.state == NotificationState::Held));

        let unpaused = ev(
            human,
            EventKind::ThreadUnpaused {
                chat_id: ChatId::new(),
                thread_id: ThreadId::new(),
                title: "t".into(),
            },
        );
        let ctx_after = RoutingContext {
            frozen: false,
            ..ctx.clone()
        };
        let drafts = route(&unpaused, &ctx_after);
        assert_eq!(drafts.len(), 2);
        assert!(drafts
            .iter()
            .all(|d| d.wake && d.kind == NotificationKind::Unfrozen));
    }
}
