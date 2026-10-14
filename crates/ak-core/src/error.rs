use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("validation failed: {0}")]
    Validation(String),

    #[error("ticket {key} is frozen: {reason}")]
    TicketFrozen { key: String, reason: String },

    #[error("version conflict: expected {expected}, found {actual}")]
    VersionConflict { expected: i64, actual: i64 },

    #[error("not permitted: {0}")]
    NotPermitted(String),
}

impl CoreError {
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }
}
