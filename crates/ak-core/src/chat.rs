//! Chats and threads. Chats are containers; every message lives in a thread,
//! which forces topic separation. The human is an implicit observer and
//! participant in every chat without being counted as a member for wake
//! purposes, except in a direct chat with an agent, where the human is one
//! of the two members.

use serde::{Deserialize, Serialize};

use crate::ids::{ActorId, ChatId, MessageId, ThreadId};
use crate::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatKind {
    /// Exactly two members. A message from one wakes the other.
    Direct,
    /// A name and any number of members. Posts wake nobody; mentions do.
    Group,
}

impl ChatKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChatKind::Direct => "direct",
            ChatKind::Group => "group",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "direct" => Some(ChatKind::Direct),
            "group" => Some(ChatKind::Group),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chat {
    pub id: ChatId,
    pub kind: ChatKind,
    /// Group chats have a name; direct chats are named by their members.
    pub name: Option<String>,
    pub created_by: ActorId,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    pub chat_id: ChatId,
    pub title: String,
    pub created_by: ActorId,
    pub created_at: Timestamp,
    pub last_activity_at: Timestamp,
    /// Agents cannot post while paused. Only the human pauses and unpauses.
    pub paused: bool,
    /// Idle for longer than the project's archive period. Posting reopens it.
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub thread_id: ThreadId,
    pub author_id: ActorId,
    /// Markdown.
    pub body: String,
    pub created_at: Timestamp,
}

/// A search result: the thread and a snippet of what matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadHit {
    pub thread: Thread,
    pub snippet: String,
}
