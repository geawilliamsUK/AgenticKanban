//! The whole loop with the real Claude Code binary, on Haiku: the human runs
//! `/standup` at the Desk, the Desk briefs @lead in a Standup thread, the
//! brief wakes @lead, and @lead turns it into a ticket for @dev and reports
//! back in the thread. @dev is halted so its work never starts. Ignored by
//! default because it spends real model calls (some tens of cents). Uses the
//! binary from the app's own settings when they name one. Run deliberately:
//!
//! ```bash
//! cargo test -p ak-supervisor --test live_standup -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use ak_bridge::{Bridge, BridgeEvent};
use ak_core::agent::{AgentDefinition, Budgets};
use ak_core::chat::ChatKind;
use ak_core::config::GlobalSettings;
use ak_core::desk::{standup_prompt, DeskEntryKind, DeskSession};
use ak_core::run::ExitReason;
use ak_runtime::{claude, config, HomePaths, ProjectRegistry};
use ak_supervisor::supervisor::RunNotice;
use ak_supervisor::{workspace, DeskHost, DeskNotice, Scheduler, Supervisor, SupervisorConfig};
use chrono::Utc;

const DEV: &str = "# @dev — developer

You are **@dev**, the developer on this project. You write the code and docs.

**You own:** every file in this repository.
**You review:** nothing.
**You never:** plan the team's work; that is @lead's.
**Done means:** the ticket's acceptance criteria are met and committed.
";

fn haiku(def: &mut AgentDefinition) {
    def.frontmatter.model = Some("haiku".into());
    def.frontmatter.budgets = Some(Budgets {
        max_turns_per_run: Some(40),
        max_usd_per_run: Some(1.0),
        max_usd_per_day: None,
        max_wall_clock_minutes: Some(8),
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spends real Claude Code calls"]
async fn standup_at_the_desk_briefs_the_lead_who_plans_and_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("README.md"),
        "# proj\n\nRun the tests with `cargo test`.\n",
    )
    .unwrap();
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

    let mut lead = AgentDefinition::lead();
    haiku(&mut lead);
    let mut dev = AgentDefinition::template("dev", "Developer: writes the code and docs");
    dev.body = DEV.into();
    haiku(&mut dev);
    project.agents.write_definition(&lead).unwrap();
    project.agents.write_definition(&dev).unwrap();
    ak_runtime::agents::reconcile(&project.agents, project.store.as_ref())
        .await
        .unwrap();
    let lead_agent = store.agent_by_handle("lead").await.unwrap().unwrap();
    let dev_agent = store.agent_by_handle("dev").await.unwrap().unwrap();

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
            stop_grace: Duration::from_secs(10),
        },
    ));
    let mut runs = supervisor.subscribe();
    let scheduler = Scheduler::spawn(
        project.clone(),
        supervisor.clone(),
        Arc::new(Semaphore::new(1)),
        GlobalSettings::default(),
    );
    scheduler.halt(dev_agent.actor_id).await.unwrap();

    // The standup, from the Desk.
    let desk = DeskHost::new(supervisor.clone());
    let mut desk_notices = desk.subscribe();
    let session = DeskSession::new(Some("haiku".into()), Utc::now());
    store.insert_desk_session(&session).await.unwrap();
    let notes = "Goal for today: the repository needs a CONTRIBUTING.md that explains how to \
                 run the tests. @dev should write it.";
    desk.send_as(
        project.clone(),
        session.id,
        standup_prompt(notes, Utc::now().date_naive()),
        Some(format!("/standup {notes}")),
    )
    .await
    .unwrap();
    println!("--- Desk");
    loop {
        match tokio::time::timeout(Duration::from_secs(240), desk_notices.recv()).await {
            Ok(Ok(DeskNotice::Entry { entry, .. })) => {
                let line: String = entry.payload.to_string().chars().take(200).collect();
                println!("  {:<12} {line}", entry.kind.as_str());
                if entry.kind == DeskEntryKind::TurnEnd {
                    break;
                }
            }
            Ok(Ok(_)) => {}
            other => panic!("the Desk did not finish: {other:?}"),
        }
    }

    // The brief went to @lead in a Standup thread, which wakes it.
    let human = project.human.id;
    let chats = store.chats().await.unwrap();
    let direct = chats
        .iter()
        .find(|c| c.kind == ChatKind::Direct)
        .expect("the Desk opened a direct chat");
    let members = store.chat_members(direct.id).await.unwrap();
    assert!(members.contains(&lead_agent.actor_id) && members.contains(&human));
    let threads = store.threads(direct.id, true).await.unwrap();
    let standup = threads
        .iter()
        .find(|t| t.title.starts_with("Standup"))
        .expect("a Standup thread");
    println!("--- brief in thread {:?}", standup.title);

    println!("--- @lead's run");
    let lead_run = loop {
        match tokio::time::timeout(Duration::from_secs(480), runs.recv()).await {
            Ok(Ok(RunNotice::Event { event, .. })) => println!("  {}", event.summary()),
            Ok(Ok(RunNotice::Ended { run, .. })) if run.agent_id == lead_agent.actor_id => {
                break run
            }
            Ok(Ok(_)) => {}
            other => panic!("@lead's run did not end: {other:?}"),
        }
    };
    println!(
        "--- @lead ended {:?}, ${:.3}, {} turns",
        lead_run.exit_reason,
        lead_run.cost_usd.unwrap_or(0.0),
        lead_run.num_turns
    );
    assert_eq!(
        lead_run.exit_reason,
        Some(ExitReason::Completed),
        "{:?}",
        lead_run.error
    );

    let tickets = store.tickets().await.unwrap();
    for t in &tickets {
        println!(
            "ticket {} {:?} assignee {:?}",
            t.key, t.title, t.assignee_id
        );
    }
    assert!(
        tickets
            .iter()
            .any(|t| t.assignee_id == Some(dev_agent.actor_id)),
        "@lead assigned a ticket to @dev"
    );
    let messages = store.messages(standup.id).await.unwrap();
    for m in &messages {
        let body: String = m.body.chars().take(300).collect();
        println!(
            "message by {}: {body}",
            if m.author_id == human { "@me" } else { "@lead" }
        );
    }
    assert!(
        messages.iter().any(|m| m.author_id == lead_agent.actor_id),
        "@lead reported back in the Standup thread"
    );
    scheduler.shutdown();
    desk.close_all();
}
