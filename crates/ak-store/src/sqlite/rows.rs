//! Row to entity mapping. Ids and timestamps are stored as text so the schema
//! stays portable; these helpers do the parsing in one place.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;
use sqlx::Row;

use ak_core::agent::{Agent, AgentState, Budgets};
use ak_core::approval::{Approval, ApprovalState};
use ak_core::board::{Column, ColumnKind, Comment, Priority, Ticket};
use ak_core::chat::{Chat, ChatKind, Message, Thread};
use ak_core::event::EventKind;
use ak_core::run::{AgentRun, ExitReason, RunEvent, RunEventKind};
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

pub fn approval_from_row(row: &SqliteRow) -> Result<Approval, StoreError> {
    let state: String = row.try_get("state")?;
    let input: String = row.try_get("input")?;
    let decided_at: Option<String> = row.try_get("decided_at")?;
    Ok(Approval {
        id: parse_id(&row.try_get::<String, _>("id")?, "approval")?,
        run_id: parse_id(&row.try_get::<String, _>("run_id")?, "run")?,
        agent_id: parse_id(&row.try_get::<String, _>("agent_actor_id")?, "actor")?,
        tool_name: row.try_get("tool_name")?,
        input: serde_json::from_str(&input)?,
        summary: row.try_get("summary")?,
        state: ApprovalState::parse(&state)
            .ok_or_else(|| StoreError::corrupt(format!("approval state {state:?}")))?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        decided_by: parse_opt_id(row.try_get("decided_by")?, "actor")?,
        decided_at: decided_at.as_deref().map(parse_ts).transpose()?,
    })
}

pub fn run_from_row(row: &SqliteRow) -> Result<AgentRun, StoreError> {
    let trigger: String = row.try_get("trigger")?;
    let ended_at: Option<String> = row.try_get("ended_at")?;
    let exit_reason: Option<String> = row.try_get("exit_reason")?;
    let workspace_path: String = row.try_get("workspace_path")?;
    let num_turns: i64 = row.try_get("num_turns")?;
    let input_tokens: i64 = row.try_get("input_tokens")?;
    let output_tokens: i64 = row.try_get("output_tokens")?;
    Ok(AgentRun {
        id: parse_id(&row.try_get::<String, _>("id")?, "run")?,
        agent_id: parse_id(&row.try_get::<String, _>("agent_actor_id")?, "actor")?,
        trigger: serde_json::from_str(&trigger)?,
        claude_session_id: row.try_get("claude_session_id")?,
        workspace_path: workspace_path.into(),
        started_at: parse_ts(&row.try_get::<String, _>("started_at")?)?,
        ended_at: ended_at.as_deref().map(parse_ts).transpose()?,
        exit_reason: exit_reason
            .as_deref()
            .map(|s| {
                ExitReason::parse(s)
                    .ok_or_else(|| StoreError::corrupt(format!("exit reason {s:?}")))
            })
            .transpose()?,
        num_turns: num_turns.max(0) as u32,
        input_tokens: input_tokens.max(0) as u64,
        output_tokens: output_tokens.max(0) as u64,
        cost_usd: row.try_get("cost_usd")?,
        error: row.try_get("error")?,
    })
}

pub fn run_event_from_row(row: &SqliteRow) -> Result<RunEvent, StoreError> {
    let kind: String = row.try_get("kind")?;
    let payload: String = row.try_get("payload")?;
    Ok(RunEvent {
        run_id: parse_id(&row.try_get::<String, _>("run_id")?, "run")?,
        seq: row.try_get("seq")?,
        at: parse_ts(&row.try_get::<String, _>("at")?)?,
        kind: RunEventKind::parse(&kind)
            .ok_or_else(|| StoreError::corrupt(format!("run event kind {kind:?}")))?,
        payload: serde_json::from_str(&payload)?,
    })
}

pub fn chat_from_row(row: &SqliteRow) -> Result<Chat, StoreError> {
    let kind: String = row.try_get("kind")?;
    Ok(Chat {
        id: parse_id(&row.try_get::<String, _>("id")?, "chat")?,
        kind: ChatKind::parse(&kind)
            .ok_or_else(|| StoreError::corrupt(format!("chat kind {kind:?}")))?,
        name: row.try_get("name")?,
        created_by: parse_id(&row.try_get::<String, _>("created_by")?, "actor")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
    })
}

pub fn thread_from_row(row: &SqliteRow) -> Result<Thread, StoreError> {
    Ok(Thread {
        id: parse_id(&row.try_get::<String, _>("id")?, "thread")?,
        chat_id: parse_id(&row.try_get::<String, _>("chat_id")?, "chat")?,
        title: row.try_get("title")?,
        created_by: parse_id(&row.try_get::<String, _>("created_by")?, "actor")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        last_activity_at: parse_ts(&row.try_get::<String, _>("last_activity_at")?)?,
        paused: row.try_get::<i64, _>("paused")? != 0,
        archived: row.try_get::<i64, _>("archived")? != 0,
    })
}

pub fn message_from_row(row: &SqliteRow) -> Result<Message, StoreError> {
    Ok(Message {
        id: parse_id(&row.try_get::<String, _>("id")?, "message")?,
        thread_id: parse_id(&row.try_get::<String, _>("thread_id")?, "thread")?,
        author_id: parse_id(&row.try_get::<String, _>("author_id")?, "actor")?,
        body: row.try_get("body")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
    })
}
