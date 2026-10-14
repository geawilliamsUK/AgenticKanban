//! Utility call records: Refine and Integrate proposals and their decisions.

use ak_core::agent::{Agent, AgentDefinition};
use ak_core::config::ProjectPolicy;
use ak_core::ids::ActorId;
use ak_core::utility::{UtilityCall, UtilityKind, UtilityState};
use ak_store::{HumanIdentity, SqliteStore, Store};
use chrono::{Duration, Utc};

async fn store_with_agent() -> (SqliteStore, ActorId) {
    let store = SqliteStore::open_in_memory().await.unwrap();
    store
        .bootstrap(&ProjectPolicy::new("Demo"), &HumanIdentity::default())
        .await
        .unwrap();
    let def = AgentDefinition::template("backend", "Backend");
    let agent = Agent::from_definition(ActorId::new(), &def, "a".into(), "m".into());
    store.upsert_agent(&agent).await.unwrap();
    (store, agent.actor_id)
}

#[tokio::test]
async fn call_lifecycle_and_pending_lookup() {
    let (store, agent) = store_with_agent().await;
    let now = Utc::now();
    let mut call = UtilityCall::start(UtilityKind::Refine, agent, None, Some("sonnet".into()), now);
    store.insert_utility_call(&call).await.unwrap();

    let loaded = store.utility_call(call.id).await.unwrap();
    assert_eq!(loaded.state, UtilityState::Running);
    assert_eq!(loaded.model.as_deref(), Some("sonnet"));
    assert!(store
        .pending_proposal(agent, UtilityKind::Refine)
        .await
        .unwrap()
        .is_none());

    call.propose(
        "---\nname: backend\n---\nbody".into(),
        vec!["x".into()],
        Some(0.02),
        Utc::now(),
    );
    store.update_utility_call(&call).await.unwrap();
    let pending = store
        .pending_proposal(agent, UtilityKind::Refine)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.id, call.id);
    assert_eq!(pending.warnings, vec!["x"]);
    assert_eq!(pending.cost_usd, Some(0.02));
    assert!(pending.ended_at.is_some());
    assert!(store
        .pending_proposal(agent, UtilityKind::Integrate)
        .await
        .unwrap()
        .is_none());

    call.state = UtilityState::Accepted;
    store.update_utility_call(&call).await.unwrap();
    assert!(store
        .pending_proposal(agent, UtilityKind::Refine)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn failures_and_cost_window() {
    let (store, agent) = store_with_agent().await;
    let old = Utc::now() - Duration::days(2);
    let mut stale = UtilityCall::start(UtilityKind::Refine, agent, None, None, old);
    stale.propose("p".into(), vec![], Some(1.5), old);
    store.insert_utility_call(&stale).await.unwrap();

    let mut failed = UtilityCall::start(UtilityKind::Refine, agent, None, None, Utc::now());
    failed.fail("boom".into(), Some(0.25), Utc::now());
    store.insert_utility_call(&failed).await.unwrap();
    let loaded = store.utility_call(failed.id).await.unwrap();
    assert_eq!(loaded.state, UtilityState::Failed);
    assert_eq!(loaded.error.as_deref(), Some("boom"));

    let today = store
        .utility_cost_since(Utc::now() - Duration::hours(1))
        .await
        .unwrap();
    assert!((today - 0.25).abs() < 1e-9, "{today}");
    let all = store
        .utility_cost_since(Utc::now() - Duration::days(3))
        .await
        .unwrap();
    assert!((all - 1.75).abs() < 1e-9, "{all}");
}
