//! Desk sessions against the fake Claude Code binary's conversation mode.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;

use ak_bridge::Bridge;
use ak_core::agent::AgentDefinition;
use ak_core::commands::NewTicket;
use ak_core::config::PermissionProfile;
use ak_core::desk::{DeskEntry, DeskEntryKind, DeskSession};
use ak_core::run::RunTrigger;
use ak_core::usage::Window;
use ak_runtime::{HomePaths, OpenProject, ProjectRegistry};
use ak_supervisor::{
    desk_updates, workspace, DeskHost, DeskNotice, DeskStatus, RunSpec, Supervisor,
    SupervisorConfig,
};
use chrono::Utc;

struct Fixture {
    _tmp: tempfile::TempDir,
    project: Arc<OpenProject>,
    supervisor: Arc<Supervisor>,
    desk: Arc<DeskHost>,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    ak_supervisor::workspace::git(&repo, &["init", "-q", "-b", "main"])
        .await
        .unwrap();
    ak_supervisor::workspace::git(
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
    let project = registry.open(id).await.unwrap();
    let supervisor = Arc::new(Supervisor::new(
        Bridge::with_addr(
            "127.0.0.1:1".parse::<SocketAddr>().unwrap(),
            Duration::from_secs(5),
        ),
        paths,
        SupervisorConfig {
            binary: Ok(PathBuf::from(env!("CARGO_BIN_EXE_fake_claude"))),
            pass_api_key: false,
            startup_timeout: Duration::from_secs(5),
            stop_grace: Duration::from_secs(2),
        },
    ));
    let desk = DeskHost::new(supervisor.clone());
    Fixture {
        _tmp: tmp,
        project,
        supervisor,
        desk,
    }
}

/// Collects notices until a turn ends, returning its entries, the streamed
/// text and the last status.
async fn turn(
    notices: &mut broadcast::Receiver<DeskNotice>,
) -> (Vec<DeskEntry>, String, Option<DeskStatus>) {
    let mut entries = Vec::new();
    let mut streamed = String::new();
    let mut status = None;
    loop {
        match tokio::time::timeout(Duration::from_secs(20), notices.recv()).await {
            Ok(Ok(DeskNotice::Entry { entry, .. })) => {
                let done = entry.kind == DeskEntryKind::TurnEnd;
                entries.push(entry);
                if done {
                    // The status after the turn follows the TurnEnd entry.
                    while let Ok(Ok(n)) =
                        tokio::time::timeout(Duration::from_millis(500), notices.recv()).await
                    {
                        if let DeskNotice::Status { status: s, .. } = n {
                            let idle = !s.busy;
                            status = Some(s);
                            if idle {
                                break;
                            }
                        }
                    }
                    return (entries, streamed, status);
                }
            }
            Ok(Ok(DeskNotice::Delta { text, .. })) => streamed.push_str(&text),
            Ok(Ok(DeskNotice::Status { status: s, .. })) => status = Some(s),
            Ok(Ok(DeskNotice::Session { .. } | DeskNotice::Reset { .. })) => {}
            other => panic!("no turn end: {other:?}"),
        }
    }
}

fn spawn_args(f: &Fixture) -> Vec<Vec<String>> {
    std::fs::read_to_string(f.project.entry.path.join("fake_claude_desk_args.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Vec<String>>(l).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_desk_session_converses_resumes_and_can_be_interrupted() {
    let f = fixture().await;
    let store = f.project.store();
    let session = DeskSession::new(None, Utc::now());
    store.insert_desk_session(&session).await.unwrap();
    let mut notices = f.desk.subscribe();

    // First turn: a tool call, streamed text, the reply, the cost.
    f.desk
        .send(f.project.clone(), session.id, "Plan the sprint".into())
        .await
        .unwrap();
    let (entries, streamed, status) = turn(&mut notices).await;
    let kinds: Vec<DeskEntryKind> = entries.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            DeskEntryKind::User,
            DeskEntryKind::ToolCall,
            DeskEntryKind::ToolResult,
            DeskEntryKind::Assistant,
            DeskEntryKind::TurnEnd
        ]
    );
    assert_eq!(entries[3].text("text"), "Echo: Plan the sprint");
    assert_eq!(streamed, "Echo: Plan the sprint");
    assert!((entries[4].payload["cost_usd"].as_f64().unwrap() - 0.01).abs() < 1e-9);
    let status = status.expect("a status");
    assert!(status.alive && !status.busy);
    assert_eq!(status.model.as_deref(), Some("fake-model"));
    assert!(status.slash_commands.contains(&"compact".to_string()));
    assert_eq!(status.context_window, Some(200_000));
    assert_eq!(status.context_tokens, Some(1228));

    let saved = store.desk_session(session.id).await.unwrap();
    assert_eq!(saved.title, "Plan the sprint");
    assert!(saved.started);
    assert!((saved.cost_usd - 0.01).abs() < 1e-9);
    assert_eq!(
        f.supervisor
            .usage()
            .latest()
            .and_then(|u| u.used(Window::FiveHour, Utc::now())),
        Some(0.3),
        "the Desk feeds the usage meter"
    );

    // A local command: its output comes from the result, not the replay.
    f.desk
        .send(f.project.clone(), session.id, "/context".into())
        .await
        .unwrap();
    let (entries, _, _) = turn(&mut notices).await;
    let texts: Vec<&str> = entries
        .iter()
        .filter(|e| e.kind == DeskEntryKind::Assistant)
        .map(|e| e.text("text"))
        .collect();
    assert_eq!(texts, vec!["## Context Usage\n42k / 200k"]);
    assert_eq!(entries.last().unwrap().payload["cost_usd"], 0.0);

    // Interrupt a reply in progress.
    f.desk
        .send(f.project.clone(), session.id, "slow".into())
        .await
        .unwrap();
    loop {
        if let Ok(DeskNotice::Delta { .. }) = notices.recv().await {
            break;
        }
    }
    assert!(f.desk.interrupt(session.id));
    let (entries, _, _) = turn(&mut notices).await;
    assert!(entries
        .iter()
        .any(|e| e.kind == DeskEntryKind::Notice && e.text("message") == "Interrupted"));
    let end = entries.last().unwrap();
    assert_eq!(end.payload["interrupted"], Value::Bool(true));
    assert_eq!(end.payload["is_error"], Value::Bool(false));

    // Closing ends the process; the next message resumes the same session.
    f.desk.close(session.id);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while f.desk.is_live(session.id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "process did not close"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    f.desk
        .send(f.project.clone(), session.id, "Still there?".into())
        .await
        .unwrap();
    let (entries, _, _) = turn(&mut notices).await;
    assert!(entries
        .iter()
        .any(|e| e.text("text") == "Echo: Still there?"));
    let spawns = spawn_args(&f);
    assert_eq!(spawns.len(), 2);
    let joined: Vec<String> = spawns.iter().map(|a| a.join(" ")).collect();
    assert!(joined[0].contains(&format!("--session-id {}", session.claude_session_id)));
    assert!(joined[1].contains(&format!("--resume {}", session.claude_session_id)));
    for needle in [
        "--input-format stream-json",
        "--include-partial-messages",
        "--permission-mode default",
        "--append-system-prompt-file",
    ] {
        assert!(
            joined[0].contains(needle),
            "missing {needle:?} in {}",
            joined[0]
        );
    }
    let all = store.desk_entries(session.id).await.unwrap();
    assert_eq!(
        all.iter().filter(|e| e.kind == DeskEntryKind::User).count(),
        4
    );
    f.desk.close_all();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_branches_the_session_and_team_updates_reach_the_desk() {
    let f = fixture().await;
    let store = f.project.store();
    let session = DeskSession::new(None, Utc::now());
    store.insert_desk_session(&session).await.unwrap();
    let mut notices = f.desk.subscribe();

    for text in ["first", "second"] {
        f.desk
            .send(f.project.clone(), session.id, text.into())
            .await
            .unwrap();
        turn(&mut notices).await;
    }
    let entries = store.desk_entries(session.id).await.unwrap();
    let second = entries
        .iter()
        .find(|e| e.kind == DeskEntryKind::User && e.text("text") == "second")
        .unwrap()
        .seq;

    // Editing the second message branches from the reply before it.
    f.desk
        .edit(
            f.project.clone(),
            session.id,
            second,
            "second, edited".into(),
            None,
        )
        .await
        .unwrap();
    turn(&mut notices).await;
    let texts: Vec<String> = store
        .desk_entries(session.id)
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(e.kind, DeskEntryKind::User | DeskEntryKind::Assistant))
        .map(|e| e.text("text").to_string())
        .collect();
    assert_eq!(
        texts,
        vec![
            "first",
            "Echo: first",
            "second, edited",
            "Echo: second, edited"
        ]
    );
    let saved = store.desk_session(session.id).await.unwrap();
    assert_eq!(
        saved.claude_session_id,
        format!("{}-fork", session.claude_session_id)
    );
    assert!(saved.resume_at.is_none(), "the branch point was used");
    let spawns = spawn_args(&f);
    let last = spawns.last().unwrap().join(" ");
    assert!(
        last.contains(&format!(
            "--resume {} --resume-session-at reply-1 --fork-session",
            session.claude_session_id
        )),
        "{last}"
    );

    // Editing the first message starts a fresh session.
    let first = store.desk_entries(session.id).await.unwrap()[0].seq;
    f.desk
        .edit(
            f.project.clone(),
            session.id,
            first,
            "first, again".into(),
            None,
        )
        .await
        .unwrap();
    turn(&mut notices).await;
    let fresh = store.desk_session(session.id).await.unwrap();
    assert_ne!(fresh.claude_session_id, saved.claude_session_id);
    let last = spawn_args(&f).last().unwrap().join(" ");
    assert!(
        last.contains(&format!("--session-id {}", fresh.claude_session_id)),
        "{last}"
    );
    assert_eq!(
        store
            .desk_entries(session.id)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == DeskEntryKind::User)
            .count(),
        1
    );

    // A team update is a card, and with a prompt the Desk answers it
    // without a message from the human appearing.
    f.desk
        .deliver_update(
            f.project.clone(),
            session.id,
            serde_json::json!({"items": [{"who": "lead", "text": "A or B?", "needs_you": true}]}),
            Some("Team update: @lead asks A or B?".into()),
        )
        .await
        .unwrap();
    let (entries, _, _) = turn(&mut notices).await;
    assert_eq!(entries[0].kind, DeskEntryKind::TeamUpdate);
    assert!(entries.iter().all(|e| e.kind != DeskEntryKind::User));
    assert!(entries
        .iter()
        .any(|e| e.text("text") == "Echo: Team update: @lead asks A or B?"));
    f.desk.close_all();
}

/// Waits for the next Desk entry of `kind`.
async fn next_entry(
    notices: &mut broadcast::Receiver<DeskNotice>,
    kind: DeskEntryKind,
) -> DeskEntry {
    loop {
        match tokio::time::timeout(Duration::from_secs(20), notices.recv()).await {
            Ok(Ok(DeskNotice::Entry { entry, .. })) if entry.kind == kind => return entry,
            Ok(Ok(_)) => {}
            other => panic!("no {kind:?} entry: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_desk_hears_team_updates_and_speaks_up_for_questions() {
    let f = fixture().await;
    let store = f.project.store();
    f.project
        .agents
        .write_definition(&AgentDefinition::lead())
        .unwrap();
    ak_runtime::agents::reconcile(&f.project.agents, f.project.store.as_ref())
        .await
        .unwrap();
    let lead = store.agent_by_handle("lead").await.unwrap().unwrap();
    let session = DeskSession::new(None, Utc::now());
    store.insert_desk_session(&session).await.unwrap();
    let mut notices = f.desk.subscribe();
    let watcher = desk_updates::spawn(
        f.project.clone(),
        f.desk.clone(),
        f.supervisor.clone(),
        desk_updates::Timing {
            batch: Duration::from_millis(300),
            idle_after: Duration::from_millis(300),
        },
    )
    .await;

    let columns = store.columns().await.unwrap();
    let ticket = store
        .create_ticket(
            NewTicket {
                title: "Flags output".into(),
                column_id: columns[1].id,
                ..Default::default()
            },
            f.project.human.id,
        )
        .await
        .unwrap();

    // An update: a card, and the Desk stays quiet.
    store
        .comment(
            ticket.id,
            "@me FYI: ASS-6 is merged.".into(),
            None,
            lead.actor_id,
        )
        .await
        .unwrap();
    let card = next_entry(&mut notices, DeskEntryKind::TeamUpdate).await;
    let items = card.payload["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["who"], "lead");
    assert_eq!(items[0]["needs_you"], false);
    assert!(items[0]["subject"]
        .as_str()
        .unwrap()
        .contains("Flags output"));
    assert!(!f.desk.is_live(session.id), "no model call for an update");

    // A question: a card, and the Desk is asked to speak.
    store
        .comment(
            ticket.id,
            "@me print flags every line, or only when they change?".into(),
            None,
            lead.actor_id,
        )
        .await
        .unwrap();
    let card = next_entry(&mut notices, DeskEntryKind::TeamUpdate).await;
    assert_eq!(card.payload["items"][0]["needs_you"], true);
    let reply = next_entry(&mut notices, DeskEntryKind::Assistant).await;
    assert!(
        reply
            .text("text")
            .starts_with("Echo: [Team update from the app"),
        "{}",
        reply.text("text")
    );
    next_entry(&mut notices, DeskEntryKind::TurnEnd).await;
    assert!(store
        .desk_entries(session.id)
        .await
        .unwrap()
        .iter()
        .all(|e| e.kind != DeskEntryKind::User));

    // A run ends and nothing follows: the team is idle, the question waits.
    let base = workspace::default_base(&f.project.entry.path);
    let ws = workspace::ensure(&f.project.entry.path, &base, "lead")
        .await
        .unwrap();
    let run = f
        .supervisor
        .start_run(RunSpec {
            project: f.project.clone(),
            agent: lead.clone(),
            trigger: RunTrigger::Manual,
            trigger_lines: vec![],
            profile: PermissionProfile::defaults()[0].clone(),
            model: None,
            max_turns: 5,
            max_usd: 1.0,
            wall_clock: Duration::from_secs(30),
            workspace: ws,
        })
        .await
        .unwrap();
    run.wait().await;
    let card = next_entry(&mut notices, DeskEntryKind::TeamUpdate).await;
    assert_eq!(card.payload["idle"], true);
    let items = card.payload["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "the open question, not the FYI");
    assert_eq!(items[0]["needs_you"], true);
    next_entry(&mut notices, DeskEntryKind::TurnEnd).await;

    watcher.abort();
    f.desk.close_all();
}
