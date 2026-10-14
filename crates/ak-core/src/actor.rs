use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::ids::ActorId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
    System,
}

impl ActorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActorKind::Human => "human",
            ActorKind::Agent => "agent",
            ActorKind::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "human" => Some(ActorKind::Human),
            "agent" => Some(ActorKind::Agent),
            "system" => Some(ActorKind::System),
            _ => None,
        }
    }
}

/// Anyone who can act on a board. The human, every agent and the reserved
/// system actor are all actors, which is what lets agents and humans share
/// one set of verbs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub id: ActorId,
    pub kind: ActorKind,
    /// Unique within a project. Used for `@mentions`.
    pub handle: String,
    pub display_name: String,
}

impl Actor {
    pub const SYSTEM_HANDLE: &'static str = "system";

    pub fn human(handle: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            id: ActorId::new(),
            kind: ActorKind::Human,
            handle: handle.into(),
            display_name: display_name.into(),
        }
    }

    pub fn system() -> Self {
        Self {
            id: ActorId::SYSTEM,
            kind: ActorKind::System,
            handle: Self::SYSTEM_HANDLE.to_string(),
            display_name: "System".to_string(),
        }
    }

    pub fn is_human(&self) -> bool {
        self.kind == ActorKind::Human
    }

    pub fn is_agent(&self) -> bool {
        self.kind == ActorKind::Agent
    }

    /// Handles are lowercase, start with a letter, and contain only letters,
    /// digits, `-` and `_`. Between 2 and 32 characters.
    pub fn validate_handle(handle: &str) -> Result<(), CoreError> {
        let len = handle.chars().count();
        if !(2..=32).contains(&len) {
            return Err(CoreError::validation("handle must be 2 to 32 characters"));
        }
        let mut chars = handle.chars();
        let first = chars.next().unwrap();
        if !first.is_ascii_lowercase() {
            return Err(CoreError::validation(
                "handle must start with a lowercase letter",
            ));
        }
        if !handle
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(CoreError::validation(
                "handle may only contain lowercase letters, digits, '-' and '_'",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_reasonable_handles() {
        for h in ["me", "backend", "db-admin", "qa_2"] {
            assert!(Actor::validate_handle(h).is_ok(), "{h}");
        }
    }

    #[test]
    fn rejects_bad_handles() {
        for h in ["", "a", "Backend", "1st", "has space", "has.dot"] {
            assert!(Actor::validate_handle(h).is_err(), "{h}");
        }
    }
}
