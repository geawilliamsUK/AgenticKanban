//! A Desk session against the real Claude Code binary, on Haiku: a reply,
//! a board tool call through the bridge, a local slash command, and a
//! resumed session after the process closes. Ignored by default because it
//! spends real model calls (a few cents). Uses the binary from the app's own
//! settings when they name one. Run deliberately:
//!
//! ```bash
//! cargo test -p ak-supervisor --test live_desk -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use ak_bridge::{Bridge, BridgeEvent};
use ak_core::desk::{DeskEntry, DeskEntryKind, DeskSession};
use ak_runtime::{claude, config, HomePaths, ProjectRegistry};
use ak_supervisor::{workspace, DeskHost, DeskNotice, Supervisor, SupervisorConfig};
use chrono::Utc;

async fn turn(notices: &mut broadcast::Receiver<DeskNotice>) -> Vec<DeskEntry> {
    let mut entries = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(180), notices.recv()).await {
            Ok(Ok(DeskNotice::Entry { entry, .. })) => {
                println!("  {:<12} {}", entry.kind.as_str(), short(&entry));
                let done = entry.kind == DeskEntryKind::TurnEnd;
                entries.push(entry);
                if done {
                    return entries;
                }
            }
            Ok(Ok(_)) => {}
            other => panic!("no turn end: {other:?}"),
        }
    }
}

fn short(e: &DeskEntry) -> String {
    let s = e.payload.to_string();
    s.chars().take(160).collect()
}

fn reply(entries: &[DeskEntry]) -> String {
    entries
        .iter()
        .filter(|e| e.kind == DeskEntryKind::Assistant)
        .map(|e| e.text("text").to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spends real Claude Code calls"]
async fn desk_session_with_real_claude_code() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# proj\n").unwrap();
    workspace::git(&repo, &["init", "-q", "-b", "main"])
        .await
        .unwrap();

    let paths = HomePaths::at(tmp.path().join("home"));
    let mut registry = ProjectRegistry::load(paths.clone()).unwrap();
    let id = registry.add_project(&repo, Some("Demo".into())).unwrap();
    let project = registry.open(id).await.unwrap();
    let store = project.store();

    let bridge = Bridge::start().await.unwrap();
    {
        let mut events = bridge.subscribe();
        tokio::spawn(async move {
            while let Ok(e) = events.recv().await {
                if let BridgeEvent::ApprovalRequested(a) = e {
                    println!("APPROVAL requested (left to expire): {}", a.summary);
                }
            }
        });
    }
    let configured = HomePaths::resolve()
        .ok()
        .and_then(|p| config::load_home_config(&p.config_file()).ok())
        .and_then(|c| c.settings.claude_binary);
    let supervisor = Arc::new(Supervisor::new(
        bridge,
        paths,
        SupervisorConfig {
            binary: claude::locate(configured.as_deref()).map_err(|e| e.to_string()),
            pass_api_key: false,
            startup_timeout: Duration::from_secs(120),
        },
    ));
    let desk = DeskHost::new(supervisor.clone());
    let mut notices = desk.subscribe();
    let session = DeskSession::new(Some("haiku".into()), Utc::now());
    store.insert_desk_session(&session).await.unwrap();

    println!("--- turn 1: a board tool call");
    desk.send(
        project.clone(),
        session.id,
        "Call the mcp__board__team_list tool, then reply with exactly the handle of the human \
         it lists, without the @, and nothing else."
            .into(),
    )
    .await
    .unwrap();
    let entries = turn(&mut notices).await;
    assert!(
        entries.iter().any(|e| e.kind == DeskEntryKind::ToolCall
            && e.text("name") == "mcp__board__team_list"),
        "the board tool was called"
    );
    assert!(reply(&entries).contains("me"), "{}", reply(&entries));
    let end = entries.last().unwrap();
    assert_eq!(end.payload["is_error"], false, "{}", end.payload);
    assert!(end.payload["cost_usd"].as_f64().unwrap() > 0.0);

    println!("--- turn 2: /context");
    desk.send(project.clone(), session.id, "/context".into())
        .await
        .unwrap();
    let entries = turn(&mut notices).await;
    assert!(reply(&entries).contains("Context"), "{}", reply(&entries));

    println!("--- turn 3: after closing, the session resumes");
    desk.close(session.id);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while desk.is_live(session.id) {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    desk.send(
        project.clone(),
        session.id,
        "What handle did you reply with in your first answer? Reply with it alone.".into(),
    )
    .await
    .unwrap();
    let entries = turn(&mut notices).await;
    assert!(reply(&entries).contains("me"), "{}", reply(&entries));

    println!("--- turn 4: the Desk adds a teammate with its own tools");
    desk.send(
        project.clone(),
        session.id,
        "Add a QA agent to the team now, handle qa, description \"Tests every change before it is \
         merged.\", on the haiku model. I approve; just do it and reply done."
            .into(),
    )
    .await
    .unwrap();
    let entries = turn(&mut notices).await;
    assert!(
        entries.iter().any(|e| e.kind == DeskEntryKind::ToolCall
            && e.text("name") == "mcp__board__agent_create"),
        "the Desk used agent_create"
    );
    let qa = store
        .agent_by_handle("qa")
        .await
        .unwrap()
        .expect("@qa was created");
    assert_eq!(qa.model.as_deref(), Some("haiku"));
    assert!(!project.agents.agent_dir("qa").join("..").join("me").exists());

    let saved = store.desk_session(session.id).await.unwrap();
    println!(
        "session: title {:?}, cost ${:.4}, started {}",
        saved.title, saved.cost_usd, saved.started
    );
    assert!(saved.started);
    assert!(supervisor.usage().latest().is_some(), "usage was reported");
    desk.close_all();
}
