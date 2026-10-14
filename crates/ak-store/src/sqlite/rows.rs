//! Row to entity mapping. Ids and timestamps are stored as text so the schema
//! stays portable; these helpers do the parsing in one place.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;
use sqlx::Row;

use ak_core::agent::{Agent, AgentState, Budgets};
use ak_core::board::{Column, ColumnKind, Comment, Priority, Ticket};
use ak_core::event::EventKind;
use ak_core::utility::{UtilityCall, UtilityKind, UtilityState};
use ak_core::{Actor, ActorKind, Event, Notification, NotificationKind, NotificationState};

use crate::error::StoreError;

pub fn agent_from_row(row: &SqliteRow) -> Result<Agent, StoreError> {
    let state: String = row.try_get("state")?;
    let budgets: Option<String> = row.try_get("budgets")?;
    let cooldown_until: Option<String> = row.try_get("cooldown_until")?;
    let missing_since: Option<String> = row.try_get("missing_since")?;
    let definition_path: String = row.try_get("definition_path")?;
    let memory_path: String = row.try_get("memory_path")?;
    Ok(Agent {
        actor_id: parse_id(&row.try_get::<String, _>("actor_id")?, "actor")?,
        handle: row.try_get("handle")?,
        description: row.try_get("description")?,
        model: row.try_get("model")?,
        permission_profile: row.try_get("permission_profile")?,
        budgets: budgets
            .as_deref()
            .map(serde_json::from_str::<Budgets>)
            .transpose()?,
        state: AgentState::parse(&state)
            .ok_or_else(|| StoreError::corrupt(format!("agent state {state:?}")))?,
        current_run_id: parse_opt_id(row.try_get("current_run_id")?, "run")?,
        cooldown_until: cooldown_until.as_deref().map(parse_ts).transpose()?,
        missing_since: missing_since.as_deref().map(parse_ts).transpose()?,
        definition_path: definition_path.into(),
        memory_path: memory_path.into(),
    })
}

pub fn parse_id<T: FromStr<Err = uuid::Error>>(raw: &str, what: &str) -> Result<T, StoreError> {
    raw.parse::<T>()
        .map_err(|_| StoreError::corrupt(format!("{what} id {raw:?}")))
}

pub fn parse_opt_id<T: FromStr<Err = uuid::Error>>(
    raw: Option<String>,
    what: &str,
) -> Result<Option<T>, StoreError> {
    raw.map(|s| parse_id(&s, what)).transpose()
}

pub fn parse_ts(raw: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|_| StoreError::corrupt(format!("timestamp {raw:?}")))
}

pub fn fmt_ts(ts: &DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

pub fn actor_from_row(row: &SqliteRow) -> Result<Actor, StoreError> {
    let kind: String = row.try_get("kind")?;
    Ok(Actor {
        id: parse_id(&row.try_get::<String, _>("id")?, "actor")?,
        kind: ActorKind::parse(&kind)
            .ok_or_else(|| StoreError::corrupt(format!("actor kind {kind:?}")))?,
        handle: row.try_get("handle")?,
        display_name: row.try_get("display_name")?,
    })
}

pub fn column_from_row(row: &SqliteRow) -> Result<Column, StoreError> {
    let kind: String = row.try_get("kind")?;
    Ok(Column {
        id: parse_id(&row.try_get::<String, _>("id")?, "column")?,
        name: row.try_get("name")?,
        position: row.try_get("position")?,
        kind: ColumnKind::parse(&kind)
            .ok_or_else(|| StoreError::corrupt(format!("column kind {kind:?}")))?,
    })
}

pub fn ticket_from_row(row: &SqliteRow) -> Result<Ticket, StoreError> {
    let priority: String = row.try_get("priority")?;
    let labels: String = row.try_get("labels")?;
    let frozen: i64 = row.try_get("frozen")?;
    Ok(Ticket {
        id: parse_id(&row.try_get::<String, _>("id")?, "ticket")?,
        key: row.try_get("key")?,
        title: row.try_get("title")?,
        description: row.try_get("description")?,
        column_id: parse_id(&row.try_get::<String, _>("column_id")?, "column")?,
        position: row.try_get("position")?,
        assignee_id: parse_opt_id(row.try_get("assignee_id")?, "actor")?,
        reporter_id: parse_id(&row.try_get::<String, _>("reporter_id")?, "actor")?,
        priority: Priority::parse(&priority)
            .ok_or_else(|| StoreError::corrupt(format!("priority {priority:?}")))?,
        labels: serde_json::from_str(&labels)?,
        parent_id: parse_opt_id(row.try_get("parent_id")?, "ticket")?,
        frozen: frozen != 0,
        freeze_reason: row.try_get("freeze_reason")?,
        version: row.try_get("version")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        updated_at: parse_ts(&row.try_get::<String, _>("updated_at")?)?,
    })
}

pub fn comment_from_row(row: &SqliteRow) -> Result<Comment, StoreError> {
    Ok(Comment {
        id: parse_id(&row.try_get::<String, _>("id")?, "comment")?,
        ticket_id: parse_id(&row.try_get::<String, _>("ticket_id")?, "ticket")?,
        author_id: parse_id(&row.try_get::<String, _>("author_id")?, "actor")?,
        body: row.try_get("body")?,
        parent_comment_id: parse_opt_id(row.try_get("parent_comment_id")?, "comment")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
    })
}

pub fn event_from_row(row: &SqliteRow) -> Result<Event, StoreError> {
    let payload: String = row.try_get("payload")?;
    let kind: EventKind = serde_json::from_str(&payload)?;
    Ok(Event {
        id: parse_id(&row.try_get::<String, _>("id")?, "event")?,
        seq: row.try_get("seq")?,
        occurred_at: parse_ts(&row.try_get::<String, _>("occurred_at")?)?,
        actor_id: parse_id(&row.try_get::<String, _>("actor_id")?, "actor")?,
        kind,
    })
}

pub fn notification_from_row(row: &SqliteRow) -> Result<Notification, StoreError> {
    let kind: String = row.try_get("kind")?;
    let state: String = row.try_get("state")?;
    let wake: i64 = row.try_get("wake")?;
    let handled_at: Option<String> = row.try_get("handled_at")?;
    Ok(Notification {
        id: parse_id(&row.try_get::<String, _>("id")?, "notification")?,
        recipient_id: parse_id(&row.try_get::<String, _>("recipient_id")?, "actor")?,
        event_id: parse_id(&row.try_get::<String, _>("event_id")?, "event")?,
        kind: NotificationKind::parse(&kind)
            .ok_or_else(|| StoreError::corrupt(format!("notification kind {kind:?}")))?,
        wake: wake != 0,
        state: NotificationState::parse(&state)
            .ok_or_else(|| StoreError::corrupt(format!("notification state {state:?}")))?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        handled_at: handled_at.as_deref().map(parse_ts).transpose()?,
        handled_in_run_id: parse_opt_id(row.try_get("handled_in_run_id")?, "run")?,
    })
}

pub fn utility_call_from_row(row: &SqliteRow) -> Result<UtilityCall, StoreError> {
    let kind: String = row.try_get("kind")?;
    let state: String = row.try_get("state")?;
    let warnings: String = row.try_get("warnings")?;
    let ended_at: Option<String> = row.try_get("ended_at")?;
    Ok(UtilityCall {
        id: parse_id(&row.try_get::<String, _>("id")?, "call")?,
        kind: UtilityKind::parse(&kind)
            .ok_or_else(|| StoreError::corrupt(format!("utility kind {kind:?}")))?,
        agent_id: parse_id(&row.try_get::<String, _>("agent_actor_id")?, "actor")?,
        target_id: parse_opt_id(row.try_get("target_actor_id")?, "actor")?,
        model: row.try_get("model")?,
        started_at: parse_ts(&row.try_get::<String, _>("started_at")?)?,
        ended_at: ended_at.as_deref().map(parse_ts).transpose()?,
        cost_usd: row.try_get("cost_usd")?,
        state: UtilityState::parse(&state)
            .ok_or_else(|| StoreError::corrupt(format!("utility state {state:?}")))?,
        proposal: row.try_get("proposal")?,
        warnings: serde_json::from_str(&warnings)?,
        error: row.try_get("error")?,
    })
}
