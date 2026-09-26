use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
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
uuid_id!(ActivityId);
uuid_id!(CheckpointId);
uuid_id!(InteractionId);
uuid_id!(RequestId);
uuid_id!(RunId);
uuid_id!(RunAttemptId);
uuid_id!(StepId);
uuid_id!(ToolCallId);
uuid_id!(TerminalId);
uuid_id!(TaskId);
uuid_id!(WorkspaceId);
uuid_id!(RepositoryId);

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
    use super::{AgentSessionId, EventSequence};
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
