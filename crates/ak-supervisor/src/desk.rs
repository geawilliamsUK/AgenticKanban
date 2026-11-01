//! The Desk host: one long-lived Claude Code process per open Desk session.
//!
//! Unlike an agent run, a Desk session is a conversation. Its process reads
//! the human's messages from stdin as stream-json and stays up between them,
//! so replies start at once and the prompt cache stays warm. It is created
//! with `--session-id` and later processes `--resume` it, so closing the app
//! or leaving the session idle for half an hour loses nothing. The session
//! reaches the board through the bridge as the human, under the project's
//! Desk permission profile, and asks for anything else through the same
//! approvals as the agents.
//!
//! Stopping a reply uses Claude Code's `interrupt` control request, which
//! works on Windows where a GUI process cannot send Ctrl-C. Output of local
//! slash commands (`/context`, `/usage`) is taken from the turn's result:
//! the `assistant` messages carrying it are replayed after `/compact`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};

use ak_bridge::RunContext;
use ak_core::agent::Agent;
use ak_core::config::PermissionProfile;
use ak_core::desk::{DeskEntry, DeskEntryKind, DeskSession};
use ak_core::ids::{DeskSessionId, ProjectId, RunId};
use ak_runtime::OpenProject;

use crate::prompts::{self, DeskPromptContext};
use crate::supervisor::{rate_limit_snapshot, roster, Supervisor};
use crate::SupervisorError;

/// The id Desk notices and requests use for the Global Desk, which belongs
/// to no one project.
pub const GLOBAL: ProjectId = ProjectId::from_uuid(uuid::Uuid::nil());

/// Where a Desk session belongs: one project, or all of them.
#[derive(Clone)]
pub enum DeskScope {
    Project(Arc<OpenProject>),
    /// Every project at once. Its sessions live in a store of their own,
    /// and it reaches each project's board through a server named for it.
    Global {
        store: Arc<dyn ak_store::Store>,
        projects: Vec<Arc<OpenProject>>,
    },
}

impl DeskScope {
    pub fn store(&self) -> Arc<dyn ak_store::Store> {
        match self {
            DeskScope::Project(p) => p.store(),
            DeskScope::Global { store, .. } => store.clone(),
        }
    }

    /// The project id its notices carry: the project's, or `GLOBAL`.
    pub fn id(&self) -> ProjectId {
        match self {
            DeskScope::Project(p) => p.id(),
            DeskScope::Global { .. } => GLOBAL,
        }
    }
}

impl From<Arc<OpenProject>> for DeskScope {
    fn from(p: Arc<OpenProject>) -> Self {
        DeskScope::Project(p)
    }
}

/// The MCP server name for a project's board in the Global Desk:
/// `board-` and the project's name, lowercased, with runs of anything but
/// letters and digits as one hyphen.
pub fn board_server(project_name: &str) -> String {
    let slug: String = project_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    format!("board-{}", if slug.is_empty() { "project".into() } else { slug })
}

/// An idle process is closed after this; the next message resumes it.
const IDLE_CLOSE: Duration = Duration::from_secs(30 * 60);

/// Tool results are kept to this many characters in the transcript.
const RESULT_CHARS: usize = 4000;

/// What the Desk view shows about a session's process.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeskStatus {
    /// A Claude Code process is up for the session.
    pub alive: bool,
    /// A turn is in flight or queued.
    pub busy: bool,
    /// What Claude Code says it is doing: "requesting", "compacting".
    pub activity: Option<String>,
    /// The model the process reported.
    pub model: Option<String>,
    /// Tokens in the conversation as of the last reply.
    pub context_tokens: Option<u64>,
    pub context_window: Option<u64>,
    /// The slash commands the process accepts, without the slash.
    pub slash_commands: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum DeskNotice {
    /// A transcript entry was recorded.
    Entry {
        project: ProjectId,
        entry: DeskEntry,
    },
    /// More of the reply being written; cleared by the next entry.
    Delta {
        project: ProjectId,
        session: DeskSessionId,
        text: String,
    },
    Status {
        project: ProjectId,
        session: DeskSessionId,
        status: DeskStatus,
    },
    /// The session's row changed: title, cost, model.
    Session {
        project: ProjectId,
        session: DeskSession,
    },
    /// The transcript changed under the view (an edit): reload it.
    Reset {
        project: ProjectId,
        session: DeskSessionId,
    },
}

enum Command {
    User(String),
    Interrupt,
    Close,
}

struct Live {
    commands: mpsc::UnboundedSender<Command>,
}

pub struct DeskHost {
    supervisor: Arc<Supervisor>,
    live: Mutex<HashMap<DeskSessionId, Live>>,
    notices: broadcast::Sender<DeskNotice>,
}

impl DeskHost {
    pub fn new(supervisor: Arc<Supervisor>) -> Arc<Self> {
        let (notices, _) = broadcast::channel(1024);
        Arc::new(Self {
            supervisor,
            live: Mutex::new(HashMap::new()),
            notices,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DeskNotice> {
        self.notices.subscribe()
    }

    pub fn is_live(&self, id: DeskSessionId) -> bool {
        self.live
            .lock()
            .expect("desk")
            .get(&id)
            .is_some_and(|l| !l.commands.is_closed())
    }

    /// Records the human's message and hands it to the session's process,
    /// starting or resuming one if needed. A message sent while a reply is
    /// being written waits its turn.
    pub async fn send(
        self: &Arc<Self>,
        project: impl Into<DeskScope>,
        id: DeskSessionId,
        text: String,
    ) -> Result<(), SupervisorError> {
        self.send_as(project.into(), id, text, None).await
    }

    /// Like `send`, but the transcript shows `shown` (a command as typed)
    /// while Claude Code receives `text` (what the command expands to).
    pub async fn send_as(
        self: &Arc<Self>,
        project: impl Into<DeskScope>,
        id: DeskSessionId,
        text: String,
        shown: Option<String>,
    ) -> Result<(), SupervisorError> {
        let project: DeskScope = project.into();
        let store = project.store();
        let mut session = store.desk_session(id).await?;
        let now = Utc::now();
        let payload = match &shown {
            Some(s) => json!({ "text": s, "expanded": text }),
            None => json!({ "text": text }),
        };
        let entry = store
            .append_desk_entry(id, DeskEntryKind::User, payload, now)
            .await?;
        self.notify(DeskNotice::Entry {
            project: project.id(),
            entry,
        });
        session.title_from(shown.as_deref().unwrap_or(&text));
        session.last_active_at = now;
        session.archived = false;
        store.update_desk_session(&session).await?;
        self.notify(DeskNotice::Session {
            project: project.id(),
            session: session.clone(),
        });
        let commands = match self.live_commands(id) {
            Some(c) => c,
            None => self.spawn(&project, &session).await?,
        };
        commands
            .send(Command::User(text))
            .map_err(|_| SupervisorError::Other("the Desk session closed".into()))
    }

    /// Replaces the human's message at `seq` and everything after it with
    /// `text`, shown as `shown` when that is set (as in `send_as`). The Claude Code session is branched from the last message
    /// before it, or started afresh when it was the first; the old branch is
    /// left as it was.
    pub async fn edit(
        self: &Arc<Self>,
        project: impl Into<DeskScope>,
        id: DeskSessionId,
        seq: i64,
        text: String,
        shown: Option<String>,
    ) -> Result<(), SupervisorError> {
        let project: DeskScope = project.into();
        let store = project.store();
        let entries = store.desk_entries(id).await?;
        let Some(target) = entries.iter().find(|e| e.seq == seq) else {
            return Err(SupervisorError::Other(
                "that message is no longer in the session".into(),
            ));
        };
        if target.kind != DeskEntryKind::User {
            return Err(SupervisorError::Other(
                "only your own messages can be edited".into(),
            ));
        }
        let sent = target
            .payload
            .get("expanded")
            .and_then(|t| t.as_str())
            .unwrap_or(target.text("text"))
            .to_string();
        let session_now = store.desk_session(id).await?;
        // The message before the edited one: from our own record, or, for
        // entries recorded before message ids were kept, from Claude Code's
        // transcript.
        let anchor = entries
            .iter()
            .filter(|e| e.seq < seq)
            .filter_map(|e| e.payload.get("uuid").and_then(|u| u.as_str()))
            .next_back()
            .map(str::to_string)
            .or_else(|| {
                claude_transcript(&session_now.claude_session_id).and_then(|t| parent_of(&t, &sent))
            });
        self.close(id);
        self.wait_closed(id).await;
        store.truncate_desk_entries(id, seq).await?;
        let mut session = store.desk_session(id).await?;
        match anchor {
            Some(at) if session.started => session.resume_at = Some(at),
            _ => {
                session.claude_session_id = uuid::Uuid::new_v4().to_string();
                session.started = false;
                session.resume_at = None;
            }
        }
        store.update_desk_session(&session).await?;
        self.notify(DeskNotice::Reset {
            project: project.id(),
            session: id,
        });
        self.send_as(project, id, text, shown).await
    }

    /// Records a team update in the session's transcript. With `prompt`, the
    /// update is also handed to Claude Code as a turn of its own, so the Desk
    /// tells the human about it; the human sees the update card, not the
    /// prompt.
    pub async fn deliver_update(
        self: &Arc<Self>,
        project: impl Into<DeskScope>,
        id: DeskSessionId,
        update: Value,
        prompt: Option<String>,
    ) -> Result<(), SupervisorError> {
        let project: DeskScope = project.into();
        let store = project.store();
        let entry = store
            .append_desk_entry(id, DeskEntryKind::TeamUpdate, update, Utc::now())
            .await?;
        self.notify(DeskNotice::Entry {
            project: project.id(),
            entry,
        });
        let Some(prompt) = prompt else {
            return Ok(());
        };
        let session = store.desk_session(id).await?;
        let commands = match self.live_commands(id) {
            Some(c) => c,
            None => self.spawn(&project, &session).await?,
        };
        commands
            .send(Command::User(prompt))
            .map_err(|_| SupervisorError::Other("the Desk session closed".into()))
    }

    /// Waits a little for a closing process to go, so a new one does not
    /// start beside it.
    async fn wait_closed(&self, id: DeskSessionId) {
        for _ in 0..100 {
            if !self.is_live(id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Asks Claude Code to stop the reply in progress. False when nothing runs.
    pub fn interrupt(&self, id: DeskSessionId) -> bool {
        self.live_commands(id)
            .is_some_and(|c| c.send(Command::Interrupt).is_ok())
    }

    /// Ends the session's process. The next message resumes the session,
    /// which is how a model change takes effect.
    pub fn close(&self, id: DeskSessionId) {
        if let Some(c) = self.live_commands(id) {
            let _ = c.send(Command::Close);
        }
    }

    /// Ends every Desk process, as the app closes.
    pub fn close_all(&self) {
        for l in self.live.lock().expect("desk").values() {
            let _ = l.commands.send(Command::Close);
        }
    }

    fn live_commands(&self, id: DeskSessionId) -> Option<mpsc::UnboundedSender<Command>> {
        self.live
            .lock()
            .expect("desk")
            .get(&id)
            .map(|l| l.commands.clone())
            .filter(|c| !c.is_closed())
    }

    fn notify(&self, notice: DeskNotice) {
        let _ = self.notices.send(notice);
    }

    async fn spawn(
        self: &Arc<Self>,
        project: &DeskScope,
        session: &DeskSession,
    ) -> Result<mpsc::UnboundedSender<Command>, SupervisorError> {
        let sup = &self.supervisor;
        let config = sup.config();
        let binary = config.binary.clone().map_err(SupervisorError::Spawn)?;
        let paths = sup.paths();
        let home = match project {
            DeskScope::Project(p) => paths.desk_dir(p.id()),
            DeskScope::Global { .. } => paths.global_desk_dir(),
        };
        let dir = home.join("sessions").join(session.id.to_string());
        std::fs::create_dir_all(&dir).map_err(|e| SupervisorError::io(&dir, e))?;
        let memory_file = home.join("memory.md");
        let memory = std::fs::read_to_string(&memory_file).unwrap_or_default();
        let run_key = RunId::from_uuid(session.id.as_uuid());

        // A Desk acts as the human in each project it reaches, under that
        // project's Desk permission profile, through one board server each.
        let desk_context = |p: &Arc<OpenProject>| {
            let policy = p.policy();
            let profile = policy
                .permission_profiles
                .iter()
                .find(|x| x.name == policy.desk_permission_profile)
                .cloned()
                .unwrap_or_else(|| {
                    PermissionProfile::defaults()
                        .into_iter()
                        .next()
                        .expect("a default profile")
                });
            RunContext {
                project: p.clone(),
                agent: Agent::stand_in_for(&p.human, memory_file.clone()),
                run_id: run_key,
                profile,
                desk_memory: Some(memory_file.clone()),
                policy_file: Some(paths.project_policy_file(p.id())),
            }
        };
        let (system, servers, cwd, strict) = match project {
            DeskScope::Project(p) => {
                let store = p.store();
                let roster = roster(store.as_ref(), p.human.id).await?;
                let columns = store.columns().await?;
                let system = prompts::desk_prompt(&DeskPromptContext {
                    project_name: &p.entry.name,
                    human_handle: &p.human.handle,
                    memory: &memory,
                    roster: &roster,
                    columns: &columns,
                    repo_path: &p.entry.path,
                });
                let token = sup.bridge().register_run(desk_context(p));
                (
                    system,
                    vec![("board".to_string(), token)],
                    p.entry.path.clone(),
                    !p.policy().allow_project_mcp,
                )
            }
            DeskScope::Global { projects, .. } => {
                if projects.is_empty() {
                    return Err(SupervisorError::Other(
                        "the Global Desk needs at least one project".into(),
                    ));
                }
                let mut boards = Vec::new();
                let mut servers = Vec::new();
                for p in projects {
                    // Two projects whose names read the same get numbered.
                    let base = board_server(&p.entry.name);
                    let mut server = base.clone();
                    let mut n = 1;
                    while servers.iter().any(|(s, _): &(String, String)| *s == server) {
                        n += 1;
                        server = format!("{base}-{n}");
                    }
                    let roster = roster(p.store().as_ref(), p.human.id).await?;
                    boards.push(prompts::GlobalBoard {
                        name: p.entry.name.clone(),
                        server: server.clone(),
                        repo_path: p.entry.path.clone(),
                        roster,
                    });
                    servers.push((server, sup.bridge().register_run(desk_context(p))));
                }
                let human = projects[0].human.handle.clone();
                let system = prompts::global_desk_prompt(&human, &memory, &boards);
                std::fs::create_dir_all(&home).map_err(|e| SupervisorError::io(&home, e))?;
                (system, servers, home.clone(), true)
            }
        };
        let tokens: Vec<String> = servers.iter().map(|(_, t)| t.clone()).collect();
        let system_file = dir.join("system.md");
        let mcp_file = dir.join("mcp.json");
        let settings_file = dir.join("settings.json");
        let mcp = json!({ "mcpServers": servers
            .iter()
            .map(|(name, token)| (name.clone(), json!({"type": "http", "url": sup.bridge().mcp_url(token)})))
            .collect::<serde_json::Map<String, Value>>() });
        // Approvals go through the first board; every board's own tools are
        // allowed by the hook.
        let written = std::fs::write(&system_file, &system)
            .map_err(|e| SupervisorError::io(&system_file, e))
            .and_then(|_| {
                std::fs::write(&mcp_file, mcp.to_string())
                    .map_err(|e| SupervisorError::io(&mcp_file, e))
            })
            .and_then(|_| {
                std::fs::write(
                    &settings_file,
                    sup.bridge().hook_settings(&tokens[0]).to_string(),
                )
                .map_err(|e| SupervisorError::io(&settings_file, e))
            });
        if let Err(e) = written {
            for t in &tokens {
                sup.bridge().unregister_run(t);
            }
            return Err(e);
        }
        let profile = match project {
            DeskScope::Project(p) => desk_context(p).profile,
            DeskScope::Global { projects, .. } => desk_context(&projects[0]).profile,
        };

        let mut args: Vec<String> = vec![
            "-p".into(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--include-partial-messages".into(),
            "--append-system-prompt-file".into(),
            system_file.to_string_lossy().to_string(),
            "--mcp-config".into(),
            mcp_file.to_string_lossy().to_string(),
            "--settings".into(),
            settings_file.to_string_lossy().to_string(),
            "--permission-mode".into(),
            profile.mode.clone(),
        ];
        match (&session.resume_at, session.started) {
            (Some(at), true) => {
                args.push("--resume".into());
                args.push(session.claude_session_id.clone());
                args.push("--resume-session-at".into());
                args.push(at.clone());
                args.push("--fork-session".into());
            }
            (None, true) => {
                args.push("--resume".into());
                args.push(session.claude_session_id.clone());
            }
            (_, false) => {
                args.push("--session-id".into());
                args.push(session.claude_session_id.clone());
            }
        }
        if strict {
            args.push("--strict-mcp-config".into());
        }
        if let Some(model) = &session.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        if !profile.allowed_tools.is_empty() {
            args.push("--allowedTools".into());
            args.extend(profile.allowed_tools.iter().cloned());
        }
        if !profile.disallowed_tools.is_empty() {
            args.push("--disallowedTools".into());
            args.extend(profile.disallowed_tools.iter().cloned());
        }
        std::fs::write(dir.join("args.json"), json!(args).to_string()).ok();

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(&args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("DISABLE_AUTOUPDATER", "1")
            .kill_on_drop(true);
        if !config.pass_api_key {
            cmd.env_remove("ANTHROPIC_API_KEY");
            cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                for t in &tokens {
                    sup.bridge().unregister_run(t);
                }
                return Err(SupervisorError::Spawn(format!(
                    "could not start {}: {e}",
                    binary.display()
                )));
            }
        };

        let (tx, rx) = mpsc::unbounded_channel();
        self.live.lock().expect("desk").insert(
            session.id,
            Live {
                commands: tx.clone(),
            },
        );
        let driver = DeskDriver {
            host: self.clone(),
            project: project.clone(),
            session_id: session.id,
            tokens,
            dir,
            run_key,
            status: DeskStatus {
                alive: true,
                model: session.model.clone(),
                ..Default::default()
            },
            pending: 0,
            process_cost: 0.0,
            commands: tx.clone(),
        };
        tokio::spawn(driver.drive(child, rx));
        Ok(tx)
    }
}

struct DeskDriver {
    host: Arc<DeskHost>,
    project: DeskScope,
    session_id: DeskSessionId,
    /// One bridge registration per board the session reaches.
    tokens: Vec<String>,
    dir: PathBuf,
    /// The session's key in the bridge and the usage tracker.
    run_key: RunId,
    status: DeskStatus,
    /// Turns sent and not yet answered.
    pending: u32,
    /// The process's running `total_cost_usd`, to take each turn's share.
    process_cost: f64,
    /// Our own sender, to tell whether the live map still points at us.
    commands: mpsc::UnboundedSender<Command>,
}

impl DeskDriver {
    async fn drive(
        mut self,
        mut child: tokio::process::Child,
        mut commands: mpsc::UnboundedReceiver<Command>,
    ) {
        let mut stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr_tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let mut t = tail.lock().expect("tail");
                    if t.len() >= 40 {
                        t.pop_front();
                    }
                    t.push_back(l);
                }
            });
        }
        let Some(stdout) = stdout else {
            return;
        };
        let mut lines = BufReader::new(stdout).lines();
        self.publish_status();

        let idle = tokio::time::sleep(IDLE_CLOSE);
        tokio::pin!(idle);
        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    Some(Command::User(text)) => {
                        let line = json!({
                            "type": "user",
                            "message": {"role": "user", "content": text},
                        });
                        if !write_line(&mut stdin, &line).await {
                            break;
                        }
                        if self.pending == 0 {
                            self.host.supervisor.usage().begin(self.run_key, Utc::now());
                        }
                        self.pending += 1;
                        self.status.busy = true;
                        self.publish_status();
                        idle.as_mut().reset(tokio::time::Instant::now() + IDLE_CLOSE);
                    }
                    Some(Command::Interrupt) => {
                        let line = json!({
                            "type": "control_request",
                            "request_id": uuid::Uuid::new_v4().to_string(),
                            "request": {"subtype": "interrupt"},
                        });
                        write_line(&mut stdin, &line).await;
                    }
                    Some(Command::Close) | None => break,
                },
                line = lines.next_line() => match line {
                    Ok(Some(l)) => {
                        self.on_line(&l).await;
                        idle.as_mut().reset(tokio::time::Instant::now() + IDLE_CLOSE);
                    }
                    _ => break,
                },
                _ = &mut idle, if self.pending == 0 => break,
            }
        }

        drop(stdin);
        let _ = child.kill().await;
        let _ = child.wait().await;
        let tail: Vec<String> = stderr_tail.lock().expect("tail").iter().cloned().collect();
        let _ = std::fs::write(self.dir.join("stderr.log"), tail.join("\n"));
        if self.pending > 0 {
            self.host.supervisor.usage().end(self.run_key);
            let last: Vec<&str> = tail.iter().rev().take(3).map(|s| s.as_str()).collect();
            let error = if last.is_empty() {
                "Claude Code stopped before it answered".to_string()
            } else {
                format!(
                    "Claude Code stopped before it answered: {}",
                    last.into_iter().rev().collect::<Vec<_>>().join(" | ")
                )
            };
            self.entry(
                DeskEntryKind::TurnEnd,
                json!({"subtype": "process_exit", "is_error": true, "cost_usd": 0.0,
                       "interrupted": false, "error": error}),
            )
            .await;
        }
        for t in &self.tokens {
            self.host.supervisor.bridge().unregister_run(t);
        }
        {
            let mut live = self.host.live.lock().expect("desk");
            if live
                .get(&self.session_id)
                .is_some_and(|l| l.commands.same_channel(&self.commands))
            {
                live.remove(&self.session_id);
            }
        }
        self.pending = 0;
        self.status.alive = false;
        self.status.busy = false;
        self.status.activity = None;
        self.publish_status();
    }

    async fn on_line(&mut self, line: &str) {
        if let Some(snapshot) = rate_limit_snapshot(line) {
            self.host.supervisor.usage().observe(snapshot);
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return;
        };
        // Subagents' traffic is theirs; the transcript shows the Agent call.
        if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
            return;
        }
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
        match s("type") {
            "stream_event" => {
                let e = &v["event"];
                if e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta" {
                    if let Some(text) = e["delta"]["text"].as_str() {
                        self.host.notify(DeskNotice::Delta {
                            project: self.project.id(),
                            session: self.session_id,
                            text: text.to_string(),
                        });
                    }
                }
            }
            "system" => match s("subtype") {
                "init" => {
                    if let Some(sid) = v.get("session_id").and_then(|x| x.as_str()) {
                        self.adopt_session_id(sid).await;
                    }
                    if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
                        self.status.model = Some(m.to_string());
                    }
                    if let Some(list) = v.get("slash_commands").and_then(|l| l.as_array()) {
                        self.status.slash_commands = list
                            .iter()
                            .filter_map(|c| c.as_str())
                            .map(|c| c.trim_start_matches('/').to_string())
                            .collect();
                    }
                    self.publish_status();
                }
                "status" => {
                    self.status.activity =
                        v.get("status").and_then(|x| x.as_str()).map(str::to_string);
                    self.publish_status();
                }
                "compact_boundary" => {
                    let m = &v["compact_metadata"];
                    self.entry(
                        DeskEntryKind::Compacted,
                        json!({"trigger": m["trigger"], "pre_tokens": m["pre_tokens"],
                               "post_tokens": m["post_tokens"]}),
                    )
                    .await;
                    if let Some(post) = m["post_tokens"].as_u64() {
                        self.status.context_tokens = Some(post);
                        self.publish_status();
                    }
                }
                _ => {}
            },
            "assistant" => {
                if v.get("local_command_source").is_some() {
                    return;
                }
                if let Some(u) = v.pointer("/message/usage") {
                    let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                    self.status.context_tokens = Some(
                        n("input_tokens")
                            + n("cache_creation_input_tokens")
                            + n("cache_read_input_tokens")
                            + n("output_tokens"),
                    );
                }
                let blocks = v
                    .pointer("/message/content")
                    .and_then(|c| c.as_array())
                    .cloned()
                    .unwrap_or_default();
                for b in blocks {
                    match b.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            if !text.trim().is_empty() {
                                self.entry(
                                    DeskEntryKind::Assistant,
                                    json!({"text": text, "uuid": v["uuid"]}),
                                )
                                .await;
                            }
                        }
                        Some("tool_use") => {
                            self.entry(
                                DeskEntryKind::ToolCall,
                                json!({"id": b["id"], "name": b["name"], "input": b["input"], "uuid": v["uuid"]}),
                            )
                            .await;
                        }
                        _ => {}
                    }
                }
            }
            "user" => {
                let flag = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
                if flag("isReplay") || flag("isSynthetic") {
                    return;
                }
                let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) else {
                    // Plain-string user messages are local command echoes.
                    return;
                };
                for b in blocks.clone() {
                    match b.get("type").and_then(|t| t.as_str()) {
                        Some("tool_result") => {
                            let content = match b.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Array(parts)) => parts
                                    .iter()
                                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                Some(other) => other.to_string(),
                                None => String::new(),
                            };
                            self.entry(
                                DeskEntryKind::ToolResult,
                                json!({"tool_use_id": b["tool_use_id"], "uuid": v["uuid"],
                                       "content": truncate(&content, RESULT_CHARS),
                                       "is_error": b.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false)}),
                            )
                            .await;
                        }
                        Some("text") => {
                            let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            if text.starts_with("[Request interrupted") {
                                self.entry(
                                    DeskEntryKind::Notice,
                                    json!({"message": "Interrupted"}),
                                )
                                .await;
                            }
                        }
                        _ => {}
                    }
                }
            }
            "result" => self.turn_end(&v).await,
            _ => {}
        }
    }

    /// Records the Claude Code session the process reports. After a fork it
    /// is a new one, and the branch point has been used.
    async fn adopt_session_id(&mut self, sid: &str) {
        let store = self.project.store();
        let Ok(mut session) = store.desk_session(self.session_id).await else {
            return;
        };
        if session.claude_session_id == sid && session.resume_at.is_none() {
            return;
        }
        session.claude_session_id = sid.to_string();
        session.resume_at = None;
        if store.update_desk_session(&session).await.is_ok() {
            self.host.notify(DeskNotice::Session {
                project: self.project.id(),
                session,
            });
        }
    }

    async fn turn_end(&mut self, v: &Value) {
        let total = v
            .get("total_cost_usd")
            .and_then(|c| c.as_f64())
            .unwrap_or(self.process_cost);
        let cost = (total - self.process_cost).max(0.0);
        self.process_cost = total;
        let text = v
            .get("result")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if v.get("local_command").is_some() && !text.is_empty() {
            self.entry(
                DeskEntryKind::Assistant,
                json!({"text": text, "local_command": true}),
            )
            .await;
        }
        if let Some(usage) = v.get("modelUsage").and_then(|m| m.as_object()) {
            let window = usage
                .values()
                .filter_map(|m| m.get("contextWindow").and_then(|w| w.as_u64()))
                .max();
            if window.is_some() {
                self.status.context_window = window;
            }
        }
        let terminal = v
            .get("terminal_reason")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let interrupted = terminal.starts_with("aborted");
        let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
        let error = (is_error && !interrupted).then(|| {
            let errors = v
                .get("errors")
                .and_then(|e| e.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .unwrap_or_default();
            if !text.is_empty() {
                text.clone()
            } else if !errors.is_empty() {
                errors
            } else {
                v.get("subtype")
                    .and_then(|s| s.as_str())
                    .unwrap_or("error")
                    .to_string()
            }
        });
        self.entry(
            DeskEntryKind::TurnEnd,
            json!({"subtype": v["subtype"], "is_error": is_error && !interrupted,
                   "cost_usd": cost, "duration_ms": v["duration_ms"],
                   "interrupted": interrupted, "error": error}),
        )
        .await;

        let store = self.project.store();
        if let Ok(mut session) = store.desk_session(self.session_id).await {
            session.cost_usd += cost;
            session.started = true;
            session.last_active_at = Utc::now();
            if store.update_desk_session(&session).await.is_ok() {
                self.host.notify(DeskNotice::Session {
                    project: self.project.id(),
                    session,
                });
            }
        }
        self.pending = self.pending.saturating_sub(1);
        if self.pending == 0 {
            self.status.busy = false;
            self.host.supervisor.usage().end(self.run_key);
        }
        self.status.activity = None;
        self.publish_status();
    }

    async fn entry(&self, kind: DeskEntryKind, payload: Value) {
        match self
            .project
            .store()
            .append_desk_entry(self.session_id, kind, payload, Utc::now())
            .await
        {
            Ok(entry) => self.host.notify(DeskNotice::Entry {
                project: self.project.id(),
                entry,
            }),
            Err(e) => tracing::warn!("could not record a Desk entry: {e}"),
        }
    }

    fn publish_status(&self) {
        self.host.notify(DeskNotice::Status {
            project: self.project.id(),
            session: self.session_id,
            status: self.status.clone(),
        });
    }
}

/// Claude Code's own transcript of a session, from its projects directory.
fn claude_transcript(session_id: &str) -> Option<String> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|h| PathBuf::from(h).join(".claude"))
        })?;
    let file = format!("{session_id}.jsonl");
    std::fs::read_dir(base.join("projects"))
        .ok()?
        .filter_map(|d| d.ok())
        .map(|d| d.path().join(&file))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::read_to_string(p).ok())
}

/// The id of the message before the human's last message reading `text`,
/// from a Claude Code transcript (one JSON object per line).
fn parent_of(transcript: &str, text: &str) -> Option<String> {
    transcript
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "user" && v["message"]["content"].as_str() == Some(text))
        .filter_map(|v| v["parentUuid"].as_str().map(str::to_string))
        .next_back()
}

/// Writes one JSON line to the process. False when stdin is gone.
pub(crate) async fn write_line(
    stdin: &mut Option<tokio::process::ChildStdin>,
    value: &Value,
) -> bool {
    let Some(pipe) = stdin.as_mut() else {
        return false;
    };
    let mut line = value.to_string();
    line.push('\n');
    pipe.write_all(line.as_bytes()).await.is_ok() && pipe.flush().await.is_ok()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parent_of_the_edited_message_comes_from_the_transcript() {
        let t = [
            r#"{"type":"user","uuid":"u1","parentUuid":null,"message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[]}}"#,
            r#"{"type":"user","uuid":"u2","parentUuid":"a1","message":{"role":"user","content":"which key?"}}"#,
            r#"{"type":"user","uuid":"t1","parentUuid":"a2","message":{"role":"user","content":[{"type":"tool_result"}]}}"#,
            "not json",
            r#"{"type":"user","uuid":"u3","parentUuid":"a9","message":{"role":"user","content":"which key?"}}"#,
        ]
        .join("
");
        assert_eq!(
            parent_of(&t, "which key?").as_deref(),
            Some("a9"),
            "the last time it was said"
        );
        assert_eq!(
            parent_of(&t, "hello"),
            None,
            "the first message has no parent"
        );
        assert_eq!(parent_of(&t, "never said"), None);
    }
}
