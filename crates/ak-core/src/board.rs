use serde::{Deserialize, Serialize};

use crate::ids::{ActorId, ColumnId, CommentId, TicketId};
use crate::Timestamp;

/// What a column means, independent of what it is called. Guardrails and
/// agents reason about kinds, not display names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    Backlog,
    Todo,
    InProgress,
    Review,
    Done,
    Custom,
}

impl ColumnKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ColumnKind::Backlog => "backlog",
            ColumnKind::Todo => "todo",
            ColumnKind::InProgress => "in_progress",
            ColumnKind::Review => "review",
            ColumnKind::Done => "done",
            ColumnKind::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "backlog" => Some(ColumnKind::Backlog),
            "todo" => Some(ColumnKind::Todo),
            "in_progress" => Some(ColumnKind::InProgress),
            "review" => Some(ColumnKind::Review),
            "done" => Some(ColumnKind::Done),
            "custom" => Some(ColumnKind::Custom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub id: ColumnId,
    pub name: String,
    pub position: i64,
    pub kind: ColumnKind,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    #[default]
    Medium,
    High,
    Urgent,
}

impl Priority {
    pub const ALL: [Priority; 4] = [
        Priority::Low,
        Priority::Medium,
        Priority::High,
        Priority::Urgent,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Medium => "medium",
            Priority::High => "high",
            Priority::Urgent => "urgent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Priority::Low),
            "medium" => Some(Priority::Medium),
            "high" => Some(Priority::High),
            "urgent" => Some(Priority::Urgent),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Priority::Low => "Low",
            Priority::Medium => "Medium",
            Priority::High => "High",
            Priority::Urgent => "Urgent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ticket {
    pub id: TicketId,
    /// Human-readable key such as `AK-42`. Unique within a project.
    pub key: String,
    pub title: String,
    /// Markdown.
    pub description: String,
    pub column_id: ColumnId,
    /// Ordering within the column. Higher is lower on the board.
    pub position: i64,
    pub assignee_id: Option<ActorId>,
    pub reporter_id: ActorId,
    pub priority: Priority,
    pub labels: Vec<String>,
    pub parent_id: Option<TicketId>,
    pub frozen: bool,
    pub freeze_reason: Option<String>,
    /// Optimistic lock. Every mutation increments it, and mutating commands
    /// must present the version they read.
    pub version: i64,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: CommentId,
    pub ticket_id: TicketId,
    pub author_id: ActorId,
    /// Markdown.
    pub body: String,
    pub parent_comment_id: Option<CommentId>,
    pub created_at: Timestamp,
}
