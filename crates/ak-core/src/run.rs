//! Agent runs: one Claude Code process, from wake to exit, and the parsed
//! transcript it produced.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{ActorId, NotificationId, RunId};
use crate::Timestamp;

/// Why a run started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunTrigger {
    /// Wake notifications were pending.
    Notifications { ids: Vec<NotificationId> },
    /// Someone asked the scheduler to wake the agent.
    Activated { by: ActorId, reason: String },
    /// Started from the UI with no pending work.
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Completed,
    Halted,
    BudgetExceeded,
    TimedOut,
    GuardrailTripped,
    Crashed,
}

impl ExitReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExitReason::Completed => "completed",
            ExitReason::Halted => "halted",
            ExitReason::BudgetExceeded => "budget_exceeded",
            ExitReason::TimedOut => "timed_out",
            ExitReason::GuardrailTripped => "guardrail_tripped",
            ExitReason::Crashed => "crashed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "completed" => Some(ExitReason::Completed),
            "halted" => Some(ExitReason::Halted),
            "budget_exceeded" => Some(ExitReason::BudgetExceeded),
            "timed_out" => Some(ExitReason::TimedOut),
            "guardrail_tripped" => Some(ExitReason::GuardrailTripped),
            "crashed" => Some(ExitReason::Crashed),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ExitReason::Completed => "Completed",
            ExitReason::Halted => "Halted",
            ExitReason::BudgetExceeded => "Budget exceeded",
            ExitReason::TimedOut => "Timed out",
            ExitReason::GuardrailTripped => "Guardrail tripped",
            ExitReason::Crashed => "Crashed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRun {
    pub id: RunId,
    pub agent_id: ActorId,
    pub trigger: RunTrigger,
    pub claude_session_id: Option<String>,
    pub workspace_path: PathBuf,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub exit_reason: Option<ExitReason>,
    pub num_turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
    /// What the run produced, tallied from its transcript. `None` for runs
    /// recorded before outcomes existed.
    pub outcome: Option<RunOutcome>,
}

impl AgentRun {
    pub fn start(
        agent_id: ActorId,
        trigger: RunTrigger,
        workspace_path: PathBuf,
        now: Timestamp,
    ) -> Self {
        Self {
            id: RunId::new(),
            agent_id,
            trigger,
            claude_session_id: None,
            workspace_path,
            started_at: now,
            ended_at: None,
            exit_reason: None,
            num_turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: None,
            error: None,
            outcome: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.ended_at.is_none()
    }
}

/// What a run produced, tallied from its transcript as it went: the numbers
/// the analyst sets against its cost.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunOutcome {
    /// Tool calls by tool name.
    pub tool_calls: BTreeMap<String, u32>,
    /// Tool results that reported an error.
    pub tool_errors: u32,
    /// `git commit` invocations.
    pub commits: u32,
    /// `git merge` invocations.
    pub merges: u32,
    pub ticket_moves: u32,
    /// Comments and replies on tickets.
    pub comments: u32,
    /// Threads started and messages posted in chats.
    pub messages: u32,
    /// Ticket keys the run read or changed through the board tools.
    pub tickets_touched: Vec<String>,
    /// The final assistant text, trimmed.
    pub result: Option<String>,
}

impl RunOutcome {
    pub fn note_tool_call(&mut self, name: &str, input: &Value) {
        *self.tool_calls.entry(name.to_string()).or_insert(0) += 1;
        let bare = name.strip_prefix("mcp__board__").unwrap_or(name);
        match bare {
            "ticket_move" => self.ticket_moves += 1,
            "ticket_comment" | "ticket_reply" => self.comments += 1,
            "thread_create" | "thread_post" => self.messages += 1,
            "Bash" => {
                let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
                for segment in crate::permissions::bash_segments(command) {
                    let mut words = segment.split_whitespace();
                    if words.next() != Some("git") {
                        continue;
                    }
                    // Skip `-c k=v` and `--no-pager` to reach the verb.
                    let verb = words.find(|w| !w.starts_with('-') && !w.contains('='));
                    match verb {
                        Some("commit") => self.commits += 1,
                        Some("merge") => self.merges += 1,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if bare.starts_with("ticket_") {
            if let Some(key) = input.get("key").and_then(|v| v.as_str()) {
                if !self.tickets_touched.iter().any(|k| k == key) {
                    self.tickets_touched.push(key.to_string());
                }
            }
        }
    }

    pub fn note_tool_result(&mut self, is_error: bool) {
        if is_error {
            self.tool_errors += 1;
        }
    }

    pub fn total_tool_calls(&self) -> u32 {
        self.tool_calls.values().sum()
    }

    /// Calls to the board tools, as opposed to the code tools.
    pub fn board_tool_calls(&self) -> u32 {
        self.tool_calls
            .iter()
            .filter(|(n, _)| n.starts_with("mcp__board__"))
            .map(|(_, c)| c)
            .sum()
    }
}

/// Why the scheduler left an agent with pending work unstarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Halted,
    Cooldown,
    WakeStorm,
    DailyBudget,
    AgentBudget,
    NoSlot,
    WorkspaceFailed,
    StartFailed,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::Halted => "halted",
            SkipReason::Cooldown => "cooldown",
            SkipReason::WakeStorm => "wake_storm",
            SkipReason::DailyBudget => "daily_budget",
            SkipReason::AgentBudget => "agent_budget",
            SkipReason::NoSlot => "no_slot",
            SkipReason::WorkspaceFailed => "workspace_failed",
            SkipReason::StartFailed => "start_failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "halted" => Some(SkipReason::Halted),
            "cooldown" => Some(SkipReason::Cooldown),
            "wake_storm" => Some(SkipReason::WakeStorm),
            "daily_budget" => Some(SkipReason::DailyBudget),
            "agent_budget" => Some(SkipReason::AgentBudget),
            "no_slot" => Some(SkipReason::NoSlot),
            "workspace_failed" => Some(SkipReason::WorkspaceFailed),
            "start_failed" => Some(SkipReason::StartFailed),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SkipReason::Halted => "halted",
            SkipReason::Cooldown => "cooling down",
            SkipReason::WakeStorm => "wake rate exceeded",
            SkipReason::DailyBudget => "daily budget reached",
            SkipReason::AgentBudget => "agent's daily budget reached",
            SkipReason::NoSlot => "no free run slot",
            SkipReason::WorkspaceFailed => "worktree could not be prepared",
            SkipReason::StartFailed => "process could not start",
        }
    }
}

/// A scheduler tick that left an agent with pending work unstarted. Recorded
/// when the reason changes or a while has passed, so a long wait is a few
/// rows rather than one per tick.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulerDecision {
    pub at: Timestamp,
    pub agent_id: ActorId,
    pub reason: SkipReason,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEventKind {
    SystemInit,
    AssistantText,
    ToolCall,
    ToolResult,
    Result,
    Stderr,
    /// Something the supervisor did or observed: spawn, timeout, kill.
    Supervisor,
    Other,
}

impl RunEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunEventKind::SystemInit => "system_init",
            RunEventKind::AssistantText => "assistant_text",
            RunEventKind::ToolCall => "tool_call",
            RunEventKind::ToolResult => "tool_result",
            RunEventKind::Result => "result",
            RunEventKind::Stderr => "stderr",
            RunEventKind::Supervisor => "supervisor",
            RunEventKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "system_init" => Some(RunEventKind::SystemInit),
            "assistant_text" => Some(RunEventKind::AssistantText),
            "tool_call" => Some(RunEventKind::ToolCall),
            "tool_result" => Some(RunEventKind::ToolResult),
            "result" => Some(RunEventKind::Result),
            "stderr" => Some(RunEventKind::Stderr),
            "supervisor" => Some(RunEventKind::Supervisor),
            "other" => Some(RunEventKind::Other),
            _ => None,
        }
    }
}

/// One line of a run's transcript, as the supervisor parsed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub run_id: RunId,
    pub seq: i64,
    pub at: Timestamp,
    pub kind: RunEventKind,
    /// Kind-specific: `{"text"}`, `{"name","input"}`, `{"content"}`, the raw
    /// result object, or `{"message"}`.
    pub payload: serde_json::Value,
}

impl RunEvent {
    /// A one-line rendering for live views and the inspector.
    pub fn summary(&self) -> String {
        let p = &self.payload;
        let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let text = match self.kind {
            RunEventKind::SystemInit => {
                format!("session {} with model {}", s("session_id"), s("model"))
            }
            RunEventKind::AssistantText => s("text"),
            RunEventKind::ToolCall => {
                let input = p.get("input").cloned().unwrap_or_default();
                let subject = crate::permissions::summarise("", &input)
                    .trim_start_matches(": ")
                    .to_string();
                let detail = if subject.is_empty() {
                    brief_fields(&input)
                } else {
                    subject
                };
                format!("{} {}", s("name"), detail)
            }
            RunEventKind::ToolResult => s("content"),
            RunEventKind::Result => format!(
                "{}{}",
                s("subtype"),
                p.get("total_cost_usd")
                    .and_then(|v| v.as_f64())
                    .map(|c| format!(", ${c:.3}"))
                    .unwrap_or_default()
            ),
            RunEventKind::Stderr | RunEventKind::Supervisor | RunEventKind::Other => s("message"),
        };
        let mut one_line: String = text.lines().collect::<Vec<_>>().join(" ");
        if one_line.chars().count() > 200 {
            one_line = one_line.chars().take(197).collect::<String>() + "…";
        }
        one_line
    }
}

/// The scalar fields of a tool input as `key=value` pairs, for calls whose
/// input has no single obvious subject (the board tools, mostly).
fn brief_fields(input: &Value) -> String {
    let Some(obj) = input.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (k, v) in obj {
        let shown = match v {
            Value::String(s) => {
                let one: String = s.lines().collect::<Vec<_>>().join(" ");
                if one.chars().count() > 60 {
                    format!("\"{}…\"", one.chars().take(57).collect::<String>())
                } else {
                    format!("\"{one}\"")
                }
            }
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Array(a) => format!("[{} items]", a.len()),
            _ => continue,
        };
        parts.push(format!("{k}={shown}"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outcome_tallies_tool_calls() {
        let mut o = RunOutcome::default();
        o.note_tool_call("mcp__board__inbox_list", &json!({}));
        o.note_tool_call("mcp__board__ticket_get", &json!({"key": "ATP-3"}));
        o.note_tool_call(
            "mcp__board__ticket_move",
            &json!({"key": "ATP-3", "column": "In progress", "version": 2}),
        );
        o.note_tool_call(
            "mcp__board__ticket_comment",
            &json!({"key": "ATP-3", "body": "hi"}),
        );
        o.note_tool_call(
            "mcp__board__thread_post",
            &json!({"thread_id": "x", "body": "hi"}),
        );
        o.note_tool_call(
            "Bash",
            &json!({"command": "cd ws && git add -A && git -c user.name=x commit -m 'ATP-3: x'"}),
        );
        o.note_tool_call(
            "Bash",
            &json!({"command": "git merge --no-ff ticket/ATP-3"}),
        );
        o.note_tool_call("Bash", &json!({"command": "git log --oneline"}));
        o.note_tool_result(false);
        o.note_tool_result(true);

        assert_eq!(o.total_tool_calls(), 8);
        assert_eq!(o.board_tool_calls(), 5);
        assert_eq!(o.ticket_moves, 1);
        assert_eq!(o.comments, 1);
        assert_eq!(o.messages, 1);
        assert_eq!(o.commits, 1);
        assert_eq!(o.merges, 1);
        assert_eq!(o.tool_errors, 1);
        assert_eq!(o.tickets_touched, vec!["ATP-3".to_string()]);
        assert_eq!(o.tool_calls["Bash"], 3);
    }

    #[test]
    fn skip_reasons_round_trip() {
        for r in [
            SkipReason::Halted,
            SkipReason::Cooldown,
            SkipReason::WakeStorm,
            SkipReason::DailyBudget,
            SkipReason::AgentBudget,
            SkipReason::NoSlot,
            SkipReason::WorkspaceFailed,
            SkipReason::StartFailed,
        ] {
            assert_eq!(SkipReason::parse(r.as_str()), Some(r));
        }
    }
}
