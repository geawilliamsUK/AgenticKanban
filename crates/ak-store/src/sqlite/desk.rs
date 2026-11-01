//! Desk sessions and their transcripts. The trait methods in the parent
//! module delegate here. Nothing here is a board event: the Desk acts on the
//! board through the bridge like anyone else, and these rows are its own
//! record of the conversation.

use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::Row;

use ak_core::desk::{DeskEntry, DeskEntryKind, DeskSession};
use ak_core::ids::DeskSessionId;
use ak_core::Timestamp;

use super::rows::*;
use super::SqliteStore;
use crate::error::StoreError;
use crate::Result;

fn session_from_row(row: &SqliteRow) -> Result<DeskSession> {
    Ok(DeskSession {
        id: parse_id(&row.try_get::<String, _>("id")?, "desk session")?,
        title: row.try_get("title")?,
        claude_session_id: row.try_get("claude_session_id")?,
        started: row.try_get::<i64, _>("started")? != 0,
        model: row.try_get("model")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        last_active_at: parse_ts(&row.try_get::<String, _>("last_active_at")?)?,
        cost_usd: row.try_get("cost_usd")?,
        archived: row.try_get::<i64, _>("archived")? != 0,
        resume_at: row.try_get("resume_at")?,
    })
}

fn entry_from_row(row: &SqliteRow) -> Result<DeskEntry> {
    let kind: String = row.try_get("kind")?;
    let payload: String = row.try_get("payload")?;
    Ok(DeskEntry {
        session_id: parse_id(&row.try_get::<String, _>("session_id")?, "desk session")?,
        seq: row.try_get("seq")?,
        at: parse_ts(&row.try_get::<String, _>("at")?)?,
        kind: DeskEntryKind::parse(&kind)
            .ok_or_else(|| StoreError::corrupt(format!("desk entry kind {kind:?}")))?,
        payload: serde_json::from_str(&payload)?,
    })
}

impl SqliteStore {
    pub(super) async fn insert_desk_session_impl(&self, s: &DeskSession) -> Result<()> {
        sqlx::query(
            "INSERT INTO desk_sessions (id, title, claude_session_id, started, model, created_at, \
             last_active_at, cost_usd, archived, resume_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(s.id.to_string())
        .bind(&s.title)
        .bind(&s.claude_session_id)
        .bind(s.started as i64)
        .bind(&s.model)
        .bind(fmt_ts(&s.created_at))
        .bind(fmt_ts(&s.last_active_at))
        .bind(s.cost_usd)
        .bind(s.archived as i64)
        .bind(&s.resume_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(super) async fn update_desk_session_impl(&self, s: &DeskSession) -> Result<()> {
        let result = sqlx::query(
            "UPDATE desk_sessions SET title = ?, claude_session_id = ?, started = ?, model = ?, \
             last_active_at = ?, cost_usd = ?, archived = ?, resume_at = ? WHERE id = ?",
        )
        .bind(&s.title)
        .bind(&s.claude_session_id)
        .bind(s.started as i64)
        .bind(&s.model)
        .bind(fmt_ts(&s.last_active_at))
        .bind(s.cost_usd)
        .bind(s.archived as i64)
        .bind(&s.resume_at)
        .bind(s.id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::not_found(format!("desk session {}", s.id)));
        }
        Ok(())
    }

    pub(super) async fn desk_session_impl(&self, id: DeskSessionId) -> Result<DeskSession> {
        let row = sqlx::query("SELECT * FROM desk_sessions WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| StoreError::not_found(format!("desk session {id}")))?;
        session_from_row(&row)
    }

    pub(super) async fn desk_sessions_impl(&self) -> Result<Vec<DeskSession>> {
        let rows = sqlx::query("SELECT * FROM desk_sessions ORDER BY last_active_at DESC")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(session_from_row).collect()
    }

    pub(super) async fn append_desk_entry_impl(
        &self,
        session: DeskSessionId,
        kind: DeskEntryKind,
        payload: Value,
        at: Timestamp,
    ) -> Result<DeskEntry> {
        let seq: i64 = sqlx::query(
            "INSERT INTO desk_entries (session_id, at, kind, payload) VALUES (?, ?, ?, ?) \
             RETURNING seq",
        )
        .bind(session.to_string())
        .bind(fmt_ts(&at))
        .bind(kind.as_str())
        .bind(serde_json::to_string(&payload)?)
        .fetch_one(&self.pool)
        .await?
        .try_get("seq")?;
        Ok(DeskEntry {
            session_id: session,
            seq,
            at,
            kind,
            payload,
        })
    }

    pub(super) async fn truncate_desk_entries_impl(
        &self,
        session: DeskSessionId,
        from_seq: i64,
    ) -> Result<u64> {
        let result = sqlx::query("DELETE FROM desk_entries WHERE session_id = ? AND seq >= ?")
            .bind(session.to_string())
            .bind(from_seq)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub(super) async fn desk_entries_impl(&self, session: DeskSessionId) -> Result<Vec<DeskEntry>> {
        let rows = sqlx::query("SELECT * FROM desk_entries WHERE session_id = ? ORDER BY seq")
            .bind(session.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(entry_from_row).collect()
    }
}
