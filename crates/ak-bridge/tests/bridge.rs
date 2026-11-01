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
use ak_core::{Actor, ActorKind, NotificationKind};
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
        desk_memory: None,
        policy_file: None,
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

    // Right version, written the way the board prints it, column by kind.
    let (err, text) = call(
        &f,
        "ticket_move",
        json!({"key": "DEM-1", "version": format!("v{}", ticket.version), "column": "in_progress"}),
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
            tool_input: json!({"command": "git push origin main"}),
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

#[tokio::test]
async fn chat_tools_round_trip_and_respect_membership() {
    let f = fixture(1, Duration::from_secs(1)).await;
    let store = f.project.store();
    let other = Actor {
        id: ActorId::new(),
        kind: ActorKind::Agent,
        handle: "frontend".into(),
        display_name: "Frontend".into(),
    };
    store.upsert_actor(&other).await.unwrap();

    let (err, text) = call(&f, "chat_list", json!({})).await;
    assert!(!err);
    assert!(text.contains("no chats yet"), "{text}");
    let (err, text) = call(&f, "chat_direct", json!({"handle": "frontend"})).await;
    assert!(!err, "{text}");
    let chat_id = text.split_whitespace().nth(1).unwrap().to_string();
    let (err, text) = call(
        &f,
        "thread_create",
        json!({"chat_id": chat_id, "title": "Hashing", "body": "bcrypt or argon2?"}),
    )
    .await;
    assert!(!err, "{text}");
    let thread_id = text.split_whitespace().nth(2).unwrap().to_string();

    // The other member was woken by the first message.
    let inbox = store.notifications(other.id, true).await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert!(inbox[0].wake);
    assert_eq!(inbox[0].kind, NotificationKind::Messaged);

    // Their reply lands in my inbox with thread context; posting handles it.
    store
        .post_message(thread_id.parse().unwrap(), "argon2".into(), other.id)
        .await
        .unwrap();
    assert_eq!(
        store
            .notifications(f.agent.actor_id, true)
            .await
            .unwrap()
            .len(),
        1
    );
    let (_, text) = call(&f, "inbox_list", json!({})).await;
    assert!(
        text.contains("thread:") && text.contains("Hashing"),
        "{text}"
    );
    let (err, text) = call(&f, "thread_read", json!({"thread_id": thread_id})).await;
    assert!(!err, "{text}");
    assert!(
        text.contains("argon2") && text.contains("@frontend"),
        "{text}"
    );
    let (err, text) = call(
        &f,
        "thread_post",
        json!({"thread_id": thread_id, "body": "argon2 it is"}),
    )
    .await;
    assert!(!err, "{text}");
    assert!(
        store
            .notifications(f.agent.actor_id, true)
            .await
            .unwrap()
            .is_empty(),
        "posting handled the notification"
    );
    let (_, text) = call(&f, "thread_search", json!({"query": "argon2"})).await;
    assert!(text.contains("Hashing"), "{text}");
    let (_, text) = call(&f, "thread_list", json!({"chat_id": chat_id})).await;
    assert!(text.contains("Hashing"), "{text}");
    let (_, text) = call(&f, "chat_list", json!({})).await;
    assert!(text.contains("direct with @frontend"), "{text}");

    // A chat I am not in is closed to me.
    let group = store
        .create_group_chat("ops".into(), vec![other.id], f.project.human.id)
        .await
        .unwrap();
    let (err, text) = call(&f, "thread_list", json!({"chat_id": group.id.to_string()})).await;
    assert!(err);
    assert!(text.contains("not a member"), "{text}");
    let (err, text) = call(&f, "thread_read", json!({"thread_id": "nonsense"})).await;
    assert!(err);
    assert!(text.contains("not a thread id"), "{text}");
}

#[tokio::test]
async fn repeated_identical_calls_are_denied_and_stop_the_run() {
    let f = fixture(1, Duration::from_secs(1)).await;
    let ctx = f.bridge.run(&f.token).unwrap();
    let mut events = f.bridge.subscribe();
    let same = PreToolUseInput {
        tool_name: "Read".into(),
        tool_input: json!({"file_path": "a.rs"}),
    };
    for _ in 0..4 {
        let v = decide(&f.bridge, &ctx, &same).await;
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
    }
    // A different call resets the streak.
    let other = PreToolUseInput {
        tool_name: "Read".into(),
        tool_input: json!({"file_path": "b.rs"}),
    };
    decide(&f.bridge, &ctx, &other).await;
    for _ in 0..4 {
        let v = decide(&f.bridge, &ctx, &same).await;
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow");
    }
    let v = decide(&f.bridge, &ctx, &same).await;
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains("guardrail"));
    match events.try_recv() {
        Ok(BridgeEvent::RunGuardrail { run_id, reason, .. }) => {
            assert_eq!(run_id, ctx.run_id);
            assert!(reason.contains("5 times"), "{reason}");
        }
        other => panic!("expected a guardrail event, got {other:?}"),
    }
}

#[tokio::test]
async fn a_desk_session_acts_as_the_human_with_its_own_memory() {
    let f = fixture(0, Duration::from_secs(5)).await;
    let memory = f.project.entry.path.join("desk").join("memory.md");
    let human = f.project.human.clone();
    let token = f.bridge.register_run(RunContext {
        project: f.project.clone(),
        agent: Agent::stand_in_for(&human, memory.clone()),
        run_id: RunId::new(),
        profile: f.project.policy().permission_profiles[0].clone(),
        desk_memory: Some(memory.clone()),
        policy_file: None,
    });
    let desk = Fixture {
        bridge: f.bridge.clone(),
        token,
        project: f.project.clone(),
        agent: f.agent.clone(),
        _tmp: tempfile::tempdir().unwrap(),
    };

    // A ticket the Desk creates is the human's, and assigning it wakes the agent.
    let (err, text) = call(
        &desk,
        "ticket_create",
        json!({"title": "Plan the auth rework", "column": "To do", "assignee": "backend"}),
    )
    .await;
    assert!(!err, "{text}");
    let store = f.project.store();
    let ticket = store.tickets().await.unwrap().pop().unwrap();
    assert_eq!(ticket.reporter_id, human.id);
    assert!(store
        .notifications(f.agent.actor_id, true)
        .await
        .unwrap()
        .iter()
        .any(|n| n.wake));

    // Memory lives in the Desk's own file, not under agents/.
    let (err, _) = call(
        &desk,
        "memory_update",
        json!({"content": "- sprint goal: auth"}),
    )
    .await;
    assert!(!err);
    assert_eq!(
        std::fs::read_to_string(&memory).unwrap(),
        "- sprint goal: auth"
    );
    let (_, text) = call(&desk, "memory_read", json!({})).await;
    assert!(text.contains("sprint goal"));
    assert!(!f
        .project
        .entry
        .path
        .join("agents")
        .join(&human.handle)
        .exists());

    // Never held at stop, even with the human's inbox full.
    let ctx = desk.bridge.run(&desk.token).unwrap();
    assert_eq!(stop_decision(&ctx).await, json!({}));
}

#[tokio::test]
async fn agent_activate_asks_the_scheduler_and_refuses_people_and_self() {
    let f = fixture(0, Duration::from_secs(5)).await;
    let mut events = f.bridge.subscribe();

    // An agent cannot wake a human or itself.
    let (err, text) = call(&f, "agent_activate", json!({"handle": "me", "reason": "x"})).await;
    assert!(err && text.contains("not an agent"), "{text}");
    let (err, text) = call(
        &f,
        "agent_activate",
        json!({"handle": "backend", "reason": "x"}),
    )
    .await;
    assert!(err && text.contains("yourself"), "{text}");

    // The human, from the Desk, wakes the agent with a reason.
    let memory = f.project.entry.path.join("desk").join("memory.md");
    let human = f.project.human.clone();
    let token = f.bridge.register_run(RunContext {
        project: f.project.clone(),
        agent: Agent::stand_in_for(&human, memory.clone()),
        run_id: RunId::new(),
        profile: f.project.policy().permission_profiles[0].clone(),
        desk_memory: Some(memory),
        policy_file: None,
    });
    let desk = Fixture {
        bridge: f.bridge.clone(),
        token,
        project: f.project.clone(),
        agent: f.agent.clone(),
        _tmp: tempfile::tempdir().unwrap(),
    };
    let (err, text) = call(
        &desk,
        "agent_activate",
        json!({"handle": "backend", "reason": "  "}),
    )
    .await;
    assert!(err && text.contains("give a reason"), "{text}");
    let (err, text) = call(
        &desk,
        "agent_activate",
        json!({"handle": "backend", "reason": "pick up the auth tickets"}),
    )
    .await;
    assert!(!err, "{text}");
    let event = loop {
        match events.try_recv() {
            Ok(BridgeEvent::ActivationRequested {
                project,
                agent_id,
                by,
                reason,
            }) => break (project, agent_id, by, reason),
            Ok(_) => continue,
            Err(e) => panic!("no activation event: {e:?}"),
        }
    };
    assert_eq!(event.0, f.project.id());
    assert_eq!(event.1, f.agent.actor_id);
    assert_eq!(event.2, human.id);
    assert_eq!(event.3, "@me asked: pick up the auth tickets");
}

#[tokio::test]
async fn ask_human_posts_a_marked_question_on_the_ticket() {
    let f = fixture(0, Duration::from_secs(5)).await;
    let (err, _) = call(
        &f,
        "ticket_create",
        json!({"title": "Flags output", "column": "To do"}),
    )
    .await;
    assert!(!err);
    let (err, text) = call(&f, "ask_human", json!({"question": "A or B?"})).await;
    assert!(err && text.contains("ticket"), "{text}");
    let (err, text) = call(
        &f,
        "ask_human",
        json!({"key": "DEM-1", "question": "Print flags every line, or only on change?"}),
    )
    .await;
    assert!(!err, "{text}");
    let store = f.project.store();
    let ticket = store.ticket_by_key("DEM-1").await.unwrap().unwrap();
    let comments = store.comments(ticket.id).await.unwrap();
    let body = &comments.last().unwrap().body;
    assert!(body.starts_with("**Question for @me:**"), "{body}");
    assert!(ak_core::notification::needs_human(body, "me", false));
    // It reaches the human as a mention.
    assert!(store
        .notifications(f.project.human.id, true)
        .await
        .unwrap()
        .iter()
        .any(|n| n.kind == NotificationKind::Mentioned));
}

#[tokio::test]
async fn the_desk_manages_the_team_and_the_settings_and_agents_cannot() {
    let f = fixture(0, Duration::from_secs(5)).await;
    let mut events = f.bridge.subscribe();

    // Agents never see the team tools, and cannot call them.
    let (_, v) = rpc(
        &f,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    let names: Vec<String> = v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(!names.contains(&"agent_create".to_string()));
    let (err, _) = call(
        &f,
        "agent_create",
        json!({"handle": "x", "description": "x"}),
    )
    .await;
    assert!(err);

    let memory = f.project.entry.path.join("desk").join("memory.md");
    let policy_file = f.project.entry.path.join("project.toml");
    let human = f.project.human.clone();
    let token = f.bridge.register_run(RunContext {
        project: f.project.clone(),
        agent: Agent::stand_in_for(&human, memory.clone()),
        run_id: RunId::new(),
        profile: f.project.policy().permission_profiles[0].clone(),
        desk_memory: Some(memory),
        policy_file: Some(policy_file.clone()),
    });
    let desk = Fixture {
        bridge: f.bridge.clone(),
        token,
        project: f.project.clone(),
        agent: f.agent.clone(),
        _tmp: tempfile::tempdir().unwrap(),
    };
    let (_, v) = rpc(
        &desk,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    )
    .await;
    assert!(v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "project_settings_update"));

    // A lead from the preset, then a specialist, assignable at once.
    let (err, text) = call(
        &desk,
        "agent_create",
        json!({"handle": "lead", "preset": "lead", "description": "Runs the team.", "model": "sonnet"}),
    )
    .await;
    assert!(!err, "{text}");
    let (err, text) = call(
        &desk,
        "agent_create",
        json!({"handle": "@QA", "description": "Tests everything.", "permission_profile": "trusted"}),
    )
    .await;
    assert!(!err, "{text}");
    let store = f.project.store();
    let qa = store
        .agent_by_handle("qa")
        .await
        .unwrap()
        .expect("qa exists");
    assert_eq!(qa.permission_profile, "trusted");
    assert_eq!(
        store
            .agent_by_handle("lead")
            .await
            .unwrap()
            .unwrap()
            .model
            .as_deref(),
        Some("sonnet")
    );
    let (err, _) = call(
        &desk,
        "agent_create",
        json!({"handle": "qa", "description": "again"}),
    )
    .await;
    assert!(err, "no duplicates");
    let (err, text) = call(
        &desk,
        "agent_create",
        json!({"handle": "x", "description": "x", "permission_profile": "nope"}),
    )
    .await;
    assert!(err && text.contains("no permission profile"), "{text}");

    // Edits, budgets, memory, reading back.
    let (err, _) = call(
        &desk,
        "agent_update",
        json!({"handle": "qa", "model": "haiku", "max_usd_per_run": 2.5}),
    )
    .await;
    assert!(!err);
    let (err, _) = call(
        &desk,
        "agent_memory_update",
        json!({"handle": "qa", "content": "- run cargo test"}),
    )
    .await;
    assert!(!err);
    let (_, text) = call(&desk, "agent_get", json!({"handle": "qa"})).await;
    assert!(text.contains("model: haiku"), "{text}");
    assert!(text.contains("max_usd_per_run: 2.5"), "{text}");
    assert!(text.contains("- run cargo test"), "{text}");

    // Halting goes to the scheduler.
    let (err, _) = call(&desk, "agent_halt", json!({"handle": "qa"})).await;
    assert!(!err);
    let mut halted = false;
    while let Ok(e) = events.try_recv() {
        if let BridgeEvent::AgentControl {
            agent_id,
            halt: true,
            ..
        } = e
        {
            halted = agent_id == qa.actor_id;
        }
    }
    assert!(halted);

    // Settings: merged, checked, saved and announced.
    let (err, text) = call(
        &desk,
        "project_settings_update",
        json!({"changes": {"guardrails": {"max_usd_per_run": 4.0}, "default_model": "sonnet"}}),
    )
    .await;
    assert!(!err, "{text}");
    assert_eq!(f.project.policy().guardrails.max_usd_per_run, 4.0);
    assert!(std::fs::read_to_string(&policy_file)
        .unwrap()
        .contains("max_usd_per_run = 4.0"));
    assert!(std::iter::from_fn(|| events.try_recv().ok())
        .any(|e| matches!(e, BridgeEvent::PolicyChanged { .. })));
    let (err, text) = call(
        &desk,
        "project_settings_update",
        json!({"changes": {"permission_profiles": []}}),
    )
    .await;
    assert!(
        err && text.contains("which these settings remove"),
        "{text}"
    );
    let (_, text) = call(&desk, "project_settings_get", json!({})).await;
    assert!(text.contains("default_model = \"sonnet\""), "{text}");

    // Retiring.
    let (err, _) = call(&desk, "agent_retire", json!({"handle": "qa"})).await;
    assert!(!err);
    assert!(store
        .agent_by_handle("qa")
        .await
        .unwrap()
        .unwrap()
        .is_retired());
}

#[tokio::test]
async fn usage_get_reports_the_windows_and_who_used_them() {
    let f = fixture(0, Duration::from_secs(5)).await;
    let (_, text) = call(&f, "usage_get", json!({})).await;
    assert!(text.contains("not tracked"), "{text}");

    let tmp = tempfile::tempdir().unwrap();
    let tracker = Arc::new(ak_runtime::UsageTracker::load(
        tmp.path().join("usage.json"),
    ));
    let now = chrono::Utc::now();
    let resets = (now + chrono::Duration::hours(2)).timestamp();
    tracker.observe(
        ak_core::usage::UsageSnapshot::from_rate_limit_info(
            &json!({"status": "allowed", "unifiedWindows": {
                "five_hour": {"utilization": 0.62, "resetsAt": resets},
                "seven_day": {"utilization": 0.8, "resetsAt": resets + 3 * 86400}}}),
            now,
        )
        .unwrap(),
    );
    // A learned rate: 2% of the 5-hour window per dollar.
    let sample = ak_core::usage::RunUsage {
        model: Some("sonnet".into()),
        before: [(
            ak_core::usage::Window::FiveHour,
            ak_core::usage::WindowUsage {
                used: 0.1,
                resets_at: None,
            },
        )]
        .into(),
        after: [(
            ak_core::usage::Window::FiveHour,
            ak_core::usage::WindowUsage {
                used: 0.14,
                resets_at: None,
            },
        )]
        .into(),
        solo: true,
    };
    tracker.set_calibration(ak_core::usage::Calibration::fit([(&sample, 2.0)]));
    f.bridge.set_usage(tracker);
    let mut run = ak_core::run::AgentRun::start(
        f.agent.actor_id,
        ak_core::run::RunTrigger::Manual,
        "w".into(),
        now - chrono::Duration::minutes(30),
    );
    run.cost_usd = Some(1.5);
    run.ended_at = Some(now);
    f.project.store().insert_run(&run).await.unwrap();

    let (err, text) = call(&f, "usage_get", json!({})).await;
    assert!(!err, "{text}");
    assert!(text.contains("5-hour: 62% used"), "{text}");
    assert!(text.contains("new runs hold at 90%"), "{text}");
    assert!(text.contains("Week: 80% used"), "{text}");
    assert!(text.contains("@backend: ≈3.0% of the 5-hour"), "{text}");
}

#[tokio::test]
async fn a_ticket_branch_merge_waits_for_review_when_the_project_asks() {
    // The integrator profile allows merging.
    let f = fixture(2, Duration::from_millis(300)).await;
    let ctx = f.bridge.run(&f.token).unwrap();
    let merge = PreToolUseInput {
        tool_name: "Bash".into(),
        tool_input: json!({"command": "git checkout develop && git merge --no-ff ticket/DEM-4"}),
    };
    let v = decide(&f.bridge, &ctx, &merge).await;
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow", "{v}");

    let mut policy = f.project.policy();
    policy.review_merges = true;
    *f.project.policy.write().unwrap() = policy;
    let mut events = f.bridge.subscribe();
    let v = decide(&f.bridge, &ctx, &merge).await;
    // Nobody answered within the timeout, so the merge did not happen.
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny", "{v}");
    let Ok(BridgeEvent::ApprovalRequested(a)) = events.try_recv() else {
        panic!("the merge asked for review");
    };
    assert!(a.summary.contains("ticket/DEM-4"), "{}", a.summary);

    // Other merges and other commands are judged by the profile as before.
    let v = decide(
        &f.bridge,
        &ctx,
        &PreToolUseInput {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "git merge --abort"}),
        },
    )
    .await;
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "allow", "{v}");
}
