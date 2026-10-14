//! End-to-end runs against the fake Claude Code binary.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Semaphore;

use ak_bridge::Bridge;
use ak_core::agent::{AgentDefinition, AgentState};
use ak_core::commands::NewTicket;
use ak_core::config::{GlobalSettings, PermissionProfile};
use ak_core::run::{ExitReason, RunEventKind, RunTrigger};
use ak_core::NotificationKind;
use ak_runtime::{HomePaths, OpenProject, ProjectRegistry};
use ak_supervisor::supervisor::{RunNotice, StopReason};
use ak_supervisor::{
    workspace, GuardrailMonitor, RunSpec, Scheduler, Supervisor, SupervisorConfig,
};
use chrono::Utc;

fn fake_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_claude"))
}

struct Fixture {
    _tmp: tempfile::TempDir,
    paths: HomePaths,
    project: Arc<OpenProject>,
    agent: ak_core::agent::Agent,
    supervisor: Arc<Supervisor>,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    workspace::git(&repo, &["init", "-q", "-b", "main"])
        .await
        .unwrap();
    workspace::git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    )
    .await
    .unwrap();

    let paths = HomePaths::at(tmp.path().join("home"));
    let mut registry = ProjectRegistry::load(paths.clone()).unwrap();
    let id = registry.add_project(&repo, Some("Demo".into())).unwrap();
    // The agent definition goes in before open so reconciliation registers it.
    let project_tmp = registry.open(id).await.unwrap();
    project_tmp
        .agents
        .write_definition(&AgentDefinition::template("backend", "Backend developer"))
        .unwrap();
    ak_runtime::agents::reconcile(&project_tmp.agents, project_tmp.store.as_ref())
        .await
        .unwrap();
    let project = project_tmp;
    let agent = project
        .store()
        .agent_by_handle("backend")
        .await
        .unwrap()
        .unwrap();

    let bridge = Bridge::with_addr(
        "127.0.0.1:1".parse::<SocketAddr>().unwrap(),
        Duration::from_secs(5),
    );
    let supervisor = Arc::new(Supervisor::new(
        bridge,
        paths.clone(),
        SupervisorConfig {
            binary: Ok(fake_binary()),
            pass_api_key: false,
            startup_timeout: Duration::from_secs(5),
        },
    ));
    Fixture {
        _tmp: tmp,
        paths,
        project,
        agent,
        supervisor,
    }
}

async fn spec(f: &Fixture, wall_clock: Duration) -> RunSpec {
    let base = workspace::default_base(&f.project.entry.path);
    let ws = workspace::ensure(&f.project.entry.path, &base, "backend")
        .await
        .unwrap();
    RunSpec {
        project: f.project.clone(),
        agent: f.agent.clone(),
        trigger: RunTrigger::Manual,
        trigger_lines: vec!["@me assigned DEM-1 to you".into()],
        profile: PermissionProfile::defaults()[1].clone(),
        model: Some("sonnet".into()),
        max_turns: 50,
        max_usd: 2.5,
        wall_clock,
        workspace: ws,
    }
}

fn read_args(ws: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(ws.join("fake_claude_args.json")).unwrap())
        .unwrap()
}

/// Tells the fake binary how to behave for this run. A file in the
/// workspace, because tests run in parallel and would share an env var.
fn set_mode(run_spec: &RunSpec, mode: &str) {
    std::fs::write(run_spec.workspace.path.join("fake_claude_mode.txt"), mode).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_completes_and_is_recorded() {
    let f = fixture().await;
    let mut notices = f.supervisor.subscribe();
    let run_spec = spec(&f, Duration::from_secs(30)).await;
    let ws_path = run_spec.workspace.path.clone();
    let handle = f.supervisor.start_run(run_spec).await.unwrap();
    let handle_run_id = handle.run_id;
    let run = handle.wait().await;
    assert_eq!(handle_run_id, run.id, "the handle names the run it started");

    assert_eq!(
        run.exit_reason,
        Some(ExitReason::Completed),
        "{:?}",
        run.error
    );
    assert!(run.claude_session_id.is_some());
    assert_eq!(run.num_turns, 2);
    assert_eq!(run.input_tokens, 120);
    assert!((run.cost_usd.unwrap() - 0.0123).abs() < 1e-9);

    let store = f.project.store();
    let stored = store.run(run.id).await.unwrap();
    // Timestamps are stored at microsecond precision, so compare the rest.
    assert_eq!(stored.exit_reason, run.exit_reason);
    assert_eq!(stored.claude_session_id, run.claude_session_id);
    assert_eq!(stored.num_turns, run.num_turns);
    assert_eq!(stored.cost_usd, run.cost_usd);
    assert!(stored.ended_at.is_some());
    let events = store.run_events(run.id).await.unwrap();
    let kinds: Vec<RunEventKind> = events.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&RunEventKind::SystemInit));
    assert!(kinds.contains(&RunEventKind::AssistantText));
    assert!(kinds.contains(&RunEventKind::ToolCall));
    assert!(kinds.contains(&RunEventKind::ToolResult));
    assert!(kinds.contains(&RunEventKind::Result));
    assert_eq!(
        store.agent(f.agent.actor_id).await.unwrap().state,
        AgentState::Idle
    );

    // The process ran in the worktree with the right flags and files.
    let args = read_args(&ws_path);
    assert_eq!(
        Path::new(args["cwd"].as_str().unwrap()).canonicalize().ok(),
        ws_path.canonicalize().ok()
    );
    let argv: Vec<&str> = args["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    let joined = argv.join(" ");
    for needle in [
        "-p You were woken by",
        "--output-format stream-json",
        "--permission-mode acceptEdits",
        "--max-turns 50",
        "--max-budget-usd 2.50",
        "--strict-mcp-config",
        "--model sonnet",
        "--allowedTools",
        "--disallowedTools",
        "--append-system-prompt-file",
    ] {
        assert!(joined.contains(needle), "missing {needle:?} in {joined}");
    }
    assert!(args["env_api_key"].is_null(), "the API key is scrubbed");

    let run_dir = f.paths.run_dir(f.project.id(), run.id);
    let system = std::fs::read_to_string(run_dir.join("system.md")).unwrap();
    assert!(system.contains("**You own:**"));
    assert!(system.contains("## How you work on this board"));
    assert!(system.contains("@me (human)"));
    let mcp: Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("mcp.json")).unwrap()).unwrap();
    assert!(mcp["mcpServers"]["board"]["url"]
        .as_str()
        .unwrap()
        .contains("/mcp/"));
    assert!(run_dir.join("stdout.jsonl").exists());

    // Notices arrived in order.
    let mut saw_started = false;
    let mut saw_ended = false;
    while let Ok(n) = notices.try_recv() {
        match n {
            RunNotice::Started { .. } => saw_started = true,
            RunNotice::Ended { run: r, .. } => {
                saw_ended = true;
                assert_eq!(r.id, run.id);
            }
            RunNotice::Event { .. } => {}
        }
    }
    assert!(saw_started && saw_ended);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hanging_run_times_out() {
    let f = fixture().await;
    let run_spec = spec(&f, Duration::from_secs(1)).await;
    set_mode(&run_spec, "hang");
    let run = f.supervisor.start_run(run_spec).await.unwrap().wait().await;
    assert_eq!(run.exit_reason, Some(ExitReason::TimedOut));
    assert!(run.error.unwrap().contains("wall clock"));
    assert_eq!(
        f.project
            .store()
            .agent(f.agent.actor_id)
            .await
            .unwrap()
            .state,
        AgentState::Idle
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_and_an_api_error_are_reported() {
    let f = fixture().await;
    let run_spec = spec(&f, Duration::from_secs(10)).await;
    set_mode(&run_spec, "crash");
    let run = f.supervisor.start_run(run_spec).await.unwrap().wait().await;
    assert_eq!(run.exit_reason, Some(ExitReason::Crashed));
    assert!(
        run.error.as_deref().unwrap().contains("without a result"),
        "{:?}",
        run.error
    );
    assert!(
        run.error
            .as_deref()
            .unwrap()
            .contains("something went wrong"),
        "{:?}",
        run.error
    );

    let run_spec = spec(&f, Duration::from_secs(10)).await;
    set_mode(&run_spec, "error");
    let run = f.supervisor.start_run(run_spec).await.unwrap().wait().await;
    assert_eq!(run.exit_reason, Some(ExitReason::Crashed));
    assert_eq!(run.error.as_deref(), Some("Failed to authenticate"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_can_be_stopped() {
    let f = fixture().await;
    let run_spec = spec(&f, Duration::from_secs(60)).await;
    set_mode(&run_spec, "hang");
    let mut handle = f.supervisor.start_run(run_spec).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    handle.stop(StopReason::Stopped);
    let run = handle.wait().await;
    assert_eq!(run.exit_reason, Some(ExitReason::Halted));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_wakes_on_assignment_and_cools_down_on_no_progress() {
    let f = fixture().await;
    let store = f.project.store();
    let scheduler = Scheduler::spawn(
        f.project.clone(),
        f.supervisor.clone(),
        Arc::new(Semaphore::new(2)),
        GlobalSettings::default(),
    );
    let mut notices = f.supervisor.subscribe();

    // Nothing pending: a tick starts nothing.
    scheduler.tick().await.unwrap();
    assert!(store.active_runs().await.unwrap().is_empty());

    // Assigning a ticket is a wake notification.
    let columns = store.columns().await.unwrap();
    store
        .create_ticket(
            NewTicket {
                title: "Do it".into(),
                column_id: columns[1].id,
                assignee_id: Some(f.agent.actor_id),
                ..Default::default()
            },
            f.project.human.id,
        )
        .await
        .unwrap();
    scheduler.tick().await.unwrap();

    let ended = loop {
        match tokio::time::timeout(Duration::from_secs(20), notices.recv()).await {
            Ok(Ok(RunNotice::Ended { run, .. })) => break run,
            Ok(Ok(_)) => {}
            other => panic!("run did not end: {other:?}"),
        }
    };
    assert_eq!(ended.exit_reason, Some(ExitReason::Completed));
    assert!(matches!(ended.trigger, RunTrigger::Notifications { ref ids } if ids.len() == 1));
    let run_dir = f.paths.run_dir(f.project.id(), ended.id);
    let wake = std::fs::read_to_string(run_dir.join("wake.md")).unwrap();
    assert!(wake.contains("Do it"), "{wake}");

    // The fake handled nothing, so the agent cools down instead of looping.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let a = store.agent(f.agent.actor_id).await.unwrap();
        if a.state == AgentState::Cooldown {
            assert!(a.cooldown_until.is_some());
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "agent state is {:?}",
            a.state
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    scheduler.tick().await.unwrap();
    assert!(
        store.active_runs().await.unwrap().is_empty(),
        "no run while cooling down"
    );
    assert_eq!(
        store
            .runs_for_agent(f.agent.actor_id, 10)
            .await
            .unwrap()
            .len(),
        3,
        "three strikes before the cooldown"
    );
    let trips = store
        .guardrail_trips_since(Utc::now() - chrono::Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(trips.len(), 1);
    assert!(f
        .project
        .store()
        .notifications(f.project.human.id, true)
        .await
        .unwrap()
        .iter()
        .any(|n| n.kind == NotificationKind::Guardrail));

    // Resume clears the cooldown and a halted agent never starts.
    scheduler.halt(f.agent.actor_id).await.unwrap();
    scheduler.tick().await.unwrap();
    assert!(store.active_runs().await.unwrap().is_empty());
    assert_eq!(
        store.agent(f.agent.actor_id).await.unwrap().state,
        AgentState::Halted
    );
    scheduler.resume(f.agent.actor_id).await.unwrap();
    assert_eq!(
        store.agent(f.agent.actor_id).await.unwrap().state,
        AgentState::Idle
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guardrail_monitor_freezes_a_ping_pong_ticket() {
    let f = fixture().await;
    let store = f.project.store();
    let _monitor = GuardrailMonitor::spawn(f.project.clone());
    let columns = store.columns().await.unwrap();
    let ticket = store
        .create_ticket(
            NewTicket {
                title: "Hot potato".into(),
                column_id: columns[1].id,
                ..Default::default()
            },
            f.project.human.id,
        )
        .await
        .unwrap();

    // An agent moves it back and forth until the guardrail freezes it.
    let mut version = ticket.version;
    for i in 0..8 {
        let to = if i % 2 == 0 {
            columns[2].id
        } else {
            columns[1].id
        };
        match store
            .move_ticket(ticket.id, to, version, f.agent.actor_id)
            .await
        {
            Ok(t) => version = t.version,
            Err(e) => {
                assert!(e.to_string().contains("frozen"), "{e}");
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let t = store.ticket(ticket.id).await.unwrap();
        if t.frozen {
            assert!(
                t.freeze_reason
                    .as_deref()
                    .unwrap_or("")
                    .contains("ping-pong"),
                "{:?}",
                t.freeze_reason
            );
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "never froze");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let inbox = store.notifications(f.project.human.id, true).await.unwrap();
    assert!(inbox.iter().any(|n| n.kind == NotificationKind::Guardrail));
    let trips = store
        .guardrail_trips_since(Utc::now() - chrono::Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(
        trips.len(),
        1,
        "one trip, then the frozen ticket is left alone"
    );
}
