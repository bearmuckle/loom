//! Client-side multi-user state: who this connection is, the session roster,
//! the composer recipient, the run revision people decide against, and the
//! session's provider credential.
//!
//! Everything here is a projection of backend answers and events. The backend
//! enforces roles; the client only hides controls a role cannot use.

use loom_core::{
    AccessRequestDecision, AccessRequestId, AgentSessionId, Author, CollaborationEntry,
    CollaborationEntryKind, CredentialId, ErrorCode, LoomError, MAX_ACCESS_REQUEST_NOTE_BYTES,
    ParticipantId, ParticipantIdentity, PresenceStatus, ProviderCredentialMetadata, Recipient,
    RosterEntry, RunControlAction, RunCredentialPin, RunId, RunRevision, SessionAccessRequest,
    SessionRole,
};
use loom_protocol::{AgentRunSnapshot, AgentRunState, CollaborationRequest, RunRequest};

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
    /// The refused run control, or `None` for a refused message.
    pub(crate) control: Option<RunControlAction>,
}

impl ConflictNotice {
    pub(crate) fn heading(&self) -> String {
        let kind = if self.control.is_some() {
            "Control conflict"
        } else {
            "Direction conflict"
        };
        format!("{kind} · run rev {}", revision_label(&self.current))
    }

    pub(crate) fn message(&self) -> String {
        match &self.control {
            Some(action) => format!(
                "The run changed before your action arrived (revision {} -> {}); someone else \
                 acted first. {} was not applied. Review the run and decide again.",
                revision_label(&self.expected),
                revision_label(&self.current),
                control_label(action)
            ),
            None => format!(
                "The run changed before your message arrived (revision {} -> {}); someone else \
                 acted first. Nothing was sent and your draft is kept. Review the run and send \
                 again.",
                revision_label(&self.expected),
                revision_label(&self.current)
            ),
        }
    }
}

/// A short name for a run control, as in "Approve".
pub(crate) const fn control_label(action: &RunControlAction) -> &'static str {
    match action {
        RunControlAction::Approve { .. } => "Approve",
        RunControlAction::Reject { .. } => "Reject",
        RunControlAction::Pause => "Pause",
        RunControlAction::Resume => "Resume",
        RunControlAction::Interrupt => "Interrupt",
        RunControlAction::Retry => "Retry",
        RunControlAction::RetryFromCheckpoint { .. } => "Retry from checkpoint",
    }
}

/// Where a run control goes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ControlRoute {
    /// The worker has no ordered collaboration path: use the run request.
    Legacy(RunRequest),
    /// The run's revision is unknown; read it, then send.
    NeedRevision { run_id: RunId },
    /// Send through the ordered gate against `expected`.
    Ordered(CollaborationRequest),
}

/// Picks the request for a run control. `ordered` is whether the worker
/// supports collaboration requests.
pub(crate) fn route_control(
    ordered: bool,
    run: &AgentRunSnapshot,
    revision: Option<RunRevision>,
    action: RunControlAction,
) -> ControlRoute {
    if !ordered {
        return ControlRoute::Legacy(legacy_control_request(run, action));
    }
    match revision.filter(|revision| revision.attempt_id == run.attempt_id) {
        Some(expected) => ControlRoute::Ordered(CollaborationRequest::ControlRun {
            run_id: run.id,
            expected,
            action,
        }),
        None => ControlRoute::NeedRevision { run_id: run.id },
    }
}

/// The pre-collaboration request for a run control, for older workers.
pub(crate) fn legacy_control_request(
    run: &AgentRunSnapshot,
    action: RunControlAction,
) -> RunRequest {
    let run_id = run.id;
    match action {
        RunControlAction::Approve { tool_call_id } => RunRequest::ApproveAgentAction {
            run_id,
            attempt_id: run.attempt_id,
            expected_control_revision: run.control_revision,
            tool_call_id,
        },
        RunControlAction::Reject {
            tool_call_id,
            reason,
        } => RunRequest::RejectAgentAction {
            run_id,
            attempt_id: run.attempt_id,
            expected_control_revision: run.control_revision,
            tool_call_id,
            reason,
        },
        RunControlAction::Pause => RunRequest::PauseAgentRun { run_id },
        RunControlAction::Resume => RunRequest::ResumeAgentRun { run_id },
        RunControlAction::Interrupt => RunRequest::InterruptAgentRun { run_id },
        RunControlAction::Retry => RunRequest::RetryAgentStep { run_id },
        RunControlAction::RetryFromCheckpoint { checkpoint_id } => {
            RunRequest::RetryAgentFromCheckpoint {
                run_id,
                checkpoint_id,
            }
        }
    }
}

/// Whether an error means the worker does not know collaboration requests,
/// so the client should use the legacy run requests instead.
pub(crate) fn is_unsupported_error(error: &LoomError) -> bool {
    matches!(
        error.code,
        ErrorCode::UnsupportedProtocol
            | ErrorCode::UnsupportedCapability
            | ErrorCode::MalformedPayload
    )
}

/// How many applied entries are remembered for de-duplication.
const MAX_SEEN_ENTRIES: usize = 2_048;

/// Whether the worker answers collaboration requests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum CollaborationSupport {
    /// Not known yet: the identity request has not been answered.
    #[default]
    Unknown,
    Supported,
    /// An older worker: use the legacy run requests.
    Legacy,
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
    /// Whether the worker answers collaboration requests.
    pub(crate) support: CollaborationSupport,
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
    /// Entries already applied, newest last, to de-duplicate live events
    /// against the history read on load.
    pub(crate) seen_entries: std::collections::VecDeque<CollaborationEntry>,
    pub(crate) request_in_flight: bool,
    /// Session access requests: the caller's own and, for owners, the
    /// pending ones.
    pub(crate) access: AccessRequestsState,
}

/// Access request state for the active session.
#[derive(Clone, Debug, Default)]
pub(crate) struct AccessRequestsState {
    /// The worker does not know access requests (older protocol); the
    /// controls are hidden.
    pub(crate) unsupported: bool,
    /// The role the requester picked; `None` means participant.
    pub(crate) draft_role: Option<SessionRole>,
    /// The caller's pending request.
    pub(crate) own: Option<SessionAccessRequest>,
    /// How the caller's last request ended (approved or denied).
    pub(crate) outcome: Option<AccessRequestDecision>,
    /// Pending requests an owner decides on, oldest first.
    pub(crate) pending: Vec<SessionAccessRequest>,
    /// The role an owner picked to approve each request with.
    pub(crate) approve_roles: Vec<(AccessRequestId, SessionRole)>,
    /// Whether the owner list was read for this session.
    pub(crate) pending_loaded: bool,
    pub(crate) in_flight: bool,
}

impl AccessRequestsState {
    pub(crate) fn draft_role(&self) -> SessionRole {
        self.draft_role.unwrap_or(SessionRole::Participant)
    }

    /// The role an approval grants: the owner's pick, else the requested one.
    pub(crate) fn approve_role(&self, request: &SessionAccessRequest) -> SessionRole {
        self.approve_roles
            .iter()
            .find(|(id, _)| *id == request.id)
            .map_or(request.requested_role, |(_, role)| *role)
    }

    pub(crate) fn set_approve_role(&mut self, request_id: AccessRequestId, role: SessionRole) {
        self.approve_roles.retain(|(id, _)| *id != request_id);
        self.approve_roles.push((request_id, role));
    }

    /// Replaces the owner list with a fresh read.
    pub(crate) fn replace_pending(&mut self, requests: Vec<SessionAccessRequest>) {
        self.approve_roles
            .retain(|(id, _)| requests.iter().any(|request| request.id == *id));
        self.pending = requests;
    }

    /// Adds a filed request, or replaces it in place when it was re-filed.
    pub(crate) fn upsert_pending(&mut self, request: SessionAccessRequest) {
        match self.pending.iter_mut().find(|known| known.id == request.id) {
            Some(known) => *known = request,
            None => self.pending.push(request),
        }
    }

    /// Records the caller's filed (or replaced) request.
    pub(crate) fn record_own(&mut self, request: SessionAccessRequest) {
        self.outcome = None;
        self.own = Some(request);
    }

    /// Applies a resolution. `mine` is whether the caller filed the request.
    pub(crate) fn resolve(
        &mut self,
        request: &SessionAccessRequest,
        decision: &AccessRequestDecision,
        mine: bool,
    ) {
        self.pending.retain(|known| known.id != request.id);
        self.approve_roles.retain(|(id, _)| *id != request.id);
        if mine && self.own.as_ref().is_none_or(|own| own.id == request.id) {
            self.own = None;
            self.outcome = match decision {
                AccessRequestDecision::Withdrawn => None,
                other => Some(other.clone()),
            };
        }
    }
}

/// The note to send with an access request: trimmed, `None` when empty.
/// Errors when it is longer than [`MAX_ACCESS_REQUEST_NOTE_BYTES`].
pub(crate) fn access_request_note(text: &str) -> Result<Option<String>, String> {
    let text = text.trim();
    if text.len() > MAX_ACCESS_REQUEST_NOTE_BYTES {
        return Err(format!(
            "The note is {} bytes; at most {MAX_ACCESS_REQUEST_NOTE_BYTES} are allowed.",
            text.len()
        ));
    }
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

/// The requester-facing text for how a request ended.
pub(crate) fn access_outcome_text(decision: &AccessRequestDecision) -> String {
    match decision {
        AccessRequestDecision::Approved { role } => {
            format!(
                "Approved: you are now {}.",
                role_label(*role).to_lowercase()
            )
        }
        AccessRequestDecision::Denied {
            reason: Some(reason),
        } => {
            format!("Denied: {reason}")
        }
        AccessRequestDecision::Denied { reason: None } => "Denied.".to_owned(),
        AccessRequestDecision::Withdrawn => "Withdrawn.".to_owned(),
    }
}

impl CollaborationState {
    /// Clears everything scoped to the previous session.
    pub(crate) fn reset_session(&mut self) {
        self.identity_loaded = false;
        self.support = CollaborationSupport::Unknown;
        self.roster_session = None;
        self.roster.clear();
        self.revision = None;
        self.conflict = None;
        self.session_credential = None;
        self.credential_loaded = false;
        self.run_pin = None;
        self.message_authors.clear();
        self.seen_entries.clear();
        self.recipient = ComposerRecipient::Agent;
        self.request_in_flight = false;
        self.access = AccessRequestsState::default();
    }

    /// Whether the connection may ask for access: a bound person who is an
    /// observer (or has no grant) on a worker that knows access requests.
    pub(crate) fn can_request_access(&self) -> bool {
        self.support == CollaborationSupport::Supported
            && !self.access.unsupported
            && self.role() == Some(SessionRole::Observer)
    }

    /// Whether the owner's pending request list applies.
    pub(crate) fn manages_access_requests(&self) -> bool {
        self.support == CollaborationSupport::Supported
            && !self.access.unsupported
            && self.role() == Some(SessionRole::Owner)
    }

    /// Applies a filed or replaced request from an event.
    pub(crate) fn apply_access_requested(
        &mut self,
        active: AgentSessionId,
        request: &SessionAccessRequest,
    ) {
        if request.session_id != active {
            return;
        }
        if self.is_requester(request) {
            self.access.record_own(request.clone());
        }
        if self.role() == Some(SessionRole::Owner) {
            self.access.upsert_pending(request.clone());
        }
    }

    /// Applies an approved, denied, or withdrawn request.
    pub(crate) fn apply_access_resolved(
        &mut self,
        active: AgentSessionId,
        request: &SessionAccessRequest,
        decision: &AccessRequestDecision,
    ) {
        if request.session_id != active {
            return;
        }
        let mine = self.is_requester(request);
        self.access.resolve(request, decision, mine);
    }

    fn is_requester(&self, request: &SessionAccessRequest) -> bool {
        self.identity
            .as_ref()
            .is_some_and(|identity| identity.id == request.participant.id)
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

    /// Whether directions, answers, and controls take the ordered
    /// collaboration path. True for participant-bound and operator
    /// connections alike once the worker has answered the identity request.
    pub(crate) fn uses_ordered_path(&self) -> bool {
        self.support == CollaborationSupport::Supported
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
            control: None,
        });
        self.restore_draft = Some(draft);
    }

    /// Applies a refused run control: records the current revision and shows
    /// the notice. The control is not applied or retried.
    pub(crate) fn record_control_conflict(
        &mut self,
        run_id: RunId,
        expected: RunRevision,
        current: RunRevision,
        action: RunControlAction,
    ) {
        self.revision = Some((run_id, current));
        self.conflict = Some(ConflictNotice {
            run_id,
            expected,
            current,
            control: Some(action),
        });
    }

    /// Records an attributed entry from the session history. Returns `false`
    /// for an entry already seen, so a live event and the same entry read back
    /// from history apply once.
    pub(crate) fn observe_entry(&mut self, entry: &CollaborationEntry) -> bool {
        if self.seen_entries.contains(entry) {
            return false;
        }
        self.seen_entries.push_back(entry.clone());
        if self.seen_entries.len() > MAX_SEEN_ENTRIES {
            self.seen_entries.pop_front();
        }
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
        true
    }

    /// Forgets which entries were applied, after the timeline was cleared for
    /// a rebuild, so the history read can put them back.
    pub(crate) fn forget_entries(&mut self) {
        self.seen_entries.clear();
        self.message_authors.clear();
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
        CollaborationEntryKind::AccessRequestDecided {
            participant,
            requested_role,
            decision,
            ..
        } => match decision {
            loom_core::AccessRequestDecision::Approved { role } => format!(
                "Approved {} as {}",
                participant.display_name,
                role_label(*role).to_lowercase()
            ),
            loom_core::AccessRequestDecision::Denied { reason } => {
                let mut text = format!(
                    "Denied {}'s request to be {}",
                    participant.display_name,
                    role_label(*requested_role).to_lowercase()
                );
                if let Some(reason) = reason.as_deref().filter(|reason| !reason.is_empty()) {
                    text.push_str(": ");
                    text.push_str(reason);
                }
                text
            }
            loom_core::AccessRequestDecision::Withdrawn => {
                format!("{} withdrew their request", participant.display_name)
            }
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
    fn controls_route_through_the_ordered_gate_or_the_legacy_request() {
        let active = run(AgentRunState::Paused);
        let current = revision(&active, 4);
        let tool_call_id = loom_core::ToolCallId::new();
        let approve = RunControlAction::Approve { tool_call_id };
        assert_eq!(
            route_control(true, &active, Some(current), approve.clone()),
            ControlRoute::Ordered(CollaborationRequest::ControlRun {
                run_id: active.id,
                expected: current,
                action: approve.clone(),
            })
        );
        assert_eq!(
            route_control(true, &active, None, RunControlAction::Resume),
            ControlRoute::NeedRevision { run_id: active.id }
        );
        let stale = RunRevision {
            attempt_id: RunAttemptId::new(),
            ..current
        };
        assert_eq!(
            route_control(true, &active, Some(stale), RunControlAction::Pause),
            ControlRoute::NeedRevision { run_id: active.id }
        );
        // An older worker keeps the run requests.
        assert_eq!(
            route_control(false, &active, Some(current), approve),
            ControlRoute::Legacy(RunRequest::ApproveAgentAction {
                run_id: active.id,
                attempt_id: active.attempt_id,
                expected_control_revision: active.control_revision,
                tool_call_id,
            })
        );
        let checkpoint_id = loom_core::CheckpointId::new();
        for (action, expected) in [
            (
                RunControlAction::Reject {
                    tool_call_id,
                    reason: None,
                },
                RunRequest::RejectAgentAction {
                    run_id: active.id,
                    attempt_id: active.attempt_id,
                    expected_control_revision: active.control_revision,
                    tool_call_id,
                    reason: None,
                },
            ),
            (
                RunControlAction::Pause,
                RunRequest::PauseAgentRun { run_id: active.id },
            ),
            (
                RunControlAction::Resume,
                RunRequest::ResumeAgentRun { run_id: active.id },
            ),
            (
                RunControlAction::Interrupt,
                RunRequest::InterruptAgentRun { run_id: active.id },
            ),
            (
                RunControlAction::Retry,
                RunRequest::RetryAgentStep { run_id: active.id },
            ),
            (
                RunControlAction::RetryFromCheckpoint { checkpoint_id },
                RunRequest::RetryAgentFromCheckpoint {
                    run_id: active.id,
                    checkpoint_id,
                },
            ),
        ] {
            assert_eq!(legacy_control_request(&active, action), expected);
        }
    }

    #[test]
    fn ordered_path_needs_worker_support_not_a_bound_person() {
        let mut state = CollaborationState::default();
        assert!(!state.uses_ordered_path());
        state.support = CollaborationSupport::Supported;
        assert!(!state.is_participant_bound());
        assert!(state.uses_ordered_path(), "operators take the ordered path");
        state.support = CollaborationSupport::Legacy;
        assert!(!state.uses_ordered_path());
        assert!(is_unsupported_error(&LoomError::new(
            ErrorCode::UnsupportedCapability,
            "x",
            false
        )));
        assert!(!is_unsupported_error(&LoomError::invalid_state("x")));
    }

    #[test]
    fn control_conflict_keeps_no_draft_and_names_the_action() {
        let mut state = CollaborationState::default();
        let active = run(AgentRunState::Executing);
        let expected = revision(&active, 1);
        let current = revision(&active, 2);
        state.record_control_conflict(active.id, expected, current, RunControlAction::Pause);
        assert_eq!(state.restore_draft, None);
        assert_eq!(state.revision_for(&active), Some(current));
        let notice = state.conflict.as_ref().unwrap();
        assert!(notice.message().contains("Pause was not applied"));
        assert!(notice.heading().starts_with("Control conflict"));
    }

    #[test]
    fn entries_are_observed_once_until_forgotten() {
        let mut state = CollaborationState::default();
        let entry = CollaborationEntry {
            session_id: AgentSessionId::new(),
            run_id: None,
            author: Author::Operator,
            recipient: Recipient::Agent,
            kind: CollaborationEntryKind::AgentDirection {
                text: "go".to_owned(),
            },
            revision: None,
            created_at: Timestamp::now(),
        };
        assert!(state.observe_entry(&entry));
        assert!(!state.observe_entry(&entry));
        assert_eq!(state.message_authors.len(), 1);
        state.forget_entries();
        assert!(state.observe_entry(&entry));
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

    fn access_request(
        session_id: AgentSessionId,
        who: &ParticipantIdentity,
        role: SessionRole,
    ) -> SessionAccessRequest {
        SessionAccessRequest {
            id: AccessRequestId::new(),
            session_id,
            participant: who.clone(),
            requested_role: role,
            note: None,
            created_at: Timestamp::now(),
        }
    }

    #[test]
    fn requester_state_moves_from_pending_to_outcome() {
        let session = AgentSessionId::new();
        let me = person("Nadia");
        let mut state = CollaborationState {
            identity: Some(me.clone()),
            support: CollaborationSupport::Supported,
            ..CollaborationState::default()
        };
        assert!(state.can_request_access(), "no grant counts as observer");
        assert_eq!(state.access.draft_role(), SessionRole::Participant);
        let request = access_request(session, &me, SessionRole::Participant);
        state.apply_access_requested(session, &request);
        assert_eq!(state.access.own.as_ref(), Some(&request));
        assert!(state.access.pending.is_empty(), "observers see no list");
        // A request for another session is ignored.
        state.apply_access_resolved(
            AgentSessionId::new(),
            &request,
            &AccessRequestDecision::Withdrawn,
        );
        assert!(state.access.own.is_some());
        let approved = AccessRequestDecision::Approved {
            role: SessionRole::Participant,
        };
        state.apply_access_resolved(session, &request, &approved);
        assert!(state.access.own.is_none());
        assert_eq!(state.access.outcome.as_ref(), Some(&approved));
        assert_eq!(
            access_outcome_text(&approved),
            "Approved: you are now participant."
        );
        // The role then comes from the roster.
        state.replace_roster(session, vec![entry(&me, SessionRole::Participant)]);
        assert!(!state.can_request_access());

        // A withdrawal clears the pending request without an outcome.
        state.access.record_own(request.clone());
        state.apply_access_resolved(session, &request, &AccessRequestDecision::Withdrawn);
        assert!(state.access.own.is_none() && state.access.outcome.is_none());
        let denied = AccessRequestDecision::Denied {
            reason: Some("not now".to_owned()),
        };
        assert_eq!(access_outcome_text(&denied), "Denied: not now");
    }

    #[test]
    fn legacy_workers_hide_access_request_controls() {
        let me = person("Nadia");
        let mut state = CollaborationState {
            identity: Some(me),
            support: CollaborationSupport::Legacy,
            ..CollaborationState::default()
        };
        assert!(!state.can_request_access());
        state.support = CollaborationSupport::Supported;
        state.access.unsupported = true;
        assert!(!state.can_request_access());
        assert!(!state.manages_access_requests());
        state.identity = None;
        state.access.unsupported = false;
        assert!(!state.can_request_access(), "operators do not request");
    }

    #[test]
    fn owner_list_follows_events_without_duplicates() {
        let session = AgentSessionId::new();
        let owner = person("Björn");
        let nadia = person("Nadia");
        let mut state = CollaborationState {
            identity: Some(owner.clone()),
            support: CollaborationSupport::Supported,
            ..CollaborationState::default()
        };
        state.replace_roster(session, vec![entry(&owner, SessionRole::Owner)]);
        assert!(state.manages_access_requests());
        let request = access_request(session, &nadia, SessionRole::Participant);
        state.access.replace_pending(vec![request.clone()]);
        state.apply_access_requested(session, &request);
        assert_eq!(state.access.pending.len(), 1, "deduplicated by id");
        assert_eq!(
            state.access.approve_role(&request),
            SessionRole::Participant
        );
        let replaced = SessionAccessRequest {
            requested_role: SessionRole::Owner,
            ..request.clone()
        };
        state.apply_access_requested(session, &replaced);
        assert_eq!(state.access.pending, vec![replaced.clone()]);
        assert_eq!(state.access.approve_role(&replaced), SessionRole::Owner);
        state
            .access
            .set_approve_role(replaced.id, SessionRole::Observer);
        assert_eq!(state.access.approve_role(&replaced), SessionRole::Observer);
        state.apply_access_resolved(
            session,
            &replaced,
            &AccessRequestDecision::Denied { reason: None },
        );
        assert!(state.access.pending.is_empty());
        assert!(state.access.approve_roles.is_empty());
        assert!(state.access.outcome.is_none(), "not the owner's request");
    }

    #[test]
    fn access_request_notes_are_trimmed_and_bounded() {
        assert_eq!(access_request_note("  "), Ok(None));
        assert_eq!(access_request_note(" hi "), Ok(Some("hi".to_owned())));
        let long = "x".repeat(MAX_ACCESS_REQUEST_NOTE_BYTES + 1);
        assert!(access_request_note(&long).is_err());
        let max = "x".repeat(MAX_ACCESS_REQUEST_NOTE_BYTES);
        assert!(access_request_note(&max).is_ok());
    }

    #[test]
    fn access_request_decisions_have_entry_text() {
        let nadia = person("Nadia");
        let decided = |decision| CollaborationEntry {
            session_id: AgentSessionId::new(),
            run_id: None,
            author: Author::Operator,
            recipient: Recipient::People,
            kind: CollaborationEntryKind::AccessRequestDecided {
                request_id: AccessRequestId::new(),
                participant: nadia.clone(),
                requested_role: SessionRole::Owner,
                decision,
            },
            revision: None,
            created_at: Timestamp::now(),
        };
        assert_eq!(
            entry_text(&decided(AccessRequestDecision::Approved {
                role: SessionRole::Participant
            })),
            "Approved Nadia as participant"
        );
        assert_eq!(
            entry_text(&decided(AccessRequestDecision::Denied {
                reason: Some("later".to_owned())
            })),
            "Denied Nadia's request to be owner: later"
        );
        assert_eq!(
            entry_text(&decided(AccessRequestDecision::Withdrawn)),
            "Nadia withdrew their request"
        );
    }
}
