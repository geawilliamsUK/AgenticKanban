//! Desk sessions and transcripts.

use ak_core::config::ProjectPolicy;
use ak_core::desk::{DeskEntryKind, DeskSession};
use ak_store::{HumanIdentity, SqliteStore, Store};
use chrono::{Duration, Utc};
use serde_json::json;

#[tokio::test]
async fn sessions_and_transcripts_round_trip() {
    let store = SqliteStore::open_in_memory().await.unwrap();
    store
        .bootstrap(&ProjectPolicy::new("Demo"), &HumanIdentity::default())
        .await
        .unwrap();

    let mut older = DeskSession::new(None, Utc::now() - Duration::hours(2));
    store.insert_desk_session(&older).await.unwrap();
    let mut s = DeskSession::new(Some("opus".into()), Utc::now());
    store.insert_desk_session(&s).await.unwrap();

    let first = store
        .append_desk_entry(s.id, DeskEntryKind::User, json!({"text": "hi"}), Utc::now())
        .await
        .unwrap();
    let second = store
        .append_desk_entry(
            s.id,
            DeskEntryKind::Assistant,
            json!({"text": "hello"}),
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(second.seq > first.seq);
    store
        .append_desk_entry(
            older.id,
            DeskEntryKind::User,
            json!({"text": "x"}),
            Utc::now(),
        )
        .await
        .unwrap();

    let entries = store.desk_entries(s.id).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, DeskEntryKind::User);
    assert_eq!(entries[1].text("text"), "hello");

    s.title_from("Plan the sprint");
    s.started = true;
    s.cost_usd = 0.42;
    s.last_active_at = Utc::now();
    store.update_desk_session(&s).await.unwrap();
    let loaded = store.desk_session(s.id).await.unwrap();
    assert_eq!(loaded.title, "Plan the sprint");
    assert!(loaded.started);
    assert_eq!(loaded.model.as_deref(), Some("opus"));
    assert_eq!(loaded.claude_session_id, s.claude_session_id);
    assert!((loaded.cost_usd - 0.42).abs() < 1e-9);

    older.archived = true;
    store.update_desk_session(&older).await.unwrap();
    let all = store.desk_sessions().await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, s.id, "most recently active first");
    assert!(all[1].archived);
}
