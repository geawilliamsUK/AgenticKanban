//! Guardrail trips as system events, and the thread event query the
//! monitor needs.

use std::sync::Arc;

use chrono::{Duration, Utc};

use ak_core::config::ProjectPolicy;
use ak_core::event::Subject;
use ak_core::guardrails::{GuardrailAction, GuardrailRule, Trip};
use ak_core::ids::ActorId;
use ak_core::{Actor, ActorKind, NotificationKind};
use ak_store::{HumanIdentity, SqliteStore, Store};

#[tokio::test]
async fn trips_are_system_events_that_notify_the_human() {
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    let human = store
        .bootstrap(&ProjectPolicy::new("Demo"), &HumanIdentity::default())
        .await
        .unwrap();
    let agent = Actor {
        id: ActorId::new(),
        kind: ActorKind::Agent,
        handle: "backend".into(),
        display_name: "Backend".into(),
    };
    store.upsert_actor(&agent).await.unwrap();

    let trip = Trip {
        rule: GuardrailRule::RunChurn,
        action: GuardrailAction::Cooldown,
        subject: Subject::Agent(agent.id),
        detail: "three runs handled nothing".into(),
    };
    let event = store.record_guardrail_trip(&trip).await.unwrap();
    assert_eq!(event.actor_id, ActorId::SYSTEM);
    assert_eq!(event.kind.name(), "guardrail.tripped");
    assert_eq!(event.subject(), Subject::Agent(agent.id));

    let inbox = store.notifications(human.id, true).await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, NotificationKind::Guardrail);
    assert!(!inbox[0].wake);
    assert!(store
        .notifications(agent.id, true)
        .await
        .unwrap()
        .is_empty());

    let recent = store
        .guardrail_trips_since(Utc::now() - Duration::minutes(1))
        .await
        .unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].id, event.id);
    assert!(store
        .guardrail_trips_since(Utc::now() + Duration::minutes(1))
        .await
        .unwrap()
        .is_empty());

    // Thread events are queryable by subject, oldest first.
    let chat = store
        .direct_chat(human.id, agent.id, human.id)
        .await
        .unwrap();
    let (thread, _) = store
        .create_thread(chat.id, "Loop".into(), "ping".into(), agent.id)
        .await
        .unwrap();
    store
        .post_message(thread.id, "pong".into(), human.id)
        .await
        .unwrap();
    let events = store.events_for_thread(thread.id).await.unwrap();
    let names: Vec<&str> = events.iter().map(|e| e.kind.name()).collect();
    assert_eq!(
        names,
        vec![
            "thread.created",
            "thread.message_posted",
            "thread.message_posted"
        ]
    );
}
