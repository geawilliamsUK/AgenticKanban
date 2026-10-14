//! SQLite implementation of `Store`.

mod rows;

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use sqlx::{Sqlite, SqliteConnection, Transaction};
use tokio::sync::broadcast;

use ak_core::agent::{Agent, AgentState};
use ak_core::approval::{Approval, ApprovalState};
use ak_core::board::{Column, Comment, Ticket};
use ak_core::commands::{self, NewTicket, TicketPatch};
use ak_core::config::ProjectPolicy;
use ak_core::event::{EventKind, MentionSource};
use ak_core::ids::*;
use ak_core::notification::{self, RoutingContext};
use ak_core::run::{AgentRun, RunEvent};
use ak_core::utility::{UtilityCall, UtilityKind};
use ak_core::{Actor, ActorKind, Event, Notification, NotificationState, Timestamp};

use crate::error::StoreError;
use crate::{HumanIdentity, Result, Store};
use rows::*;

const META_KEY_PREFIX: &str = "key_prefix";
const META_HUMAN_ID: &str = "human_actor_id";

pub struct SqliteStore {
    pool: SqlitePool,
    bus: broadcast::Sender<Arc<Event>>,
}

impl SqliteStore {
    /// Opens or creates the database at `path` and applies migrations.
    pub async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        Self::from_pool(pool).await
    }

    /// A private in-memory database, for tests. One connection, kept alive,
    /// because every connection to `:memory:` is a different database.
    pub async fn open_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> Result<Self> {
        sqlx::migrate!("./migrations").run(&pool).await?;
        let (bus, _) = broadcast::channel(1024);
        Ok(Self { pool, bus })
    }

    /// Closes every connection and waits for them to finish. Required before
    /// deleting the database file on Windows, which refuses to remove open files.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    fn publish(&self, events: Vec<Event>) {
        for e in events {
            // No subscribers is not an error.
            let _ = self.bus.send(Arc::new(e));
        }
    }

    // Transaction helpers. Everything below takes the connection explicitly
    // so a command can compose several steps inside one transaction.

    async fn load_actor(conn: &mut SqliteConnection, id: ActorId) -> Result<Actor> {
        let row = sqlx::query("SELECT id, kind, handle, display_name FROM actors WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| StoreError::not_found(format!("actor {id}")))?;
        actor_from_row(&row)
    }

    async fn load_actor_by_handle(
        conn: &mut SqliteConnection,
        handle: &str,
    ) -> Result<Option<Actor>> {
        let row = sqlx::query("SELECT id, kind, handle, display_name FROM actors WHERE handle = ?")
            .bind(handle)
            .fetch_optional(&mut *conn)
            .await?;
        row.as_ref().map(actor_from_row).transpose()
    }

    async fn load_ticket(conn: &mut SqliteConnection, id: TicketId) -> Result<Ticket> {
        let row = sqlx::query("SELECT * FROM tickets WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| StoreError::not_found(format!("ticket {id}")))?;
        ticket_from_row(&row)
    }

    async fn load_comment(conn: &mut SqliteConnection, id: CommentId) -> Result<Comment> {
        let row = sqlx::query("SELECT * FROM comments WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| StoreError::not_found(format!("comment {id}")))?;
        comment_from_row(&row)
    }

    async fn load_watchers(
        conn: &mut SqliteConnection,
        ticket_id: TicketId,
    ) -> Result<Vec<ActorId>> {
        let rows = sqlx::query("SELECT actor_id FROM watchers WHERE ticket_id = ?")
            .bind(ticket_id.to_string())
            .fetch_all(&mut *conn)
            .await?;
        rows.iter()
            .map(|r| {
                let raw: String = sqlx::Row::try_get(r, "actor_id")?;
                parse_id(&raw, "actor")
            })
            .collect()
    }

    async fn insert_ticket(conn: &mut SqliteConnection, t: &Ticket, key_number: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO tickets (id, key, key_number, title, description, column_id, position, \
             assignee_id, reporter_id, priority, labels, parent_id, frozen, freeze_reason, \
             version, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(t.id.to_string())
        .bind(&t.key)
        .bind(key_number)
        .bind(&t.title)
        .bind(&t.description)
        .bind(t.column_id.to_string())
        .bind(t.position)
        .bind(t.assignee_id.map(|a| a.to_string()))
        .bind(t.reporter_id.to_string())
        .bind(t.priority.as_str())
        .bind(serde_json::to_string(&t.labels)?)
        .bind(t.parent_id.map(|p| p.to_string()))
        .bind(t.frozen as i64)
        .bind(&t.freeze_reason)
        .bind(t.version)
        .bind(fmt_ts(&t.created_at))
        .bind(fmt_ts(&t.updated_at))
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn save_ticket(conn: &mut SqliteConnection, t: &Ticket) -> Result<()> {
        sqlx::query(
            "UPDATE tickets SET title = ?, description = ?, column_id = ?, position = ?, \
             assignee_id = ?, priority = ?, labels = ?, parent_id = ?, frozen = ?, \
             freeze_reason = ?, version = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&t.title)
        .bind(&t.description)
        .bind(t.column_id.to_string())
        .bind(t.position)
        .bind(t.assignee_id.map(|a| a.to_string()))
        .bind(t.priority.as_str())
        .bind(serde_json::to_string(&t.labels)?)
        .bind(t.parent_id.map(|p| p.to_string()))
        .bind(t.frozen as i64)
        .bind(&t.freeze_reason)
        .bind(t.version)
        .bind(fmt_ts(&t.updated_at))
        .bind(t.id.to_string())
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn add_watcher(
        conn: &mut SqliteConnection,
        ticket_id: TicketId,
        actor_id: ActorId,
    ) -> Result<()> {
        if actor_id == ActorId::SYSTEM {
            return Ok(());
        }
        sqlx::query("INSERT OR IGNORE INTO watchers (ticket_id, actor_id) VALUES (?, ?)")
            .bind(ticket_id.to_string())
            .bind(actor_id.to_string())
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    async fn next_position(conn: &mut SqliteConnection, column_id: ColumnId) -> Result<i64> {
        let row = sqlx::query(
            "SELECT COALESCE(MAX(position), -1) + 1 AS next FROM tickets WHERE column_id = ?",
        )
        .bind(column_id.to_string())
        .fetch_one(&mut *conn)
        .await?;
        Ok(sqlx::Row::try_get(&row, "next")?)
    }

    async fn next_key(conn: &mut SqliteConnection) -> Result<(String, i64)> {
        let prefix: String = sqlx::query("SELECT value FROM meta WHERE key = ?")
            .bind(META_KEY_PREFIX)
            .fetch_optional(&mut *conn)
            .await?
            .map(|r| sqlx::Row::try_get::<String, _>(&r, "value"))
            .transpose()?
            .unwrap_or_else(|| "T".to_string());
        let row = sqlx::query("SELECT COALESCE(MAX(key_number), 0) + 1 AS next FROM tickets")
            .fetch_one(&mut *conn)
            .await?;
        let number: i64 = sqlx::Row::try_get(&row, "next")?;
        Ok((format!("{prefix}-{number}"), number))
    }

    /// Resolves `@handles` to actor ids, silently dropping unknown handles.
    async fn resolve_handles(
        conn: &mut SqliteConnection,
        handles: &[String],
    ) -> Result<Vec<ActorId>> {
        let mut out = Vec::new();
        for h in handles {
            if let Some(a) = Self::load_actor_by_handle(conn, h).await? {
                out.push(a.id);
            }
        }
        Ok(out)
    }

    /// Appends events, routes notifications for each, and returns the
    /// persisted events with their sequence numbers. `ticket` is the subject
    /// after the mutation, which routing needs for assignee and frozen state.
    async fn append_events(
        tx: &mut Transaction<'_, Sqlite>,
        actor: &Actor,
        now: Timestamp,
        kinds: Vec<EventKind>,
        ticket: &Ticket,
    ) -> Result<Vec<Event>> {
        let watchers = Self::load_watchers(tx, ticket.id).await?;
        let ctx = RoutingContext {
            assignee_id: ticket.assignee_id,
            reporter_id: Some(ticket.reporter_id),
            watchers,
            frozen: ticket.frozen,
        };
        let mut persisted = Vec::with_capacity(kinds.len());
        for kind in kinds {
            let mut event = Event::new(actor.id, now, kind);
            let subject = event.subject();
            let result = sqlx::query(
                "INSERT INTO events (id, occurred_at, actor_id, kind, subject_kind, subject_id, payload) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(event.id.to_string())
            .bind(fmt_ts(&event.occurred_at))
            .bind(event.actor_id.to_string())
            .bind(event.kind.name())
            .bind(subject.kind_str())
            .bind(subject.id_string())
            .bind(serde_json::to_string(&event.kind)?)
            .execute(&mut **tx)
            .await?;
            event.seq = result.last_insert_rowid();

            for draft in notification::route(&event, &ctx) {
                sqlx::query(
                    "INSERT INTO notifications (id, recipient_id, event_id, kind, wake, state, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(NotificationId::new().to_string())
                .bind(draft.recipient_id.to_string())
                .bind(event.id.to_string())
                .bind(draft.kind.as_str())
                .bind(draft.wake as i64)
                .bind(draft.state.as_str())
                .bind(fmt_ts(&now))
                .execute(&mut **tx)
                .await?;
            }
            persisted.push(event);
        }
        Ok(persisted)
    }

    async fn ticket_command<F>(&self, id: TicketId, actor_id: ActorId, decide: F) -> Result<Ticket>
    where
        F: FnOnce(
                &Ticket,
                &Actor,
                Timestamp,
            )
                -> std::result::Result<commands::Outcome<Ticket>, ak_core::CoreError>
            + Send,
    {
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let actor = Self::load_actor(&mut tx, actor_id).await?;
        let before = Self::load_ticket(&mut tx, id).await?;
        let outcome = decide(&before, &actor, now)?;
        if outcome.events.is_empty() {
            return Ok(outcome.value);
        }
        Self::save_ticket(&mut tx, &outcome.value).await?;
        let mut kinds = outcome.events;
        let mentioned = Self::resolve_handles(&mut tx, &outcome.mention_handles).await?;
        kinds.extend(commands::mention_events(
            MentionSource::TicketDescription { ticket_id: id },
            &mentioned,
        ));
        let events = Self::append_events(&mut tx, &actor, now, kinds, &outcome.value).await?;
        tx.commit().await?;
        self.publish(events);
        Ok(outcome.value)
    }
}

#[async_trait]
impl Store for SqliteStore {
    fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.bus.subscribe()
    }

    async fn bootstrap(&self, policy: &ProjectPolicy, human: &HumanIdentity) -> Result<Actor> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let system = Actor::system();
        sqlx::query(
            "INSERT OR IGNORE INTO actors (id, kind, handle, display_name) VALUES (?, ?, ?, ?)",
        )
        .bind(system.id.to_string())
        .bind(system.kind.as_str())
        .bind(&system.handle)
        .bind(&system.display_name)
        .execute(&mut *tx)
        .await?;

        sqlx::query("INSERT OR IGNORE INTO meta (key, value) VALUES (?, ?)")
            .bind(META_KEY_PREFIX)
            .bind(&policy.key_prefix)
            .execute(&mut *tx)
            .await?;

        let existing_human: Option<String> = sqlx::query("SELECT value FROM meta WHERE key = ?")
            .bind(META_HUMAN_ID)
            .fetch_optional(&mut *tx)
            .await?
            .map(|r| sqlx::Row::try_get(&r, "value"))
            .transpose()?;

        let human_actor = match existing_human {
            Some(raw) => Self::load_actor(&mut tx, parse_id(&raw, "actor")?).await?,
            None => {
                Actor::validate_handle(&human.handle)?;
                let actor = Actor::human(human.handle.clone(), human.display_name.clone());
                sqlx::query(
                    "INSERT INTO actors (id, kind, handle, display_name) VALUES (?, ?, ?, ?)",
                )
                .bind(actor.id.to_string())
                .bind(actor.kind.as_str())
                .bind(&actor.handle)
                .bind(&actor.display_name)
                .execute(&mut *tx)
                .await?;
                sqlx::query("INSERT INTO meta (key, value) VALUES (?, ?)")
                    .bind(META_HUMAN_ID)
                    .bind(actor.id.to_string())
                    .execute(&mut *tx)
                    .await?;
                actor
            }
        };

        let column_count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM columns")
            .fetch_one(&mut *tx)
            .await
            .and_then(|r| sqlx::Row::try_get(&r, "n"))?;
        if column_count == 0 {
            for (i, spec) in policy.columns.iter().enumerate() {
                sqlx::query("INSERT INTO columns (id, name, position, kind) VALUES (?, ?, ?, ?)")
                    .bind(ColumnId::new().to_string())
                    .bind(&spec.name)
                    .bind(i as i64)
                    .bind(spec.kind.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
        }

        tx.commit().await?;
        Ok(human_actor)
    }

    async fn actors(&self) -> Result<Vec<Actor>> {
        let rows =
            sqlx::query("SELECT id, kind, handle, display_name FROM actors ORDER BY kind, handle")
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(actor_from_row).collect()
    }

    async fn actor(&self, id: ActorId) -> Result<Actor> {
        let mut conn = self.pool.acquire().await?;
        Self::load_actor(&mut conn, id).await
    }

    async fn actor_by_handle(&self, handle: &str) -> Result<Option<Actor>> {
        let mut conn = self.pool.acquire().await?;
        Self::load_actor_by_handle(&mut conn, handle).await
    }

    async fn human(&self) -> Result<Actor> {
        let raw: String = sqlx::query("SELECT value FROM meta WHERE key = ?")
            .bind(META_HUMAN_ID)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| sqlx::Row::try_get(&r, "value"))
            .transpose()?
            .ok_or_else(|| StoreError::not_found("human actor (bootstrap first)"))?;
        self.actor(parse_id(&raw, "actor")?).await
    }

    async fn columns(&self) -> Result<Vec<Column>> {
        let rows = sqlx::query("SELECT id, name, position, kind FROM columns ORDER BY position")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(column_from_row).collect()
    }

    async fn tickets(&self) -> Result<Vec<Ticket>> {
        let rows = sqlx::query("SELECT * FROM tickets ORDER BY column_id, position, created_at")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(ticket_from_row).collect()
    }

    async fn ticket(&self, id: TicketId) -> Result<Ticket> {
        let mut conn = self.pool.acquire().await?;
        Self::load_ticket(&mut conn, id).await
    }

    async fn comments(&self, ticket_id: TicketId) -> Result<Vec<Comment>> {
        let rows =
            sqlx::query("SELECT * FROM comments WHERE ticket_id = ? ORDER BY created_at, id")
                .bind(ticket_id.to_string())
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(comment_from_row).collect()
    }

    async fn watchers(&self, ticket_id: TicketId) -> Result<Vec<ActorId>> {
        let mut conn = self.pool.acquire().await?;
        Self::load_watchers(&mut conn, ticket_id).await
    }

    async fn events_for_ticket(&self, ticket_id: TicketId) -> Result<Vec<Event>> {
        let rows = sqlx::query(
            "SELECT * FROM events WHERE subject_kind = 'ticket' AND subject_id = ? ORDER BY seq",
        )
        .bind(ticket_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(event_from_row).collect()
    }

    async fn events_since(&self, after_seq: i64, limit: u32) -> Result<Vec<Event>> {
        let rows = sqlx::query("SELECT * FROM events WHERE seq > ? ORDER BY seq LIMIT ?")
            .bind(after_seq)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(event_from_row).collect()
    }

    async fn notifications(
        &self,
        recipient: ActorId,
        open_only: bool,
    ) -> Result<Vec<Notification>> {
        let sql = if open_only {
            "SELECT * FROM notifications WHERE recipient_id = ? AND state IN ('pending', 'seen') \
             ORDER BY created_at DESC"
        } else {
            "SELECT * FROM notifications WHERE recipient_id = ? ORDER BY created_at DESC"
        };
        let rows = sqlx::query(sql)
            .bind(recipient.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(notification_from_row).collect()
    }

    async fn upsert_actor(&self, actor: &Actor) -> Result<()> {
        Actor::validate_handle(&actor.handle)?;
        if actor.kind == ActorKind::System {
            return Err(ak_core::CoreError::NotPermitted("system actor is reserved".into()).into());
        }
        sqlx::query(
            "INSERT INTO actors (id, kind, handle, display_name) VALUES (?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, handle = excluded.handle, \
             display_name = excluded.display_name",
        )
        .bind(actor.id.to_string())
        .bind(actor.kind.as_str())
        .bind(&actor.handle)
        .bind(&actor.display_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn create_ticket(&self, input: NewTicket, actor_id: ActorId) -> Result<Ticket> {
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let actor = Self::load_actor(&mut tx, actor_id).await?;
        let (key, key_number) = Self::next_key(&mut tx).await?;
        let position = Self::next_position(&mut tx, input.column_id).await?;
        let outcome = commands::create_ticket(input, key, position, &actor, now)?;
        let ticket = outcome.value;
        Self::insert_ticket(&mut tx, &ticket, key_number).await?;
        Self::add_watcher(&mut tx, ticket.id, ticket.reporter_id).await?;
        let mut kinds = outcome.events;
        let mentioned = Self::resolve_handles(&mut tx, &outcome.mention_handles).await?;
        kinds.extend(commands::mention_events(
            MentionSource::TicketDescription {
                ticket_id: ticket.id,
            },
            &mentioned,
        ));
        let events = Self::append_events(&mut tx, &actor, now, kinds, &ticket).await?;
        tx.commit().await?;
        self.publish(events);
        Ok(ticket)
    }

    async fn update_ticket(
        &self,
        id: TicketId,
        patch: TicketPatch,
        expected_version: i64,
        actor: ActorId,
    ) -> Result<Ticket> {
        self.ticket_command(id, actor, move |t, a, now| {
            commands::update_ticket(t, patch, expected_version, a, now)
        })
        .await
    }

    async fn move_ticket(
        &self,
        id: TicketId,
        to: ColumnId,
        expected_version: i64,
        actor_id: ActorId,
    ) -> Result<Ticket> {
        // Position is allocated inside the transaction, so this one is written out.
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let actor = Self::load_actor(&mut tx, actor_id).await?;
        let before = Self::load_ticket(&mut tx, id).await?;
        let position = Self::next_position(&mut tx, to).await?;
        let outcome = commands::move_ticket(&before, to, position, expected_version, &actor, now)?;
        if outcome.events.is_empty() {
            return Ok(outcome.value);
        }
        Self::save_ticket(&mut tx, &outcome.value).await?;
        let events =
            Self::append_events(&mut tx, &actor, now, outcome.events, &outcome.value).await?;
        tx.commit().await?;
        self.publish(events);
        Ok(outcome.value)
    }

    async fn assign_ticket(
        &self,
        id: TicketId,
        to: Option<ActorId>,
        expected_version: i64,
        actor_id: ActorId,
    ) -> Result<Ticket> {
        if let Some(to) = to {
            let mut conn = self.pool.acquire().await?;
            let target = Self::load_actor(&mut conn, to).await?;
            if target.kind == ActorKind::System {
                return Err(
                    ak_core::CoreError::validation("cannot assign to the system actor").into(),
                );
            }
        }
        let ticket = self
            .ticket_command(id, actor_id, move |t, a, now| {
                commands::assign_ticket(t, to, expected_version, a, now)
            })
            .await?;
        if let Some(to) = to {
            let mut conn = self.pool.acquire().await?;
            Self::add_watcher(&mut conn, id, to).await?;
        }
        Ok(ticket)
    }

    async fn comment(
        &self,
        ticket_id: TicketId,
        body: String,
        parent: Option<CommentId>,
        actor_id: ActorId,
    ) -> Result<Comment> {
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let actor = Self::load_actor(&mut tx, actor_id).await?;
        let ticket = Self::load_ticket(&mut tx, ticket_id).await?;
        let parent_comment = match parent {
            Some(pid) => Some(Self::load_comment(&mut tx, pid).await?),
            None => None,
        };
        let outcome = commands::comment(&ticket, body, parent_comment.as_ref(), &actor, now)?;
        let comment = outcome.value;
        sqlx::query(
            "INSERT INTO comments (id, ticket_id, author_id, body, parent_comment_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(comment.id.to_string())
        .bind(comment.ticket_id.to_string())
        .bind(comment.author_id.to_string())
        .bind(&comment.body)
        .bind(comment.parent_comment_id.map(|p| p.to_string()))
        .bind(fmt_ts(&comment.created_at))
        .execute(&mut *tx)
        .await?;
        Self::add_watcher(&mut tx, ticket_id, actor.id).await?;
        let mut kinds = outcome.events;
        let mentioned = Self::resolve_handles(&mut tx, &outcome.mention_handles).await?;
        kinds.extend(commands::mention_events(
            MentionSource::Comment {
                ticket_id,
                comment_id: comment.id,
            },
            &mentioned,
        ));
        let events = Self::append_events(&mut tx, &actor, now, kinds, &ticket).await?;
        tx.commit().await?;
        self.publish(events);
        Ok(comment)
    }

    async fn freeze_ticket(&self, id: TicketId, reason: String, actor: ActorId) -> Result<Ticket> {
        self.ticket_command(id, actor, move |t, a, now| {
            commands::freeze_ticket(t, reason, a, now)
        })
        .await
    }

    async fn unfreeze_ticket(&self, id: TicketId, actor: ActorId) -> Result<Ticket> {
        let ticket = self
            .ticket_command(id, actor, move |t, a, now| {
                commands::unfreeze_ticket(t, a, now)
            })
            .await?;
        // Release notifications that were held while the ticket was frozen.
        sqlx::query(
            "UPDATE notifications SET state = 'pending' WHERE state = 'held' AND event_id IN \
             (SELECT id FROM events WHERE subject_kind = 'ticket' AND subject_id = ?)",
        )
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(ticket)
    }

    async fn set_notification_state(
        &self,
        id: NotificationId,
        state: NotificationState,
    ) -> Result<()> {
        let handled_at = match state {
            NotificationState::Handled | NotificationState::Dismissed => Some(fmt_ts(&Utc::now())),
            _ => None,
        };
        let result = sqlx::query("UPDATE notifications SET state = ?, handled_at = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(handled_at)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::not_found(format!("notification {id}")));
        }
        Ok(())
    }

    async fn mark_notifications_seen(&self, recipient: ActorId) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE notifications SET state = 'seen' WHERE recipient_id = ? AND state = 'pending'",
        )
        .bind(recipient.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn agents(&self) -> Result<Vec<Agent>> {
        let rows = sqlx::query(
            "SELECT g.*, a.handle FROM agents g JOIN actors a ON a.id = g.actor_id ORDER BY a.handle",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(agent_from_row).collect()
    }

    async fn agent(&self, actor_id: ActorId) -> Result<Agent> {
        let row = sqlx::query(
            "SELECT g.*, a.handle FROM agents g JOIN actors a ON a.id = g.actor_id WHERE g.actor_id = ?",
        )
        .bind(actor_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| StoreError::not_found(format!("agent {actor_id}")))?;
        agent_from_row(&row)
    }

    async fn agent_by_handle(&self, handle: &str) -> Result<Option<Agent>> {
        let row = sqlx::query(
            "SELECT g.*, a.handle FROM agents g JOIN actors a ON a.id = g.actor_id WHERE a.handle = ?",
        )
        .bind(handle)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(agent_from_row).transpose()
    }

    async fn upsert_agent(&self, agent: &Agent) -> Result<()> {
        Actor::validate_handle(&agent.handle)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // The handle must not belong to a different actor.
        if let Some(other) = Self::load_actor_by_handle(&mut tx, &agent.handle).await? {
            if other.id != agent.actor_id {
                return Err(ak_core::CoreError::validation(format!(
                    "handle @{} is already used by {}",
                    agent.handle,
                    other.kind.as_str()
                ))
                .into());
            }
        }
        sqlx::query(
            "INSERT INTO actors (id, kind, handle, display_name) VALUES (?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET handle = excluded.handle, \
             display_name = excluded.display_name",
        )
        .bind(agent.actor_id.to_string())
        .bind(ActorKind::Agent.as_str())
        .bind(&agent.handle)
        .bind(&agent.handle)
        .execute(&mut *tx)
        .await?;
        // Runtime state columns are only written on insert; later upserts from
        // the definition file leave them alone.
        sqlx::query(
            "INSERT INTO agents (actor_id, description, model, permission_profile, budgets, state, \
             current_run_id, cooldown_until, missing_since, definition_path, memory_path) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(actor_id) DO UPDATE SET description = excluded.description, \
             model = excluded.model, permission_profile = excluded.permission_profile, \
             budgets = excluded.budgets, missing_since = excluded.missing_since, \
             definition_path = excluded.definition_path, memory_path = excluded.memory_path",
        )
        .bind(agent.actor_id.to_string())
        .bind(&agent.description)
        .bind(&agent.model)
        .bind(&agent.permission_profile)
        .bind(agent.budgets.as_ref().map(serde_json::to_string).transpose()?)
        .bind(agent.state.as_str())
        .bind(agent.current_run_id.map(|r| r.to_string()))
        .bind(agent.cooldown_until.as_ref().map(fmt_ts))
        .bind(agent.missing_since.as_ref().map(fmt_ts))
        .bind(agent.definition_path.to_string_lossy().to_string())
        .bind(agent.memory_path.to_string_lossy().to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_agent_missing(&self, actor_id: ActorId, since: Option<Timestamp>) -> Result<()> {
        let result = sqlx::query("UPDATE agents SET missing_since = ? WHERE actor_id = ?")
            .bind(since.as_ref().map(fmt_ts))
            .bind(actor_id.to_string())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::not_found(format!("agent {actor_id}")));
        }
        Ok(())
    }

    async fn set_agent_state(
        &self,
        actor_id: ActorId,
        state: AgentState,
        current_run_id: Option<RunId>,
    ) -> Result<()> {
        let result =
            sqlx::query("UPDATE agents SET state = ?, current_run_id = ? WHERE actor_id = ?")
                .bind(state.as_str())
                .bind(current_run_id.map(|r| r.to_string()))
                .bind(actor_id.to_string())
                .execute(&self.pool)
                .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::not_found(format!("agent {actor_id}")));
        }
        Ok(())
    }

    async fn insert_utility_call(&self, call: &UtilityCall) -> Result<()> {
        sqlx::query(
            "INSERT INTO utility_calls (id, kind, agent_actor_id, target_actor_id, model, \
             started_at, ended_at, cost_usd, state, proposal, warnings, error) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(call.id.to_string())
        .bind(call.kind.as_str())
        .bind(call.agent_id.to_string())
        .bind(call.target_id.map(|t| t.to_string()))
        .bind(&call.model)
        .bind(fmt_ts(&call.started_at))
        .bind(call.ended_at.as_ref().map(fmt_ts))
        .bind(call.cost_usd)
        .bind(call.state.as_str())
        .bind(&call.proposal)
        .bind(serde_json::to_string(&call.warnings)?)
        .bind(&call.error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_utility_call(&self, call: &UtilityCall) -> Result<()> {
        let result = sqlx::query(
            "UPDATE utility_calls SET ended_at = ?, cost_usd = ?, state = ?, proposal = ?, \
             warnings = ?, error = ? WHERE id = ?",
        )
        .bind(call.ended_at.as_ref().map(fmt_ts))
        .bind(call.cost_usd)
        .bind(call.state.as_str())
        .bind(&call.proposal)
        .bind(serde_json::to_string(&call.warnings)?)
        .bind(&call.error)
        .bind(call.id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::not_found(format!("utility call {}", call.id)));
        }
        Ok(())
    }

    async fn utility_call(&self, id: CallId) -> Result<UtilityCall> {
        let row = sqlx::query("SELECT * FROM utility_calls WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| StoreError::not_found(format!("utility call {id}")))?;
        utility_call_from_row(&row)
    }

    async fn pending_proposal(
        &self,
        agent_id: ActorId,
        kind: UtilityKind,
    ) -> Result<Option<UtilityCall>> {
        let row = sqlx::query(
            "SELECT * FROM utility_calls WHERE agent_actor_id = ? AND kind = ? \
             AND state = 'proposed' ORDER BY started_at DESC LIMIT 1",
        )
        .bind(agent_id.to_string())
        .bind(kind.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(utility_call_from_row).transpose()
    }

    async fn pending_proposals(
        &self,
        agent_id: ActorId,
        kind: UtilityKind,
    ) -> Result<Vec<UtilityCall>> {
        let rows = sqlx::query(
            "SELECT * FROM utility_calls WHERE agent_actor_id = ? AND kind = ? \n             AND state = 'proposed' ORDER BY started_at DESC",
        )
        .bind(agent_id.to_string())
        .bind(kind.as_str())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(utility_call_from_row).collect()
    }

    async fn utility_cost_since(&self, since: Timestamp) -> Result<f64> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(cost_usd), 0.0) AS total FROM utility_calls WHERE started_at >= ?",
        )
        .bind(fmt_ts(&since))
        .fetch_one(&self.pool)
        .await?;
        Ok(sqlx::Row::try_get(&row, "total")?)
    }

    async fn ticket_by_key(&self, key: &str) -> Result<Option<Ticket>> {
        let row = sqlx::query("SELECT * FROM tickets WHERE key = ?")
            .bind(key.trim().to_uppercase())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(ticket_from_row).transpose()
    }

    async fn mark_notifications_handled(
        &self,
        ids: &[NotificationId],
        run: Option<RunId>,
    ) -> Result<u64> {
        let now = fmt_ts(&Utc::now());
        let mut count = 0;
        for id in ids {
            let result = sqlx::query(
                "UPDATE notifications SET state = 'handled', handled_at = ?, handled_in_run_id = ? \
                 WHERE id = ? AND state IN ('pending', 'seen')",
            )
            .bind(&now)
            .bind(run.map(|r| r.to_string()))
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
            count += result.rows_affected();
        }
        Ok(count)
    }

    async fn insert_approval(&self, approval: &Approval) -> Result<()> {
        sqlx::query(
            "INSERT INTO approvals (id, run_id, agent_actor_id, tool_name, input, summary, state, \
             created_at, decided_by, decided_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(approval.id.to_string())
        .bind(approval.run_id.to_string())
        .bind(approval.agent_id.to_string())
        .bind(&approval.tool_name)
        .bind(serde_json::to_string(&approval.input)?)
        .bind(&approval.summary)
        .bind(approval.state.as_str())
        .bind(fmt_ts(&approval.created_at))
        .bind(approval.decided_by.map(|a| a.to_string()))
        .bind(approval.decided_at.as_ref().map(fmt_ts))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn approval(&self, id: ApprovalId) -> Result<Approval> {
        let row = sqlx::query("SELECT * FROM approvals WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| StoreError::not_found(format!("approval {id}")))?;
        approval_from_row(&row)
    }

    async fn pending_approvals(&self) -> Result<Vec<Approval>> {
        let rows =
            sqlx::query("SELECT * FROM approvals WHERE state = 'pending' ORDER BY created_at")
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(approval_from_row).collect()
    }

    async fn decide_approval(
        &self,
        id: ApprovalId,
        state: ApprovalState,
        decided_by: Option<ActorId>,
    ) -> Result<Approval> {
        let result = sqlx::query(
            "UPDATE approvals SET state = ?, decided_by = ?, decided_at = ? \
             WHERE id = ? AND state = 'pending'",
        )
        .bind(state.as_str())
        .bind(decided_by.map(|a| a.to_string()))
        .bind(fmt_ts(&Utc::now()))
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            // Either unknown or already decided; report which.
            return match self.approval(id).await {
                Ok(a) => Err(StoreError::Core(ak_core::CoreError::validation(format!(
                    "approval already {}",
                    a.state.as_str()
                )))),
                Err(e) => Err(e),
            };
        }
        self.approval(id).await
    }

    async fn insert_run(&self, run: &AgentRun) -> Result<()> {
        sqlx::query(
            "INSERT INTO runs (id, agent_actor_id, trigger, claude_session_id, workspace_path, \
             started_at, ended_at, exit_reason, num_turns, input_tokens, output_tokens, cost_usd, \
             error) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(run.id.to_string())
        .bind(run.agent_id.to_string())
        .bind(serde_json::to_string(&run.trigger)?)
        .bind(&run.claude_session_id)
        .bind(run.workspace_path.to_string_lossy().to_string())
        .bind(fmt_ts(&run.started_at))
        .bind(run.ended_at.as_ref().map(fmt_ts))
        .bind(run.exit_reason.map(|r| r.as_str()))
        .bind(run.num_turns as i64)
        .bind(run.input_tokens as i64)
        .bind(run.output_tokens as i64)
        .bind(run.cost_usd)
        .bind(&run.error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update_run(&self, run: &AgentRun) -> Result<()> {
        let result = sqlx::query(
            "UPDATE runs SET claude_session_id = ?, ended_at = ?, exit_reason = ?, num_turns = ?, \
             input_tokens = ?, output_tokens = ?, cost_usd = ?, error = ? WHERE id = ?",
        )
        .bind(&run.claude_session_id)
        .bind(run.ended_at.as_ref().map(fmt_ts))
        .bind(run.exit_reason.map(|r| r.as_str()))
        .bind(run.num_turns as i64)
        .bind(run.input_tokens as i64)
        .bind(run.output_tokens as i64)
        .bind(run.cost_usd)
        .bind(&run.error)
        .bind(run.id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::not_found(format!("run {}", run.id)));
        }
        Ok(())
    }

    async fn run(&self, id: RunId) -> Result<AgentRun> {
        let row = sqlx::query("SELECT * FROM runs WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| StoreError::not_found(format!("run {id}")))?;
        run_from_row(&row)
    }

    async fn runs_for_agent(&self, agent: ActorId, limit: u32) -> Result<Vec<AgentRun>> {
        let rows = sqlx::query(
            "SELECT * FROM runs WHERE agent_actor_id = ? ORDER BY started_at DESC LIMIT ?",
        )
        .bind(agent.to_string())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(run_from_row).collect()
    }

    async fn active_runs(&self) -> Result<Vec<AgentRun>> {
        let rows = sqlx::query("SELECT * FROM runs WHERE ended_at IS NULL ORDER BY started_at")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(run_from_row).collect()
    }

    async fn append_run_event(&self, event: &RunEvent) -> Result<()> {
        sqlx::query(
            "INSERT INTO run_events (run_id, seq, at, kind, payload) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(event.run_id.to_string())
        .bind(event.seq)
        .bind(fmt_ts(&event.at))
        .bind(event.kind.as_str())
        .bind(serde_json::to_string(&event.payload)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn run_events(&self, run: RunId) -> Result<Vec<RunEvent>> {
        let rows = sqlx::query("SELECT * FROM run_events WHERE run_id = ? ORDER BY seq")
            .bind(run.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(run_event_from_row).collect()
    }

    async fn run_cost_since(&self, since: Timestamp, agent: Option<ActorId>) -> Result<f64> {
        let row =
            match agent {
                Some(a) => {
                    sqlx::query(
                        "SELECT COALESCE(SUM(cost_usd), 0.0) AS total FROM runs \
                     WHERE started_at >= ? AND agent_actor_id = ?",
                    )
                    .bind(fmt_ts(&since))
                    .bind(a.to_string())
                    .fetch_one(&self.pool)
                    .await?
                }
                None => sqlx::query(
                    "SELECT COALESCE(SUM(cost_usd), 0.0) AS total FROM runs WHERE started_at >= ?",
                )
                .bind(fmt_ts(&since))
                .fetch_one(&self.pool)
                .await?,
            };
        Ok(sqlx::Row::try_get(&row, "total")?)
    }

    async fn set_agent_cooldown(&self, actor_id: ActorId, until: Option<Timestamp>) -> Result<()> {
        let result = sqlx::query("UPDATE agents SET cooldown_until = ? WHERE actor_id = ?")
            .bind(until.as_ref().map(fmt_ts))
            .bind(actor_id.to_string())
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(StoreError::not_found(format!("agent {actor_id}")));
        }
        Ok(())
    }
}
