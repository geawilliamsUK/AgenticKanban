//! The human asks an agent a question in a direct thread; the agent, woken
//! by the message, answers in the thread. Ignored by default: it spends a
//! real model call (capped at $1). Run deliberately:
//!
//! ```bash
//! cargo test -p ak-supervisor --test live_chat -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use ak_bridge::{Bridge, BridgeEvent};
use ak_core::agent::{AgentDefinition, Budgets};
use ak_core::config::GlobalSettings;
use ak_core::run::ExitReason;
use ak_runtime::{claude, HomePaths, ProjectRegistry};
use ak_supervisor::supervisor::RunNotice;
use ak_supervisor::{workspace, Scheduler, Supervisor, SupervisorConfig};

const BODY: &str = "# dev

You are @dev, the developer on this project. You answer questions about the repository \
briefly and accurately.

**You own:** every file in this repository.

**You review:** nothing.

**You never:** change files when only asked a question.

**Done means:** the question is answered where it was asked, in one or two lines.
";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spends real Claude Code calls"]
async fn agent_answers_a_direct_message() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# proj\n").unwrap();
    std::fs::write(repo.join("greeting.txt"), "hello there\n").unwrap();
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

    let mut def = AgentDefinition::template("dev", "Developer who knows this repository");
    def.frontmatter.model = Some("sonnet".into());
    def.frontmatter.permission_profile = "trusted".into();
    def.frontmatter.budgets = Some(Budgets {
        max_turns_per_run: Some(30),
        max_usd_per_run: Some(1.0),
        max_usd_per_day: None,
        max_wall_clock_minutes: Some(6),
    });
    def.body = BODY.into();
    project.agents.write_definition(&def).unwrap();
    ak_runtime::agents::reconcile(&project.agents, project.store.as_ref())
        .await
        .unwrap();
    let agent = store.agent_by_handle("dev").await.unwrap().unwrap();
    let human = project.human.id;

    let bridge = Bridge::start().await.unwrap();
    {
        let bridge = bridge.clone();
        let store = store.clone();
        let mut events = bridge.subscribe();
        tokio::spawn(async move {
            while let Ok(e) = events.recv().await {
                if let BridgeEvent::ApprovalRequested(a) = e {
                    println!("APPROVAL requested: {}  -> approving", a.summary);
                    let _ = bridge.decide(store.as_ref(), a.id, true, human).await;
                }
            }
        });
    }
    let supervisor = Arc::new(Supervisor::new(
        bridge.clone(),
        paths.clone(),
        SupervisorConfig {
            binary: claude::locate(None).map_err(|e| e.to_string()),
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

    // The human asks in a direct thread; the message wakes the agent.
    let chat = store
        .direct_chat(human, agent.actor_id, human)
        .await
        .unwrap();
    let (thread, _) = store
        .create_thread(
            chat.id,
            "Quick question".into(),
            "@dev which file in this repository holds the greeting text? Answer here in one \
             line, then stop."
                .into(),
            human,
        )
        .await
        .unwrap();
    scheduler.tick().await.unwrap();

    let started = std::time::Instant::now();
    let ended = loop {
        match tokio::time::timeout(Duration::from_secs(8 * 60), notices.recv()).await {
            Ok(Ok(RunNotice::Started { run, .. })) => println!("RUN started {}", run.id),
            Ok(Ok(RunNotice::Event { event, .. })) => println!(
                "[{:>5.1}s] {:<14} {}",
                started.elapsed().as_secs_f32(),
                event.kind.as_str(),
                event.summary()
            ),
            Ok(Ok(RunNotice::Ended { run, .. })) => break run,
            other => panic!("run did not end: {other:?}"),
        }
    };
    scheduler.shutdown();
    println!(
        "exit: {:?}  turns: {}  cost: {:?}  error: {:?}",
        ended.exit_reason, ended.num_turns, ended.cost_usd, ended.error
    );

    let messages = store.messages(thread.id).await.unwrap();
    for m in &messages {
        println!("message by {}: {}", m.author_id, m.body);
    }
    assert_eq!(
        ended.exit_reason,
        Some(ExitReason::Completed),
        "{:?}",
        ended.error
    );
    assert!(messages.len() >= 2, "the agent answered in the thread");
    let answer = messages
        .iter()
        .rev()
        .find(|m| m.author_id == agent.actor_id);
    let answer = answer.expect("an agent message");
    assert!(
        answer.body.contains("greeting.txt"),
        "named the file: {}",
        answer.body
    );
    assert!(
        store
            .notifications(agent.actor_id, true)
            .await
            .unwrap()
            .is_empty(),
        "posting handled the wake"
    );
    let status = workspace::git(
        &workspace::default_base(&repo).join("dev"),
        &["status", "--porcelain"],
    )
    .await
    .unwrap();
    assert!(
        status.trim().is_empty(),
        "a question changed nothing: {status}"
    );
}
