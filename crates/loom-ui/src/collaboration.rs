//! Client-side multi-user state: who this connection is, the session roster,
//! the composer recipient, the run revision people decide against, and the
//! session's provider credential.
//!
//! Everything here is a projection of backend answers and events. The backend
//! enforces roles; the client only hides controls a role cannot use.

use loom_core::{
    AgentSessionId, Author, CollaborationEntry, CollaborationEntryKind, CredentialId, ErrorCode,
    LoomError, ParticipantId, ParticipantIdentity, PresenceStatus, ProviderCredentialMetadata,
    Recipient, RosterEntry, RunControlAction, RunCredentialPin, RunId, RunRevision, SessionRole,
};
use loom_protocol::{AgentRunSnapshot, AgentRunState};

/// Whom the composer sends to. The choice is explicit so peer chat never
/// reaches the agent by accident.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ComposerRecipient {
    #[default]
    Agent,
    People,
}

impl ComposerRecipient {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Agent => "Agent",
            Self::People => "People",
        }
    }
}

/// What the controls of the active session allow. An operator connection that
/// is not bound to a person is not subject to roles and may do everything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SessionPermissions {
    /// Chat, direct the agent, answer, approve or reject, pause, resume, and
    /// interrupt.
    pub(crate) can_act: bool,
    /// Manage access, credentials, and repositories.
    pub(crate) can_manage: bool,
}

impl SessionPermissions {
    pub(crate) const UNRESTRICTED: Self = Self {
        can_act: true,
        can_manage: true,
    };

    pub(crate) const fn for_role(role: Option<SessionRole>) -> Self {
        match role {
            None => Self::UNRESTRICTED,
            Some(role) => Self {
                can_act: role.can_act(),
                can_manage: role.can_manage(),
            },
        }
    }
}

/// Where a composer submission goes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ComposerRoute {
    /// Observers cannot send anything.
    ReadOnly,
    /// Peer chat: stored in the history, never sent to the agent.
    PeerMessage,
    /// No run yet (or the last one ended): start a run with the text as task.
    StartRun,
    /// The run's revision is unknown; read it, then send.
    NeedRevision { run_id: RunId },
    DirectAgent {
        run_id: RunId,
        expected: RunRevision,
    },
    AnswerAgentQuestion {
        run_id: RunId,
        expected: RunRevision,
    },
}

/// Picks the request for a composer submission.
///
/// `revision` is the last revision the client saw for the run; it is what the
/// person decided against, so a newer server revision yields a conflict rather
/// than silently applying the text to a run that moved on.
pub(crate) fn route_composer(
    permissions: SessionPermissions,
    recipient: ComposerRecipient,
    run: Option<&AgentRunSnapshot>,
    awaiting_input: bool,
    revision: Option<RunRevision>,
) -> ComposerRoute {
    if !permissions.can_act {
        return ComposerRoute::ReadOnly;
    }
    if recipient == ComposerRecipient::People {
        return ComposerRoute::PeerMessage;
    }
    let Some(run) = run.filter(|run| !run_is_finished(run.state)) else {
        return ComposerRoute::StartRun;
    };
    let Some(expected) = revision.filter(|revision| revision.attempt_id == run.attempt_id) else {
        return ComposerRoute::NeedRevision { run_id: run.id };
    };
    if awaiting_input || run.state == AgentRunState::NeedsInput {
        ComposerRoute::AnswerAgentQuestion {
            run_id: run.id,
            expected,
        }
    } else {
        ComposerRoute::DirectAgent {
            run_id: run.id,
            expected,
        }
    }
}

fn run_is_finished(state: AgentRunState) -> bool {
    matches!(
        state,
        AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
    )
}

/// A run action the server refused because the run had moved on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConflictNotice {
    pub(crate) run_id: RunId,
    pub(crate) expected: RunRevision,
    pub(crate) current: RunRevision,
}

impl ConflictNotice {
    pub(crate) fn message(&self) -> String {
        format!(
            "The run changed before your message arrived (revision {} -> {}); someone else \
             acted first. Nothing was sent and your draft is kept. Review the run and send again.",
            revision_label(&self.expected),
            revision_label(&self.current)
        )
    }
}

pub(crate) fn revision_label(revision: &RunRevision) -> String {
    format!("{}.{}", revision.control_revision, revision.action_sequence)
}

/// Multi-user projection for the active session.
#[derive(Clone, Debug, Default)]
pub(crate) struct CollaborationState {
    /// Whether the identity request has been answered.
    pub(crate) identity_loaded: bool,
    /// The person this connection is bound to; `None` for an operator.
    pub(crate) identity: Option<ParticipantIdentity>,
    pub(crate) roster_session: Option<AgentSessionId>,
    pub(crate) roster: Vec<RosterEntry>,
    /// People an owner can grant access to.
    pub(crate) directory: Vec<ParticipantIdentity>,
    pub(crate) recipient: ComposerRecipient,
    /// The last revision seen for the active run.
    pub(crate) revision: Option<(RunId, RunRevision)>,
    pub(crate) conflict: Option<ConflictNotice>,
    /// Text to put back into the composer after a refused send.
    pub(crate) restore_draft: Option<String>,
    /// Credentials the owner may select.
    pub(crate) credentials: Vec<ProviderCredentialMetadata>,
    /// The session's selected credential; `None` with `credential_loaded`
    /// means the worker default.
    pub(crate) session_credential: Option<ProviderCredentialMetadata>,
    pub(crate) credential_loaded: bool,
    pub(crate) run_pin: Option<(RunId, Option<RunCredentialPin>)>,
    /// Who sent each agent-directed text, newest last, so the transcript can
    /// attribute the user messages it rebuilds from the run.
    pub(crate) message_authors: Vec<(String, Author)>,
    pub(crate) request_in_flight: bool,
}

impl CollaborationState {
    /// Clears everything scoped to the previous session.
    pub(crate) fn reset_session(&mut self) {
        self.identity_loaded = false;
        self.roster_session = None;
        self.roster.clear();
        self.revision = None;
        self.conflict = None;
        self.session_credential = None;
        self.credential_loaded = false;
        self.run_pin = None;
        self.message_authors.clear();
        self.recipient = ComposerRecipient::Agent;
        self.request_in_flight = false;
    }

    /// The connection's role in the active session. `None` for an operator
    /// connection. A bound person missing from the roster is treated as an
    /// observer until the roster says otherwise.
    pub(crate) fn role(&self) -> Option<SessionRole> {
        let identity = self.identity.as_ref()?;
        Some(
            self.roster
                .iter()
                .find(|entry| entry.participant.id == identity.id)
                .map_or(SessionRole::Observer, |entry| entry.role),
        )
    }

    pub(crate) fn permissions(&self) -> SessionPermissions {
        SessionPermissions::for_role(self.role())
    }

    /// Whether this connection is bound to a person. Only then are roles and
    /// the roster relevant.
    pub(crate) fn is_participant_bound(&self) -> bool {
        self.identity.is_some()
    }

    pub(crate) fn replace_roster(&mut self, session_id: AgentSessionId, entries: Vec<RosterEntry>) {
        self.roster_session = Some(session_id);
        self.roster = sorted_roster(entries);
    }

    /// Records a revision for `run_id`, keeping the newest one.
    pub(crate) fn observe_revision(&mut self, run_id: RunId, revision: RunRevision) {
        match &self.revision {
            Some((current_run, current))
                if *current_run == run_id
                    && current.attempt_id == revision.attempt_id
                    && (current.control_revision, current.action_sequence)
                        > (revision.control_revision, revision.action_sequence) => {}
            _ => self.revision = Some((run_id, revision)),
        }
    }

    /// The revision to send for `run`: the last one seen, with the attempt and
    /// control revision the client has since learned from run events.
    pub(crate) fn revision_for(&self, run: &AgentRunSnapshot) -> Option<RunRevision> {
        let (run_id, revision) = self.revision.as_ref()?;
        (*run_id == run.id && revision.attempt_id == run.attempt_id).then_some(RunRevision {
            attempt_id: run.attempt_id,
            control_revision: run.control_revision.max(revision.control_revision),
            action_sequence: revision.action_sequence,
        })
    }

    /// Applies a refused action: keeps the draft, records the current revision
    /// so the next send is decided against it, and shows the notice.
    pub(crate) fn record_conflict(
        &mut self,
        run_id: RunId,
        expected: RunRevision,
        current: RunRevision,
        draft: String,
    ) {
        self.revision = Some((run_id, current));
        self.conflict = Some(ConflictNotice {
            run_id,
            expected,
            current,
        });
        self.restore_draft = Some(draft);
    }

    /// Records an attributed entry from the session history.
    pub(crate) fn observe_entry(&mut self, entry: &CollaborationEntry) {
        if let (Some(run_id), Some(revision)) = (entry.run_id, entry.revision) {
            self.observe_revision(run_id, revision);
        }
        if let CollaborationEntryKind::AgentDirection { text }
        | CollaborationEntryKind::AgentAnswer { text, .. } = &entry.kind
        {
            self.message_authors
                .push((text.clone(), entry.author.clone()));
            // Bounded: only recent messages need attribution.
            if self.message_authors.len() > 512 {
                self.message_authors.remove(0);
            }
        }
    }

    /// The author of an agent-directed message text, when known.
    pub(crate) fn author_of(&self, text: &str) -> Option<&Author> {
        self.message_authors
            .iter()
            .rev()
            .find(|(candidate, _)| candidate == text)
            .map(|(_, author)| author)
    }

    /// People an owner can still grant access to.
    pub(crate) fn grantable(&self) -> Vec<&ParticipantIdentity> {
        self.directory
            .iter()
            .filter(|person| {
                !self
                    .roster
                    .iter()
                    .any(|entry| entry.participant.id == person.id)
            })
            .collect()
    }

    /// How many owners the roster lists; the last one cannot be demoted.
    pub(crate) fn owner_count(&self) -> usize {
        self.roster
            .iter()
            .filter(|entry| entry.role == SessionRole::Owner)
            .count()
    }

    /// Whether an owner may change or revoke `participant_id`.
    pub(crate) fn can_change(&self, participant_id: ParticipantId) -> bool {
        if !self.permissions().can_manage {
            return false;
        }
        let is_owner = self.roster.iter().any(|entry| {
            entry.participant.id == participant_id && entry.role == SessionRole::Owner
        });
        !is_owner || self.owner_count() > 1
    }

    pub(crate) fn selected_credential_id(&self) -> Option<CredentialId> {
        self.session_credential
            .as_ref()
            .map(|credential| credential.id)
    }
}

fn sorted_roster(mut entries: Vec<RosterEntry>) -> Vec<RosterEntry> {
    entries.sort_by(|left, right| {
        right.role.cmp(&left.role).then_with(|| {
            left.participant
                .display_name
                .to_lowercase()
                .cmp(&right.participant.display_name.to_lowercase())
        })
    });
    entries
}

pub(crate) const fn role_label(role: SessionRole) -> &'static str {
    match role {
        SessionRole::Observer => "Observer",
        SessionRole::Participant => "Participant",
        SessionRole::Owner => "Owner",
    }
}

pub(crate) const fn presence_label(presence: PresenceStatus) -> &'static str {
    match presence {
        PresenceStatus::Online => "online",
        PresenceStatus::Away => "away",
        PresenceStatus::Offline => "offline",
    }
}

/// "Name · Role" for a person, "Operator" for an unbound connection.
pub(crate) fn author_label(author: &Author) -> String {
    match author {
        Author::Participant { participant, role } => {
            format!("{} · {}", participant.display_name, role_label(*role))
        }
        Author::Operator => "Operator".to_owned(),
    }
}

/// The heading of an attributed history entry, for example
/// "Sam Rivera · Participant -> Agent".
pub(crate) fn entry_heading(entry: &CollaborationEntry) -> String {
    let recipient = match entry.recipient {
        Recipient::Agent => "Agent",
        Recipient::People => "People",
    };
    format!("{} -> {recipient}", author_label(&entry.author))
}

/// The body of an attributed entry that is not plain text.
pub(crate) fn entry_text(entry: &CollaborationEntry) -> String {
    match &entry.kind {
        CollaborationEntryKind::PeerMessage { text }
        | CollaborationEntryKind::AgentDirection { text }
        | CollaborationEntryKind::AgentAnswer { text, .. } => text.clone(),
        CollaborationEntryKind::RunControl { action } => match action {
            RunControlAction::Approve { .. } => "Approved the pending action".to_owned(),
            RunControlAction::Reject { reason, .. } => match reason {
                Some(reason) => format!("Rejected the pending action: {reason}"),
                None => "Rejected the pending action".to_owned(),
            },
            RunControlAction::Pause => "Paused the run".to_owned(),
            RunControlAction::Resume => "Resumed the run".to_owned(),
            RunControlAction::Interrupt => "Interrupted the run".to_owned(),
            RunControlAction::Retry => "Retried the step".to_owned(),
            RunControlAction::RetryFromCheckpoint { .. } => "Retried from a checkpoint".to_owned(),
        },
        CollaborationEntryKind::AccessChanged { participant, role } => match role {
            Some(role) => format!(
                "{} is now {}",
                participant.display_name,
                role_label(*role).to_lowercase()
            ),
            None => format!("Revoked access for {}", participant.display_name),
        },
    }
}

/// "Provider · owner label · vN" for a credential.
pub(crate) fn credential_label(credential: &ProviderCredentialMetadata) -> String {
    let mut label = format!(
        "{} · {} · v{}",
        credential.provider, credential.owner_label, credential.version
    );
    if credential.revoked {
        label.push_str(" (revoked)");
    }
    label
}

pub(crate) fn pin_label(pin: &RunCredentialPin) -> String {
    match pin.version {
        Some(version) => format!("{} · {} · v{version}", pin.provider, pin.owner_label),
        None => format!("{} · {}", pin.provider, pin.owner_label),
    }
}

/// The recovery step for a provider authentication failure, which never falls
/// back to another account.
pub(crate) fn provider_auth_recovery(error: &LoomError) -> Option<&'static str> {
    (error.code == ErrorCode::ProviderAuthentication).then_some(
        "Recovery: the session owner signs in again or selects another credential under \
         People -> Run credential, then retries.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_core::{RunAttemptId, Timestamp};

    fn person(name: &str) -> ParticipantIdentity {
        ParticipantIdentity {
            id: ParticipantId::new(),
            display_name: name.to_owned(),
        }
    }

    fn entry(participant: &ParticipantIdentity, role: SessionRole) -> RosterEntry {
        RosterEntry {
            participant: participant.clone(),
            role,
            presence: PresenceStatus::Online,
            connections: 1,
            last_active_at: None,
        }
    }

    fn run(state: AgentRunState) -> AgentRunSnapshot {
        AgentRunSnapshot {
            id: RunId::new(),
            attempt_id: RunAttemptId::new(),
            control_revision: 3,
            session_id: AgentSessionId::new(),
            task: "task".to_owned(),
            model: loom_model::ModelId::new("deterministic/demo"),
            state,
            started_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: None,
        }
    }

    fn revision(run: &AgentRunSnapshot, sequence: u64) -> RunRevision {
        RunRevision {
            attempt_id: run.attempt_id,
            control_revision: run.control_revision,
            action_sequence: sequence,
        }
    }

    #[test]
    fn roles_gate_permissions() {
        assert_eq!(
            SessionPermissions::for_role(None),
            SessionPermissions::UNRESTRICTED
        );
        let observer = SessionPermissions::for_role(Some(SessionRole::Observer));
        assert!(!observer.can_act && !observer.can_manage);
        let participant = SessionPermissions::for_role(Some(SessionRole::Participant));
        assert!(participant.can_act && !participant.can_manage);
        let owner = SessionPermissions::for_role(Some(SessionRole::Owner));
        assert!(owner.can_act && owner.can_manage);
    }

    #[test]
    fn role_comes_from_the_roster_and_defaults_to_observer() {
        let mut state = CollaborationState::default();
        assert_eq!(state.role(), None, "operator connections have no role");
        let me = person("Sam");
        state.identity = Some(me.clone());
        assert_eq!(state.role(), Some(SessionRole::Observer));
        assert!(!state.permissions().can_act);
        let session = AgentSessionId::new();
        state.replace_roster(session, vec![entry(&me, SessionRole::Participant)]);
        assert_eq!(state.role(), Some(SessionRole::Participant));
        assert!(state.permissions().can_act);
        assert!(!state.permissions().can_manage);
    }

    #[test]
    fn roster_sorts_owners_first_and_protects_the_last_owner() {
        let mut state = CollaborationState::default();
        let owner = person("Zed");
        let other = person("amy");
        state.identity = Some(owner.clone());
        state.replace_roster(
            AgentSessionId::new(),
            vec![
                entry(&other, SessionRole::Observer),
                entry(&owner, SessionRole::Owner),
            ],
        );
        assert_eq!(state.roster[0].participant.id, owner.id);
        assert!(!state.can_change(owner.id), "last owner stays");
        assert!(state.can_change(other.id));
        let stranger = person("Kim");
        state.directory = vec![other.clone(), stranger.clone()];
        assert_eq!(state.grantable(), vec![&stranger]);
    }

    #[test]
    fn routing_follows_recipient_role_and_run_state() {
        let all = SessionPermissions::UNRESTRICTED;
        let observer = SessionPermissions::for_role(Some(SessionRole::Observer));
        let active = run(AgentRunState::Executing);
        let current = revision(&active, 4);
        assert_eq!(
            route_composer(
                observer,
                ComposerRecipient::People,
                Some(&active),
                false,
                None
            ),
            ComposerRoute::ReadOnly
        );
        assert_eq!(
            route_composer(all, ComposerRecipient::People, Some(&active), false, None),
            ComposerRoute::PeerMessage
        );
        assert_eq!(
            route_composer(all, ComposerRecipient::Agent, None, false, None),
            ComposerRoute::StartRun
        );
        assert_eq!(
            route_composer(
                all,
                ComposerRecipient::Agent,
                Some(&run(AgentRunState::Completed)),
                false,
                None
            ),
            ComposerRoute::StartRun
        );
        assert_eq!(
            route_composer(all, ComposerRecipient::Agent, Some(&active), false, None),
            ComposerRoute::NeedRevision { run_id: active.id }
        );
        assert_eq!(
            route_composer(
                all,
                ComposerRecipient::Agent,
                Some(&active),
                false,
                Some(current)
            ),
            ComposerRoute::DirectAgent {
                run_id: active.id,
                expected: current
            }
        );
        assert_eq!(
            route_composer(
                all,
                ComposerRecipient::Agent,
                Some(&active),
                true,
                Some(current)
            ),
            ComposerRoute::AnswerAgentQuestion {
                run_id: active.id,
                expected: current
            }
        );
        // A revision from an earlier attempt is not decided against this one.
        let stale = RunRevision {
            attempt_id: RunAttemptId::new(),
            ..current
        };
        assert_eq!(
            route_composer(
                all,
                ComposerRecipient::Agent,
                Some(&active),
                false,
                Some(stale)
            ),
            ComposerRoute::NeedRevision { run_id: active.id }
        );
    }

    #[test]
    fn conflict_keeps_the_draft_and_adopts_the_current_revision() {
        let mut state = CollaborationState::default();
        let active = run(AgentRunState::Paused);
        let expected = revision(&active, 1);
        let current = revision(&active, 2);
        state.observe_revision(active.id, expected);
        state.record_conflict(active.id, expected, current, "please resume".to_owned());
        assert_eq!(state.restore_draft.as_deref(), Some("please resume"));
        assert_eq!(state.revision_for(&active), Some(current));
        let message = state.conflict.as_ref().unwrap().message();
        assert!(message.contains("Nothing was sent"));
        assert!(message.contains("3.1 -> 3.2"));
    }

    #[test]
    fn revisions_only_move_forward_and_follow_run_control() {
        let mut state = CollaborationState::default();
        let mut active = run(AgentRunState::Executing);
        state.observe_revision(active.id, revision(&active, 5));
        state.observe_revision(active.id, revision(&active, 2));
        assert_eq!(state.revision_for(&active).unwrap().action_sequence, 5);
        active.control_revision += 1;
        assert_eq!(
            state.revision_for(&active).unwrap().control_revision,
            active.control_revision
        );
        assert_eq!(state.revision_for(&run(AgentRunState::Executing)), None);
    }

    #[test]
    fn entries_attribute_agent_messages_and_describe_actions() {
        let mut state = CollaborationState::default();
        let sam = person("Sam Rivera");
        let author = Author::Participant {
            participant: sam.clone(),
            role: SessionRole::Participant,
        };
        let active = run(AgentRunState::Executing);
        let direction = CollaborationEntry {
            session_id: AgentSessionId::new(),
            run_id: Some(active.id),
            author: author.clone(),
            recipient: Recipient::Agent,
            kind: CollaborationEntryKind::AgentDirection {
                text: "cover reconnects".to_owned(),
            },
            revision: Some(revision(&active, 7)),
            created_at: Timestamp::now(),
        };
        state.observe_entry(&direction);
        assert_eq!(state.author_of("cover reconnects"), Some(&author));
        assert_eq!(state.revision_for(&active).unwrap().action_sequence, 7);
        assert_eq!(
            entry_heading(&direction),
            "Sam Rivera · Participant -> Agent"
        );
        let peer = CollaborationEntry {
            recipient: Recipient::People,
            kind: CollaborationEntryKind::PeerMessage {
                text: "hi".to_owned(),
            },
            run_id: None,
            revision: None,
            ..direction.clone()
        };
        assert_eq!(entry_heading(&peer), "Sam Rivera · Participant -> People");
        let pause = CollaborationEntry {
            kind: CollaborationEntryKind::RunControl {
                action: RunControlAction::Pause,
            },
            ..direction.clone()
        };
        assert_eq!(entry_text(&pause), "Paused the run");
        let revoke = CollaborationEntry {
            kind: CollaborationEntryKind::AccessChanged {
                participant: sam,
                role: None,
            },
            ..direction
        };
        assert_eq!(entry_text(&revoke), "Revoked access for Sam Rivera");
        assert_eq!(author_label(&Author::Operator), "Operator");
    }

    #[test]
    fn provider_authentication_errors_carry_a_recovery_hint() {
        let auth = LoomError::new(ErrorCode::ProviderAuthentication, "revoked", false);
        assert!(
            provider_auth_recovery(&auth)
                .unwrap()
                .contains("Run credential")
        );
        assert!(provider_auth_recovery(&LoomError::invalid_state("x")).is_none());
    }

    #[test]
    fn credential_labels_name_provider_owner_and_version() {
        let credential = ProviderCredentialMetadata {
            id: CredentialId::new(),
            provider: "github-copilot".to_owned(),
            owner: None,
            owner_label: "Björn".to_owned(),
            version: 3,
            created_at: Timestamp::now(),
            updated_at: Timestamp::now(),
            revoked: true,
        };
        assert_eq!(
            credential_label(&credential),
            "github-copilot · Björn · v3 (revoked)"
        );
        let pin = RunCredentialPin {
            credential_id: None,
            version: Some(2),
            provider: "openai".to_owned(),
            model: "m".to_owned(),
            owner: None,
            owner_label: "Worker default".to_owned(),
        };
        assert_eq!(pin_label(&pin), "openai · Worker default · v2");
    }
}
