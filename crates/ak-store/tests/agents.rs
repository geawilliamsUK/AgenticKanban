//! Agent rows: upsert, lookup, retirement and state.

use ak_core::agent::{Agent, AgentDefinition, AgentState, Budgets};
use ak_core::config::ProjectPolicy;
use ak_core::ids::{ActorId, RunId};
use ak_core::ActorKind;
use ak_store::{HumanIdentity, SqliteStore, Store};

fn agent(handle: &str, role: &str) -> Agent {
    let def = AgentDefinition::template(handle, role);
    Agent::from_definition(
        ActorId::new(),
        &def,
        format!("{handle}/agent.md").into(),
        format!("{handle}/memory.md").into(),
    )
}

#[tokio::test]
async fn upsert_creates_actor_and_agent_and_updates_in_place() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    store
        .bootstrap(&ProjectPolicy::new("Demo"), &HumanIdentity::default())
        .await
        .unwrap();

    let mut a = agent("backend", "Backend developer");
    a.budgets = Some(Budgets {
        max_usd_per_run: Some(2.5),
        ..Default::default()
    });
    store.upsert_agent(&a).await.unwrap();

    let actors = store.actors().await.unwrap();
    let actor = actors.iter().find(|x| x.handle == "backend").unwrap();
    assert_eq!(actor.kind, ActorKind::Agent);
    assert_eq!(actor.id, a.actor_id);

    let loaded = store.agent_by_handle("backend").await.unwrap().unwrap();
    assert_eq!(loaded, a);
    assert_eq!(loaded.budgets.as_ref().unwrap().max_usd_per_run, Some(2.5));

    a.description = "Owns the API".into();
    a.model = Some("opus".into());
    store.upsert_agent(&a).await.unwrap();
    let again = store.agent(a.actor_id).await.unwrap();
    assert_eq!(again.description, "Owns the API");
    assert_eq!(again.model.as_deref(), Some("opus"));
    assert_eq!(store.agents().await.unwrap().len(), 1);
}

#[tokio::test]
async fn missing_and_state_round_trip() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    store
        .bootstrap(&ProjectPolicy::new("Demo"), &HumanIdentity::default())
        .await
        .unwrap();
    let a = agent("qa", "QA");
    store.upsert_agent(&a).await.unwrap();

    let now = chrono::Utc::now();
    store
        .set_agent_missing(a.actor_id, Some(now))
        .await
        .unwrap();
    let loaded = store.agent(a.actor_id).await.unwrap();
    assert!(loaded.is_retired());
    assert_eq!(
        loaded.missing_since.unwrap().timestamp_micros(),
        now.timestamp_micros()
    );

    store.set_agent_missing(a.actor_id, None).await.unwrap();
    assert!(!store.agent(a.actor_id).await.unwrap().is_retired());

    let run = RunId::new();
    store
        .set_agent_state(a.actor_id, AgentState::Running, Some(run))
        .await
        .unwrap();
    let loaded = store.agent(a.actor_id).await.unwrap();
    assert_eq!(loaded.state, AgentState::Running);
    assert_eq!(loaded.current_run_id, Some(run));

    // Upserting from a definition never clobbers runtime state.
    let mut from_file = agent("qa", "QA engineer");
    from_file.actor_id = a.actor_id;
    store.upsert_agent(&from_file).await.unwrap();
    let loaded = store.agent(a.actor_id).await.unwrap();
    assert_eq!(loaded.state, AgentState::Running, "state preserved");
    assert_eq!(loaded.current_run_id, Some(run));
}

#[tokio::test]
async fn agent_handles_cannot_collide_with_the_human() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    store
        .bootstrap(&ProjectPolicy::new("Demo"), &HumanIdentity::default())
        .await
        .unwrap();
    let a = agent("me", "Impostor");
    assert!(store.upsert_agent(&a).await.is_err());
    assert!(store.agent_by_handle("me").await.unwrap().is_none());
}
