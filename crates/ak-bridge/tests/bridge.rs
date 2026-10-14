//! Drives the bridge router without a network: MCP tools as an agent, and the
//! two hooks.

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use ak_bridge::hooks::{decide, stop_decision, PreToolUseInput};
use ak_bridge::{Bridge, BridgeEvent, RunContext};
use ak_core::agent::{Agent, AgentDefinition};
use ak_core::approval::ApprovalState;
use ak_core::commands::NewTicket;
use ak_core::config::{ProjectEntry, ProjectPolicy};
use ak_core::ids::{ActorId, ProjectId, RunId};
use ak_core::NotificationKind;
use ak_runtime::{AgentFiles, OpenProject};
use ak_store::{HumanIdentity, SqliteStore, Store};

struct Fixture {
    bridge: Bridge,
    token: String,
    project: Arc<OpenProject>,
    agent: Agent,
    _tmp: tempfile::TempDir,
}

async fn fixture(profile_index: usize, approval_timeout: Duration) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteStore::open_in_memory().await.unwrap());
    let policy = ProjectPolicy::new("Demo");
    let human = store
        .bootstrap(&policy, &HumanIdentity::default())
        .await
        .unwrap();
    let files = AgentFiles::new(tmp.path().join("agents"));
    let def = AgentDefinition::template("backend", "Backend developer");
    files.write_definition(&def).unwrap();
    let agent = Agent::from_definition(
        ActorId::new(),
        &def,
        files.definition_path("backend"),
        files.memory_path("backend"),
    );
    store.upsert_agent(&agent).await.unwrap();

    let profile = policy.permission_profiles[profile_index].clone();
    let project = Arc::new(OpenProject {
        entry: ProjectEntry {
            id: ProjectId::new(),
            name: "Demo".into(),
            path: tmp.path().to_path_buf(),
            worktree_base: None,
        },
        policy: RwLock::new(policy),
        store,
        human,
        agents: files,
    });
    let bridge = Bridge::with_addr(
        "127.0.0.1:1".parse::<SocketAddr>().unwrap(),
        approval_timeout,
    );
    let token = bridge.register_run(RunContext {
        project: project.clone(),
        agent: agent.clone(),
        run_id: RunId::new(),
        profile,
    });
    Fixture {
        bridge,
        token,
        project,
        agent,
        _tmp: tmp,
    }
}

async fn rpc(f: &Fixture, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/mcp/{}", f.token))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = f.bridge.router().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn call(f: &Fixture, tool: &str, args: Value) -> (bool, String) {
    let (status, v) = rpc(
        f,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let result = &v["result"];
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    (is_error, text)
}

#[tokio::test]
async fn initialize_and_list_tools() {
    let f = fixture(0, Duration::from_secs(5)).await;
    let (status, v) = rpc(
        &f,
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    assert!(v["result"]["capabilities"]["tools"].is_object());

    // Notifications get no body.
    let (status, v) = rpc(
        &f,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(v.is_null());

    let (_, v) = rpc(
        &f,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .await;
    let names: Vec<&str> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "inbox_list",
        "ticket_create",
        "ticket_move",
        "memory_update",
        "team_list",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    let create = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "ticket_create")
        .unwrap();
    assert!(create["inputSchema"]["properties"]["title"].is_object());

    let (_, v) = rpc(&f, json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).await;
    assert_eq!(v["error"]["code"], -32601);
}

#[tokio::test]
async fn unknown_token_is_rejected() {
    let f = fixture(0, Duration::from_secs(5)).await;
    let req = Request::builder()
        .method("POST")
        .uri("/mcp/not-a-token")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = f.bridge.router().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn agent_works_a_ticket_through_the_tools() {
    let f = fixture(1, Duration::from_secs(5)).await;
    let store = f.project.store();

    let (err, text) = call(
        &f,
        "ticket_create",
        json!({"title": "Add login", "description": "cc @me", "priority": "high", "labels": ["auth"]}),
    )
    .await;
    assert!(!err, "{text}");
    assert!(text.contains("DEM-1"), "{text}");
    let ticket = store.ticket_by_key("dem-1").await.unwrap().unwrap();
    assert_eq!(
        ticket.reporter_id, f.agent.actor_id,
        "the agent is the reporter"
    );
    assert_eq!(ticket.priority, ak_core::Priority::High);

    // The human was mentioned in the description.
    let human_inbox = store.notifications(f.project.human.id, true).await.unwrap();
    assert_eq!(human_inbox.len(), 1);
    assert_eq!(human_inbox[0].kind, NotificationKind::Mentioned);

    // Wrong version: a clear, retryable error.
    let (err, text) = call(
        &f,
        "ticket_move",
        json!({"key": "DEM-1", "version": 7, "column": "In progress"}),
    )
    .await;
    assert!(err);
    assert!(text.contains("version conflict"), "{text}");

    // Right version, column by kind.
    let (err, text) = call(
        &f,
        "ticket_move",
        json!({"key": "DEM-1", "version": ticket.version, "column": "in_progress"}),
    )
    .await;
    assert!(!err, "{text}");
    assert!(text.contains("In progress"), "{text}");

    let (err, text) = call(
        &f,
        "ticket_comment",
        json!({"key": "DEM-1", "body": "Started. @me please review when done."}),
    )
    .await;
    assert!(!err, "{text}");
    let comment_id = text
        .rsplit("comment id ")
        .next()
        .unwrap()
        .trim_end_matches(')');
    let (err, text) = call(
        &f,
        "ticket_reply",
        json!({"key": "DEM-1", "comment_id": comment_id, "body": "self-note"}),
    )
    .await;
    assert!(!err, "{text}");

    let (err, text) = call(&f, "ticket_get", json!({"key": "DEM-1"})).await;
    assert!(!err);
    assert!(text.contains("## Comments (2)"), "{text}");
    assert!(text.contains("moved DEM-1"), "{text}");

    let (err, text) = call(&f, "ticket_list", json!({"column": "in_progress"})).await;
    assert!(!err);
    assert!(text.contains("DEM-1"));
    let (_, text) = call(
        &f,
        "ticket_list",
        json!({"assignee": "unassigned", "text": "login"}),
    )
    .await;
    assert!(text.contains("DEM-1"));
    let (_, text) = call(&f, "ticket_list", json!({"column": "done"})).await;
    assert_eq!(text, "no tickets match");

    let (err, text) = call(&f, "team_list", json!({})).await;
    assert!(!err);
    assert!(text.contains("@me (human)"));
    assert!(text.contains("@backend (agent, idle)"));

    let (_, text) = call(&f, "memory_read", json!({})).await;
    assert!(text.contains("empty"));
    let (err, _) = call(
        &f,
        "memory_update",
        json!({"content": "# Memory\n- login lives in auth/\n"}),
    )
    .await;
    assert!(!err);
    assert!(f
        .project
        .agents
        .read_memory("backend")
        .unwrap()
        .contains("auth/"));

    let (err, text) = call(
        &f,
        "ticket_move",
        json!({"key": "DEM-9", "version": 1, "column": "done"}),
    )
    .await;
    assert!(err);
    assert!(text.contains("no ticket with key"));
    let (err, text) = call(
        &f,
        "ticket_move",
        json!({"key": "DEM-1", "version": 2, "column": "Nowhere"}),
    )
    .await;
    assert!(err);
    assert!(text.contains("columns are:"), "{text}");
}

#[tokio::test]
async fn inbox_lists_wake_items_and_mutations_handle_them() {
    let f = fixture(1, Duration::from_secs(5)).await;
    let store = f.project.store();
    let human = f.project.human.id;
    let columns = store.columns().await.unwrap();

    // The human assigns a ticket to the agent: a wake notification.
    let t = store
        .create_ticket(
            NewTicket {
                title: "Fix it".into(),
                column_id: columns[1].id,
                assignee_id: Some(f.agent.actor_id),
                ..Default::default()
            },
            human,
        )
        .await
        .unwrap();
    let (err, text) = call(&f, "inbox_list", json!({})).await;
    assert!(!err);
    assert!(text.contains("1 notification"), "{text}");
    assert!(text.contains("assigned (woke you)"), "{text}");

    // The stop hook blocks while it is pending.
    let ctx = f.bridge.run(&f.token).unwrap();
    let v = stop_decision(&ctx).await;
    assert_eq!(v["decision"], "block");
    assert!(
        v["reason"].as_str().unwrap().contains("created DEM-1"),
        "{v}"
    );

    // Moving the ticket handles the notification; the stop is then allowed.
    let (err, _) = call(
        &f,
        "ticket_move",
        json!({"key": t.key, "version": t.version, "column": "review"}),
    )
    .await;
    assert!(!err);
    let (_, text) = call(&f, "inbox_list", json!({})).await;
    assert!(text.contains("inbox empty"), "{text}");
    let v = stop_decision(&ctx).await;
    assert_eq!(v, json!({}));
    let handled = store.notifications(f.agent.actor_id, false).await.unwrap();
    assert_eq!(handled[0].handled_in_run_id, Some(ctx.run_id));

    // A comment that mentions the agent, handled explicitly.
    store
        .comment(t.id, "@backend thoughts?".into(), None, human)
        .await
        .unwrap();
    let (_, text) = call(&f, "inbox_list", json!({})).await;
    let id = text
        .lines()
        .find_map(|l| l.strip_prefix("- id "))
        .map(|rest| rest.split(' ').next().unwrap().to_string())
        .unwrap();
    let (err, text) = call(&f, "inbox_handle", json!({"notification_ids": [id]})).await;
    assert!(!err);
    assert_eq!(text, "handled 1 notification(s)");
}

#[tokio::test]
async fn pre_tool_use_follows_the_profile_and_asks_the_human() {
    // Supervised profile: Read allowed, destructive denied, everything else asks.
    let f = fixture(0, Duration::from_secs(5)).await;
    let ctx = f.bridge.run(&f.token).unwrap();

    let v = decide(
        &f.bridge,
        &ctx,
        &PreToolUseInput {
            tool_name: "Read".into(),
            tool_input: json!({"file_path": "x"}),
        },
    )
    .await;
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");

    let v = decide(
        &f.bridge,
        &ctx,
        &PreToolUseInput {
            tool_name: "mcp__board__ticket_move".into(),
            tool_input: json!({}),
        },
    )
    .await;
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");

    let v = decide(
        &f.bridge,
        &ctx,
        &PreToolUseInput {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "rm -rf target"}),
        },
    )
    .await;
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");

    // Something not covered: an approval is raised; the human approves it.
    let mut events = f.bridge.subscribe();
    let bridge = f.bridge.clone();
    let ctx2 = ctx.clone();
    let human = f.project.human.id;
    let decider = tokio::spawn(async move {
        let BridgeEvent::ApprovalRequested(a) = events.recv().await.unwrap() else {
            panic!("expected a request");
        };
        assert_eq!(a.tool_name, "Bash");
        assert!(a.summary.contains("git push"));
        bridge
            .decide(ctx2.store().as_ref(), a.id, true, human)
            .await
            .unwrap()
    });
    let v = decide(
        &f.bridge,
        &ctx,
        &PreToolUseInput {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "git push origin main"}),
        },
    )
    .await;
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"], "allow",
        "{v}"
    );
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecisionReason"],
        "approved by the human"
    );
    let decided = decider.await.unwrap();
    assert_eq!(decided.state, ApprovalState::Approved);
    assert_eq!(decided.decided_by, Some(human));
    assert!(f
        .project
        .store()
        .pending_approvals()
        .await
        .unwrap()
        .is_empty());

    // Denied by the human.
    let bridge = f.bridge.clone();
    let ctx2 = ctx.clone();
    let mut events = f.bridge.subscribe();
    tokio::spawn(async move {
        if let Ok(BridgeEvent::ApprovalRequested(a)) = events.recv().await {
            bridge
                .decide(ctx2.store().as_ref(), a.id, false, human)
                .await
                .unwrap();
        }
    });
    let v = decide(
        &f.bridge,
        &ctx,
        &PreToolUseInput {
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "a"}),
        },
    )
    .await;
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[tokio::test]
async fn unanswered_approvals_expire_and_deny() {
    let f = fixture(0, Duration::from_millis(200)).await;
    let ctx = f.bridge.run(&f.token).unwrap();
    let v = decide(
        &f.bridge,
        &ctx,
        &PreToolUseInput {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "cargo build"}),
        },
    )
    .await;
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains("no decision"));
    let all = f.project.store().pending_approvals().await.unwrap();
    assert!(all.is_empty(), "expired approvals are no longer pending");
}

#[tokio::test]
async fn hook_endpoints_answer_over_http_shapes() {
    let f = fixture(1, Duration::from_secs(5)).await;
    let req = Request::builder()
        .method("POST")
        .uri(format!("/hooks/{}/pre-tool-use", f.token))
        .header("content-type", "application/json")
        .body(Body::from(json!({"session_id": "s", "hook_event_name": "PreToolUse", "tool_name": "Read", "tool_input": {"file_path": "a"}}).to_string()))
        .unwrap();
    let resp = f.bridge.router().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");

    let req = Request::builder()
        .method("POST")
        .uri(format!("/hooks/{}/stop", f.token))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"session_id": "s", "hook_event_name": "Stop", "stop_hook_active": false})
                .to_string(),
        ))
        .unwrap();
    let resp = f.bridge.router().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v, json!({}));

    // A finished run denies tools and allows stopping.
    f.bridge.unregister_run(&f.token);
    let req = Request::builder()
        .method("POST")
        .uri(format!("/hooks/{}/pre-tool-use", f.token))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"tool_name": "Read", "tool_input": {}}).to_string(),
        ))
        .unwrap();
    let resp = f.bridge.router().oneshot(req).await.unwrap();
    let v: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[tokio::test]
async fn run_configs_point_at_the_bridge() {
    let f = fixture(0, Duration::from_secs(60)).await;
    let mcp = f.bridge.mcp_config(&f.token);
    assert_eq!(mcp["mcpServers"]["board"]["type"], "http");
    assert!(mcp["mcpServers"]["board"]["url"]
        .as_str()
        .unwrap()
        .ends_with(&format!("/mcp/{}", f.token)));
    let hooks = f.bridge.hook_settings(&f.token);
    assert!(hooks["hooks"]["PreToolUse"][0]["hooks"][0]["url"]
        .as_str()
        .unwrap()
        .contains("pre-tool-use"));
    assert_eq!(hooks["hooks"]["Stop"][0]["hooks"][0]["timeout"], 30);
}
