//! Keeping the Desk told what the team does. One watcher per project
//! turns new mentions, messages and replies from agents into update cards
//! in the most recently active Desk session, batched so a burst is one card.
//! When the last run ends and the team stays quiet, it posts that the team
//! is idle, with the questions still waiting on the human. The Desk itself
//! is asked to speak only when something needs the human, so routine
//! updates cost no model call.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use ak_core::chat::ChatKind;
use ak_core::event::{EventKind, MentionSource};
use ak_core::ids::{ActorId, NotificationId, ThreadId, TicketId};
use ak_core::notification::needs_human;
use ak_core::{Actor, ActorKind, AgentState, Event, NotificationKind};
use ak_runtime::OpenProject;
use ak_store::Store;

use crate::desk::DeskHost;
use crate::supervisor::{RunNotice, Supervisor};

const EXCERPT_CHARS: usize = 400;

/// How long the watcher lets activity settle.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// A burst of team activity becomes one update after this.
    pub batch: Duration,
    /// The team counts as idle this long after the last run ends.
    pub idle_after: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            batch: Duration::from_secs(20),
            idle_after: Duration::from_secs(30),
        }
    }
}

/// The text behind an event that carries some, and whether it was a message
/// in a direct chat.
pub async fn body_of(store: &dyn Store, event: &Event) -> Option<(String, bool)> {
    let comment = |ticket: TicketId, id| async move {
        store
            .comments(ticket)
            .await
            .ok()?
            .into_iter()
            .find(|c| c.id == id)
            .map(|c| (c.body, false))
    };
    let message = |thread: ThreadId, id| async move {
        let body = store
            .messages(thread)
            .await
            .ok()?
            .into_iter()
            .find(|m| m.id == id)?
            .body;
        let chat = store.thread(thread).await.ok()?.chat_id;
        let direct = store.chat(chat).await.ok()?.kind == ChatKind::Direct;
        Some((body, direct))
    };
    match &event.kind {
        EventKind::MentionCreated { source, .. } => match source {
            MentionSource::Comment {
                ticket_id,
                comment_id,
            } => comment(*ticket_id, *comment_id).await,
            MentionSource::Message {
                thread_id,
                message_id,
            } => message(*thread_id, *message_id).await,
            MentionSource::TicketDescription { .. } => None,
        },
        EventKind::TicketCommented {
            ticket_id,
            comment_id,
        }
        | EventKind::TicketReplied {
            ticket_id,
            comment_id,
            ..
        } => comment(*ticket_id, *comment_id).await,
        EventKind::ThreadMessagePosted {
            thread_id,
            message_id,
            ..
        } => message(*thread_id, *message_id).await,
        _ => None,
    }
}

/// A comment or message cut to a length a card or prompt can carry.
pub fn excerpt(body: &str) -> String {
    let body = body.trim();
    if body.chars().count() > EXCERPT_CHARS {
        format!("{}…", body.chars().take(EXCERPT_CHARS).collect::<String>())
    } else {
        body.to_string()
    }
}

/// One thing the team did, as a Desk update card shows it.
async fn update_item(
    store: &dyn Store,
    human: &Actor,
    actors: &HashMap<ActorId, Actor>,
    event: &Event,
) -> Option<Value> {
    let who = actors.get(&event.actor_id)?;
    if who.kind != ActorKind::Agent {
        return None;
    }
    let (body, direct) = body_of(store, event).await?;
    let (subject, ticket, thread) = match (event.kind.ticket_id(), event.kind.thread_id()) {
        (Some(t), _) => {
            let ticket = store.ticket(t).await.ok()?;
            (format!("{} {}", ticket.key, ticket.title), Some(t), None)
        }
        (_, Some(th)) => {
            let thread = store.thread(th).await.ok()?;
            (format!("thread \"{}\"", thread.title), None, Some(th))
        }
        _ => (String::new(), None, None),
    };
    Some(json!({
        "who": who.handle,
        "text": excerpt(&body),
        "subject": subject,
        "ticket_id": ticket,
        "thread_id": thread,
        "needs_you": needs_human(&body, &human.handle, direct),
    }))
}

/// What the Desk is asked to do with an update it should speak to.
fn update_prompt(human: &str, items: &[Value], idle: bool) -> String {
    let mut out = String::from("[Team update from the app; not a message from the human]\n\n");
    if idle {
        out.push_str("The team has gone idle: no agent is running or scheduled.\n\n");
    }
    for i in items {
        let s = |k: &str| i[k].as_str().unwrap_or("").to_string();
        let tag = if i["needs_you"].as_bool() == Some(true) {
            "QUESTION for you"
        } else {
            "update"
        };
        out.push_str(&format!(
            "- {tag} from @{} on {}:\n{}\n\n",
            s("who"),
            s("subject"),
            s("text")
        ));
    }
    out.push_str(&format!(
        "Tell @{human} in a few lines what needs them and what they can ignore, most important \
first. For a question, say what is being asked and the options; offer to answer on their behalf \
only once they tell you what to say. Do not change the board unless they ask."
    ));
    out
}

/// Watches one project and keeps its most recent Desk session told what the
/// team is doing. Cards always; the Desk itself speaks only when something
/// needs the human, so quiet updates cost nothing.
///
/// What already reached the human is read before this returns, and the
/// event bus is joined before that, so nothing that happens after the call
/// is missed or taken for old.
pub async fn spawn(
    project: Arc<OpenProject>,
    desk: Arc<DeskHost>,
    supervisor: Arc<Supervisor>,
    timing: Timing,
) -> JoinHandle<()> {
    let mut events = project.store.subscribe();
    let mut runs = supervisor.subscribe();
    let human = project.human.clone();
    let mut seen: HashSet<NotificationId> = project
        .store()
        .notifications(human.id, false)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|n| n.id)
        .collect();
    tokio::spawn(async move {
        let store = project.store();
        let mut pending: Vec<Value> = Vec::new();
        let mut flush_at: Option<Instant> = None;
        let mut idle_at: Option<Instant> = None;
        let mut worked_since_idle = false;

        loop {
            tokio::select! {
                r = events.recv() => {
                    if matches!(r, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                        return;
                    }
                    let fresh: Vec<_> = store
                        .notifications(human.id, false)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|n| seen.insert(n.id))
                        .filter(|n| matches!(
                            n.kind,
                            NotificationKind::Mentioned | NotificationKind::Messaged | NotificationKind::Replied
                        ))
                        .collect();
                    if fresh.is_empty() {
                        continue;
                    }
                    let actors: HashMap<ActorId, Actor> = store
                        .actors()
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .map(|a| (a.id, a))
                        .collect();
                    let all = store.events_since(0, 5000).await.unwrap_or_default();
                    for n in fresh {
                        if let Some(e) = all.iter().find(|e| e.id == n.event_id) {
                            if let Some(item) = update_item(store.as_ref(), &human, &actors, e).await {
                                pending.push(item);
                            }
                        }
                    }
                    if !pending.is_empty() && flush_at.is_none() {
                        flush_at = Some(Instant::now() + timing.batch);
                    }
                }
                n = runs.recv() => match n {
                    Ok(RunNotice::Started { project: p, .. }) if p == project.id() => {
                        worked_since_idle = true;
                        idle_at = None;
                    }
                    Ok(RunNotice::Ended { project: p, .. }) if p == project.id() => {
                        idle_at = Some(Instant::now() + timing.idle_after);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    _ => {}
                },
                _ = wait(flush_at) => {
                    flush_at = None;
                    let items = std::mem::take(&mut pending);
                    let speak = items.iter().any(|i| i["needs_you"].as_bool() == Some(true));
                    deliver(&project, &desk, &human, items, false, speak).await;
                }
                _ = wait(idle_at) => {
                    idle_at = None;
                    if !worked_since_idle || !team_is_idle(store.as_ref()).await {
                        continue;
                    }
                    worked_since_idle = false;
                    let questions = open_questions(store.as_ref(), &human).await;
                    let speak = !questions.is_empty();
                    deliver(&project, &desk, &human, questions, true, speak).await;
                }
            }
        }
    })
}

async fn wait(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

async fn team_is_idle(store: &dyn Store) -> bool {
    store.active_runs().await.is_ok_and(|r| r.is_empty())
        && store.agents().await.is_ok_and(|agents| {
            !agents.iter().any(|a| {
                matches!(
                    a.state,
                    AgentState::Scheduled
                        | AgentState::Starting
                        | AgentState::Running
                        | AgentState::Stopping
                )
            })
        })
}

/// The questions still waiting on the human, as update items.
async fn open_questions(store: &dyn Store, human: &Actor) -> Vec<Value> {
    let actors: HashMap<ActorId, Actor> = store
        .actors()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    let events = store.events_since(0, 5000).await.unwrap_or_default();
    let mut out = Vec::new();
    // One comment can reach the human twice (as a mention and as a comment
    // on a ticket they follow); it is one question.
    let mut asked: HashSet<(String, String)> = HashSet::new();
    for n in store
        .notifications(human.id, true)
        .await
        .unwrap_or_default()
    {
        let Some(e) = events.iter().find(|e| e.id == n.event_id) else {
            continue;
        };
        if let Some(item) = update_item(store, human, &actors, e).await {
            let key = (item["who"].to_string(), item["text"].to_string());
            if item["needs_you"].as_bool() == Some(true) && asked.insert(key) {
                out.push(item);
            }
        }
    }
    out
}

/// Posts an update into the project's most recently active Desk session, if
/// there is one; the inbox has it either way.
async fn deliver(
    project: &Arc<OpenProject>,
    desk: &Arc<DeskHost>,
    human: &Actor,
    items: Vec<Value>,
    idle: bool,
    speak: bool,
) {
    if items.is_empty() && !idle {
        return;
    }
    let store = project.store();
    let Some(session) = store
        .desk_sessions()
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|s| !s.archived)
    else {
        return;
    };
    let prompt = speak.then(|| update_prompt(&human.handle, &items, idle));
    let update = json!({"items": items, "idle": idle});
    if let Err(e) = desk
        .deliver_update(project.clone(), session.id, update, prompt)
        .await
    {
        tracing::warn!("could not post a team update to the Desk: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_update_prompt_puts_questions_first_and_says_who_sent_it() {
        let items = vec![
            json!({"who": "lead", "subject": "ASS-6 Output", "text": "Merged.", "needs_you": false}),
            json!({"who": "lead", "subject": "ASS-9 mul", "text": "**Question for @me:** A or B?", "needs_you": true}),
        ];
        let p = update_prompt("me", &items, true);
        assert!(p.starts_with("[Team update from the app; not a message from the human]"));
        assert!(p.contains("gone idle"));
        assert!(p.contains("QUESTION for you from @lead on ASS-9 mul"));
        assert!(p.contains("update from @lead on ASS-6 Output"));
        assert!(p.contains("Do not change the board"));
    }
}
