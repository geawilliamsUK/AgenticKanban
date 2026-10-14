//! Read models the UI draws from. Built from store queries, refreshed on
//! events, never mutated by the UI directly.

use std::collections::{HashMap, HashSet};

use ak_core::agent::Agent;
use ak_core::board::{Column, Comment, Priority, Ticket};
use ak_core::event::{Event, EventKind, MentionSource};
use ak_core::ids::{ActorId, ColumnId, TicketId};
use ak_core::run::{AgentRun, RunEvent};
use ak_core::{Actor, ActorKind, Notification, NotificationKind};

#[derive(Default, Clone)]
pub struct BoardModel {
    pub columns: Vec<Column>,
    pub tickets: Vec<Ticket>,
    pub actors: Vec<Actor>,
    pub agents: Vec<Agent>,
    /// Open notifications per agent, for the Team view.
    pub agent_inbox_counts: HashMap<ActorId, usize>,
    tickets_by_id: HashMap<TicketId, usize>,
    actors_by_id: HashMap<ActorId, usize>,
    retired: HashSet<ActorId>,
}

impl BoardModel {
    pub fn new(
        columns: Vec<Column>,
        tickets: Vec<Ticket>,
        actors: Vec<Actor>,
        agents: Vec<Agent>,
        agent_inbox_counts: HashMap<ActorId, usize>,
    ) -> Self {
        let tickets_by_id = tickets.iter().enumerate().map(|(i, t)| (t.id, i)).collect();
        let actors_by_id = actors.iter().enumerate().map(|(i, a)| (a.id, i)).collect();
        let retired = agents
            .iter()
            .filter(|a| a.is_retired())
            .map(|a| a.actor_id)
            .collect();
        Self {
            columns,
            tickets,
            actors,
            agents,
            agent_inbox_counts,
            tickets_by_id,
            actors_by_id,
            retired,
        }
    }

    pub fn agent_by_handle(&self, handle: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.handle == handle)
    }

    pub fn active_agents(&self) -> impl Iterator<Item = &Agent> {
        self.agents.iter().filter(|a| !a.is_retired())
    }

    pub fn ticket(&self, id: TicketId) -> Option<&Ticket> {
        self.tickets_by_id.get(&id).map(|i| &self.tickets[*i])
    }

    pub fn actor(&self, id: ActorId) -> Option<&Actor> {
        self.actors_by_id.get(&id).map(|i| &self.actors[*i])
    }

    pub fn handle(&self, id: ActorId) -> String {
        match self.actor(id) {
            Some(a) => format!("@{}", a.handle),
            None => "@unknown".to_string(),
        }
    }

    pub fn column(&self, id: ColumnId) -> Option<&Column> {
        self.columns.iter().find(|c| c.id == id)
    }

    pub fn tickets_in(&self, column: ColumnId) -> impl Iterator<Item = &Ticket> {
        self.tickets.iter().filter(move |t| t.column_id == column)
    }

    /// Actors a ticket can be assigned to: everyone but the system actor and
    /// retired agents.
    pub fn assignable(&self) -> impl Iterator<Item = &Actor> {
        self.actors
            .iter()
            .filter(|a| a.kind != ActorKind::System && !self.retired.contains(&a.id))
    }
}

#[derive(Clone)]
pub struct TicketDetail {
    pub ticket: Ticket,
    pub comments: Vec<Comment>,
    pub events: Vec<Event>,
    pub watchers: Vec<ActorId>,
}

#[derive(Clone)]
pub struct InboxItem {
    pub notification: Notification,
    pub event: Option<Event>,
}

pub fn priority_color(p: Priority) -> egui::Color32 {
    match p {
        Priority::Low => egui::Color32::from_rgb(120, 130, 140),
        Priority::Medium => egui::Color32::from_rgb(70, 130, 220),
        Priority::High => egui::Color32::from_rgb(235, 150, 40),
        Priority::Urgent => egui::Color32::from_rgb(220, 60, 60),
    }
}

pub fn kind_label(k: NotificationKind) -> &'static str {
    match k {
        NotificationKind::Activity => "Activity",
        NotificationKind::Moved => "Moved",
        NotificationKind::Commented => "Comment",
        NotificationKind::Replied => "Reply",
        NotificationKind::Assigned => "Assigned",
        NotificationKind::Mentioned => "Mention",
        NotificationKind::Unfrozen => "Unfrozen",
        NotificationKind::Activated => "Activated",
    }
}

pub fn format_time(ts: &ak_core::Timestamp) -> String {
    ts.with_timezone(&chrono::Local)
        .format("%d %b %H:%M")
        .to_string()
}

/// One line describing an event, for timelines and the inbox.
pub fn describe_event(event: &Event, board: &BoardModel) -> String {
    let who = board.handle(event.actor_id);
    let key = |id: TicketId| {
        board
            .ticket(id)
            .map(|t| t.key.clone())
            .unwrap_or_else(|| "?".into())
    };
    let col = |id: ColumnId| {
        board
            .column(id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "?".into())
    };
    match &event.kind {
        EventKind::TicketCreated {
            ticket_id, title, ..
        } => {
            format!("{who} created {} \"{title}\"", key(*ticket_id))
        }
        EventKind::TicketUpdated { ticket_id, changes } => {
            let fields: Vec<&str> = changes.iter().map(|c| c.field.as_str()).collect();
            format!("{who} updated {} ({})", key(*ticket_id), fields.join(", "))
        }
        EventKind::TicketMoved {
            ticket_id,
            from,
            to,
        } => {
            format!(
                "{who} moved {} from {} to {}",
                key(*ticket_id),
                col(*from),
                col(*to)
            )
        }
        EventKind::TicketAssigned {
            ticket_id,
            to: Some(to),
            ..
        } => {
            format!(
                "{who} assigned {} to {}",
                key(*ticket_id),
                board.handle(*to)
            )
        }
        EventKind::TicketAssigned {
            ticket_id,
            to: None,
            ..
        } => {
            format!("{who} unassigned {}", key(*ticket_id))
        }
        EventKind::TicketCommented { ticket_id, .. } => {
            format!("{who} commented on {}", key(*ticket_id))
        }
        EventKind::TicketReplied {
            ticket_id,
            parent_author_id,
            ..
        } => {
            format!(
                "{who} replied to {} on {}",
                board.handle(*parent_author_id),
                key(*ticket_id)
            )
        }
        EventKind::TicketFrozen { ticket_id, reason } => {
            format!("{who} froze {}: {reason}", key(*ticket_id))
        }
        EventKind::TicketUnfrozen { ticket_id } => format!("{who} unfroze {}", key(*ticket_id)),
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
                board.handle(*mentioned_id),
                key(source.ticket_id())
            )
        }
    }
}

/// The run in progress, or the last one, for an agent: a bounded transcript
/// for the live view.
#[derive(Clone)]
pub struct LiveRun {
    pub run: AgentRun,
    pub events: std::collections::VecDeque<RunEvent>,
}

impl LiveRun {
    pub const CAP: usize = 300;

    pub fn new(run: AgentRun) -> Self {
        Self {
            run,
            events: std::collections::VecDeque::new(),
        }
    }

    pub fn push(&mut self, event: RunEvent) {
        if self.events.len() >= Self::CAP {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }
}
