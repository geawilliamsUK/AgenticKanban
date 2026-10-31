//! Desk sessions against the fake Claude Code binary's conversation mode.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;

use ak_bridge::Bridge;
use ak_core::desk::{DeskEntry, DeskEntryKind, DeskSession};
use ak_core::usage::Window;
use ak_runtime::{HomePaths, OpenProject, ProjectRegistry};
use ak_supervisor::{DeskHost, DeskNotice, DeskStatus, Supervisor, SupervisorConfig};
use chrono::Utc;

struct Fixture {
    _tmp: tempfile::TempDir,
    project: Arc<OpenProject>,
    supervisor: Arc<Supervisor>,
    desk: Arc<DeskHost>,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    ak_supervisor::workspace::git(&repo, &["init", "-q", "-b", "main"])
        .await
        .unwrap();
    let paths = HomePaths::at(tmp.path().join("home"));
    let mut registry = ProjectRegistry::load(paths.clone()).unwrap();
    let id = registry.add_project(&repo, Some("Demo".into())).unwrap();
    let project = registry.open(id).await.unwrap();
    let supervisor = Arc::new(Supervisor::new(
        Bridge::with_addr(
            "127.0.0.1:1".parse::<SocketAddr>().unwrap(),
            Duration::from_secs(5),
        ),
        paths,
        SupervisorConfig {
            binary: Ok(PathBuf::from(env!("CARGO_BIN_EXE_fake_claude"))),
            pass_api_key: false,
            startup_timeout: Duration::from_secs(5),
        },
    ));
    let desk = DeskHost::new(supervisor.clone());
    Fixture {
        _tmp: tmp,
        project,
        supervisor,
        desk,
    }
}

/// Collects notices until a turn ends, returning its entries, the streamed
/// text and the last status.
async fn turn(
    notices: &mut broadcast::Receiver<DeskNotice>,
) -> (Vec<DeskEntry>, String, Option<DeskStatus>) {
    let mut entries = Vec::new();
    let mut streamed = String::new();
    let mut status = None;
    loop {
        match tokio::time::timeout(Duration::from_secs(20), notices.recv()).await {
            Ok(Ok(DeskNotice::Entry { entry, .. })) => {
                let done = entry.kind == DeskEntryKind::TurnEnd;
                entries.push(entry);
                if done {
                    // The status after the turn follows the TurnEnd entry.
                    while let Ok(Ok(n)) =
                        tokio::time::timeout(Duration::from_millis(500), notices.recv()).await
                    {
                        if let DeskNotice::Status { status: s, .. } = n {
                            let idle = !s.busy;
                            status = Some(s);
                            if idle {
                                break;
                            }
                        }
                    }
                    return (entries, streamed, status);
                }
            }
            Ok(Ok(DeskNotice::Delta { text, .. })) => streamed.push_str(&text),
            Ok(Ok(DeskNotice::Status { status: s, .. })) => status = Some(s),
            Ok(Ok(DeskNotice::Session { .. })) => {}
            other => panic!("no turn end: {other:?}"),
        }
    }
}

fn spawn_args(f: &Fixture) -> Vec<Vec<String>> {
    std::fs::read_to_string(f.project.entry.path.join("fake_claude_desk_args.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Vec<String>>(l).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_desk_session_converses_resumes_and_can_be_interrupted() {
    let f = fixture().await;
    let store = f.project.store();
    let session = DeskSession::new(None, Utc::now());
    store.insert_desk_session(&session).await.unwrap();
    let mut notices = f.desk.subscribe();

    // First turn: a tool call, streamed text, the reply, the cost.
    f.desk
        .send(f.project.clone(), session.id, "Plan the sprint".into())
        .await
        .unwrap();
    let (entries, streamed, status) = turn(&mut notices).await;
    let kinds: Vec<DeskEntryKind> = entries.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            DeskEntryKind::User,
            DeskEntryKind::ToolCall,
            DeskEntryKind::ToolResult,
            DeskEntryKind::Assistant,
            DeskEntryKind::TurnEnd
        ]
    );
    assert_eq!(entries[3].text("text"), "Echo: Plan the sprint");
    assert_eq!(streamed, "Echo: Plan the sprint");
    assert!((entries[4].payload["cost_usd"].as_f64().unwrap() - 0.01).abs() < 1e-9);
    let status = status.expect("a status");
    assert!(status.alive && !status.busy);
    assert_eq!(status.model.as_deref(), Some("fake-model"));
    assert!(status.slash_commands.contains(&"compact".to_string()));
    assert_eq!(status.context_window, Some(200_000));
    assert_eq!(status.context_tokens, Some(1228));

    let saved = store.desk_session(session.id).await.unwrap();
    assert_eq!(saved.title, "Plan the sprint");
    assert!(saved.started);
    assert!((saved.cost_usd - 0.01).abs() < 1e-9);
    assert_eq!(
        f.supervisor
            .usage()
            .latest()
            .and_then(|u| u.used(Window::FiveHour, Utc::now())),
        Some(0.3),
        "the Desk feeds the usage meter"
    );

    // A local command: its output comes from the result, not the replay.
    f.desk
        .send(f.project.clone(), session.id, "/context".into())
        .await
        .unwrap();
    let (entries, _, _) = turn(&mut notices).await;
    let texts: Vec<&str> = entries
        .iter()
        .filter(|e| e.kind == DeskEntryKind::Assistant)
        .map(|e| e.text("text"))
        .collect();
    assert_eq!(texts, vec!["## Context Usage\n42k / 200k"]);
    assert_eq!(entries.last().unwrap().payload["cost_usd"], 0.0);

    // Interrupt a reply in progress.
    f.desk
        .send(f.project.clone(), session.id, "slow".into())
        .await
        .unwrap();
    loop {
        if let Ok(DeskNotice::Delta { .. }) = notices.recv().await {
            break;
        }
    }
    assert!(f.desk.interrupt(session.id));
    let (entries, _, _) = turn(&mut notices).await;
    assert!(entries
        .iter()
        .any(|e| e.kind == DeskEntryKind::Notice && e.text("message") == "Interrupted"));
    let end = entries.last().unwrap();
    assert_eq!(end.payload["interrupted"], Value::Bool(true));
    assert_eq!(end.payload["is_error"], Value::Bool(false));

    // Closing ends the process; the next message resumes the same session.
    f.desk.close(session.id);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while f.desk.is_live(session.id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "process did not close"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f.desk
        .send(f.project.clone(), session.id, "Still there?".into())
        .await
        .unwrap();
    let (entries, _, _) = turn(&mut notices).await;
    assert!(entries
        .iter()
        .any(|e| e.text("text") == "Echo: Still there?"));
    let spawns = spawn_args(&f);
    assert_eq!(spawns.len(), 2);
    let joined: Vec<String> = spawns.iter().map(|a| a.join(" ")).collect();
    assert!(joined[0].contains(&format!("--session-id {}", session.claude_session_id)));
    assert!(joined[1].contains(&format!("--resume {}", session.claude_session_id)));
    for needle in [
        "--input-format stream-json",
        "--include-partial-messages",
        "--permission-mode default",
        "--append-system-prompt-file",
    ] {
        assert!(
            joined[0].contains(needle),
            "missing {needle:?} in {}",
            joined[0]
        );
    }
    let all = store.desk_entries(session.id).await.unwrap();
    assert_eq!(
        all.iter().filter(|e| e.kind == DeskEntryKind::User).count(),
        4
    );
    f.desk.close_all();
}
