//! Guardrail rules evaluated over a subject's recent events. Pure: the
//! monitor feeds events in and applies the action it gets back. Run-level
//! rules (churn, repeated tool calls, budgets, wake storms) live with the
//! scheduler and the bridge, which see the signals.

use std::collections::HashSet;

use chrono::Duration;
use serde::{Deserialize, Serialize};

use crate::config::GuardrailThresholds;
use crate::event::{Event, EventKind, Subject};
use crate::ids::{ActorId, ThreadId, TicketId};
use crate::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailRule {
    TicketPingPong,
    CommentStorm,
    MentionLoop,
    RunChurn,
    RepeatedToolCall,
    WakeStorm,
}

impl GuardrailRule {
    pub fn as_str(&self) -> &'static str {
        match self {
            GuardrailRule::TicketPingPong => "ticket_ping_pong",
            GuardrailRule::CommentStorm => "comment_storm",
            GuardrailRule::MentionLoop => "mention_loop",
            GuardrailRule::RunChurn => "run_churn",
            GuardrailRule::RepeatedToolCall => "repeated_tool_call",
            GuardrailRule::WakeStorm => "wake_storm",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            GuardrailRule::TicketPingPong => "ticket ping-pong",
            GuardrailRule::CommentStorm => "comment storm",
            GuardrailRule::MentionLoop => "mention loop",
            GuardrailRule::RunChurn => "run churn",
            GuardrailRule::RepeatedToolCall => "repeated tool call",
            GuardrailRule::WakeStorm => "wake storm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailAction {
    FreezeTicket,
    PauseThread,
    Cooldown,
    StopRun,
    Notify,
}

impl GuardrailAction {
    pub fn label(&self) -> &'static str {
        match self {
            GuardrailAction::FreezeTicket => "froze the ticket",
            GuardrailAction::PauseThread => "paused the thread",
            GuardrailAction::Cooldown => "cooled the agent down",
            GuardrailAction::StopRun => "stopped the run",
            GuardrailAction::Notify => "notified you",
        }
    }
}

/// A rule that fired on a subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trip {
    pub rule: GuardrailRule,
    pub action: GuardrailAction,
    pub subject: Subject,
    pub detail: String,
}

/// Checks one ticket's events, oldest first. `agents` tells agent-authored
/// events apart from the human's.
pub fn check_ticket(
    ticket: TicketId,
    events: &[Event],
    agents: &HashSet<ActorId>,
    t: &GuardrailThresholds,
    now: Timestamp,
) -> Option<Trip> {
    let window = Duration::minutes(t.ping_pong_window_minutes as i64);
    let moves = events
        .iter()
        .filter(|e| {
            now - e.occurred_at <= window
                && matches!(
                    e.kind,
                    EventKind::TicketMoved { .. } | EventKind::TicketAssigned { .. }
                )
        })
        .count() as u32;
    if t.ping_pong_moves > 0 && moves >= t.ping_pong_moves {
        return Some(Trip {
            rule: GuardrailRule::TicketPingPong,
            action: GuardrailAction::FreezeTicket,
            subject: Subject::Ticket(ticket),
            detail: format!(
                "{moves} moves or reassignments in {} minutes",
                t.ping_pong_window_minutes
            ),
        });
    }

    let window = Duration::minutes(t.comment_storm_window_minutes as i64);
    let comments = events
        .iter()
        .filter(|e| {
            agents.contains(&e.actor_id)
                && now - e.occurred_at <= window
                && matches!(
                    e.kind,
                    EventKind::TicketCommented { .. } | EventKind::TicketReplied { .. }
                )
        })
        .count() as u32;
    if t.comment_storm_comments > 0 && comments >= t.comment_storm_comments {
        return Some(Trip {
            rule: GuardrailRule::CommentStorm,
            action: GuardrailAction::FreezeTicket,
            subject: Subject::Ticket(ticket),
            detail: format!(
                "{comments} agent comments in {} minutes",
                t.comment_storm_window_minutes
            ),
        });
    }

    mention_loop(events, agents, t, now).map(|detail| Trip {
        rule: GuardrailRule::MentionLoop,
        action: GuardrailAction::FreezeTicket,
        subject: Subject::Ticket(ticket),
        detail,
    })
}

/// Checks one thread's events, oldest first.
pub fn check_thread(
    thread: ThreadId,
    events: &[Event],
    agents: &HashSet<ActorId>,
    t: &GuardrailThresholds,
    now: Timestamp,
) -> Option<Trip> {
    mention_loop(events, agents, t, now).map(|detail| Trip {
        rule: GuardrailRule::MentionLoop,
        action: GuardrailAction::PauseThread,
        subject: Subject::Thread(thread),
        detail,
    })
}

/// The trailing run of agent conversation (comments, posts, mentions) with
/// no progress and no human word in between. Returns the detail when the
/// agent-to-agent mentions in it reach the threshold inside the window.
fn mention_loop(
    events: &[Event],
    agents: &HashSet<ActorId>,
    t: &GuardrailThresholds,
    now: Timestamp,
) -> Option<String> {
    let window = Duration::minutes(t.mention_loop_window_minutes as i64);
    let mut mentions = 0u32;
    let mut participants: HashSet<ActorId> = HashSet::new();
    for e in events.iter().rev() {
        if now - e.occurred_at > window {
            break;
        }
        let by_agent = agents.contains(&e.actor_id);
        match &e.kind {
            EventKind::MentionCreated { mentioned_id, .. }
                if by_agent && agents.contains(mentioned_id) =>
            {
                mentions += 1;
                participants.insert(e.actor_id);
                participants.insert(*mentioned_id);
            }
            EventKind::TicketCommented { .. }
            | EventKind::TicketReplied { .. }
            | EventKind::ThreadMessagePosted { .. }
                if by_agent => {}
            // A human word, a mention involving the human, or progress on the
            // subject ends the loop.
            _ => break,
        }
    }
    if t.mention_loop_mentions > 0 && mentions >= t.mention_loop_mentions {
        Some(format!(
            "{mentions} mentions between {} agents with no progress in between",
            participants.len()
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::MentionSource;
    use crate::ids::{ChatId, ColumnId, CommentId, MessageId};
    use chrono::Utc;

    fn ev(actor: ActorId, minutes_ago: i64, kind: EventKind) -> Event {
        Event::new(actor, Utc::now() - Duration::minutes(minutes_ago), kind)
    }

    fn moved(ticket: TicketId, actor: ActorId, minutes_ago: i64) -> Event {
        ev(
            actor,
            minutes_ago,
            EventKind::TicketMoved {
                ticket_id: ticket,
                from: ColumnId::new(),
                to: ColumnId::new(),
            },
        )
    }

    fn commented(ticket: TicketId, actor: ActorId, minutes_ago: i64) -> Event {
        ev(
            actor,
            minutes_ago,
            EventKind::TicketCommented {
                ticket_id: ticket,
                comment_id: CommentId::new(),
            },
        )
    }

    fn mentioned(ticket: TicketId, actor: ActorId, target: ActorId, minutes_ago: i64) -> Event {
        ev(
            actor,
            minutes_ago,
            EventKind::MentionCreated {
                source: MentionSource::Comment {
                    ticket_id: ticket,
                    comment_id: CommentId::new(),
                },
                mentioned_id: target,
            },
        )
    }

    #[test]
    fn ping_pong_counts_moves_and_assignments_inside_the_window() {
        let t = GuardrailThresholds::default();
        let ticket = TicketId::new();
        let agent = ActorId::new();
        let agents: HashSet<ActorId> = [agent].into_iter().collect();
        let now = Utc::now();
        let recent: Vec<Event> = (0..6).map(|i| moved(ticket, agent, i)).collect();
        let trip = check_ticket(ticket, &recent, &agents, &t, now).unwrap();
        assert_eq!(trip.rule, GuardrailRule::TicketPingPong);
        assert_eq!(trip.action, GuardrailAction::FreezeTicket);
        assert_eq!(trip.subject, Subject::Ticket(ticket));
        assert!(check_ticket(ticket, &recent[..5], &agents, &t, now).is_none());
        let stale: Vec<Event> = (0..3)
            .map(|i| moved(ticket, agent, i))
            .chain((0..3).map(|i| moved(ticket, agent, 120 + i)))
            .collect();
        assert!(check_ticket(ticket, &stale, &agents, &t, now).is_none());
    }

    #[test]
    fn comment_storm_counts_agent_comments_only() {
        let t = GuardrailThresholds::default();
        let ticket = TicketId::new();
        let agent = ActorId::new();
        let human = ActorId::new();
        let agents: HashSet<ActorId> = [agent].into_iter().collect();
        let now = Utc::now();
        let by_agent: Vec<Event> = (0..15).map(|i| commented(ticket, agent, i)).collect();
        let trip = check_ticket(ticket, &by_agent, &agents, &t, now).unwrap();
        assert_eq!(trip.rule, GuardrailRule::CommentStorm);
        let by_human: Vec<Event> = (0..15).map(|i| commented(ticket, human, i)).collect();
        assert!(check_ticket(ticket, &by_human, &agents, &t, now).is_none());
    }

    #[test]
    fn mention_loop_needs_consecutive_agent_mentions_without_progress() {
        let t = GuardrailThresholds::default();
        let ticket = TicketId::new();
        let a = ActorId::new();
        let b = ActorId::new();
        let human = ActorId::new();
        let agents: HashSet<ActorId> = [a, b].into_iter().collect();
        let now = Utc::now();
        let mut events = Vec::new();
        for i in 0..8 {
            let (from, to) = if i % 2 == 0 { (a, b) } else { (b, a) };
            events.push(commented(ticket, from, 8 - i));
            events.push(mentioned(ticket, from, to, 8 - i));
        }
        let trip = check_ticket(ticket, &events, &agents, &t, now).unwrap();
        assert_eq!(trip.rule, GuardrailRule::MentionLoop);
        assert!(
            trip.detail.contains("8 mentions between 2 agents"),
            "{}",
            trip.detail
        );

        // Progress in the middle resets the count.
        let mut with_progress = events.clone();
        with_progress.insert(8, moved(ticket, a, 4));
        assert!(check_ticket(ticket, &with_progress, &agents, &t, now).is_none());

        // The human speaking resets it too.
        let mut with_human = events.clone();
        with_human.push(commented(ticket, human, 0));
        assert!(check_ticket(ticket, &with_human, &agents, &t, now).is_none());

        // Threads pause instead of freezing.
        let thread = ThreadId::new();
        let mut thread_events = Vec::new();
        for i in 0..8 {
            let (from, to) = if i % 2 == 0 { (a, b) } else { (b, a) };
            thread_events.push(ev(
                from,
                8 - i,
                EventKind::ThreadMessagePosted {
                    chat_id: ChatId::new(),
                    thread_id: thread,
                    message_id: MessageId::new(),
                    title: "t".into(),
                },
            ));
            thread_events.push(ev(
                from,
                8 - i,
                EventKind::MentionCreated {
                    source: MentionSource::Message {
                        thread_id: thread,
                        message_id: MessageId::new(),
                    },
                    mentioned_id: to,
                },
            ));
        }
        let trip = check_thread(thread, &thread_events, &agents, &t, now).unwrap();
        assert_eq!(trip.action, GuardrailAction::PauseThread);
        assert_eq!(trip.subject, Subject::Thread(thread));
    }
}
