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
        project: Arc<OpenProject>,
        id: DeskSessionId,
        text: String,
    ) -> Result<(), SupervisorError> {
        let store = project.store();
        let mut session = store.desk_session(id).await?;
        let now = Utc::now();
        let entry = store
            .append_desk_entry(id, DeskEntryKind::User, json!({ "text": text }), now)
            .await?;
        self.notify(DeskNotice::Entry {
            project: project.id(),
            entry,
        });
        session.title_from(&text);
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
        project: &Arc<OpenProject>,
        session: &DeskSession,
    ) -> Result<mpsc::UnboundedSender<Command>, SupervisorError> {
        let sup = &self.supervisor;
        let config = sup.config();
        let binary = config.binary.clone().map_err(SupervisorError::Spawn)?;
        let paths = sup.paths();
        let dir = paths
            .desk_dir(project.id())
            .join("sessions")
            .join(session.id.to_string());
        std::fs::create_dir_all(&dir).map_err(|e| SupervisorError::io(&dir, e))?;
        let store = project.store();

        let memory_file = paths.desk_memory_file(project.id());
        let memory = std::fs::read_to_string(&memory_file).unwrap_or_default();
        let roster = roster(store.as_ref(), project.human.id).await?;
        let columns = store.columns().await?;
        let system = prompts::desk_prompt(&DeskPromptContext {
            project_name: &project.entry.name,
            human_handle: &project.human.handle,
            memory: &memory,
            roster: &roster,
            columns: &columns,
            repo_path: &project.entry.path,
        });
        let system_file = dir.join("system.md");
        std::fs::write(&system_file, &system).map_err(|e| SupervisorError::io(&system_file, e))?;

        let policy = project.policy();
        let profile = policy
            .permission_profiles
            .iter()
            .find(|p| p.name == policy.desk_permission_profile)
            .cloned()
            .unwrap_or_else(|| {
                PermissionProfile::defaults()
                    .into_iter()
                    .next()
                    .expect("a default profile")
            });
        let run_key = RunId::from_uuid(session.id.as_uuid());
        let token = sup.bridge().register_run(RunContext {
            project: project.clone(),
            agent: Agent::stand_in_for(&project.human, memory_file.clone()),
            run_id: run_key,
            profile: profile.clone(),
            desk_memory: Some(memory_file),
        });
        let mcp_file = dir.join("mcp.json");
        let settings_file = dir.join("settings.json");
        let written = std::fs::write(&mcp_file, sup.bridge().mcp_config(&token).to_string())
            .map_err(|e| SupervisorError::io(&mcp_file, e))
            .and_then(|_| {
                std::fs::write(
                    &settings_file,
                    sup.bridge().hook_settings(&token).to_string(),
                )
                .map_err(|e| SupervisorError::io(&settings_file, e))
            });
        if let Err(e) = written {
            sup.bridge().unregister_run(&token);
            return Err(e);
        }

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
        if session.started {
            args.push("--resume".into());
        } else {
            args.push("--session-id".into());
        }
        args.push(session.claude_session_id.clone());
        if !policy.allow_project_mcp {
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
            .current_dir(&project.entry.path)
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
                sup.bridge().unregister_run(&token);
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
            token,
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
    project: Arc<OpenProject>,
    session_id: DeskSessionId,
    token: String,
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
        self.host.supervisor.bridge().unregister_run(&self.token);
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
                                self.entry(DeskEntryKind::Assistant, json!({"text": text}))
                                    .await;
                            }
                        }
                        Some("tool_use") => {
                            self.entry(
                                DeskEntryKind::ToolCall,
                                json!({"id": b["id"], "name": b["name"], "input": b["input"]}),
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
                                json!({"tool_use_id": b["tool_use_id"],
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

/// Writes one JSON line to the process. False when stdin is gone.
async fn write_line(stdin: &mut Option<tokio::process::ChildStdin>, value: &Value) -> bool {
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
