//! Utility calls: Refine, Integrate and Analyse. Each is a tool-less Claude
//! Code call whose result is a proposal the human reviews: a definition for
//! the first two, a set of recommendations for the analyst. The record keeps
//! the proposal, its cost and the decision.

use serde::{Deserialize, Serialize};

use crate::ids::{ActorId, CallId};
use crate::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UtilityKind {
    Refine,
    Integrate,
    /// The team analyst. Its agent is the system actor.
    Analyse,
}

impl UtilityKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            UtilityKind::Refine => "refine",
            UtilityKind::Integrate => "integrate",
            UtilityKind::Analyse => "analyse",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "refine" => Some(UtilityKind::Refine),
            "integrate" => Some(UtilityKind::Integrate),
            "analyse" => Some(UtilityKind::Analyse),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UtilityState {
    Running,
    /// The call returned a proposal that awaits the human's decision.
    Proposed,
    Accepted,
    Rejected,
    Failed,
}

impl UtilityState {
    pub fn as_str(&self) -> &'static str {
        match self {
            UtilityState::Running => "running",
            UtilityState::Proposed => "proposed",
            UtilityState::Accepted => "accepted",
            UtilityState::Rejected => "rejected",
            UtilityState::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(UtilityState::Running),
            "proposed" => Some(UtilityState::Proposed),
            "accepted" => Some(UtilityState::Accepted),
            "rejected" => Some(UtilityState::Rejected),
            "failed" => Some(UtilityState::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UtilityCall {
    pub id: CallId,
    pub kind: UtilityKind,
    /// The agent being authored.
    pub agent_id: ActorId,
    /// For Integrate, the agent whose definition the proposal changes.
    pub target_id: Option<ActorId>,
    pub model: Option<String>,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub cost_usd: Option<f64>,
    pub state: UtilityState,
    /// The proposed file, rendered; for Analyse, the analysis as JSON.
    pub proposal: Option<String>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

impl UtilityCall {
    pub fn start(
        kind: UtilityKind,
        agent_id: ActorId,
        target_id: Option<ActorId>,
        model: Option<String>,
        now: Timestamp,
    ) -> Self {
        Self {
            id: CallId::new(),
            kind,
            agent_id,
            target_id,
            model,
            started_at: now,
            ended_at: None,
            cost_usd: None,
            state: UtilityState::Running,
            proposal: None,
            warnings: Vec::new(),
            error: None,
        }
    }

    pub fn propose(
        &mut self,
        proposal: String,
        warnings: Vec<String>,
        cost_usd: Option<f64>,
        now: Timestamp,
    ) {
        self.state = UtilityState::Proposed;
        self.proposal = Some(proposal);
        self.warnings = warnings;
        self.cost_usd = cost_usd;
        self.ended_at = Some(now);
    }

    pub fn fail(&mut self, error: String, cost_usd: Option<f64>, now: Timestamp) {
        self.state = UtilityState::Failed;
        self.error = Some(error);
        self.cost_usd = cost_usd;
        self.ended_at = Some(now);
    }
}
