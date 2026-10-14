use thiserror::Error;

use ak_core::CoreError;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Core(#[from] CoreError),

    #[error("{0} not found")]
    NotFound(String),

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("corrupt row: {0}")]
    Corrupt(String),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

impl StoreError {
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }

    pub fn corrupt(what: impl Into<String>) -> Self {
        Self::Corrupt(what.into())
    }

    /// True for errors the caller caused: validation, conflicts, permissions,
    /// missing rows. False for infrastructure failures.
    pub fn is_client_error(&self) -> bool {
        matches!(self, StoreError::Core(_) | StoreError::NotFound(_))
    }
}
