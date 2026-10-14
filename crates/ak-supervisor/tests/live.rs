//! One agent completing an assigned ticket against the real Claude Code
//! binary. Ignored by default: it spends real model calls (capped at $1.50)
//! and takes a few minutes. Run deliberately:
//!
//! ```bash
//! cargo test -p ak-supervisor --test live -- --ignored --nocapture
//! ```
//!
//! The run goes through the scheduler, so the wake notification, the bridge,
//! the hooks, the worktree and the run record are all the real thing. There
//! is no human, so every approval request is approved automatically and
//! printed.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use ak_bridge::{Bridge, BridgeEvent};
use ak_core::agent::{AgentDefinition, Budgets};
use ak_core::board::ColumnKind;
use ak_core::commands::NewTicket;
use ak_core::config::GlobalSettings;
use ak_core::run::ExitReason;
use ak_runtime::{claude, HomePaths, ProjectRegistry};
use ak_supervisor::supervisor::RunNotice;
use ak_supervisor::{workspace, Scheduler, Supervisor, SupervisorConfig};

const BODY: &str = "# dev

You are @dev, the developer on this project. You make small, careful, committed changes.

**You own:** every file in this repository.

**You review:** nothing. The human reviews your branches.

**You never:** push, force anything, rewrite history, or touch files the ticket does not ask for.

**Done means:** the change is committed on the ticket branch with the ticket key in the \
message, the ticket has a short comment saying what changed and which branch it is on, and \
the ticket is in Review.
";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spends real Claude Code calls"]
async fn agent_completes_an_assigned_ticket() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("README.md"),
        "# proj\n\nA tiny repository for an agent to work in.\n",
    )
    .unwrap();
    std::fs::write(repo.join("hello.txt"), "hello\n").unwrap();
    workspace::git(&repo, &["init", "-q", "-b", "main"])
        .await
        .unwrap();
    workspace::git(&repo, &["add", "-A"]).await.unwrap();
    workspace::git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    )
    .await
    .unwrap();

    let paths = HomePaths::at(tmp.path().join("home"));
    let mut registry = ProjectRegistry::load(paths.clone()).unwrap();
    let id = registry.add_project(&repo, Some("Demo".into())).unwrap();
    let project = registry.open(id).await.unwrap();
    let store = project.store();

    let mut def = AgentDefinition::template(
        "dev",
        "Developer who makes small, careful changes to this repository",
    );
    def.frontmatter.model = Some("sonnet".into());
    def.frontmatter.permission_profile = "trusted".into();
    def.frontmatter.budgets = Some(Budgets {
        max_turns_per_run: Some(40),
        max_usd_per_run: Some(1.5),
        max_usd_per_day: None,
        max_wall_clock_minutes: Some(8),
    });
    def.body = BODY.into();
    project.agents.write_definition(&def).unwrap();
    ak_runtime::agents::reconcile(&project.agents, project.store.as_ref())
        .await
        .unwrap();
    let agent = store.agent_by_handle("dev").await.unwrap().unwrap();
    let human = project.human.id;

    let bridge = Bridge::start().await.unwrap();
    println!("bridge: {}", bridge.addr());
    {
        // No human here: approve everything, loudly.
        let bridge = bridge.clone();
        let store = store.clone();
        let mut events = bridge.subscribe();
        tokio::spawn(async move {
            while let Ok(e) = events.recv().await {
                if let BridgeEvent::ApprovalRequested(a) = e {
                    println!("APPROVAL requested: {}  -> approving", a.summary);
                    if let Err(e) = bridge.decide(store.as_ref(), a.id, true, human).await {
                        println!("approval failed: {e}");
                    }
                }
            }
        });
    }

    let binary = claude::locate(None).map_err(|e| e.to_string());
    println!("binary: {binary:?}");
    let supervisor = Arc::new(Supervisor::new(
        bridge.clone(),
        paths.clone(),
        SupervisorConfig {
            binary,
            pass_api_key: false,
            startup_timeout: Duration::from_secs(120),
        },
    ));
    let mut notices = supervisor.subscribe();
    let scheduler = Scheduler::spawn(
        project.clone(),
        supervisor.clone(),
        Arc::new(Semaphore::new(1)),
        GlobalSettings::default(),
    );

    let columns = store.columns().await.unwrap();
    let todo = columns
        .iter()
        .find(|c| c.kind == ColumnKind::Todo)
        .unwrap()
        .id;
    let ticket = store
        .create_ticket(
            NewTicket {
                title: "Add a greeting to hello.txt".into(),
                description: "Append a second line to `hello.txt` that says exactly:\n\n\
                              Hello from the board.\n\n\
                              Keep the change to that one file. Commit it with the ticket key \
                              in the message. Comment here with what changed and the branch \
                              name, then move this ticket to Review."
                    .into(),
                column_id: todo,
                assignee_id: Some(agent.actor_id),
                ..Default::default()
            },
            human,
        )
        .await
        .unwrap();
    println!("ticket: {} {}", ticket.key, ticket.title);
    scheduler.tick().await.unwrap();

    let started = std::time::Instant::now();
    let ended = loop {
        match tokio::time::timeout(Duration::from_secs(9 * 60), notices.recv()).await {
            Ok(Ok(RunNotice::Started { run, .. })) => {
                println!("RUN started {} in {}", run.id, run.workspace_path.display());
            }
            Ok(Ok(RunNotice::Event { event, .. })) => {
                println!(
                    "[{:>5.1}s] {:<14} {}",
                    started.elapsed().as_secs_f32(),
                    event.kind.as_str(),
                    event.summary()
                );
            }
            Ok(Ok(RunNotice::Ended { run, .. })) => break run,
            other => panic!("run did not end: {other:?}"),
        }
    };
    scheduler.shutdown();

    println!("----- run -----");
    println!(
        "exit: {:?}  turns: {}  tokens: {} in / {} out  cost: {:?}  error: {:?}",
        ended.exit_reason,
        ended.num_turns,
        ended.input_tokens,
        ended.output_tokens,
        ended.cost_usd,
        ended.error
    );
    let run_dir = paths.run_dir(project.id(), ended.id);
    println!("run dir: {}", run_dir.display());
    println!(
        "----- system prompt -----\n{}",
        std::fs::read_to_string(run_dir.join("system.md")).unwrap_or_default()
    );

    let ticket = store.ticket(ticket.id).await.unwrap();
    let column = columns.iter().find(|c| c.id == ticket.column_id).unwrap();
    println!(
        "----- ticket -----\n{} in {} (v{})",
        ticket.key, column.name, ticket.version
    );
    let comments = store.comments(ticket.id).await.unwrap();
    for c in &comments {
        println!("comment by {}: {}", c.author_id, c.body);
    }
    let memory = project.agents.read_memory("dev").unwrap();
    println!("----- memory -----\n{memory}");

    let ws = workspace::default_base(&repo).join("dev");
    let branches = workspace::git(&ws, &["branch", "--list"]).await.unwrap();
    let log = workspace::git(&ws, &["log", "--all", "--oneline"])
        .await
        .unwrap();
    let status = workspace::git(&ws, &["status", "--porcelain", "--branch"])
        .await
        .unwrap();
    println!("----- worktree -----\n{branches}\n{log}\n{status}");
    let hello = std::fs::read_to_string(ws.join("hello.txt")).unwrap_or_default();
    println!("hello.txt:\n{hello}");

    assert_eq!(
        ended.exit_reason,
        Some(ExitReason::Completed),
        "{:?}",
        ended.error
    );
    assert_ne!(column.kind, ColumnKind::Todo, "the ticket was moved");
    assert!(
        comments.iter().any(|c| c.author_id == agent.actor_id),
        "the agent commented"
    );
    assert!(
        hello.contains("Hello from the board"),
        "the file was changed"
    );
    assert!(log.contains(&ticket.key), "a commit mentions the ticket");
    assert!(!memory.trim().is_empty(), "memory was written");
}
