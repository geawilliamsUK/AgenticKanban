//! The Desk: the human's own long-running Claude Code conversation about one
//! project. It plans, prioritises and talks to the team through the board,
//! acting as the human, the way a main session sits above a team lead.
//!
//! A session is one Claude Code session, resumed across processes. Its
//! transcript is kept here as entries so the app can show it after a
//! restart without reading Claude Code's own files.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::DeskSessionId;
use crate::Timestamp;

#[derive(Debug, Clone, PartialEq)]
pub struct DeskSession {
    pub id: DeskSessionId,
    pub title: String,
    /// The Claude Code session: created with `--session-id`, then resumed.
    pub claude_session_id: String,
    /// Claude Code has written the session, so the next process resumes it.
    pub started: bool,
    /// None runs Claude Code's own default.
    pub model: Option<String>,
    pub created_at: Timestamp,
    pub last_active_at: Timestamp,
    /// API-equivalent cost of every turn so far.
    pub cost_usd: f64,
    pub archived: bool,
}

impl DeskSession {
    pub const UNTITLED: &'static str = "New session";

    pub fn new(model: Option<String>, now: Timestamp) -> Self {
        Self {
            id: DeskSessionId::new(),
            title: Self::UNTITLED.into(),
            claude_session_id: uuid::Uuid::new_v4().to_string(),
            started: false,
            model,
            created_at: now,
            last_active_at: now,
            cost_usd: 0.0,
            archived: false,
        }
    }

    /// Names an untitled session after the first thing the human said.
    pub fn title_from(&mut self, text: &str) {
        if self.title != Self::UNTITLED {
            return;
        }
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        if line.is_empty() || line.starts_with('/') {
            return;
        }
        self.title = if line.chars().count() > 60 {
            format!("{}…", line.chars().take(59).collect::<String>().trim_end())
        } else {
            line.to_string()
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeskEntryKind {
    /// What the human sent: `{"text"}`.
    User,
    /// A finished block of the assistant's text: `{"text"}`.
    Assistant,
    /// `{"id","name","input"}`.
    ToolCall,
    /// `{"tool_use_id","content","is_error"}`.
    ToolResult,
    /// The end of a turn: `{"subtype","is_error","cost_usd","duration_ms",
    /// "interrupted","error"}`, cost being this turn's alone.
    TurnEnd,
    /// The conversation was compacted: `{"trigger","pre_tokens","post_tokens"}`.
    Compacted,
    /// Something the app or Claude Code said about the session: `{"message"}`.
    Notice,
}

impl DeskEntryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeskEntryKind::User => "user",
            DeskEntryKind::Assistant => "assistant",
            DeskEntryKind::ToolCall => "tool_call",
            DeskEntryKind::ToolResult => "tool_result",
            DeskEntryKind::TurnEnd => "turn_end",
            DeskEntryKind::Compacted => "compacted",
            DeskEntryKind::Notice => "notice",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(DeskEntryKind::User),
            "assistant" => Some(DeskEntryKind::Assistant),
            "tool_call" => Some(DeskEntryKind::ToolCall),
            "tool_result" => Some(DeskEntryKind::ToolResult),
            "turn_end" => Some(DeskEntryKind::TurnEnd),
            "compacted" => Some(DeskEntryKind::Compacted),
            "notice" => Some(DeskEntryKind::Notice),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeskEntry {
    pub session_id: DeskSessionId,
    pub seq: i64,
    pub at: Timestamp,
    pub kind: DeskEntryKind,
    pub payload: Value,
}

impl DeskEntry {
    pub fn text(&self, key: &str) -> &str {
        self.payload.get(key).and_then(|v| v.as_str()).unwrap_or("")
    }
}

/// What `/standup [notes]` asks the Desk to do: catch the human up, then
/// brief the lead in a dated thread, which wakes it. The human's notes
/// become part of the brief.
pub fn standup_prompt(notes: &str, today: chrono::NaiveDate) -> String {
    let notes = notes.trim();
    let notes = if notes.is_empty() {
        "(none: keep the current priorities)".to_string()
    } else {
        notes.to_string()
    };
    format!(
        "Run a standup.\n\n\
1. Catch me up. Read the board (ticket_list), my inbox (inbox_list) and the most recent \
standup thread with @lead if there is one (chat_list, thread_list, thread_read). Then tell me, \
briefly: what moved since the last standup, what is in progress and with whom, what is blocked, \
and anything waiting on my decision.\n\
2. Brief the team. If there is an @lead, open a direct chat with it (chat_direct) and start a \
thread titled \"Standup {today}\" (thread_create) with today's priorities in order and my notes \
below; that wakes @lead, which turns them into tickets and assignments and reports back in that \
thread. If there is no @lead, propose the tickets and who should take each, and wait for my \
go-ahead before creating them.\n\
3. Finish by telling me what you sent and to whom. Do not do any implementation work yourself.\n\n\
My notes for this standup:\n{notes}\n",
        today = today.format("%Y-%m-%d"),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_standup_prompt_carries_the_notes_and_the_date() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
        let p = super::standup_prompt("  ship auth first\nthen billing ", day);
        assert!(p.contains("Standup 2026-10-31"));
        assert!(p.contains("ship auth first\nthen billing"));
        assert!(p.contains("chat_direct"));
        assert!(p.contains("no @lead"));
        assert!(super::standup_prompt("", day).contains("keep the current priorities"));
    }

    use super::*;
    use chrono::Utc;

    #[test]
    fn titles_come_from_the_first_real_message() {
        let mut s = DeskSession::new(None, Utc::now());
        s.title_from("/compact");
        assert_eq!(s.title, DeskSession::UNTITLED);
        s.title_from("\n  Plan the auth rework for next week  \nmore");
        assert_eq!(s.title, "Plan the auth rework for next week");
        s.title_from("something else");
        assert_eq!(
            s.title, "Plan the auth rework for next week",
            "kept once set"
        );

        let mut long = DeskSession::new(None, Utc::now());
        long.title_from(&"word ".repeat(30));
        assert!(long.title.ends_with('…'));
        assert!(long.title.chars().count() <= 60);
    }

    #[test]
    fn entry_kinds_round_trip() {
        for k in [
            DeskEntryKind::User,
            DeskEntryKind::Assistant,
            DeskEntryKind::ToolCall,
            DeskEntryKind::ToolResult,
            DeskEntryKind::TurnEnd,
            DeskEntryKind::Compacted,
            DeskEntryKind::Notice,
        ] {
            assert_eq!(DeskEntryKind::parse(k.as_str()), Some(k));
        }
    }
}
