//! Agent runs: one Claude Code process, from wake to exit, and the parsed
//! transcript it produced.

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
        }
    }

    pub fn is_active(&self) -> bool {
        self.ended_at.is_none()
    }
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
                format!(
                    "{} {}",
                    s("name"),
                    crate::permissions::summarise("", &input).trim_start_matches(": ")
                )
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
