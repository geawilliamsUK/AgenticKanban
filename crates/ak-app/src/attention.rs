//! What needs the human. The inbox is everything that reached them; this
//! picks out what waits on them (a question an agent asked, with
//! `ask_human` or in passing) and clears it once they answer where it was
//! asked. Telling the Desk what the team does is the supervisor's
//! `desk_updates`.

use std::collections::{HashMap, HashSet};

use ak_core::ids::{ActorId, NotificationId, ThreadId, TicketId};
use ak_core::notification::needs_human;
use ak_core::{Actor, ActorKind, Event};
use ak_store::Store;
use ak_supervisor::desk_updates::{body_of, excerpt};

use crate::model::InboxItem;

/// Fills in the text of the human's items from agents, and marks the open
/// ones that are questions for them.
pub async fn enrich(store: &dyn Store, human: &Actor, items: &mut [InboxItem]) {
    let agents: HashSet<ActorId> = store
        .actors()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.kind == ActorKind::Agent)
        .map(|a| a.id)
        .collect();
    // One comment can reach the human twice (as a mention and as a comment
    // on a ticket they follow); it is pinned once.
    let mut asked: HashSet<(ActorId, String)> = HashSet::new();
    for item in items.iter_mut() {
        let Some(event) = &item.event else { continue };
        if !agents.contains(&event.actor_id) {
            continue;
        }
        if let Some((body, direct)) = body_of(store, event).await {
            item.needs_you = item.notification.state.is_open()
                && needs_human(&body, &human.handle, direct)
                && asked.insert((event.actor_id, body.clone()));
            item.excerpt = Some(excerpt(&body));
        }
    }
}

/// The human answered on a ticket or in a thread: what reached them about
/// it is dealt with.
pub async fn answered(
    store: &dyn Store,
    human: ActorId,
    ticket: Option<TicketId>,
    thread: Option<ThreadId>,
) {
    let Ok(open) = store.notifications(human, true).await else {
        return;
    };
    if open.is_empty() {
        return;
    }
    let events: HashMap<_, Event> = store
        .events_since(0, 5000)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.id, e))
        .collect();
    let ids: Vec<NotificationId> = open
        .iter()
        .filter(|n| {
            events.get(&n.event_id).is_some_and(|e| {
                (ticket.is_some() && e.kind.ticket_id() == ticket)
                    || (thread.is_some() && e.kind.thread_id() == thread)
            })
        })
        .map(|n| n.id)
        .collect();
    if !ids.is_empty() {
        if let Err(e) = store.mark_notifications_handled(&ids, None).await {
            tracing::warn!("could not mark answered notifications: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ak_core::agent::{Agent, AgentDefinition};
    use ak_core::commands::NewTicket;
    use ak_core::config::ProjectPolicy;
    use ak_store::{HumanIdentity, SqliteStore};

    async fn inbox(store: &SqliteStore, human: &Actor) -> Vec<InboxItem> {
        let events: HashMap<_, Event> = store
            .events_since(0, 5000)
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.id, e))
            .collect();
        let mut items: Vec<InboxItem> = store
            .notifications(human.id, false)
            .await
            .unwrap()
            .into_iter()
            .map(|n| InboxItem {
                event: events.get(&n.event_id).cloned(),
                notification: n,
                excerpt: None,
                needs_you: false,
            })
            .collect();
        enrich(store, human, &mut items).await;
        items
    }

    #[tokio::test]
    async fn a_question_is_pinned_once_and_cleared_by_answering() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let human = store
            .bootstrap(&ProjectPolicy::new("Demo"), &HumanIdentity::default())
            .await
            .unwrap();
        let def = AgentDefinition::lead();
        let lead = Agent::from_definition(ActorId::new(), &def, "a".into(), "m".into());
        store.upsert_agent(&lead).await.unwrap();
        let columns = store.columns().await.unwrap();
        let ticket = store
            .create_ticket(
                NewTicket {
                    title: "Flags".into(),
                    column_id: columns[1].id,
                    ..Default::default()
                },
                human.id,
            )
            .await
            .unwrap();
        store
            .comment(ticket.id, "@me FYI: merged.".into(), None, lead.actor_id)
            .await
            .unwrap();
        store
            .comment(ticket.id, "@me A or B?".into(), None, lead.actor_id)
            .await
            .unwrap();

        let items = inbox(&store, &human).await;
        let pinned: Vec<&InboxItem> = items.iter().filter(|i| i.needs_you).collect();
        assert_eq!(pinned.len(), 1, "one question, though it arrived twice");
        assert_eq!(pinned[0].excerpt.as_deref(), Some("@me A or B?"));
        assert!(items
            .iter()
            .any(|i| i.excerpt.as_deref() == Some("@me FYI: merged.") && !i.needs_you));

        // Answering on the ticket deals with everything about it.
        store
            .comment(ticket.id, "B, please.".into(), None, human.id)
            .await
            .unwrap();
        answered(&store, human.id, Some(ticket.id), None).await;
        assert!(inbox(&store, &human).await.iter().all(|i| !i.needs_you));
    }
}
