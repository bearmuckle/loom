use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        uuid_id!($name, Uuid::new_v4);
    };
    ($name:ident, $generator:path) => {
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self($generator())
            }

            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self::from_uuid(value)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self::from_uuid(Uuid::parse_str(value)?))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

uuid_id!(AgentSessionId);
uuid_id!(ProjectId);
uuid_id!(AgentMessageId);
uuid_id!(ActivityId);
uuid_id!(CheckpointId);
uuid_id!(InteractionId);
uuid_id!(RequestId, Uuid::now_v7);
uuid_id!(RunId);
uuid_id!(RunAttemptId);
uuid_id!(StepId);
uuid_id!(ToolCallId);
uuid_id!(TerminalId);
uuid_id!(TaskId);
uuid_id!(WorkspaceId);
uuid_id!(RepositoryId);
uuid_id!(ProjectManagerWaitId);

impl RequestId {
    /// Returns the immutable Unix-millisecond issue time for UUIDv7 request IDs.
    /// Older UUIDv4 request IDs have no embedded issue time.
    pub fn issued_at_unix_millis(&self) -> Option<u64> {
        let uuid = self.as_uuid();
        if uuid.get_version_num() != 7 {
            return None;
        }
        let bytes = uuid.as_bytes();
        Some(u64::from_be_bytes([
            0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5],
        ]))
    }
}

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct EventSequence(u64);

impl EventSequence {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for EventSequence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentSessionId, EventSequence, RequestId};
    use std::str::FromStr;
    use uuid::Uuid;

    #[test]
    fn uuid_ids_round_trip_through_string_and_serde() {
        let uuid = Uuid::from_u128(42);
        let id = AgentSessionId::from_uuid(uuid);

        assert_eq!(*id.as_uuid(), uuid);
        assert_eq!(AgentSessionId::from_str(&id.to_string()).unwrap(), id);
        assert!(AgentSessionId::from_str("not-a-uuid").is_err());
        assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{uuid}\""));
        assert_eq!(
            serde_json::from_str::<AgentSessionId>(&format!("\"{uuid}\"")).unwrap(),
            id
        );
        assert_ne!(AgentSessionId::new(), AgentSessionId::new());
        let request_id = RequestId::new();
        assert_eq!(request_id.as_uuid().get_version_num(), 7);
        assert!(request_id.issued_at_unix_millis().is_some());
        let legacy_request_id = RequestId::from_uuid(Uuid::new_v4());
        assert_eq!(legacy_request_id.issued_at_unix_millis(), None);
        assert_eq!(AgentSessionId::default().as_uuid().get_version_num(), 4);
    }

    #[test]
    fn event_sequence_saturates_and_displays() {
        assert_eq!(EventSequence::default().value(), 0);
        assert_eq!(EventSequence::new(7).next(), EventSequence::new(8));
        assert_eq!(
            EventSequence::new(u64::MAX).next(),
            EventSequence::new(u64::MAX)
        );
        assert_eq!(EventSequence::new(7).to_string(), "7");
        assert_eq!(
            serde_json::from_str::<EventSequence>("9").unwrap().value(),
            9
        );
    }
}
