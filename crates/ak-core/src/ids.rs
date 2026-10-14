//! Strongly typed identifiers. Every entity gets its own newtype over a UUID so
//! a ticket id can never be passed where an actor id is expected.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

id_type!(
    /// A project as registered on this machine.
    ProjectId
);
id_type!(
    /// Anyone who can act on a board: the human, an agent, or the system.
    ActorId
);
id_type!(ColumnId);
id_type!(TicketId);
id_type!(CommentId);
id_type!(EventId);
id_type!(NotificationId);
id_type!(AgentId);
id_type!(RunId);
id_type!(ChatId);
id_type!(ThreadId);
id_type!(MessageId);
id_type!(CallId);

impl ActorId {
    /// The reserved actor used for guardrail and system actions.
    pub const SYSTEM: ActorId = ActorId(Uuid::from_u128(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_through_strings() {
        let id = TicketId::new();
        let parsed: TicketId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn ids_serialize_as_plain_uuid_strings() {
        let id = ActorId::SYSTEM;
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", Uuid::from_u128(1)));
    }
}
