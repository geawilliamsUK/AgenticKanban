//! A tool call an agent wants to make that its permission profile does not
//! settle, waiting for the human.

use serde::{Deserialize, Serialize};

use crate::ids::{ActorId, ApprovalId, RunId};
use crate::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
    /// The hook timed out or the run ended before the human decided.
    Expired,
}

impl ApprovalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalState::Pending => "pending",
            ApprovalState::Approved => "approved",
            ApprovalState::Denied => "denied",
            ApprovalState::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(ApprovalState::Pending),
            "approved" => Some(ApprovalState::Approved),
            "denied" => Some(ApprovalState::Denied),
            "expired" => Some(ApprovalState::Expired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Approval {
    pub id: ApprovalId,
    pub run_id: RunId,
    pub agent_id: ActorId,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub summary: String,
    pub state: ApprovalState,
    pub created_at: Timestamp,
    pub decided_by: Option<ActorId>,
    pub decided_at: Option<Timestamp>,
}

impl Approval {
    pub fn new(
        run_id: RunId,
        agent_id: ActorId,
        tool_name: impl Into<String>,
        input: serde_json::Value,
        now: Timestamp,
    ) -> Self {
        let tool_name = tool_name.into();
        let summary = crate::permissions::summarise(&tool_name, &input);
        Self {
            id: ApprovalId::new(),
            run_id,
            agent_id,
            tool_name,
            input,
            summary,
            state: ApprovalState::Pending,
            created_at: now,
            decided_by: None,
            decided_at: None,
        }
    }
}
