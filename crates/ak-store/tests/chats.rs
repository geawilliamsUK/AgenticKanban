//! Chats and threads against the in-memory store: membership, routing,
//! pausing, search and archiving.

use std::sync::Arc;

use chrono::{Duration, Utc};

use ak_core::chat::ChatKind;
use ak_core::config::ProjectPolicy;
use ak_core::ids::ActorId;
use ak_core::{Actor, ActorKind, CoreError, NotificationKind, NotificationState};
use ak_store::{HumanIdentity, SqliteStore, Store, StoreError};

struct Fixture {
    store: Arc<SqliteStore>,
    human: Actor,
    backend: Actor,
    frontend: Actor,
    qa: Actor,
}

async fn fixture() -> Fixture {
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    let human = store
        .bootstrap(&ProjectPolicy::new("Demo"), &HumanIdentity::default())
        .await
        .unwrap();
    let make = |handle: &str| Actor {
        id: ActorId::new(),
        kind: ActorKind::Agent,
        handle: handle.into(),
        display_name: handle.into(),
    };
    let backend = make("backend");
    let frontend = make("frontend");
    let qa = make("qa");
    for a in [&backend, &frontend, &qa] {
        store.upsert_actor(a).await.unwrap();
    }
    Fixture {
        store,
        human,
        backend,
        frontend,
        qa,
    }
}

fn is_not_permitted(err: &StoreError) -> bool {
    matches!(err, StoreError::Core(CoreError::NotPermitted(_)))
}

#[tokio::test]
async fn direct_chat_is_created_once_and_messages_wake_the_other_member() {
    let f = fixture().await;
    let chat = f
        .store
        .direct_chat(f.backend.id, f.frontend.id, f.backend.id)
        .await
        .unwrap();
    assert_eq!(chat.kind, ChatKind::Direct);
    let again = f
        .store
        .direct_chat(f.frontend.id, f.backend.id, f.human.id)
        .await
        .unwrap();
    assert_eq!(again.id, chat.id, "the same pair gets the same chat");
    let members = f.store.chat_members(chat.id).await.unwrap();
    assert_eq!(members.len(), 2);
    assert!(members.contains(&f.backend.id) && members.contains(&f.frontend.id));
    assert_eq!(f.store.chats_for(f.backend.id).await.unwrap().len(), 1);
    assert!(f.store.chats_for(f.human.id).await.unwrap().is_empty());
    assert_eq!(f.store.chats().await.unwrap().len(), 1);
    assert!(f
        .store
        .direct_chat(f.backend.id, f.backend.id, f.backend.id)
        .await
        .is_err());

    let (thread, first) = f
        .store
        .create_thread(
            chat.id,
            "Auth question".into(),
            "Which hashing library are we using?".into(),
            f.backend.id,
        )
        .await
        .unwrap();
    assert_eq!(first.thread_id, thread.id);
    assert_eq!(thread.title, "Auth question");

    // The other member is woken; the human is informed without a wake; the
    // author hears nothing.
    let to_frontend = f.store.notifications(f.frontend.id, true).await.unwrap();
    assert_eq!(to_frontend.len(), 1);
    assert_eq!(to_frontend[0].kind, NotificationKind::Messaged);
    assert!(to_frontend[0].wake);
    let to_human = f.store.notifications(f.human.id, true).await.unwrap();
    assert_eq!(to_human.len(), 1);
    assert!(!to_human[0].wake);
    assert!(f
        .store
        .notifications(f.backend.id, true)
        .await
        .unwrap()
        .is_empty());

    // A reply wakes the first author, and a mention of a non-member notifies
    // them too.
    let reply = f
        .store
        .post_message(
            thread.id,
            "bcrypt. @qa please check the tests.".into(),
            f.frontend.id,
        )
        .await
        .unwrap();
    let to_backend = f.store.notifications(f.backend.id, true).await.unwrap();
    assert_eq!(to_backend.len(), 1);
    assert_eq!(to_backend[0].kind, NotificationKind::Messaged);
    assert!(to_backend[0].wake);
    let to_qa = f.store.notifications(f.qa.id, true).await.unwrap();
    assert_eq!(to_qa.len(), 1);
    assert_eq!(to_qa[0].kind, NotificationKind::Mentioned);

    let messages = f.store.messages(thread.id).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].id, reply.id);
    let participants = f.store.thread_participants(thread.id).await.unwrap();
    assert_eq!(participants.len(), 2);
    let listed = f.store.threads(chat.id, false).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].last_activity_at >= thread.last_activity_at);

    // Non-members cannot read into the chat by posting.
    let err = f
        .store
        .post_message(thread.id, "hi".into(), f.qa.id)
        .await
        .unwrap_err();
    assert!(is_not_permitted(&err), "{err}");
}

#[tokio::test]
async fn group_posts_reach_participants_and_only_the_human_wakes_them() {
    let f = fixture().await;
    let chat = f
        .store
        .create_group_chat("team".into(), vec![f.backend.id, f.frontend.id], f.human.id)
        .await
        .unwrap();
    assert_eq!(chat.kind, ChatKind::Group);
    assert_eq!(chat.name.as_deref(), Some("team"));
    let members = f.store.chat_members(chat.id).await.unwrap();
    assert_eq!(members.len(), 2, "the human is an observer, not a member");
    // Members learn of the chat without being woken.
    assert!(f
        .store
        .notifications(f.backend.id, true)
        .await
        .unwrap()
        .iter()
        .all(|n| n.kind == NotificationKind::Activity && !n.wake));

    let (thread, _) = f
        .store
        .create_thread(
            chat.id,
            "Release plan".into(),
            "Friday?".into(),
            f.backend.id,
        )
        .await
        .unwrap();
    // A new group thread is announced to members, no wake.
    let to_frontend = f.store.notifications(f.frontend.id, true).await.unwrap();
    assert!(to_frontend.iter().all(|n| !n.wake));
    assert!(to_frontend
        .iter()
        .any(|n| n.kind == NotificationKind::Activity));

    // The human's post wakes the participants (only @backend so far).
    f.store
        .post_message(thread.id, "Yes, Friday.".into(), f.human.id)
        .await
        .unwrap();
    let to_backend = f.store.notifications(f.backend.id, true).await.unwrap();
    let woken: Vec<_> = to_backend.iter().filter(|n| n.wake).collect();
    assert_eq!(woken.len(), 1);
    assert_eq!(woken[0].kind, NotificationKind::Messaged);
    assert!(
        f.store
            .notifications(f.frontend.id, true)
            .await
            .unwrap()
            .iter()
            .all(|n| !n.wake),
        "non-participants are not woken"
    );

    // An agent's post reaches the other participants without a wake.
    f.store
        .post_message(thread.id, "I can cut the branch.".into(), f.frontend.id)
        .await
        .unwrap();
    let to_human = f.store.notifications(f.human.id, true).await.unwrap();
    assert!(to_human
        .iter()
        .any(|n| n.kind == NotificationKind::Messaged && !n.wake));
    let to_backend = f.store.notifications(f.backend.id, true).await.unwrap();
    assert_eq!(
        to_backend.iter().filter(|n| n.wake).count(),
        1,
        "still just the human's"
    );

    // Outsiders cannot post; adding them fixes that.
    let err = f
        .store
        .post_message(thread.id, "me too".into(), f.qa.id)
        .await
        .unwrap_err();
    assert!(is_not_permitted(&err));
    f.store
        .add_chat_member(chat.id, f.qa.id, f.human.id)
        .await
        .unwrap();
    f.store
        .post_message(thread.id, "me too".into(), f.qa.id)
        .await
        .unwrap();
    assert_eq!(f.store.chat_members(chat.id).await.unwrap().len(), 3);
    assert!(
        f.store
            .add_chat_member(chat.id, f.qa.id, f.human.id)
            .await
            .is_err(),
        "already a member"
    );
}

#[tokio::test]
async fn paused_threads_reject_agents_and_hold_until_unpaused() {
    let f = fixture().await;
    let chat = f
        .store
        .direct_chat(f.backend.id, f.frontend.id, f.human.id)
        .await
        .unwrap();
    let (thread, _) = f
        .store
        .create_thread(chat.id, "Loop".into(), "ping".into(), f.backend.id)
        .await
        .unwrap();
    f.store
        .post_message(thread.id, "pong".into(), f.frontend.id)
        .await
        .unwrap();
    f.store
        .mark_notifications_handled(
            &f.store
                .notifications(f.backend.id, true)
                .await
                .unwrap()
                .iter()
                .map(|n| n.id)
                .collect::<Vec<_>>(),
            None,
        )
        .await
        .unwrap();

    let err = f
        .store
        .set_thread_paused(thread.id, true, f.backend.id)
        .await
        .unwrap_err();
    assert!(is_not_permitted(&err));
    let paused = f
        .store
        .set_thread_paused(thread.id, true, f.human.id)
        .await
        .unwrap();
    assert!(paused.paused);
    assert!(f.store.thread(thread.id).await.unwrap().paused);

    let err = f
        .store
        .post_message(thread.id, "ping".into(), f.backend.id)
        .await
        .unwrap_err();
    assert!(is_not_permitted(&err), "{err}");

    // The human can still post, but what that routes is held.
    f.store
        .post_message(thread.id, "Stop. Use the ticket.".into(), f.human.id)
        .await
        .unwrap();
    let held = f.store.notifications(f.backend.id, false).await.unwrap();
    assert!(held
        .iter()
        .any(|n| n.state == NotificationState::Held && n.kind == NotificationKind::Messaged));
    assert!(
        f.store
            .notifications(f.backend.id, true)
            .await
            .unwrap()
            .is_empty(),
        "nothing open while paused"
    );

    let unpaused = f
        .store
        .set_thread_paused(thread.id, false, f.human.id)
        .await
        .unwrap();
    assert!(!unpaused.paused);
    let open = f.store.notifications(f.backend.id, true).await.unwrap();
    assert!(
        open.iter().any(|n| n.kind == NotificationKind::Messaged),
        "released"
    );
    assert!(open
        .iter()
        .any(|n| n.kind == NotificationKind::Unfrozen && n.wake));
}

#[tokio::test]
async fn search_is_scoped_and_archiving_is_reversible() {
    let f = fixture().await;
    let chat = f
        .store
        .direct_chat(f.backend.id, f.frontend.id, f.human.id)
        .await
        .unwrap();
    let (deploy, _) = f
        .store
        .create_thread(
            chat.id,
            "Deploy checklist".into(),
            "Remember to run the migrations first.".into(),
            f.backend.id,
        )
        .await
        .unwrap();
    f.store
        .create_thread(
            chat.id,
            "Lunch".into(),
            "Pizza on Friday?".into(),
            f.frontend.id,
        )
        .await
        .unwrap();

    let hits = f
        .store
        .search_threads(None, "migrations", 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].thread.id, deploy.id);
    assert!(
        hits[0].snippet.contains("[migrations]"),
        "{}",
        hits[0].snippet
    );
    let by_title = f.store.search_threads(None, "deploy", 10).await.unwrap();
    assert_eq!(by_title.len(), 1);
    assert_eq!(by_title[0].thread.id, deploy.id);
    assert!(
        f.store
            .search_threads(Some(f.qa.id), "deploy", 10)
            .await
            .unwrap()
            .is_empty(),
        "scoped to the caller's chats"
    );
    assert_eq!(
        f.store
            .search_threads(Some(f.backend.id), "pizza \"friday", 10)
            .await
            .unwrap()
            .len(),
        1,
        "punctuation never breaks the query"
    );
    assert!(f
        .store
        .search_threads(None, "   ", 10)
        .await
        .unwrap()
        .is_empty());

    // Nothing is idle yet; then everything is; then a post reopens one.
    assert_eq!(
        f.store
            .archive_idle_threads(Utc::now() - Duration::days(7))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f.store
            .archive_idle_threads(Utc::now() + Duration::seconds(1))
            .await
            .unwrap(),
        2
    );
    assert!(f.store.threads(chat.id, false).await.unwrap().is_empty());
    assert_eq!(f.store.threads(chat.id, true).await.unwrap().len(), 2);
    f.store
        .post_message(deploy.id, "Done.".into(), f.frontend.id)
        .await
        .unwrap();
    let open = f.store.threads(chat.id, false).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, deploy.id);
    assert!(!open[0].archived);
    // Archived threads stay searchable.
    assert_eq!(
        f.store
            .search_threads(None, "pizza", 10)
            .await
            .unwrap()
            .len(),
        1
    );
}
