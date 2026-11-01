//! The Global Desk against the fake Claude Code binary: one session, a
//! board server for every project.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use ak_bridge::Bridge;
use ak_core::commands::NewTicket;
use ak_core::desk::{DeskEntryKind, DeskSession};
use ak_runtime::{HomePaths, OpenProject, ProjectRegistry};
use ak_store::{SqliteStore, Store};
use ak_supervisor::desk::{board_server, GLOBAL};
use ak_supervisor::{DeskHost, DeskNotice, DeskScope, Supervisor, SupervisorConfig};
use chrono::Utc;

async fn repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    ak_supervisor::workspace::git(path, &["init", "-q", "-b", "main"])
        .await
        .unwrap();
}

/// Calls a board tool through the bridge's MCP route for one token.
async fn call(bridge: &Bridge, token: &str, tool: &str, args: Value) -> String {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": tool, "arguments": args}});
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/{token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = bridge.router().oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_global_desk_reaches_every_projects_board() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = HomePaths::at(tmp.path().join("home"));
    let mut registry = ProjectRegistry::load(paths.clone()).unwrap();
    let mut projects: Vec<Arc<OpenProject>> = Vec::new();
    for name in ["Web App", "Api"] {
        let path = tmp.path().join(name);
        repo(&path).await;
        let id = registry.add_project(&path, Some(name.into())).unwrap();
        projects.push(registry.open(id).await.unwrap());
    }
    // A ticket in each, so each board answers differently.
    for (p, title) in projects.iter().zip(["Fix the login page", "Rate-limit the API"]) {
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
    }

    let bridge = Bridge::with_addr(
        "127.0.0.1:1".parse::<SocketAddr>().unwrap(),
        Duration::from_secs(5),
    );
    let supervisor = Arc::new(Supervisor::new(
        bridge.clone(),
        paths.clone(),
        SupervisorConfig {
            binary: Ok(PathBuf::from(env!("CARGO_BIN_EXE_fake_claude"))),
            pass_api_key: false,
            startup_timeout: Duration::from_secs(5),
            stop_grace: Duration::from_secs(2),
        },
    ));
    let desk = DeskHost::new(supervisor.clone());
    let home = paths.global_desk_dir();
    std::fs::create_dir_all(&home).unwrap();
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&home.join("desk.db")).await.unwrap());
    let session = DeskSession::new(None, Utc::now());
    store.insert_desk_session(&session).await.unwrap();
    let scope = DeskScope::Global {
        store: store.clone(),
        projects: projects.clone(),
    };

    let mut notices = desk.subscribe();
    desk.send(scope.clone(), session.id, "What's waiting on me?".into())
        .await
        .unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(20), notices.recv()).await {
            Ok(Ok(DeskNotice::Entry { project, entry })) => {
                assert_eq!(project, GLOBAL);
                if entry.kind == DeskEntryKind::TurnEnd {
                    break;
                }
            }
            Ok(Ok(_)) => {}
            other => panic!("no turn end: {other:?}"),
        }
    }
    // The transcript is in the Global Desk's own store, not a project's.
    assert!(store.desk_entries(session.id).await.unwrap().len() > 2);
    for p in &projects {
        assert!(p.store().desk_sessions().await.unwrap().is_empty());
    }

    // It ran in the Global Desk's folder, with only its own servers.
    let args: Vec<String> = serde_json::from_str(
        std::fs::read_to_string(home.join("fake_claude_desk_args.jsonl"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert!(args.iter().any(|a| a == "--strict-mcp-config"), "{args:?}");
    let dir = home.join("sessions").join(session.id.to_string());
    let system = std::fs::read_to_string(dir.join("system.md")).unwrap();
    assert!(system.contains("Global Desk"));
    let mcp: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
    let servers = mcp["mcpServers"].as_object().unwrap();
    assert_eq!(servers.len(), 2);

    // Each server is that project's board, acting as its human.
    let mut tokens = Vec::new();
    for (p, title) in projects.iter().zip(["Fix the login page", "Rate-limit the API"]) {
        let server = board_server(&p.entry.name);
        assert!(system.contains(&format!("mcp__{server}__")), "{server} not in prompt");
        let url = servers[&server]["url"].as_str().unwrap();
        let token = url.rsplit('/').next().unwrap().to_string();
        let ctx = bridge.run(&token).expect("registered while the session lives");
        assert_eq!(ctx.project.id(), p.id());
        assert_eq!(ctx.agent.actor_id, p.human.id);
        assert!(ctx.is_desk());
        assert_eq!(ctx.desk_memory.as_deref(), Some(home.join("memory.md").as_path()));
        let list = call(&bridge, &token, "ticket_list", json!({})).await;
        assert!(list.contains(title), "{server}: {list}");
        tokens.push(token);
    }
    assert_eq!(board_server("Web App"), "board-web-app");

    // Closing the session lets go of every board.
    desk.close(session.id);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokens.iter().any(|t| bridge.run(t).is_some()) {
        assert!(tokio::time::Instant::now() < deadline, "tokens still registered");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
