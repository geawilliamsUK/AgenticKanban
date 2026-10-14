//! Run records and transcripts.

use ak_core::agent::{Agent, AgentDefinition};
use ak_core::config::ProjectPolicy;
use ak_core::ids::{ActorId, NotificationId};
use ak_core::run::{AgentRun, ExitReason, RunEvent, RunEventKind, RunTrigger};
use ak_store::{HumanIdentity, SqliteStore, Store};
use chrono::{Duration, Utc};
use serde_json::json;

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
async fn run_lifecycle_and_transcript() {
    let (store, agent) = store_with_agent().await;
    let trigger = RunTrigger::Notifications {
        ids: vec![NotificationId::new()],
    };
    let mut run = AgentRun::start(agent, trigger.clone(), "C:/ws/backend".into(), Utc::now());
    store.insert_run(&run).await.unwrap();

    let active = store.active_runs().await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].trigger, trigger);
    assert!(active[0].is_active());

    for (seq, (kind, payload)) in [
        (
            RunEventKind::SystemInit,
            json!({"session_id": "s1", "model": "fake"}),
        ),
        (
            RunEventKind::ToolCall,
            json!({"name": "Bash", "input": {"command": "cargo test"}}),
        ),
        (
            RunEventKind::Result,
            json!({"subtype": "success", "total_cost_usd": 0.5}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        store
            .append_run_event(&RunEvent {
                run_id: run.id,
                seq: seq as i64,
                at: Utc::now(),
                kind,
                payload,
            })
            .await
            .unwrap();
    }
    let events = store.run_events(run.id).await.unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[1].kind, RunEventKind::ToolCall);
    assert_eq!(events[1].summary(), "Bash cargo test");
    assert_eq!(events[2].summary(), "success, $0.500");

    run.claude_session_id = Some("s1".into());
    run.ended_at = Some(Utc::now());
    run.exit_reason = Some(ExitReason::Completed);
    run.num_turns = 4;
    run.input_tokens = 1000;
    run.output_tokens = 200;
    run.cost_usd = Some(0.5);
    store.update_run(&run).await.unwrap();

    let loaded = store.run(run.id).await.unwrap();
    assert_eq!(loaded.id, run.id);
    assert_eq!(loaded.exit_reason, run.exit_reason);
    assert_eq!(loaded.num_turns, run.num_turns);
    assert_eq!(loaded.cost_usd, run.cost_usd);
    assert_eq!(loaded.claude_session_id, run.claude_session_id);
    assert_eq!(loaded.input_tokens, run.input_tokens);
    assert!(store.active_runs().await.unwrap().is_empty());
    let recent = store.runs_for_agent(agent, 10).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].exit_reason, Some(ExitReason::Completed));
}

#[tokio::test]
async fn cost_windows_and_cooldown() {
    let (store, agent) = store_with_agent().await;
    let old = Utc::now() - Duration::days(2);
    let mut stale = AgentRun::start(agent, RunTrigger::Manual, "x".into(), old);
    stale.cost_usd = Some(2.0);
    stale.ended_at = Some(old);
    store.insert_run(&stale).await.unwrap();
    let mut fresh = AgentRun::start(agent, RunTrigger::Manual, "x".into(), Utc::now());
    fresh.cost_usd = Some(0.75);
    store.insert_run(&fresh).await.unwrap();

    let today = store
        .run_cost_since(Utc::now() - Duration::hours(1), None)
        .await
        .unwrap();
    assert!((today - 0.75).abs() < 1e-9, "{today}");
    let mine = store
        .run_cost_since(Utc::now() - Duration::days(3), Some(agent))
        .await
        .unwrap();
    assert!((mine - 2.75).abs() < 1e-9, "{mine}");
    let nobody = store
        .run_cost_since(Utc::now() - Duration::days(3), Some(ActorId::new()))
        .await
        .unwrap();
    assert_eq!(nobody, 0.0);

    let until = Utc::now() + Duration::minutes(30);
    store.set_agent_cooldown(agent, Some(until)).await.unwrap();
    let a = store.agent(agent).await.unwrap();
    assert_eq!(
        a.cooldown_until.unwrap().timestamp_micros(),
        until.timestamp_micros()
    );
    store.set_agent_cooldown(agent, None).await.unwrap();
    assert!(store.agent(agent).await.unwrap().cooldown_until.is_none());
}

#[tokio::test]
async fn recovery_closes_open_runs_and_pruning_drops_old_transcripts() {
    let (store, agent) = store_with_agent().await;
    let open = AgentRun::start(agent, RunTrigger::Manual, "C:/ws".into(), Utc::now());
    store.insert_run(&open).await.unwrap();
    store
        .set_agent_state(agent, ak_core::agent::AgentState::Running, Some(open.id))
        .await
        .unwrap();

    let closed = store
        .recover_stale_runs("the app was not running")
        .await
        .unwrap();
    assert_eq!(closed, 1);
    let run = store.run(open.id).await.unwrap();
    assert_eq!(run.exit_reason, Some(ExitReason::Crashed));
    assert!(run.ended_at.is_some());
    assert_eq!(run.error.as_deref(), Some("the app was not running"));
    assert_eq!(
        store.agent(agent).await.unwrap().state,
        ak_core::agent::AgentState::Idle
    );
    assert!(store.active_runs().await.unwrap().is_empty());
    assert_eq!(store.recover_stale_runs("again").await.unwrap(), 0);

    // An old run loses its transcript but keeps its row and its cost.
    let mut old = AgentRun::start(
        agent,
        RunTrigger::Manual,
        "C:/ws".into(),
        Utc::now() - Duration::days(40),
    );
    old.ended_at = Some(Utc::now() - Duration::days(40));
    old.exit_reason = Some(ExitReason::Completed);
    old.cost_usd = Some(1.5);
    store.insert_run(&old).await.unwrap();
    store.update_run(&old).await.unwrap();
    store
        .append_run_event(&RunEvent {
            run_id: old.id,
            seq: 0,
            at: old.started_at,
            kind: RunEventKind::Supervisor,
            payload: json!({"message": "x"}),
        })
        .await
        .unwrap();
    let pruned = store
        .prune_run_events(Utc::now() - Duration::days(30))
        .await
        .unwrap();
    assert_eq!(pruned, vec![old.id]);
    assert!(store.run_events(old.id).await.unwrap().is_empty());
    assert_eq!(store.run(old.id).await.unwrap().cost_usd, Some(1.5));
    assert!(
        store
            .prune_run_events(Utc::now() - Duration::days(30))
            .await
            .unwrap()
            .is_empty(),
        "nothing left to prune"
    );
}
