//! Integration tests for the SQLite store against an in-memory database.

use std::sync::Arc;

use ak_core::board::{ColumnKind, Priority};
use ak_core::commands::{NewTicket, TicketPatch};
use ak_core::config::ProjectPolicy;
use ak_core::ids::ActorId;
use ak_core::{Actor, ActorKind, EventKind, NotificationKind, NotificationState};
use ak_store::{HumanIdentity, SqliteStore, Store, StoreError};

struct Fixture {
    store: Arc<SqliteStore>,
    human: Actor,
    agent: Actor,
    todo: ak_core::ids::ColumnId,
    in_progress: ak_core::ids::ColumnId,
}

async fn fixture() -> Fixture {
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    let human = store
        .bootstrap(
            &ProjectPolicy::new("Demo Project"),
            &HumanIdentity::default(),
        )
        .await
        .unwrap();
    let agent = Actor {
        id: ActorId::new(),
        kind: ActorKind::Agent,
        handle: "backend".into(),
        display_name: "Backend".into(),
    };
    store.upsert_actor(&agent).await.unwrap();
    let columns = store.columns().await.unwrap();
    let find = |k: ColumnKind| columns.iter().find(|c| c.kind == k).unwrap().id;
    Fixture {
        store,
        human,
        agent,
        todo: find(ColumnKind::Todo),
        in_progress: find(ColumnKind::InProgress),
    }
}

#[tokio::test]
async fn bootstrap_is_idempotent_and_creates_columns() {
    let f = fixture().await;
    let again = f
        .store
        .bootstrap(
            &ProjectPolicy::new("Demo Project"),
            &HumanIdentity::default(),
        )
        .await
        .unwrap();
    assert_eq!(again.id, f.human.id);
    assert_eq!(f.store.columns().await.unwrap().len(), 5);
    assert_eq!(f.store.human().await.unwrap().handle, "me");
    let actors = f.store.actors().await.unwrap();
    assert!(actors.iter().any(|a| a.kind == ActorKind::System));
}

#[tokio::test]
async fn tickets_get_sequential_keys_and_positions() {
    let f = fixture().await;
    let a = f
        .store
        .create_ticket(
            NewTicket {
                title: "First".into(),
                column_id: f.todo,
                ..Default::default()
            },
            f.human.id,
        )
        .await
        .unwrap();
    let b = f
        .store
        .create_ticket(
            NewTicket {
                title: "Second".into(),
                column_id: f.todo,
                ..Default::default()
            },
            f.human.id,
        )
        .await
        .unwrap();
    assert_eq!(a.key, "DP-1");
    assert_eq!(b.key, "DP-2");
    assert_eq!(a.position, 0);
    assert_eq!(b.position, 1);
    assert_eq!(f.store.tickets().await.unwrap().len(), 2);
}

#[tokio::test]
async fn assignment_notifies_and_wakes_the_assignee() {
    let f = fixture().await;
    let t = f
        .store
        .create_ticket(
            NewTicket {
                title: "Work".into(),
                column_id: f.todo,
                ..Default::default()
            },
            f.human.id,
        )
        .await
        .unwrap();
    let t = f
        .store
        .assign_ticket(t.id, Some(f.agent.id), t.version, f.human.id)
        .await
        .unwrap();
    assert_eq!(t.assignee_id, Some(f.agent.id));
    assert_eq!(t.version, 2);

    let inbox = f.store.notifications(f.agent.id, true).await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, NotificationKind::Assigned);
    assert!(inbox[0].wake);

    // The human caused it, so the human is not notified.
    assert!(f
        .store
        .notifications(f.human.id, true)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn version_conflicts_surface_as_core_errors() {
    let f = fixture().await;
    let t = f
        .store
        .create_ticket(
            NewTicket {
                title: "Work".into(),
                column_id: f.todo,
                ..Default::default()
            },
            f.human.id,
        )
        .await
        .unwrap();
    let err = f
        .store
        .move_ticket(t.id, f.in_progress, 42, f.human.id)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Core(ak_core::CoreError::VersionConflict { .. })
    ));
    assert!(err.is_client_error());
}

#[tokio::test]
async fn comments_mentions_watchers_and_replies() {
    let f = fixture().await;
    let t = f
        .store
        .create_ticket(
            NewTicket {
                title: "Work".into(),
                column_id: f.todo,
                ..Default::default()
            },
            f.human.id,
        )
        .await
        .unwrap();

    // Human mentions the agent in a comment: agent gets a wake notification and
    // the human is now a watcher (already was, as reporter).
    let c = f
        .store
        .comment(t.id, "can you look @backend?".into(), None, f.human.id)
        .await
        .unwrap();
    let inbox = f.store.notifications(f.agent.id, true).await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].kind, NotificationKind::Mentioned);
    assert!(inbox[0].wake);

    // Agent replies: the human, as parent author, gets a Replied wake notification.
    let r = f
        .store
        .comment(t.id, "on it".into(), Some(c.id), f.agent.id)
        .await
        .unwrap();
    assert_eq!(r.parent_comment_id, Some(c.id));
    let human_inbox = f.store.notifications(f.human.id, true).await.unwrap();
    assert_eq!(human_inbox.len(), 1);
    assert_eq!(human_inbox[0].kind, NotificationKind::Replied);

    // Agent commented, so it is a watcher now.
    let watchers = f.store.watchers(t.id).await.unwrap();
    assert!(watchers.contains(&f.agent.id));
    assert!(watchers.contains(&f.human.id));

    let events = f.store.events_for_ticket(t.id).await.unwrap();
    let names: Vec<&str> = events.iter().map(|e| e.kind.name()).collect();
    assert_eq!(
        names,
        vec![
            "ticket.created",
            "ticket.commented",
            "mention.created",
            "ticket.replied"
        ]
    );
    assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));

    let comments = f.store.comments(t.id).await.unwrap();
    assert_eq!(comments.len(), 2);
}

#[tokio::test]
async fn freeze_holds_notifications_and_unfreeze_releases_them() {
    let f = fixture().await;
    let t = f
        .store
        .create_ticket(
            NewTicket {
                title: "Work".into(),
                column_id: f.todo,
                assignee_id: Some(f.agent.id),
                ..Default::default()
            },
            f.human.id,
        )
        .await
        .unwrap();
    // Clear the assignment notification so counts below are clean.
    for n in f.store.notifications(f.agent.id, true).await.unwrap() {
        f.store
            .set_notification_state(n.id, NotificationState::Handled)
            .await
            .unwrap();
    }

    let t = f
        .store
        .freeze_ticket(t.id, "ping-pong".into(), ActorId::SYSTEM)
        .await
        .unwrap();
    assert!(t.frozen);

    // Agent cannot touch it.
    let err = f
        .store
        .comment(t.id, "hi".into(), None, f.agent.id)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Core(ak_core::CoreError::TicketFrozen { .. })
    ));

    // Human moves it while frozen: the agent's notification is held, not pending.
    let t = f
        .store
        .move_ticket(t.id, f.in_progress, t.version, f.human.id)
        .await
        .unwrap();
    assert!(f
        .store
        .notifications(f.agent.id, true)
        .await
        .unwrap()
        .is_empty());
    let all = f.store.notifications(f.agent.id, false).await.unwrap();
    assert!(all.iter().any(|n| n.state == NotificationState::Held));

    // Unfreeze releases the held one and adds an Unfrozen wake.
    let t = f.store.unfreeze_ticket(t.id, f.human.id).await.unwrap();
    assert!(!t.frozen);
    let open = f.store.notifications(f.agent.id, true).await.unwrap();
    let kinds: Vec<NotificationKind> = open.iter().map(|n| n.kind).collect();
    assert!(kinds.contains(&NotificationKind::Unfrozen));
    assert!(kinds.contains(&NotificationKind::Moved));
    assert!(open.iter().all(|n| n.state == NotificationState::Pending));
}

#[tokio::test]
async fn updates_record_changes_and_events_publish_to_subscribers() {
    let f = fixture().await;
    let mut rx = f.store.subscribe();
    let t = f
        .store
        .create_ticket(
            NewTicket {
                title: "Work".into(),
                column_id: f.todo,
                ..Default::default()
            },
            f.human.id,
        )
        .await
        .unwrap();
    let created = rx.recv().await.unwrap();
    assert!(matches!(created.kind, EventKind::TicketCreated { .. }));

    let t = f
        .store
        .update_ticket(
            t.id,
            TicketPatch {
                priority: Some(Priority::Urgent),
                labels: Some(vec!["Bug".into()]),
                ..Default::default()
            },
            t.version,
            f.human.id,
        )
        .await
        .unwrap();
    assert_eq!(t.priority, Priority::Urgent);
    assert_eq!(t.labels, vec!["bug"]);
    let updated = rx.recv().await.unwrap();
    match &updated.kind {
        EventKind::TicketUpdated { changes, .. } => assert_eq!(changes.len(), 2),
        other => panic!("unexpected {other:?}"),
    }

    let since = f.store.events_since(0, 10).await.unwrap();
    assert_eq!(since.len(), 2);
}

#[tokio::test]
async fn seen_and_handled_states() {
    let f = fixture().await;
    let t = f
        .store
        .create_ticket(
            NewTicket {
                title: "Work".into(),
                column_id: f.todo,
                assignee_id: Some(f.agent.id),
                ..Default::default()
            },
            f.human.id,
        )
        .await
        .unwrap();
    let _ = t;
    assert_eq!(
        f.store.mark_notifications_seen(f.agent.id).await.unwrap(),
        1
    );
    let open = f.store.notifications(f.agent.id, true).await.unwrap();
    assert_eq!(open[0].state, NotificationState::Seen);
    f.store
        .set_notification_state(open[0].id, NotificationState::Handled)
        .await
        .unwrap();
    assert!(f
        .store
        .notifications(f.agent.id, true)
        .await
        .unwrap()
        .is_empty());
    let all = f.store.notifications(f.agent.id, false).await.unwrap();
    assert!(all[0].handled_at.is_some());
}
