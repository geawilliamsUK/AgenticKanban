//! Chats and threads. The trait methods in the parent module delegate here.
//! Every mutation follows the board pattern: load, decide in core, write,
//! append events with routed notifications, commit, publish.

use std::collections::HashSet;

use chrono::Utc;
use sqlx::{Row, SqliteConnection};

use ak_core::chat::{Chat, ChatKind, Message, Thread, ThreadHit};
use ak_core::commands;
use ak_core::event::MentionSource;
use ak_core::ids::{ActorId, ChatId, ThreadId};
use ak_core::notification::{ChatRouting, RoutingContext};
use ak_core::Timestamp;

use super::rows::*;
use super::{SqliteStore, META_HUMAN_ID};
use crate::error::StoreError;
use crate::Result;

/// Turns free text into an FTS5 query: every word becomes a quoted prefix
/// term, so punctuation in the input can never break the query.
fn fts_query(text: &str) -> String {
    text.split_whitespace()
        .map(|w| w.replace('"', ""))
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{w}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn ids_from_rows(rows: &[sqlx::sqlite::SqliteRow], column: &str) -> Result<Vec<ActorId>> {
    rows.iter()
        .map(|r| {
            let raw: String = r.try_get(column)?;
            parse_id(&raw, "actor")
        })
        .collect()
}

impl SqliteStore {
    async fn load_chat(conn: &mut SqliteConnection, id: ChatId) -> Result<Chat> {
        let row = sqlx::query("SELECT * FROM chats WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| StoreError::not_found(format!("chat {id}")))?;
        chat_from_row(&row)
    }

    async fn load_thread(conn: &mut SqliteConnection, id: ThreadId) -> Result<Thread> {
        let row = sqlx::query("SELECT * FROM threads WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| StoreError::not_found(format!("thread {id}")))?;
        thread_from_row(&row)
    }

    async fn load_members(conn: &mut SqliteConnection, chat: ChatId) -> Result<Vec<ActorId>> {
        let rows =
            sqlx::query("SELECT actor_id FROM chat_members WHERE chat_id = ? ORDER BY rowid")
                .bind(chat.to_string())
                .fetch_all(&mut *conn)
                .await?;
        ids_from_rows(&rows, "actor_id")
    }

    async fn load_participants(
        conn: &mut SqliteConnection,
        thread: ThreadId,
    ) -> Result<Vec<ActorId>> {
        let rows = sqlx::query(
            "SELECT actor_id FROM thread_participants WHERE thread_id = ? ORDER BY rowid",
        )
        .bind(thread.to_string())
        .fetch_all(&mut *conn)
        .await?;
        ids_from_rows(&rows, "actor_id")
    }

    pub(super) async fn load_human_id(conn: &mut SqliteConnection) -> Result<Option<ActorId>> {
        let row = sqlx::query("SELECT value FROM meta WHERE key = ?")
            .bind(META_HUMAN_ID)
            .fetch_optional(&mut *conn)
            .await?;
        row.map(|r| {
            let raw: String = r.try_get("value")?;
            parse_id(&raw, "actor")
        })
        .transpose()
    }

    /// Routing for a thread event: members, participants, the human as an
    /// observer of direct chats they are not in, and the paused state.
    async fn chat_routing(
        conn: &mut SqliteConnection,
        chat: &Chat,
        members: &[ActorId],
        thread: Option<&Thread>,
        participants: Vec<ActorId>,
    ) -> Result<RoutingContext> {
        let mut observers = Vec::new();
        if chat.kind == ChatKind::Direct {
            if let Some(h) = Self::load_human_id(conn).await? {
                if !members.contains(&h) {
                    observers.push(h);
                }
            }
        }
        Ok(RoutingContext {
            frozen: thread.map(|t| t.paused).unwrap_or(false),
            chat: Some(ChatRouting {
                direct: chat.kind == ChatKind::Direct,
                members: members.to_vec(),
                participants,
                observers,
            }),
            ..Default::default()
        })
    }

    async fn insert_chat(
        conn: &mut SqliteConnection,
        chat: &Chat,
        members: &[ActorId],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO chats (id, kind, name, created_by, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(chat.id.to_string())
        .bind(chat.kind.as_str())
        .bind(&chat.name)
        .bind(chat.created_by.to_string())
        .bind(fmt_ts(&chat.created_at))
        .execute(&mut *conn)
        .await?;
        for m in members {
            sqlx::query("INSERT OR IGNORE INTO chat_members (chat_id, actor_id) VALUES (?, ?)")
                .bind(chat.id.to_string())
                .bind(m.to_string())
                .execute(&mut *conn)
                .await?;
        }
        Ok(())
    }

    async fn insert_message(conn: &mut SqliteConnection, message: &Message) -> Result<()> {
        sqlx::query(
            "INSERT INTO messages (id, thread_id, author_id, body, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(message.id.to_string())
        .bind(message.thread_id.to_string())
        .bind(message.author_id.to_string())
        .bind(&message.body)
        .bind(fmt_ts(&message.created_at))
        .execute(&mut *conn)
        .await?;
        sqlx::query("INSERT INTO messages_fts (body, thread_id, message_id) VALUES (?, ?, ?)")
            .bind(&message.body)
            .bind(message.thread_id.to_string())
            .bind(message.id.to_string())
            .execute(&mut *conn)
            .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO thread_participants (thread_id, actor_id) VALUES (?, ?)",
        )
        .bind(message.thread_id.to_string())
        .bind(message.author_id.to_string())
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub(super) async fn chats_impl(&self) -> Result<Vec<Chat>> {
        let rows = sqlx::query("SELECT * FROM chats ORDER BY created_at, rowid")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(chat_from_row).collect()
    }

    pub(super) async fn chats_for_impl(&self, actor: ActorId) -> Result<Vec<Chat>> {
        let rows = sqlx::query(
            "SELECT c.* FROM chats c JOIN chat_members m ON m.chat_id = c.id \
             WHERE m.actor_id = ? ORDER BY c.created_at, c.rowid",
        )
        .bind(actor.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(chat_from_row).collect()
    }

    pub(super) async fn chat_impl(&self, id: ChatId) -> Result<Chat> {
        let mut conn = self.pool.acquire().await?;
        Self::load_chat(&mut conn, id).await
    }

    pub(super) async fn chat_members_impl(&self, id: ChatId) -> Result<Vec<ActorId>> {
        let mut conn = self.pool.acquire().await?;
        Self::load_members(&mut conn, id).await
    }

    pub(super) async fn direct_chat_impl(
        &self,
        a: ActorId,
        b: ActorId,
        actor: ActorId,
    ) -> Result<Chat> {
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing = sqlx::query(
            "SELECT c.* FROM chats c WHERE c.kind = 'direct' AND (SELECT COUNT(*) FROM chat_members m \
             WHERE m.chat_id = c.id AND m.actor_id IN (?, ?)) = 2 LIMIT 1",
        )
        .bind(a.to_string())
        .bind(b.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing {
            let chat = chat_from_row(&row)?;
            tx.commit().await?;
            return Ok(chat);
        }
        let by = Self::load_actor(&mut tx, actor).await?;
        let first = Self::load_actor(&mut tx, a).await?;
        let second = Self::load_actor(&mut tx, b).await?;
        let outcome = commands::create_direct_chat(&first, &second, &by, now)?;
        let chat = outcome.value;
        Self::insert_chat(&mut tx, &chat, &[a, b]).await?;
        let ctx = RoutingContext::default();
        let events = Self::append_events_with(&mut tx, &by, now, outcome.events, &ctx).await?;
        tx.commit().await?;
        self.publish(events);
        Ok(chat)
    }

    pub(super) async fn create_group_chat_impl(
        &self,
        name: String,
        members: Vec<ActorId>,
        actor: ActorId,
    ) -> Result<Chat> {
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let by = Self::load_actor(&mut tx, actor).await?;
        let outcome = commands::create_group_chat(name, members, &by, now)?;
        let (chat, ids) = outcome.value;
        for m in &ids {
            Self::load_actor(&mut tx, *m).await?;
        }
        Self::insert_chat(&mut tx, &chat, &ids).await?;
        let ctx = RoutingContext::default();
        let events = Self::append_events_with(&mut tx, &by, now, outcome.events, &ctx).await?;
        tx.commit().await?;
        self.publish(events);
        Ok(chat)
    }

    pub(super) async fn add_chat_member_impl(
        &self,
        chat_id: ChatId,
        member: ActorId,
        actor: ActorId,
    ) -> Result<()> {
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let chat = Self::load_chat(&mut tx, chat_id).await?;
        let members = Self::load_members(&mut tx, chat_id).await?;
        let by = Self::load_actor(&mut tx, actor).await?;
        let added = Self::load_actor(&mut tx, member).await?;
        let outcome = commands::add_chat_member(&chat, &members, &added, &by, now)?;
        sqlx::query("INSERT OR IGNORE INTO chat_members (chat_id, actor_id) VALUES (?, ?)")
            .bind(chat_id.to_string())
            .bind(member.to_string())
            .execute(&mut *tx)
            .await?;
        let ctx = RoutingContext::default();
        let events = Self::append_events_with(&mut tx, &by, now, outcome.events, &ctx).await?;
        tx.commit().await?;
        self.publish(events);
        Ok(())
    }

    pub(super) async fn threads_impl(
        &self,
        chat: ChatId,
        include_archived: bool,
    ) -> Result<Vec<Thread>> {
        let sql = if include_archived {
            "SELECT * FROM threads WHERE chat_id = ? ORDER BY last_activity_at DESC, rowid DESC"
        } else {
            "SELECT * FROM threads WHERE chat_id = ? AND archived = 0 \
             ORDER BY last_activity_at DESC, rowid DESC"
        };
        let rows = sqlx::query(sql)
            .bind(chat.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(thread_from_row).collect()
    }

    pub(super) async fn thread_impl(&self, id: ThreadId) -> Result<Thread> {
        let mut conn = self.pool.acquire().await?;
        Self::load_thread(&mut conn, id).await
    }

    pub(super) async fn thread_participants_impl(&self, id: ThreadId) -> Result<Vec<ActorId>> {
        let mut conn = self.pool.acquire().await?;
        Self::load_participants(&mut conn, id).await
    }

    pub(super) async fn messages_impl(&self, thread: ThreadId) -> Result<Vec<Message>> {
        let rows =
            sqlx::query("SELECT * FROM messages WHERE thread_id = ? ORDER BY created_at, rowid")
                .bind(thread.to_string())
                .fetch_all(&self.pool)
                .await?;
        rows.iter().map(message_from_row).collect()
    }

    pub(super) async fn create_thread_impl(
        &self,
        chat_id: ChatId,
        title: String,
        body: String,
        actor: ActorId,
    ) -> Result<(Thread, Message)> {
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let chat = Self::load_chat(&mut tx, chat_id).await?;
        let members = Self::load_members(&mut tx, chat_id).await?;
        let author = Self::load_actor(&mut tx, actor).await?;
        let outcome = commands::create_thread(&chat, &members, title, body, &author, now)?;
        let (thread, message) = outcome.value;
        sqlx::query(
            "INSERT INTO threads (id, chat_id, title, created_by, created_at, last_activity_at, paused, archived) \
             VALUES (?, ?, ?, ?, ?, ?, 0, 0)",
        )
        .bind(thread.id.to_string())
        .bind(thread.chat_id.to_string())
        .bind(&thread.title)
        .bind(thread.created_by.to_string())
        .bind(fmt_ts(&thread.created_at))
        .bind(fmt_ts(&thread.last_activity_at))
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO threads_fts (title, thread_id) VALUES (?, ?)")
            .bind(&thread.title)
            .bind(thread.id.to_string())
            .execute(&mut *tx)
            .await?;
        Self::insert_message(&mut tx, &message).await?;

        let mut kinds = outcome.events;
        let mentioned = Self::resolve_handles(&mut tx, &outcome.mention_handles).await?;
        kinds.extend(commands::mention_events(
            MentionSource::Message {
                thread_id: thread.id,
                message_id: message.id,
            },
            &mentioned,
        ));
        let mut ctx =
            Self::chat_routing(&mut tx, &chat, &members, Some(&thread), vec![author.id]).await?;
        ctx.actor_is_human = author.is_human();
        let events = Self::append_events_with(&mut tx, &author, now, kinds, &ctx).await?;
        tx.commit().await?;
        self.publish(events);
        Ok((thread, message))
    }

    pub(super) async fn post_message_impl(
        &self,
        thread_id: ThreadId,
        body: String,
        actor: ActorId,
    ) -> Result<Message> {
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let thread = Self::load_thread(&mut tx, thread_id).await?;
        let chat = Self::load_chat(&mut tx, thread.chat_id).await?;
        let members = Self::load_members(&mut tx, chat.id).await?;
        let participants = Self::load_participants(&mut tx, thread_id).await?;
        let author = Self::load_actor(&mut tx, actor).await?;
        let outcome = commands::post_message(&chat, &members, &thread, body, &author, now)?;
        let message = outcome.value;
        Self::insert_message(&mut tx, &message).await?;
        sqlx::query("UPDATE threads SET last_activity_at = ?, archived = 0 WHERE id = ?")
            .bind(fmt_ts(&now))
            .bind(thread_id.to_string())
            .execute(&mut *tx)
            .await?;

        let mut kinds = outcome.events;
        let mentioned = Self::resolve_handles(&mut tx, &outcome.mention_handles).await?;
        kinds.extend(commands::mention_events(
            MentionSource::Message {
                thread_id,
                message_id: message.id,
            },
            &mentioned,
        ));
        let mut ctx =
            Self::chat_routing(&mut tx, &chat, &members, Some(&thread), participants).await?;
        ctx.actor_is_human = author.is_human();
        let events = Self::append_events_with(&mut tx, &author, now, kinds, &ctx).await?;
        tx.commit().await?;
        self.publish(events);
        Ok(message)
    }

    pub(super) async fn set_thread_paused_impl(
        &self,
        thread_id: ThreadId,
        paused: bool,
        actor: ActorId,
    ) -> Result<Thread> {
        let now = Utc::now();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let thread = Self::load_thread(&mut tx, thread_id).await?;
        let chat = Self::load_chat(&mut tx, thread.chat_id).await?;
        let members = Self::load_members(&mut tx, chat.id).await?;
        let participants = Self::load_participants(&mut tx, thread_id).await?;
        let by = Self::load_actor(&mut tx, actor).await?;
        let outcome = commands::set_thread_paused(&thread, paused, &by, now)?;
        let updated = outcome.value;
        sqlx::query("UPDATE threads SET paused = ? WHERE id = ?")
            .bind(paused as i64)
            .bind(thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        if !paused {
            // Release what was held while paused.
            sqlx::query(
                "UPDATE notifications SET state = 'pending' WHERE state = 'held' AND event_id IN \
                 (SELECT id FROM events WHERE subject_kind = 'thread' AND subject_id = ?)",
            )
            .bind(thread_id.to_string())
            .execute(&mut *tx)
            .await?;
        }
        let mut ctx =
            Self::chat_routing(&mut tx, &chat, &members, Some(&updated), participants).await?;
        ctx.actor_is_human = by.is_human();
        let events = Self::append_events_with(&mut tx, &by, now, outcome.events, &ctx).await?;
        tx.commit().await?;
        self.publish(events);
        Ok(updated)
    }

    pub(super) async fn search_threads_impl(
        &self,
        actor: Option<ActorId>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ThreadHit>> {
        let q = fts_query(query);
        if q.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut hits: Vec<ThreadHit> = Vec::new();
        let rows = sqlx::query(
            "SELECT t.*, t.title AS snippet FROM threads_fts \
             JOIN threads t ON t.id = threads_fts.thread_id \
             WHERE threads_fts MATCH ? ORDER BY threads_fts.rank LIMIT ?",
        )
        .bind(&q)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        for r in &rows {
            hits.push(ThreadHit {
                thread: thread_from_row(r)?,
                snippet: r.try_get("snippet")?,
            });
        }
        let rows = sqlx::query(
            "SELECT t.*, snippet(messages_fts, 0, '[', ']', '…', 12) AS snippet FROM messages_fts \
             JOIN threads t ON t.id = messages_fts.thread_id \
             WHERE messages_fts MATCH ? ORDER BY messages_fts.rank LIMIT ?",
        )
        .bind(&q)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        for r in &rows {
            let thread = thread_from_row(r)?;
            if hits.iter().any(|h| h.thread.id == thread.id) {
                continue;
            }
            hits.push(ThreadHit {
                thread,
                snippet: r.try_get("snippet")?,
            });
        }
        if let Some(actor) = actor {
            let mine: HashSet<ChatId> = self
                .chats_for_impl(actor)
                .await?
                .into_iter()
                .map(|c| c.id)
                .collect();
            hits.retain(|h| mine.contains(&h.thread.chat_id));
        }
        hits.truncate(limit);
        Ok(hits)
    }

    pub(super) async fn archive_idle_threads_impl(&self, idle_since: Timestamp) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE threads SET archived = 1 WHERE archived = 0 AND last_activity_at < ?",
        )
        .bind(fmt_ts(&idle_since))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
