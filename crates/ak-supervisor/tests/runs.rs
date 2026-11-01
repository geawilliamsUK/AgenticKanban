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
use ak_core::run::{ExitReason, RunEventKind, RunTrigger, SkipReason};
use ak_core::usage::{UsageSnapshot, Window};
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
            stop_grace: Duration::from_secs(2),
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
    let outcome = stored.outcome.expect("an outcome is tallied");
    assert_eq!(outcome.tool_calls["mcp__board__inbox_list"], 1);
    assert_eq!(outcome.board_tool_calls(), 1);
    assert_eq!(outcome.tool_errors, 0);
    assert!(outcome.result.is_some(), "the final text is kept");
    // The rate-limit event fed the tracker and the run's usage record.
    let usage = stored.usage.expect("usage is recorded");
    assert_eq!(usage.model.as_deref(), Some("fake-model"));
    assert!(usage.solo, "nothing else was running");
    assert_eq!(usage.after[&Window::FiveHour].used, 0.25);
    assert_eq!(
        usage.before[&Window::FiveHour].used,
        0.25,
        "first report as baseline"
    );
    let latest = f
        .supervisor
        .usage()
        .latest()
        .expect("the tracker has a report");
    assert_eq!(latest.used(Window::Week, Utc::now()), Some(0.5));
    assert!(f.paths.usage_file().exists(), "the report is saved");
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
        "-p --input-format stream-json",
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
    assert!(
        args["stdin"]
            .as_str()
            .unwrap()
            .starts_with("You were woken by"),
        "the wake prompt goes in on stdin"
    );

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
    let since = Utc::now() - chrono::Duration::hours(1);
    let decisions = store.scheduler_decisions_since(since).await.unwrap();
    assert!(
        decisions.iter().any(|d| d.reason == SkipReason::Cooldown
            && d.agent_id == f.agent.actor_id
            && d.detail.contains("wake item")),
        "the cooldown skip is recorded: {decisions:?}"
    );
    // A second tick with the same reason records nothing new.
    scheduler.tick().await.unwrap();
    assert_eq!(
        store.scheduler_decisions_since(since).await.unwrap().len(),
        decisions.len()
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
    assert!(
        store
            .scheduler_decisions_since(since)
            .await
            .unwrap()
            .iter()
            .any(|d| d.reason == SkipReason::Halted),
        "the halted skip is recorded"
    );
    scheduler.resume(f.agent.actor_id).await.unwrap();
    assert_eq!(
        store.agent(f.agent.actor_id).await.unwrap().state,
        AgentState::Idle
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_holds_runs_past_a_usage_threshold() {
    let f = fixture().await;
    let store = f.project.store();
    let scheduler = Scheduler::spawn(
        f.project.clone(),
        f.supervisor.clone(),
        Arc::new(Semaphore::new(2)),
        GlobalSettings::default(),
    );
    scheduler.set_paused(true);
    // The 5-hour window at 93%, past the default 90% hold.
    f.supervisor.usage().observe(
        UsageSnapshot::from_rate_limit_info(
            &serde_json::json!({"status": "allowed", "unifiedWindows": {
                "five_hour": {"utilization": 0.93, "resetsAt": 1893456000}}}),
            Utc::now(),
        )
        .unwrap(),
    );
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
    scheduler.set_paused(false);
    scheduler.tick().await.unwrap();
    assert!(store.active_runs().await.unwrap().is_empty(), "held");
    let decisions = store
        .scheduler_decisions_since(Utc::now() - chrono::Duration::hours(1))
        .await
        .unwrap();
    let held = decisions
        .iter()
        .find(|d| d.reason == SkipReason::UsageLimit)
        .expect("the hold is recorded");
    assert!(
        held.detail.contains("5-hour window is at 93%"),
        "{}",
        held.detail
    );

    // Turning the hold off lets the run start.
    let mut settings = GlobalSettings::default();
    settings.usage_holds.five_hour_pct = 0;
    scheduler.update_settings(settings);
    scheduler.tick().await.unwrap();
    assert_eq!(
        store
            .runs_for_agent(f.agent.actor_id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    scheduler.stop_all();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_activation_request_wakes_the_agent_with_its_reason() {
    let f = fixture().await;
    let store = f.project.store();
    let scheduler = Scheduler::spawn(
        f.project.clone(),
        f.supervisor.clone(),
        Arc::new(Semaphore::new(2)),
        GlobalSettings::default(),
    );
    let mut notices = f.supervisor.subscribe();
    // A request for another project is not this scheduler's.
    f.supervisor.bridge().request_activation(
        ak_core::ids::ProjectId::new(),
        f.agent.actor_id,
        f.project.human.id,
        "not ours".into(),
    );
    f.supervisor.bridge().request_activation(
        f.project.id(),
        f.agent.actor_id,
        f.project.human.id,
        "@me asked: pick up the auth tickets".into(),
    );
    let ended = loop {
        match tokio::time::timeout(Duration::from_secs(20), notices.recv()).await {
            Ok(Ok(RunNotice::Ended { run, .. })) => break run,
            Ok(Ok(_)) => {}
            other => panic!("no run: {other:?}"),
        }
    };
    match &ended.trigger {
        RunTrigger::Activated { by, reason } => {
            assert_eq!(*by, f.project.human.id);
            assert_eq!(reason, "@me asked: pick up the auth tickets");
        }
        other => panic!("trigger {other:?}"),
    }
    let wake =
        std::fs::read_to_string(f.paths.run_dir(f.project.id(), ended.id).join("wake.md")).unwrap();
    assert!(wake.contains("pick up the auth tickets"), "{wake}");
    assert_eq!(
        store
            .runs_for_agent(f.agent.actor_id, 10)
            .await
            .unwrap()
            .len(),
        1,
        "one activation, one run"
    );
    scheduler.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_desk_can_halt_and_resume_an_agent_through_the_bridge() {
    let f = fixture().await;
    let store = f.project.store();
    let scheduler = Scheduler::spawn(
        f.project.clone(),
        f.supervisor.clone(),
        Arc::new(Semaphore::new(2)),
        GlobalSettings::default(),
    );
    let wait_for = |state: AgentState| {
        let store = store.clone();
        let id = f.agent.actor_id;
        async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                if store.agent(id).await.unwrap().state == state {
                    return;
                }
                assert!(tokio::time::Instant::now() < deadline, "never {state:?}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    f.supervisor
        .bridge()
        .control_agent(f.project.id(), f.agent.actor_id, true);
    wait_for(AgentState::Halted).await;
    f.supervisor
        .bridge()
        .control_agent(f.project.id(), f.agent.actor_id, false);
    wait_for(AgentState::Idle).await;
    scheduler.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_agent_hears_a_message_mid_run() {
    let f = fixture().await;
    let mut notices = f.supervisor.subscribe();
    let run_spec = spec(&f, Duration::from_secs(30)).await;
    set_mode(&run_spec, "steer");
    let handle = f.supervisor.start_run(run_spec).await.unwrap();
    // Wait until it is busy with its first step.
    loop {
        match tokio::time::timeout(Duration::from_secs(10), notices.recv()).await {
            Ok(Ok(RunNotice::Event { event, .. })) if event.kind == RunEventKind::ToolCall => break,
            Ok(Ok(_)) => {}
            other => panic!("no tool call: {other:?}"),
        }
    }
    assert!(
        handle
            .message("skip the tests, ship it".into(), "me".into(), true)
            .await,
        "delivered"
    );
    let run = handle.wait().await;
    assert_eq!(
        run.exit_reason,
        Some(ExitReason::Completed),
        "{:?}",
        run.error
    );
    let events = f.project.store().run_events(run.id).await.unwrap();
    let said: Vec<String> = events.iter().map(|e| e.summary()).collect();
    assert!(
        said.iter()
            .any(|s| s.contains("interrupted for a message from @me: skip the tests")),
        "{said:?}"
    );
    assert!(
        said.iter().any(|s| s.contains(
            "Heard: Message from @me while you work (your last step was interrupted for it): skip the tests"
        )),
        "{said:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_ignores_the_interrupt_is_killed_after_the_grace() {
    let f = fixture().await;
    let run_spec = spec(&f, Duration::from_secs(60)).await;
    set_mode(&run_spec, "stubborn");
    let mut handle = f.supervisor.start_run(run_spec).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = std::time::Instant::now();
    handle.stop(StopReason::Stopped);
    let run = handle.wait().await;
    assert_eq!(run.exit_reason, Some(ExitReason::Halted));
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "it got its grace"
    );
    let events = f.project.store().run_events(run.id).await.unwrap();
    assert!(events
        .iter()
        .any(|e| e.summary().contains("did not stop within 2 seconds")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_through_the_bridge_reaches_the_running_agent() {
    let f = fixture().await;
    let scheduler = Scheduler::spawn(
        f.project.clone(),
        f.supervisor.clone(),
        Arc::new(Semaphore::new(2)),
        GlobalSettings::default(),
    );
    let mut notices = f.supervisor.subscribe();
    // The scheduler starts the run in the agent's worktree; set its mode there.
    let base = workspace::default_base(&f.project.entry.path);
    let ws = workspace::ensure(&f.project.entry.path, &base, "backend")
        .await
        .unwrap();
    std::fs::write(ws.path.join("fake_claude_mode.txt"), "steer").unwrap();
    scheduler.activate(f.agent.actor_id, f.project.human.id, "try it".into());
    loop {
        match tokio::time::timeout(Duration::from_secs(20), notices.recv()).await {
            Ok(Ok(RunNotice::Event { event, .. })) if event.kind == RunEventKind::ToolCall => break,
            Ok(Ok(_)) => {}
            other => panic!("no tool call: {other:?}"),
        }
    }
    f.supervisor.bridge().message_agent(
        f.project.id(),
        f.agent.actor_id,
        "me".into(),
        "use the staging database".into(),
        false,
    );
    let ended = loop {
        match tokio::time::timeout(Duration::from_secs(20), notices.recv()).await {
            Ok(Ok(RunNotice::Ended { run, .. })) => break run,
            Ok(Ok(_)) => {}
            other => panic!("run did not end: {other:?}"),
        }
    };
    assert_eq!(
        ended.exit_reason,
        Some(ExitReason::Completed),
        "{:?}",
        ended.error
    );
    let events = f.project.store().run_events(ended.id).await.unwrap();
    assert!(events.iter().any(|e| e
        .summary()
        .contains("Heard: Message from @me while you work: use the staging database")));
    assert!(
        !scheduler
            .message(f.agent.actor_id, "late".into(), "me".into(), false)
            .await,
        "nothing to deliver to once the run is over"
    );
    scheduler.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_past_its_share_of_the_five_hour_window_waits() {
    let f = fixture().await;
    let store = f.project.store();
    // Cap @backend at 5% of the window.
    let mut def = f.project.agents.read_definition("backend").unwrap();
    def.frontmatter.budgets = Some(ak_core::agent::Budgets {
        max_five_hour_pct: Some(5),
        ..Default::default()
    });
    f.project.agents.write_definition(&def).unwrap();
    ak_runtime::agents::reconcile(&f.project.agents, f.project.store.as_ref())
        .await
        .unwrap();

    let now = Utc::now();
    let tracker = f.supervisor.usage();
    tracker.observe(
        UsageSnapshot::from_rate_limit_info(
            &serde_json::json!({"status": "allowed", "unifiedWindows": {
                "five_hour": {"utilization": 0.3, "resetsAt": (now + chrono::Duration::hours(2)).timestamp()}}}),
            now,
        )
        .unwrap(),
    );
    // 2% of the window per dollar, and a $4 run this period: about 8%.
    let sample = ak_core::usage::RunUsage {
        model: None,
        before: [(
            Window::FiveHour,
            ak_core::usage::WindowUsage {
                used: 0.1,
                resets_at: None,
            },
        )]
        .into(),
        after: [(
            Window::FiveHour,
            ak_core::usage::WindowUsage {
                used: 0.14,
                resets_at: None,
            },
        )]
        .into(),
        solo: true,
    };
    tracker.set_calibration(ak_core::usage::Calibration::fit([(&sample, 2.0)]));
    let mut earlier = ak_core::run::AgentRun::start(
        f.agent.actor_id,
        RunTrigger::Manual,
        "w".into(),
        now - chrono::Duration::minutes(40),
    );
    earlier.cost_usd = Some(4.0);
    earlier.ended_at = Some(now - chrono::Duration::minutes(10));
    earlier.exit_reason = Some(ExitReason::Completed);
    store.insert_run(&earlier).await.unwrap();

    let scheduler = Scheduler::spawn(
        f.project.clone(),
        f.supervisor.clone(),
        Arc::new(Semaphore::new(2)),
        GlobalSettings::default(),
    );
    scheduler.set_paused(true);
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
    scheduler.set_paused(false);
    scheduler.tick().await.unwrap();
    assert!(store.active_runs().await.unwrap().is_empty(), "held");
    let held = store
        .scheduler_decisions_since(now - chrono::Duration::hours(1))
        .await
        .unwrap()
        .into_iter()
        .find(|d| d.reason == SkipReason::UsageLimit)
        .expect("the cap is recorded");
    assert!(
        held.detail
            .contains("has used about 8% of this 5-hour window, its cap is 5%"),
        "{}",
        held.detail
    );
    scheduler.shutdown();
}
