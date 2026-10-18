use std::time::Duration;

use crate::{AccessRequestId, AgentSessionId, ParticipantId, Timestamp};
use serde::{Deserialize, Serialize};

/// Most UTF-8 bytes a participant display name may carry.
pub const MAX_PARTICIPANT_DISPLAY_NAME_BYTES: usize = 128;

/// What a person may do in one session.
///
/// Roles are ordered: every role can do everything the roles below it can.
///
/// - `Observer` reads the session, its roster, its history, and its run
///   state. It cannot send anything or control a run.
/// - `Participant` additionally does everything inside a run that the owner
///   can: peer chat, agent direction, answers, approving or rejecting tool
///   actions, and pausing, resuming, or interrupting a run.
/// - `Owner` additionally manages access (grant, promote, demote, revoke),
///   selects the run credential, and browses or attaches repositories.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    Observer,
    Participant,
    Owner,
}

impl SessionRole {
    /// The role a new grant starts with; the owner promotes it afterwards.
    pub const DEFAULT_GRANT: Self = Self::Observer;

    /// Whether the role may act inside a run (send, direct, approve, control).
    pub const fn can_act(self) -> bool {
        matches!(self, Self::Participant | Self::Owner)
    }

    /// Whether the role may manage access, credentials, and repositories.
    pub const fn can_manage(self) -> bool {
        matches!(self, Self::Owner)
    }

    /// Stable lowercase name, used in storage and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observer => "observer",
            Self::Participant => "participant",
            Self::Owner => "owner",
        }
    }

    /// Parses the name written by [`SessionRole::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "observer" => Some(Self::Observer),
            "participant" => Some(Self::Participant),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }
}

/// A person known to the backend.
///
/// The identity is derived by the server from the credential a connection
/// authenticated with; a client never supplies it. It is separate from any
/// provider or GitHub credential.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ParticipantIdentity {
    pub id: ParticipantId,
    pub display_name: String,
}

/// First bytes of every participant id derived from a GitHub account.
///
/// They keep a GitHub-derived id from colliding with a randomly generated one
/// and make the origin of an id visible in a dump.
const GITHUB_PARTICIPANT_ID_NAMESPACE: [u8; 8] = *b"loomgit1";

/// The stable participant id of one GitHub account.
///
/// The id comes from the immutable numeric account id, so it survives a login
/// rename, a re-authentication, and a restart of the worker. The login is a
/// display name only and never identifies the person; see
/// [`GitHubSignInStatus::SignedIn`].
pub fn github_participant_id(github_user_id: u64) -> ParticipantId {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&GITHUB_PARTICIPANT_ID_NAMESPACE);
    bytes[8..].copy_from_slice(&github_user_id.to_be_bytes());
    // Shape the derived bytes as an RFC 9562 version 4 identifier so tooling
    // that inspects the version and variant bits keeps working.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    ParticipantId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

/// Progress of one GitHub sign-in attempt on a worker.
///
/// A sign-in starts `Pending` and ends either `SignedIn`, carrying the identity
/// the worker derived from the GitHub account, or `Failed` with the reason. The
/// worker keeps the GitHub token only long enough to read the account and never
/// stores it as a provider credential.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", content = "data", rename_all = "snake_case")]
pub enum GitHubSignInStatus {
    /// Waiting for the person to authorize the device code on GitHub.
    Pending,
    /// The connection is bound to this person.
    SignedIn { participant: ParticipantIdentity },
    /// The sign-in ended without binding a person.
    Failed { message: String },
}

/// One person's access to one session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionGrant {
    pub session_id: AgentSessionId,
    pub participant_id: ParticipantId,
    pub role: SessionRole,
    /// Who created or last changed the grant; `None` for the server itself
    /// (for example the creator's implicit owner grant).
    pub granted_by: Option<ParticipantId>,
    pub granted_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Most UTF-8 bytes the note of an access request may carry.
pub const MAX_ACCESS_REQUEST_NOTE_BYTES: usize = 1024;

/// A person's pending request for access to a session, or for a higher role
/// in it. Only pending requests are stored; a decision or a withdrawal
/// removes the request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionAccessRequest {
    pub id: AccessRequestId,
    pub session_id: AgentSessionId,
    /// The requester, derived by the server from the connection.
    pub participant: ParticipantIdentity,
    pub requested_role: SessionRole,
    pub note: Option<String>,
    pub created_at: Timestamp,
}

/// How a pending access request ended.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AccessRequestDecision {
    /// An owner approved it; `role` is the role the requester now holds.
    Approved { role: SessionRole },
    /// An owner denied it.
    Denied { reason: Option<String> },
    /// The requester withdrew it.
    Withdrawn,
}

/// Aggregated presence of one person across all of their connections.
///
/// Policy (see [`PresenceStatus::from_activity`]):
///
/// - A connection is live while it shows transport liveness (any frame, ping,
///   or pong) within [`PresenceStatus::STALE_AFTER`]. A connection that stays
///   silent longer is stale: it is dropped from presence even if its socket
///   was never closed, and the remote listener closes it on the same bound.
/// - A person with at least one live connection is `Online` when any live
///   connection sent a request within [`PresenceStatus::AWAY_AFTER`], and
///   `Away` otherwise.
/// - A person with no live connection is `Offline`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceStatus {
    Online,
    Away,
    Offline,
}

impl PresenceStatus {
    /// Liveness window of one connection: three server heartbeat intervals.
    pub const STALE_AFTER: Duration = Duration::from_secs(45);
    /// How long a live person may be idle before showing as away.
    pub const AWAY_AFTER: Duration = Duration::from_secs(300);

    /// Presence from the age of the freshest liveness signal and the freshest
    /// request across one person's connections. `None` means no connection.
    pub fn from_activity(liveness_age: Option<Duration>, request_age: Option<Duration>) -> Self {
        match liveness_age {
            Some(age) if age <= Self::STALE_AFTER => {
                if request_age.is_some_and(|age| age <= Self::AWAY_AFTER) {
                    Self::Online
                } else {
                    Self::Away
                }
            }
            _ => Self::Offline,
        }
    }
}

/// One person in a session roster.
///
/// A roster carries exactly one entry per person, however many connections
/// that person has open.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RosterEntry {
    pub participant: ParticipantIdentity,
    pub role: SessionRole,
    pub presence: PresenceStatus,
    /// Live connections this person currently holds.
    pub connections: u32,
    /// Most recent request from any of this person's connections, if known.
    pub last_active_at: Option<Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_ordered_and_name_their_permissions() {
        assert!(SessionRole::Observer < SessionRole::Participant);
        assert!(SessionRole::Participant < SessionRole::Owner);
        assert!(!SessionRole::Observer.can_act());
        assert!(SessionRole::Participant.can_act());
        assert!(!SessionRole::Participant.can_manage());
        assert!(SessionRole::Owner.can_manage());
        assert_eq!(SessionRole::DEFAULT_GRANT, SessionRole::Observer);
        for role in [
            SessionRole::Observer,
            SessionRole::Participant,
            SessionRole::Owner,
        ] {
            assert_eq!(SessionRole::parse(role.as_str()), Some(role));
            assert_eq!(
                serde_json::to_string(&role).unwrap(),
                format!("\"{}\"", role.as_str())
            );
        }
        assert_eq!(SessionRole::parse("admin"), None);
    }

    #[test]
    fn github_participant_ids_are_stable_per_account_and_distinct_across_accounts() {
        let first = github_participant_id(141030);
        // Derived from the immutable numeric id only, so it is the same in any
        // process and after a login rename or a re-authentication.
        assert_eq!(first, github_participant_id(141030));
        assert_ne!(first, github_participant_id(141031));
        assert_ne!(first, ParticipantId::new());
        assert_eq!(first.as_uuid().get_version_num(), 4);
        let bytes = first.as_uuid().as_bytes();
        // The namespace survives except where the version and variant bits are
        // shaped; the numeric account id is carried verbatim.
        assert_eq!(&bytes[..6], &b"loomgi"[..]);
        assert_eq!(bytes[7], b'1');
        assert_eq!(&bytes[9..], &141030_u64.to_be_bytes()[1..]);
        // Distinct accounts that share a display name stay distinct people.
        assert_ne!(
            ParticipantIdentity {
                id: github_participant_id(1),
                display_name: "same".to_owned(),
            },
            ParticipantIdentity {
                id: github_participant_id(2),
                display_name: "same".to_owned(),
            }
        );
    }

    #[test]
    fn sign_in_status_reports_progress_and_the_bound_person() {
        let participant = ParticipantIdentity {
            id: github_participant_id(7),
            display_name: "octocat".to_owned(),
        };
        let statuses = [
            GitHubSignInStatus::Pending,
            GitHubSignInStatus::SignedIn {
                participant: participant.clone(),
            },
            GitHubSignInStatus::Failed {
                message: "expired".to_owned(),
            },
        ];
        for status in statuses {
            let encoded = serde_json::to_string(&status).unwrap();
            assert_eq!(
                serde_json::from_str::<GitHubSignInStatus>(&encoded).unwrap(),
                status
            );
        }
        assert_eq!(
            serde_json::to_string(&GitHubSignInStatus::Pending).unwrap(),
            r#"{"status":"pending"}"#
        );
        assert_eq!(
            serde_json::to_string(&GitHubSignInStatus::SignedIn {
                participant: participant.clone()
            })
            .unwrap(),
            format!(
                r#"{{"status":"signed_in","data":{{"participant":{}}}}}"#,
                serde_json::to_string(&participant).unwrap()
            )
        );
    }

    #[test]
    fn presence_follows_the_documented_policy() {
        let fresh = Some(Duration::from_secs(1));
        assert_eq!(
            PresenceStatus::from_activity(fresh, fresh),
            PresenceStatus::Online
        );
        assert_eq!(
            PresenceStatus::from_activity(fresh, None),
            PresenceStatus::Away
        );
        assert_eq!(
            PresenceStatus::from_activity(fresh, Some(Duration::from_secs(301))),
            PresenceStatus::Away
        );
        assert_eq!(
            PresenceStatus::from_activity(Some(Duration::from_secs(46)), fresh),
            PresenceStatus::Offline
        );
        assert_eq!(
            PresenceStatus::from_activity(None, None),
            PresenceStatus::Offline
        );
    }
}
