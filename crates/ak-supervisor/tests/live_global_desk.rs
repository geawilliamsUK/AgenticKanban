//! The Global Desk against the real Claude Code binary, on Haiku: one turn
//! that reads two projects' boards through their own servers. Ignored by
//! default because it spends real model calls (a few cents). Run
//! deliberately:
//!
//! ```bash
//! cargo test -p ak-supervisor --test live_global_desk -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use ak_bridge::Bridge;
use ak_core::commands::NewTicket;
use ak_core::desk::{DeskEntryKind, DeskSession};
use ak_runtime::{claude, config, HomePaths, ProjectRegistry};
use ak_store::{SqliteStore, Store};
use ak_supervisor::{workspace, DeskHost, DeskNotice, DeskScope, Supervisor, SupervisorConfig};
use chrono::Utc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spends real Claude Code calls"]
async fn global_desk_with_real_claude_code() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = HomePaths::at(tmp.path().join("home"));
    let mut registry = ProjectRegistry::load(paths.clone()).unwrap();
    let mut projects = Vec::new();
    for (name, title) in [("Web App", "Fix the PELICAN page"), ("Api", "Rate-limit the OTTER route")] {
        let repo = tmp.path().join(name);
        std::fs::create_dir_all(&repo).unwrap();
        workspace::git(&repo, &["init", "-q", "-b", "main"])
            .await
            .unwrap();
        let id = registry.add_project(&repo, Some(name.into())).unwrap();
        let p = registry.open(id).await.unwrap();
        let columns = p.store().columns().await.unwrap();
        p.store()
            .create_ticket(
                NewTicket {
                    title: title.into(),
                    column_id: columns[0].id,
                    ..Default::default()
                },
                p.human.id,
            )
            .await
            .unwrap();
        projects.push(p);
    }

    let configured = HomePaths::resolve()
        .ok()
        .and_then(|p| config::load_home_config(&p.config_file()).ok())
        .and_then(|c| c.settings.claude_binary);
    let supervisor = Arc::new(Supervisor::new(
        Bridge::start().await.unwrap(),
        paths.clone(),
        SupervisorConfig {
            binary: claude::locate(configured.as_deref()).map_err(|e| e.to_string()),
            pass_api_key: false,
            startup_timeout: Duration::from_secs(120),
            stop_grace: Duration::from_secs(10),
        },
    ));
    let desk = DeskHost::new(supervisor.clone());
    let home = paths.global_desk_dir();
    std::fs::create_dir_all(&home).unwrap();
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&home.join("desk.db")).await.unwrap());
    let session = DeskSession::new(Some("haiku".into()), Utc::now());
    store.insert_desk_session(&session).await.unwrap();

    let mut notices = desk.subscribe();
    desk.send(
        DeskScope::Global { store, projects },
        session.id,
        "Call ticket_list on each project's board, then reply with only the one capitalised \
         animal word from each ticket title, separated by a space."
            .into(),
    )
    .await
    .unwrap();
    let mut tools = Vec::new();
    let mut reply = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(180), notices.recv()).await {
            Ok(Ok(DeskNotice::Entry { entry, .. })) => {
                println!("  {:<12} {}", entry.kind.as_str(), entry.payload.to_string().chars().take(160).collect::<String>());
                match entry.kind {
                    DeskEntryKind::ToolCall => tools.push(entry.text("name").to_string()),
                    DeskEntryKind::Assistant => reply.push_str(entry.text("text")),
                    DeskEntryKind::TurnEnd => {
                        assert_eq!(entry.payload["is_error"], false, "{}", entry.payload);
                        break;
                    }
                    _ => {}
                }
            }
            Ok(Ok(_)) => {}
            other => panic!("no turn end: {other:?}"),
        }
    }
    desk.close_all();
    assert!(tools.iter().any(|t| t == "mcp__board-web-app__ticket_list"), "{tools:?}");
    assert!(tools.iter().any(|t| t == "mcp__board-api__ticket_list"), "{tools:?}");
    assert!(reply.contains("PELICAN") && reply.contains("OTTER"), "{reply}");
}
