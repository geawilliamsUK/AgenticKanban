//! Spawning and watching one Claude Code process per run.
//!
//! The supervisor writes the run directory, registers the run with the
//! bridge, spawns the process with the flags from the design, parses its
//! stream-json output into run events, enforces the wall clock, kills on
//! request, and records how it ended.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use ak_bridge::{Bridge, RunContext};
use ak_core::agent::{Agent, AgentState};
use ak_core::config::PermissionProfile;
use ak_core::ids::{ProjectId, RunId};
use ak_core::run::{AgentRun, ExitReason, RunEvent, RunEventKind, RunTrigger};
use ak_core::ActorKind;
use ak_runtime::{HomePaths, OpenProject};

use crate::prompts::{self, PromptContext, RosterLine};
use crate::workspace::Workspace;
use crate::SupervisorError;

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub binary: Result<PathBuf, String>,
    pub pass_api_key: bool,
    /// How long to wait for the init message before giving up.
    pub startup_timeout: Duration,
}

/// Everything needed to start one run.
pub struct RunSpec {
    pub project: Arc<OpenProject>,
    pub agent: Agent,
    pub trigger: RunTrigger,
    /// Human-readable lines for the wake prompt.
    pub trigger_lines: Vec<String>,
    pub profile: PermissionProfile,
    pub model: Option<String>,
    pub max_turns: u32,
    pub max_usd: f64,
    pub wall_clock: Duration,
    pub workspace: Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The human stopped the run; the agent may be woken again.
    Stopped,
    /// The human halted the agent; it stays halted.
    Halted,
    Guardrail,
}

/// What the UI and the scheduler learn as a run progresses.
#[derive(Debug, Clone)]
pub enum RunNotice {
    Started { project: ProjectId, run: AgentRun },
    Event { project: ProjectId, event: RunEvent },
    Ended { project: ProjectId, run: AgentRun },
}

pub struct RunHandle {
    pub run_id: RunId,
    stop: Option<oneshot::Sender<StopReason>>,
    done: JoinHandle<AgentRun>,
}

impl RunHandle {
    pub fn stop(&mut self, reason: StopReason) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(reason);
        }
    }

    /// Waits for the run to finish.
    pub async fn wait(self) -> AgentRun {
        self.done
            .await
            .unwrap_or_else(|e| panic!("run task failed: {e}"))
    }
}

pub struct Supervisor {
    bridge: Bridge,
    paths: HomePaths,
    config: std::sync::RwLock<SupervisorConfig>,
    notices: broadcast::Sender<RunNotice>,
}

impl Supervisor {
    pub fn new(bridge: Bridge, paths: HomePaths, config: SupervisorConfig) -> Self {
        let (notices, _) = broadcast::channel(1024);
        Self {
            bridge,
            paths,
            config: std::sync::RwLock::new(config),
            notices,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RunNotice> {
        self.notices.subscribe()
    }

    pub fn bridge(&self) -> &Bridge {
        &self.bridge
    }

    pub fn config(&self) -> SupervisorConfig {
        self.config.read().expect("config").clone()
    }

    /// Replaces the binary and limits for runs started from now on.
    pub fn set_config(&self, config: SupervisorConfig) {
        *self.config.write().expect("config") = config;
    }

    /// Starts a run. Returns once the process is spawned.
    pub async fn start_run(&self, spec: RunSpec) -> Result<RunHandle, SupervisorError> {
        let binary = self
            .config()
            .binary
            .clone()
            .map_err(SupervisorError::Spawn)?;
        let project = spec.project.clone();
        let store = project.store();
        let project_id = project.id();
        let now = Utc::now();

        let mut run = AgentRun::start(
            spec.agent.actor_id,
            spec.trigger.clone(),
            spec.workspace.path.clone(),
            now,
        );
        store.insert_run(&run).await?;

        // Prompts and per-run files.
        let run_dir = self.paths.run_dir(project_id, run.id);
        std::fs::create_dir_all(&run_dir).map_err(|e| SupervisorError::io(&run_dir, e))?;
        let system = self.compose_system_prompt(&spec).await?;
        let system_file = run_dir.join("system.md");
        std::fs::write(&system_file, &system).map_err(|e| SupervisorError::io(&system_file, e))?;
        let wake = prompts::wake_prompt(&spec.trigger_lines, &spec.workspace);
        std::fs::write(run_dir.join("wake.md"), &wake).ok();

        let token = self.bridge.register_run(RunContext {
            project: project.clone(),
            agent: spec.agent.clone(),
            run_id: run.id,
            profile: spec.profile.clone(),
        });
        let mcp_file = run_dir.join("mcp.json");
        std::fs::write(&mcp_file, self.bridge.mcp_config(&token).to_string())
            .map_err(|e| SupervisorError::io(&mcp_file, e))?;
        let settings_file = run_dir.join("settings.json");
        std::fs::write(
            &settings_file,
            self.bridge.hook_settings(&token).to_string(),
        )
        .map_err(|e| SupervisorError::io(&settings_file, e))?;

        let session_id = uuid::Uuid::new_v4().to_string();
        let mut args: Vec<String> = vec![
            "-p".into(),
            wake,
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--session-id".into(),
            session_id.clone(),
            "--append-system-prompt-file".into(),
            system_file.to_string_lossy().to_string(),
            "--mcp-config".into(),
            mcp_file.to_string_lossy().to_string(),
            "--settings".into(),
            settings_file.to_string_lossy().to_string(),
            "--permission-mode".into(),
            spec.profile.mode.clone(),
            "--max-turns".into(),
            spec.max_turns.to_string(),
            "--max-budget-usd".into(),
            format!("{:.2}", spec.max_usd),
        ];
        if !project.policy().allow_project_mcp {
            args.push("--strict-mcp-config".into());
        }
        if let Some(model) = &spec.model {
            args.push("--model".into());
            args.push(model.clone());
        }
        if !spec.profile.allowed_tools.is_empty() {
            args.push("--allowedTools".into());
            args.extend(spec.profile.allowed_tools.iter().cloned());
        }
        if !spec.profile.disallowed_tools.is_empty() {
            args.push("--disallowedTools".into());
            args.extend(spec.profile.disallowed_tools.iter().cloned());
        }
        std::fs::write(run_dir.join("args.json"), json!(args).to_string()).ok();

        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(&args)
            .current_dir(&spec.workspace.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("DISABLE_AUTOUPDATER", "1")
            .kill_on_drop(true);
        if !self.config().pass_api_key {
            cmd.env_remove("ANTHROPIC_API_KEY");
            cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        store
            .set_agent_state(spec.agent.actor_id, AgentState::Starting, Some(run.id))
            .await?;
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("could not start {}: {e}", binary.display());
                run.ended_at = Some(Utc::now());
                run.exit_reason = Some(ExitReason::Crashed);
                run.error = Some(msg.clone());
                store.update_run(&run).await?;
                store
                    .set_agent_state(spec.agent.actor_id, AgentState::Idle, None)
                    .await?;
                self.bridge.unregister_run(&token);
                return Err(SupervisorError::Spawn(msg));
            }
        };
        // Nothing goes in on stdin; close it so the process never waits on it.
        drop(child.stdin.take());

        let _ = self.notices.send(RunNotice::Started {
            project: project_id,
            run: run.clone(),
        });

        let (stop_tx, stop_rx) = oneshot::channel();
        let run_id = run.id;
        let driver = Driver {
            bridge: self.bridge.clone(),
            notices: self.notices.clone(),
            project,
            agent: spec.agent.clone(),
            run,
            run_dir,
            token,
            wall_clock: spec.wall_clock,
            startup_timeout: self.config().startup_timeout,
        };
        let done = tokio::spawn(driver.drive(child, stop_rx));
        Ok(RunHandle {
            run_id,
            stop: Some(stop_tx),
            done,
        })
    }

    async fn compose_system_prompt(&self, spec: &RunSpec) -> Result<String, SupervisorError> {
        let project = &spec.project;
        let store = project.store();
        let definition = project.agents.read_definition(&spec.agent.handle)?;
        let memory = project.agents.read_memory(&spec.agent.handle)?;
        let actors = store.actors().await?;
        let agents = store.agents().await?;
        let mut roster = Vec::new();
        for a in actors {
            match a.kind {
                ActorKind::System => continue,
                ActorKind::Human => roster.push(RosterLine {
                    handle: a.handle,
                    kind: "human",
                    description:
                        "the person running this board; can see everything and may answer you"
                            .into(),
                    state: String::new(),
                }),
                ActorKind::Agent => {
                    if let Some(g) = agents.iter().find(|g| g.actor_id == a.id) {
                        if g.is_retired() {
                            continue;
                        }
                        let state = if a.id == spec.agent.actor_id {
                            "you".to_string()
                        } else {
                            g.state.label().to_lowercase()
                        };
                        roster.push(RosterLine {
                            handle: a.handle,
                            kind: "agent",
                            description: g.description.clone(),
                            state,
                        });
                    }
                }
            }
        }
        let columns = store.columns().await?;
        let policy = project.policy();
        Ok(prompts::system_prompt(&PromptContext {
            agent: &spec.agent,
            definition_body: &definition.body,
            memory: &memory,
            roster: &roster,
            columns: &columns,
            workspace: &spec.workspace,
            push_allowed: policy.push_policy == ak_core::config::PushPolicy::Allowed,
            integration_branch: policy.integration_branch.as_deref(),
        }))
    }
}

struct Driver {
    bridge: Bridge,
    notices: broadcast::Sender<RunNotice>,
    project: Arc<OpenProject>,
    agent: Agent,
    run: AgentRun,
    run_dir: PathBuf,
    token: String,
    wall_clock: Duration,
    startup_timeout: Duration,
}

enum Line {
    Out(String),
    Err(String),
    Closed,
}

impl Driver {
    async fn drive(
        mut self,
        mut child: tokio::process::Child,
        stop_rx: oneshot::Receiver<StopReason>,
    ) -> AgentRun {
        let store = self.project.store();
        let project_id = self.project.id();
        let (line_tx, mut line_rx) = mpsc::channel::<Line>(256);

        if let Some(stdout) = child.stdout.take() {
            let tx = line_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send(Line::Out(l)).await.is_err() {
                        break;
                    }
                }
                let _ = tx.send(Line::Closed).await;
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let tx = line_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if tx.send(Line::Err(l)).await.is_err() {
                        break;
                    }
                }
            });
        }
        drop(line_tx);

        let mut seq: i64 = 0;
        let mut stderr_tail: VecDeque<String> = VecDeque::new();
        let mut raw_stdout = String::new();
        let mut got_init = false;
        let mut result: Option<Value> = None;
        let mut forced_exit: Option<(ExitReason, String)> = None;
        let mut stop_rx = stop_rx;
        let deadline = tokio::time::sleep(self.wall_clock);
        tokio::pin!(deadline);
        let startup = tokio::time::sleep(self.startup_timeout);
        tokio::pin!(startup);

        self.record(
            &mut seq,
            RunEventKind::Supervisor,
            json!({"message": "process started"}),
        )
        .await;

        loop {
            tokio::select! {
                line = line_rx.recv() => match line {
                    Some(Line::Out(l)) => {
                        raw_stdout.push_str(&l);
                        raw_stdout.push('\n');
                        for (kind, payload) in parse_line(&l) {
                            if kind == RunEventKind::SystemInit {
                                got_init = true;
                                if let Some(sid) = payload.get("session_id").and_then(|v| v.as_str()) {
                                    self.run.claude_session_id = Some(sid.to_string());
                                }
                                let board_ok = payload
                                    .get("mcp_servers")
                                    .and_then(|v| v.as_array())
                                    .map(|servers| servers.iter().any(|s| s.get("name").and_then(|n| n.as_str()) == Some("board") && s.get("status").and_then(|st| st.as_str()) == Some("connected")))
                                    .unwrap_or(true);
                                if !board_ok {
                                    forced_exit = Some((ExitReason::Crashed, "the board MCP server did not connect".into()));
                                    let _ = child.kill().await;
                                }
                                let _ = store.set_agent_state(self.agent.actor_id, AgentState::Running, Some(self.run.id)).await;
                            }
                            if kind == RunEventKind::Result {
                                result = Some(payload.clone());
                            }
                            self.record(&mut seq, kind, payload).await;
                        }
                    }
                    Some(Line::Err(l)) => {
                        if stderr_tail.len() >= 40 {
                            stderr_tail.pop_front();
                        }
                        stderr_tail.push_back(l);
                    }
                    Some(Line::Closed) | None => break,
                },
                _ = &mut deadline => {
                    forced_exit = Some((ExitReason::TimedOut, format!("exceeded the wall clock of {} minutes", self.wall_clock.as_secs() / 60)));
                    let _ = child.kill().await;
                    break;
                }
                _ = &mut startup, if !got_init => {
                    forced_exit = Some((ExitReason::Crashed, format!("no init message within {} seconds", self.startup_timeout.as_secs())));
                    let _ = child.kill().await;
                    break;
                }
                reason = &mut stop_rx => {
                    let reason = reason.unwrap_or(StopReason::Stopped);
                    let exit = match reason {
                        StopReason::Stopped | StopReason::Halted => ExitReason::Halted,
                        StopReason::Guardrail => ExitReason::GuardrailTripped,
                    };
                    forced_exit = Some((exit, format!("stopped: {reason:?}")));
                    let _ = child.kill().await;
                    break;
                }
            }
        }

        let status = child.wait().await.ok();
        let _ = std::fs::write(self.run_dir.join("stdout.jsonl"), &raw_stdout);
        let _ = std::fs::write(
            self.run_dir.join("stderr.log"),
            stderr_tail.iter().cloned().collect::<Vec<_>>().join("\n"),
        );

        let (exit, error) = match forced_exit {
            Some((e, msg)) => (e, Some(msg)),
            None => classify(result.as_ref(), status.as_ref(), &stderr_tail),
        };
        if let Some(r) = &result {
            self.run.num_turns = r.get("num_turns").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            self.run.cost_usd = r.get("total_cost_usd").and_then(|v| v.as_f64());
            if let Some(u) = r.get("usage") {
                // Cached input is still input the model read.
                let n = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                self.run.input_tokens = n("input_tokens")
                    + n("cache_creation_input_tokens")
                    + n("cache_read_input_tokens");
                self.run.output_tokens = n("output_tokens");
            }
        }
        self.run.ended_at = Some(Utc::now());
        self.run.exit_reason = Some(exit);
        self.run.error = error.clone();
        self.record(
            &mut seq,
            RunEventKind::Supervisor,
            json!({"message": format!("ended: {}{}", exit.label(), error.map(|e| format!(" ({e})")).unwrap_or_default())}),
        )
        .await;
        if let Err(e) = store.update_run(&self.run).await {
            tracing::warn!("could not record run end: {e}");
        }

        // The scheduler owns Halted; everything else returns to Idle.
        let keep_halted = matches!(
            store.agent(self.agent.actor_id).await.map(|a| a.state),
            Ok(AgentState::Halted)
        );
        if !keep_halted {
            let _ = store
                .set_agent_state(self.agent.actor_id, AgentState::Idle, None)
                .await;
        } else {
            let _ = store
                .set_agent_state(self.agent.actor_id, AgentState::Halted, None)
                .await;
        }
        self.bridge.unregister_run(&self.token);
        let _ = self.notices.send(RunNotice::Ended {
            project: project_id,
            run: self.run.clone(),
        });
        self.run
    }

    async fn record(&self, seq: &mut i64, kind: RunEventKind, payload: Value) {
        let event = RunEvent {
            run_id: self.run.id,
            seq: *seq,
            at: Utc::now(),
            kind,
            payload,
        };
        *seq += 1;
        if let Err(e) = self.project.store().append_run_event(&event).await {
            tracing::warn!("could not record run event: {e}");
        }
        let _ = self.notices.send(RunNotice::Event {
            project: self.project.id(),
            event,
        });
    }
}

/// How a run ended when the supervisor did not force it.
fn classify(
    result: Option<&Value>,
    status: Option<&std::process::ExitStatus>,
    stderr_tail: &VecDeque<String>,
) -> (ExitReason, Option<String>) {
    match result {
        Some(r) => {
            let subtype = r.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            let is_error = r.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
            let text = r
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            match (subtype, is_error) {
                ("success", false) => (ExitReason::Completed, None),
                ("error_max_turns", _) | ("error_max_budget_usd", _) => {
                    (ExitReason::BudgetExceeded, Some(subtype.to_string()))
                }
                _ => {
                    let errors = r
                        .get("errors")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|e| e.as_str())
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .unwrap_or_default();
                    let msg = if !text.is_empty() {
                        text
                    } else if !errors.is_empty() {
                        errors
                    } else {
                        subtype.to_string()
                    };
                    (ExitReason::Crashed, Some(msg))
                }
            }
        }
        None => {
            let tail: Vec<&str> = stderr_tail
                .iter()
                .rev()
                .take(5)
                .map(|s| s.as_str())
                .collect();
            let mut msg = format!(
                "process exited {} without a result",
                status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".into())
            );
            if !tail.is_empty() {
                msg.push_str(": ");
                msg.push_str(&tail.into_iter().rev().collect::<Vec<_>>().join(" | "));
            }
            (ExitReason::Crashed, Some(msg))
        }
    }
}

/// Parses one stream-json line into zero or more run events.
pub fn parse_line(line: &str) -> Vec<(RunEventKind, Value)> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        if line.trim().is_empty() {
            return Vec::new();
        }
        return vec![(
            RunEventKind::Other,
            json!({"message": truncate(line, 2000)}),
        )];
    };
    // Rate-limit notices arrive under their own top-level type; nobody reads them.
    if v.get("rate_limit_info").is_some() {
        return Vec::new();
    }
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "system" => {
            let subtype = v
                .get("subtype")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            match subtype.as_str() {
                "init" => vec![(RunEventKind::SystemInit, v)],
                // Bookkeeping Claude Code emits constantly; nothing a reader wants.
                "thinking_tokens" | "rate_limit_info" | "hook_started" => Vec::new(),
                "hook_response" => {
                    let outcome = text("outcome");
                    if outcome == "success" {
                        return Vec::new();
                    }
                    let message =
                        format!(
                            "hook {} {outcome} (from your own Claude Code settings or plugins, not this app): {}",
                            text("hook_name"),
                            text("output")
                        );
                    vec![(
                        RunEventKind::Other,
                        json!({"message": truncate(&message, 2000)}),
                    )]
                }
                "task_summary" => {
                    let detail = text("detail");
                    if detail.trim().is_empty() {
                        Vec::new()
                    } else {
                        vec![(
                            RunEventKind::Other,
                            json!({"message": format!("task: {detail}")}),
                        )]
                    }
                }
                s if s.starts_with("vcs_state_change") => vec![(
                    RunEventKind::Other,
                    json!({"message": format!("git {} on {}", text("kind"), text("branch"))}),
                )],
                _ => vec![(
                    RunEventKind::Other,
                    json!({"message": truncate(&v.to_string(), 2000)}),
                )],
            }
        }
        "assistant" => {
            let mut out = Vec::new();
            if let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                for b in blocks {
                    match b.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            if !text.trim().is_empty() {
                                out.push((
                                    RunEventKind::AssistantText,
                                    json!({"text": truncate(text, 4000)}),
                                ));
                            }
                        }
                        Some("tool_use") => out.push((
                            RunEventKind::ToolCall,
                            json!({
                                "name": b.get("name").cloned().unwrap_or(Value::Null),
                                "input": b.get("input").cloned().unwrap_or(Value::Null),
                                "id": b.get("id").cloned().unwrap_or(Value::Null),
                            }),
                        )),
                        _ => {}
                    }
                }
            }
            out
        }
        "user" => {
            let mut out = Vec::new();
            if let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                        let content = match b.get("content") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        };
                        out.push((
                            RunEventKind::ToolResult,
                            json!({
                                "tool_use_id": b.get("tool_use_id").cloned().unwrap_or(Value::Null),
                                "content": truncate(&content, 2000),
                                "is_error": b.get("is_error").cloned().unwrap_or(Value::Bool(false)),
                            }),
                        ));
                    }
                }
            }
            out
        }
        "result" => vec![(RunEventKind::Result, v)],
        _ => vec![(
            RunEventKind::Other,
            json!({"message": truncate(&v.to_string(), 2000)}),
        )],
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_json_shapes() {
        let init = r#"{"type":"system","subtype":"init","session_id":"s","model":"m","mcp_servers":[{"name":"board","status":"connected"}]}"#;
        let ev = parse_line(init);
        assert_eq!(ev[0].0, RunEventKind::SystemInit);

        let assistant = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#;
        let ev = parse_line(assistant);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].0, RunEventKind::AssistantText);
        assert_eq!(ev[1].0, RunEventKind::ToolCall);
        assert_eq!(ev[1].1["name"], "Bash");

        let user = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file.txt"}]}}"#;
        let ev = parse_line(user);
        assert_eq!(ev[0].0, RunEventKind::ToolResult);
        assert_eq!(ev[0].1["content"], "file.txt");

        let result = r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.1,"num_turns":3}"#;
        let ev = parse_line(result);
        assert_eq!(ev[0].0, RunEventKind::Result);

        assert!(parse_line("").is_empty());
        assert_eq!(parse_line("garbage")[0].0, RunEventKind::Other);
    }

    #[test]
    fn classifies_results() {
        let ok = json!({"subtype": "success", "is_error": false});
        assert_eq!(
            classify(Some(&ok), None, &VecDeque::new()).0,
            ExitReason::Completed
        );
        let budget = json!({"subtype": "error_max_budget_usd", "is_error": true});
        assert_eq!(
            classify(Some(&budget), None, &VecDeque::new()).0,
            ExitReason::BudgetExceeded
        );
        let auth =
            json!({"subtype": "success", "is_error": true, "result": "Failed to authenticate"});
        let (e, msg) = classify(Some(&auth), None, &VecDeque::new());
        assert_eq!(e, ExitReason::Crashed);
        assert_eq!(msg.as_deref(), Some("Failed to authenticate"));
        let mut tail = VecDeque::new();
        tail.push_back("boom".to_string());
        let (e, msg) = classify(None, None, &tail);
        assert_eq!(e, ExitReason::Crashed);
        assert!(msg.unwrap().contains("boom"));
    }
}
