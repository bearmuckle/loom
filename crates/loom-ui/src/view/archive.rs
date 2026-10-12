//! The archived-session management surface.
//!
//! It lists the archived sessions of the workspaces this client already knows,
//! grouped by the project the backend projects for them. Nothing here guesses a
//! hierarchy: the grouping uses `GetProjectSnapshotForSession`, the same
//! authoritative projection the sidebar uses, and the retention card edits the
//! policy the backend reports.

use super::*;
use loom_core::Capability;
use loom_protocol::ArchiveRetentionPolicy;

/// How many characters of a session id stand in for a project label when the
/// project root is not itself among the archived sessions.
const SHORT_SESSION_ID_CHARS: usize = 8;

/// The heading for archived sessions that belong to no project.
const NO_PROJECT_LABEL: &str = "No project";

/// An archived session and the worker node that owns it. The delete request is
/// routed to that node rather than to the client's active backend.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ArchivedSessionEntry {
    pub(crate) node_id: String,
    pub(crate) session: AgentSessionSnapshot,
}

/// One archived session row: its rendered archive age, and — for a project root
/// — how many descendant sessions a delete cascades to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ArchivedSessionRow {
    pub(crate) node_id: String,
    pub(crate) session: AgentSessionSnapshot,
    /// `Archived 3d ago`, derived from the session's archive timestamp.
    pub(crate) age: String,
    pub(crate) is_project_root: bool,
    pub(crate) descendant_count: usize,
}

/// Archived sessions grouped by project, with an explicit group for sessions
/// that belong to no project.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ArchivedSessionGroup {
    /// The project root, or `None` for the no-project group.
    pub(crate) project_root: Option<AgentSessionId>,
    pub(crate) label: String,
    pub(crate) rows: Vec<ArchivedSessionRow>,
}

/// The archive-retention policy the surface can render.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum RetentionPolicyState {
    /// Not asked yet, or a refresh in flight.
    #[default]
    Loading,
    Available(loom_protocol::ArchiveRetentionPolicy),
    /// The backend does not report a policy (an older or in-memory backend).
    /// It renders as unavailable instead of blocking the surface.
    Unavailable,
}

/// The read-only retention text: a headline plus one supporting line.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RetentionPolicyText {
    pub(crate) headline: String,
    pub(crate) detail: String,
}

impl RetentionPolicyText {
    /// Describes the policy without offering any way to change it.
    pub(crate) fn of(state: &RetentionPolicyState) -> Self {
        match state {
            RetentionPolicyState::Loading => Self {
                headline: "Reading the retention policy…".to_owned(),
                detail: String::new(),
            },
            RetentionPolicyState::Unavailable => Self {
                headline: "Unknown on this worker".to_owned(),
                detail: "This worker does not report an archive-retention policy, so whether old archived sessions are deleted automatically is unknown. Archived sessions stay until someone deletes them.".to_owned(),
            },
            RetentionPolicyState::Available(policy) if !policy.is_enabled() => Self {
                headline: "Automatic deletion is off".to_owned(),
                detail: "Archived sessions stay until someone deletes them. No sweep runs, so no worktree is discarded automatically.".to_owned(),
            },
            RetentionPolicyState::Available(policy) => {
                let window = policy
                    .retention()
                    .map(retention_window_label)
                    .unwrap_or_else(|| "an unknown window".to_owned());
                Self {
                    headline: format!("Automatic deletion after {window}"),
                    detail: if policy.force_discard_worktrees {
                        "The automatic sweep may discard dirty or locked worktrees.".to_owned()
                    } else {
                        "The automatic sweep skips worktrees that have changes or a lock."
                            .to_owned()
                    },
                }
            }
        }
    }
}

/// The retention windows the card's editor offers, "Off" first.
pub(crate) const RETENTION_WINDOW_PRESETS: [Option<Duration>; 6] = [
    None,
    Some(Duration::from_secs(86_400)),
    Some(Duration::from_secs(7 * 86_400)),
    Some(Duration::from_secs(14 * 86_400)),
    Some(Duration::from_secs(30 * 86_400)),
    Some(Duration::from_secs(90 * 86_400)),
];

/// The label of the choice that turns automatic deletion off.
const RETENTION_OFF_LABEL: &str = "Off";

/// What one window choice of the card means for the draft.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetentionWindowChoice {
    /// No automatic deletion.
    Off,
    /// A window the card offers as a preset.
    Window(Duration),
    /// The stored window no preset covers. Choosing it keeps that window.
    Custom,
}

/// Reads a choice label back to what it means for the draft.
pub(crate) fn retention_window_choice(label: &str) -> RetentionWindowChoice {
    if label == RETENTION_OFF_LABEL {
        return RetentionWindowChoice::Off;
    }
    RETENTION_WINDOW_PRESETS
        .iter()
        .flatten()
        .find(|window| retention_window_label(**window) == label)
        .map_or(RetentionWindowChoice::Custom, |window| {
            RetentionWindowChoice::Window(*window)
        })
}

/// The label of one window choice.
fn retention_window_choice_label(window: Option<Duration>) -> String {
    match window {
        None => RETENTION_OFF_LABEL.to_owned(),
        Some(window) => retention_window_label(window),
    }
}

/// The card's window choices and the one a policy selects.
///
/// A stored window that matches no preset is offered as its own
/// `Custom (…)` entry instead of being passed off as a preset, so it stays in
/// place until the user picks another value.
pub(crate) fn retention_window_choices(policy: ArchiveRetentionPolicy) -> (Vec<String>, String) {
    let mut choices = RETENTION_WINDOW_PRESETS
        .iter()
        .map(|window| retention_window_choice_label(*window))
        .collect::<Vec<_>>();
    let stored = policy.retention();
    match stored {
        Some(window) if !RETENTION_WINDOW_PRESETS.contains(&Some(window)) => {
            let label = format!("Custom ({})", retention_window_label(window));
            choices.push(label.clone());
            (choices, label)
        }
        _ => {
            let label = retention_window_choice_label(stored);
            (choices, label)
        }
    }
}

/// A human-readable retention window, largest unit first.
pub(crate) fn retention_window_label(window: Duration) -> String {
    let minutes = window.as_millis() as u64 / 60_000;
    if minutes == 0 {
        return "under a minute".to_owned();
    }
    let days = minutes / 1_440;
    let hours = (minutes % 1_440) / 60;
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days} {}", unit(days, "day", "days")));
    }
    if hours > 0 {
        parts.push(format!("{hours} {}", unit(hours, "hour", "hours")));
    }
    // Below a day the minutes carry the precision; above it they are noise.
    let minutes = minutes % 60;
    if days == 0 && minutes > 0 {
        parts.push(format!("{minutes} {}", unit(minutes, "minute", "minutes")));
    }
    parts.join(" ")
}

fn unit(value: u64, singular: &str, plural: &str) -> String {
    if value == 1 {
        singular.to_owned()
    } else {
        plural.to_owned()
    }
}

/// `Archived 3d ago` for a session's archive age.
///
/// Archiving is the last mutation an archived session can receive, so the
/// session's `updated_at` is its archive timestamp and no extra field is
/// needed to read the age.
pub(crate) fn archived_age_label(archived_at_millis: u64, now_millis: u64) -> String {
    format!("Archived {}", relative_time(archived_at_millis, now_millis))
}

fn short_session_id(session_id: AgentSessionId) -> String {
    session_id
        .to_string()
        .chars()
        .take(SHORT_SESSION_ID_CHARS)
        .collect()
}

/// Groups archived sessions by the project the backend projects for them.
///
/// A project's group holds its archived root first and then its archived
/// descendants in depth order, so the delete cascade reads as a tree. Sessions
/// that no known project covers go into one explicit trailing group.
pub(crate) fn archived_session_groups(
    entries: &[ArchivedSessionEntry],
    projects: &[loom_core::ProjectSnapshot],
    now_millis: u64,
) -> Vec<ArchivedSessionGroup> {
    let by_id = entries
        .iter()
        .map(|entry| (entry.session.id, entry))
        .collect::<BTreeMap<_, _>>();
    let mut grouped = BTreeSet::new();
    let mut groups = Vec::new();
    for project in projects {
        let mut members = project
            .agents
            .iter()
            .filter(|agent| by_id.contains_key(&agent.session_id))
            .filter(|agent| !grouped.contains(&agent.session_id))
            .map(|agent| (agent.depth, agent.session_id))
            .collect::<Vec<_>>();
        if members.is_empty() {
            continue;
        }
        members.sort_by_key(|(depth, session_id)| (*depth, *session_id));
        let rows = members
            .into_iter()
            .filter_map(|(_, session_id)| by_id.get(&session_id).copied())
            .collect::<Vec<_>>();
        if rows.is_empty() {
            continue;
        }
        let descendant_count = rows.len().saturating_sub(1);
        let label = rows
            .iter()
            .find(|entry| entry.session.id == project.root_session_id)
            .map(|entry| entry.session.name.clone())
            .unwrap_or_else(|| format!("Project {}", short_session_id(project.root_session_id)));
        groups.push(ArchivedSessionGroup {
            project_root: Some(project.root_session_id),
            label,
            rows: rows
                .into_iter()
                .map(|entry| {
                    let is_project_root = entry.session.id == project.root_session_id;
                    archived_session_row(entry, is_project_root, descendant_count, now_millis)
                })
                .collect(),
        });
        for agent in &project.agents {
            grouped.insert(agent.session_id);
        }
    }
    let mut loose = entries
        .iter()
        .filter(|entry| !grouped.contains(&entry.session.id))
        .collect::<Vec<_>>();
    // Newest archive first, with the name breaking ties so the order is stable.
    loose.sort_by_key(|entry| {
        (
            std::cmp::Reverse(entry.session.updated_at.as_unix_millis()),
            entry.session.name.clone(),
        )
    });
    if !loose.is_empty() {
        groups.push(ArchivedSessionGroup {
            project_root: None,
            label: NO_PROJECT_LABEL.to_owned(),
            rows: loose
                .into_iter()
                .map(|entry| archived_session_row(entry, false, 0, now_millis))
                .collect(),
        });
    }
    groups
}

fn archived_session_row(
    entry: &ArchivedSessionEntry,
    is_project_root: bool,
    descendant_count: usize,
    now_millis: u64,
) -> ArchivedSessionRow {
    ArchivedSessionRow {
        node_id: entry.node_id.clone(),
        session: entry.session.clone(),
        age: archived_age_label(entry.session.updated_at.as_unix_millis(), now_millis),
        is_project_root,
        descendant_count: if is_project_root { descendant_count } else { 0 },
    }
}

/// The delete request for one archived session. `force` allows the backend to
/// discard a worktree that has changes or a lock.
pub(crate) fn delete_archived_session_request(
    session_id: AgentSessionId,
    force: bool,
) -> ClientRequest {
    ClientRequest::Session(SessionRequest::DeleteAgentSession { session_id, force })
}

/// The archived-session surface state. `entries` and `projects` are the last
/// successful load; a refresh keeps them on screen while it runs.
#[derive(Clone, Debug, Default)]
pub(crate) struct ArchivedSessionsState {
    pub(crate) open: bool,
    pub(crate) loading: bool,
    pub(crate) loaded: bool,
    pub(crate) entries: Vec<ArchivedSessionEntry>,
    /// Project snapshots the entries group by. Only snapshots the client did
    /// not already have are requested.
    pub(crate) projects: Vec<loom_core::ProjectSnapshot>,
    pub(crate) retention: RetentionPolicyState,
    /// Whether the active backend advertises `ConfigureArchiveRetention`, so the
    /// card can store a change. A worker without durable storage does not.
    pub(crate) retention_editable: bool,
    /// The policy the card's controls edit. `Some` only while the card is an
    /// editor, so a read-only card holds no draft.
    pub(crate) retention_draft: Option<ArchiveRetentionPolicy>,
    /// A `SetArchiveRetentionPolicy` request in flight.
    pub(crate) retention_saving: bool,
    /// A save the worker confirmed, shown until the draft is edited again.
    pub(crate) retention_saved: bool,
    /// The last refused save and the backend's reason, which keeps the policy
    /// the worker reported.
    pub(crate) retention_refusal: Option<String>,
    /// A load failure, shown without hiding what did load.
    pub(crate) error: Option<String>,
    /// The last refused delete and the backend's reason.
    pub(crate) refusal: Option<String>,
    /// Rows whose delete may discard dirty or locked worktrees.
    pub(crate) force: BTreeSet<AgentSessionId>,
    /// The session with a delete request in flight, if any.
    pub(crate) delete_in_flight: Option<AgentSessionId>,
}

impl LoomView {
    /// Opens the archived-sessions surface and loads what it shows.
    pub(crate) fn open_archived_sessions(&mut self, cx: &mut Context<Self>) {
        self.archived_sessions.open = true;
        self.archived_sessions.refusal = None;
        self.reload_archived_sessions(cx);
    }

    pub(crate) fn close_archived_sessions(&mut self, cx: &mut Context<Self>) {
        self.archived_sessions.open = false;
        cx.notify();
    }

    /// Sets the force choice of one archived session row. With force the backend
    /// may discard a worktree that has changes or a lock.
    pub(crate) fn set_archived_session_force(
        &mut self,
        session_id: AgentSessionId,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        if force {
            self.archived_sessions.force.insert(session_id);
        } else {
            self.archived_sessions.force.remove(&session_id);
        }
        cx.notify();
    }

    /// Sets the draft's force-discard switch: whether the sweep may discard a
    /// worktree that has changes or a lock.
    pub(crate) fn set_archive_retention_force(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(draft) = self.archived_sessions.retention_draft.as_mut() else {
            return;
        };
        if draft.force_discard_worktrees == force {
            return;
        }
        draft.force_discard_worktrees = force;
        self.archived_sessions.retention_saved = false;
        cx.notify();
    }

    /// Applies a window choice of the card's editor to the draft.
    ///
    /// The custom entry keeps the window the worker stored, which is the only
    /// thing that entry can mean, so a stored window no preset covers survives
    /// until the user picks another value.
    pub(crate) fn choose_archive_retention_window(&mut self, choice: &str, cx: &mut Context<Self>) {
        let window = match retention_window_choice(choice) {
            RetentionWindowChoice::Off => None,
            RetentionWindowChoice::Window(window) => Some(window.as_millis() as u64),
            RetentionWindowChoice::Custom => return,
        };
        let Some(draft) = self.archived_sessions.retention_draft.as_mut() else {
            return;
        };
        if draft.retention_ms == window {
            return;
        }
        draft.retention_ms = window;
        self.archived_sessions.retention_saved = false;
        cx.notify();
    }

    /// Whether the draft differs from the policy the worker reported.
    pub(crate) fn archive_retention_draft_is_dirty(&self) -> bool {
        match (
            &self.archived_sessions.retention_draft,
            &self.archived_sessions.retention,
        ) {
            (Some(draft), RetentionPolicyState::Available(policy)) => draft != policy,
            _ => false,
        }
    }

    /// Writes the draft policy to the active backend, which stores it durably
    /// and answers with the policy it stored.
    pub(crate) fn save_archive_retention_policy(&mut self, cx: &mut Context<Self>) {
        if self.archived_sessions.retention_saving {
            log::info!("[loom-ui] ignoring the archive-retention change: a save is in flight");
            return;
        }
        let Some(policy) = self.archived_sessions.retention_draft else {
            return;
        };
        if !self.archive_retention_draft_is_dirty() {
            return;
        }
        self.archived_sessions.retention_saving = true;
        self.archived_sessions.retention_saved = false;
        self.archived_sessions.retention_refusal = None;
        log::info!("[loom-ui] storing the archive retention policy {policy:?}");
        self.dispatch(
            cx,
            ClientRequest::Control(ControlRequest::SetArchiveRetentionPolicy { policy }),
            |view, response, cx| view.finish_archive_retention_save(response, cx),
        );
        cx.notify();
    }

    /// Applies the answer to a retention-policy save.
    ///
    /// The card adopts the policy in the answer, so it shows exactly what the
    /// worker stored rather than what was asked for. A refusal shows the
    /// backend's reason and keeps the stored policy; the draft stays as the
    /// user left it, so the change can be retried.
    pub(crate) fn finish_archive_retention_save(
        &mut self,
        response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        self.archived_sessions.retention_saving = false;
        match response.result {
            Ok(ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(stored))) => {
                log::info!("[loom-ui] stored the archive retention policy {stored:?}");
                self.archived_sessions.retention = RetentionPolicyState::Available(stored);
                self.archived_sessions.retention_draft = Some(stored);
                self.archived_sessions.retention_saved = true;
                self.archived_sessions.retention_refusal = None;
            }
            Err(error) => {
                log::warn!(
                    "[loom-ui] storing the archive retention policy was refused: {:?}: {}",
                    error.code,
                    error.message
                );
                self.archived_sessions.retention_refusal = Some(error.message);
            }
            Ok(response) => {
                log::warn!(
                    "[loom-ui] storing the archive retention policy returned an unexpected response"
                );
                let error = unexpected_response("archive retention policy", response);
                self.archived_sessions.retention_refusal = Some(error.message);
            }
        }
        cx.notify();
    }

    /// The workspaces this client already knows, each with its owning worker.
    ///
    /// A workspace belongs to exactly one worker, so the session list is
    /// requested from that worker rather than from the active backend.
    fn archived_session_targets(&self) -> Vec<(String, WorkspaceId)> {
        let mut targets = Vec::new();
        for node_id in self.node_backends.keys() {
            let workspaces: &[WorkspaceRecord] = if *node_id == self.default_backend_node_id {
                &self.workspaces
            } else {
                self.node_workspaces
                    .get(node_id)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
            };
            for workspace in workspaces {
                if targets
                    .iter()
                    .any(|(_, workspace_id)| *workspace_id == workspace.id)
                {
                    continue;
                }
                targets.push((node_id.clone(), workspace.id));
            }
        }
        targets
    }

    /// Reloads the archived sessions of every known workspace, the project
    /// projections they group by, and the active retention policy.
    pub(crate) fn reload_archived_sessions(&mut self, cx: &mut Context<Self>) {
        let targets = self.archived_session_targets();
        let backends = self
            .node_backends
            .iter()
            .map(|(node_id, backend)| (node_id.clone(), backend.clone()))
            .collect::<BTreeMap<_, _>>();
        let retention_backend = self.backend.clone();
        // Project snapshots the client already has are never re-fetched: the
        // sidebar's cached trees and the surface's own load both count.
        let mut known_projects = self
            .project_tree_snapshots
            .iter()
            .cloned()
            .chain(self.project_snapshot.iter().cloned())
            .chain(self.archived_sessions.projects.iter().cloned())
            .collect::<Vec<_>>();
        self.archived_sessions.loading = true;
        self.archived_sessions.error = None;
        cx.notify();
        cx.spawn(async move |view, cx| {
            let mut entries = Vec::new();
            let mut errors = Vec::new();
            for (node_id, workspace_id) in targets {
                let Some(backend) = backends.get(&node_id) else {
                    continue;
                };
                match list_archived_sessions(backend, workspace_id).await {
                    Ok(sessions) => {
                        entries.extend(sessions.into_iter().map(|session| ArchivedSessionEntry {
                            node_id: node_id.clone(),
                            session,
                        }))
                    }
                    Err(error) => errors.push(error),
                }
            }
            // One project request covers a whole project tree, and the snapshot
            // answers for its descendants too, so it is requested once per
            // session the client does not already cover.
            let mut covered = known_projects
                .iter()
                .flat_map(|project| project.agents.iter().map(|agent| agent.session_id))
                .collect::<BTreeSet<_>>();
            for entry in &entries {
                if covered.contains(&entry.session.id) {
                    continue;
                }
                let Some(backend) = backends.get(&entry.node_id) else {
                    continue;
                };
                match project_snapshot_for_session(backend, entry.session.id).await {
                    Ok(snapshot) => {
                        covered.extend(snapshot.agents.iter().map(|agent| agent.session_id));
                        known_projects.push(snapshot);
                    }
                    Err(error) => errors.push(error),
                }
            }
            let retention = retention_policy(&retention_backend).await;
            // The card is an editor only when the worker can store a change, so
            // a worker without durable storage keeps the read-only card.
            let retention_editable = archive_retention_editable(&retention_backend).await;
            view.update(cx, |view, cx| {
                view.finish_archived_sessions_load(
                    entries,
                    known_projects,
                    retention,
                    retention_editable,
                    errors,
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    /// Applies a completed archived-session load.
    pub(crate) fn finish_archived_sessions_load(
        &mut self,
        entries: Vec<ArchivedSessionEntry>,
        projects: Vec<loom_core::ProjectSnapshot>,
        retention: RetentionPolicyState,
        retention_editable: bool,
        errors: Vec<LoomError>,
        cx: &mut Context<Self>,
    ) {
        self.archived_sessions.loading = false;
        self.archived_sessions.loaded = true;
        let mut seen_sessions = BTreeSet::new();
        self.archived_sessions.entries = entries
            .into_iter()
            .filter(|entry| seen_sessions.insert(entry.session.id))
            .collect();
        let mut seen_projects = BTreeSet::new();
        self.archived_sessions.projects = projects
            .into_iter()
            .filter(|project| seen_projects.insert(project.project_id))
            .collect();
        self.archived_sessions.retention = retention;
        // The editor starts from the policy the worker reports and is offered
        // only when the worker can store a change. A load discards an unapplied
        // draft, so the card always shows the policy in effect.
        let stored = match &self.archived_sessions.retention {
            RetentionPolicyState::Available(policy) => Some(*policy),
            RetentionPolicyState::Loading | RetentionPolicyState::Unavailable => None,
        };
        let editable = retention_editable && stored.is_some();
        self.archived_sessions.retention_editable = editable;
        self.archived_sessions.retention_draft = if editable { stored } else { None };
        self.archived_sessions.retention_saving = false;
        self.archived_sessions.retention_saved = false;
        self.archived_sessions.retention_refusal = None;
        self.archived_sessions.force.retain(|session_id| {
            self.archived_sessions
                .entries
                .iter()
                .any(|entry| entry.session.id == *session_id)
        });
        self.archived_sessions.error = errors.first().map(|error| error.message.clone());
        if let Some(error) = errors.first() {
            log::warn!(
                "[loom-ui] loading archived sessions reported {} failure(s); first: {:?}: {}",
                errors.len(),
                error.code,
                error.message
            );
        }
        log::debug!(
            "[loom-ui] loaded {} archived session(s) across {} project snapshot(s)",
            self.archived_sessions.entries.len(),
            self.archived_sessions.projects.len()
        );
        cx.notify();
    }

    /// The node and request a delete of `session_id` would use, or `None` when
    /// the session is not part of the loaded list.
    pub(crate) fn archived_session_delete_request(
        &self,
        session_id: AgentSessionId,
    ) -> Option<(String, ClientRequest)> {
        let entry = self
            .archived_sessions
            .entries
            .iter()
            .find(|entry| entry.session.id == session_id)?;
        let force = self.archived_sessions.force.contains(&session_id);
        Some((
            entry.node_id.clone(),
            delete_archived_session_request(session_id, force),
        ))
    }

    /// Deletes one archived session, carrying the row's force choice. The
    /// request goes to the worker that owns the session.
    pub(crate) fn delete_archived_session(
        &mut self,
        session_id: AgentSessionId,
        cx: &mut Context<Self>,
    ) {
        if self.archived_sessions.delete_in_flight.is_some() {
            log::info!(
                "[loom-ui] ignoring the delete of archived session {session_id}: another delete is already in flight"
            );
            return;
        }
        let Some((node_id, request)) = self.archived_session_delete_request(session_id) else {
            log::warn!(
                "[loom-ui] ignoring the delete of {session_id}: it is not in the archived-session list"
            );
            return;
        };
        let force = self.archived_sessions.force.contains(&session_id);
        self.archived_sessions.delete_in_flight = Some(session_id);
        self.archived_sessions.refusal = None;
        log::info!(
            "[loom-ui] deleting archived session {session_id} on worker {node_id} (force={force})"
        );
        self.dispatch_to_node(cx, node_id, request, move |view, response, cx| {
            view.finish_archived_session_delete(session_id, response, cx);
        });
        cx.notify();
    }

    /// Applies the answer to a delete request.
    ///
    /// A refusal keeps the row and shows the backend's reason in the surface;
    /// success drops the session and, for a project root, every descendant the
    /// backend cascaded.
    pub(crate) fn finish_archived_session_delete(
        &mut self,
        session_id: AgentSessionId,
        response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        self.archived_sessions.delete_in_flight = None;
        match response.result {
            Ok(ServerResponse::Session(SessionResponse::AgentSessionDeleted {
                session_id: deleted,
            })) => {
                if deleted != session_id {
                    log::warn!(
                        "[loom-ui] the delete of {session_id} reported session {deleted} as deleted"
                    );
                }
                log::info!("[loom-ui] deleted archived session {deleted}");
                self.archived_sessions.refusal = None;
                let was_active = self.forget_archived_session(deleted);
                if was_active {
                    // The session on screen no longer exists, so move the
                    // selection instead of leaving a deleted session active.
                    if let Some(session) = self.sessions.first().cloned() {
                        self.select_session(session, cx);
                    } else {
                        self.activate_session(empty_session_snapshot(self.workspace_id));
                        self.review.open = false;
                    }
                }
                self.record_status("Archived session deleted");
                self.reload_archived_sessions(cx);
                cx.notify();
            }
            Err(error) => {
                log::warn!(
                    "[loom-ui] deleting archived session {session_id} was refused: {:?}: {}",
                    error.code,
                    error.message
                );
                self.archived_sessions.refusal = Some(error.message);
                cx.notify();
            }
            Ok(response) => {
                log::warn!(
                    "[loom-ui] deleting archived session {session_id} returned an unexpected response"
                );
                let error = unexpected_response("archived session deletion", response);
                self.archived_sessions.refusal = Some(error.message);
                cx.notify();
            }
        }
    }
}

/// Lists the archived sessions of one workspace on one worker.
async fn list_archived_sessions(
    backend: &BackendWorker,
    workspace_id: WorkspaceId,
) -> Result<Vec<AgentSessionSnapshot>, LoomError> {
    let response = backend
        .submit(RequestEnvelope::new(ClientRequest::Workspace(
            WorkspaceRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived: true,
            },
        )))
        .wait()
        .await;
    match response.result? {
        ServerResponse::Session(SessionResponse::AgentSessions { sessions }) => Ok(sessions
            .into_iter()
            .filter(|session| session.state == AgentSessionState::Archived)
            .collect()),
        response => Err(unexpected_response("archived session list", response)),
    }
}

/// The authoritative project hierarchy a session belongs to.
async fn project_snapshot_for_session(
    backend: &BackendWorker,
    session_id: AgentSessionId,
) -> Result<loom_core::ProjectSnapshot, LoomError> {
    let response = backend
        .submit(RequestEnvelope::new(ClientRequest::Project(
            ProjectRequest::GetProjectSnapshotForSession { session_id },
        )))
        .wait()
        .await;
    match response.result? {
        ServerResponse::Project(ProjectResponse::ProjectSnapshot(snapshot)) => Ok(snapshot),
        response => Err(unexpected_response("project snapshot", response)),
    }
}

/// The active archive-retention policy.
///
/// A backend that does not report one (an older worker, or one that has no
/// durable storage) reads as unavailable instead of failing the surface.
async fn retention_policy(backend: &BackendWorker) -> RetentionPolicyState {
    let response = backend
        .submit(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::GetArchiveRetentionPolicy,
        )))
        .wait()
        .await;
    retention_policy_state(response.result)
}

/// Maps a retention-policy answer to what the surface renders.
///
/// Only the policy response yields a policy; every other answer, including a
/// backend that does not support the request at all, reads as unavailable.
pub(crate) fn retention_policy_state(
    result: Result<ServerResponse, LoomError>,
) -> RetentionPolicyState {
    match result {
        Ok(ServerResponse::Control(ControlResponse::ArchiveRetentionPolicy(policy))) => {
            RetentionPolicyState::Available(policy)
        }
        Ok(response) => {
            log::debug!(
                "[loom-ui] the archive-retention request returned an unexpected response: {response:?}"
            );
            RetentionPolicyState::Unavailable
        }
        Err(error) => {
            log::debug!(
                "[loom-ui] the archive-retention request failed: {:?}: {}; the policy is unavailable",
                error.code,
                error.message
            );
            RetentionPolicyState::Unavailable
        }
    }
}

/// Whether the active backend can store a changed archive-retention policy.
async fn archive_retention_editable(backend: &BackendWorker) -> bool {
    let response = backend
        .submit(RequestEnvelope::new(ClientRequest::Control(
            ControlRequest::GetWorkerNodeStatus,
        )))
        .wait()
        .await;
    retention_editable_state(response.result)
}

/// Maps a worker-node-status answer to whether the card can offer an editor.
///
/// The capability is what a worker with durable storage advertises, so a worker
/// without one stays read-only instead of offering a control that its answer to
/// every save would refuse.
pub(crate) fn retention_editable_state(result: Result<ServerResponse, LoomError>) -> bool {
    match result {
        Ok(ServerResponse::Control(ControlResponse::WorkerNodeStatus(status))) => status
            .capabilities
            .contains(Capability::ConfigureArchiveRetention),
        Ok(response) => {
            log::debug!(
                "[loom-ui] the worker-node-status request returned an unexpected response: {response:?}; the retention policy stays read-only"
            );
            false
        }
        Err(error) => {
            log::debug!(
                "[loom-ui] the worker-node-status request failed: {:?}: {}; the retention policy stays read-only",
                error.code,
                error.message
            );
            false
        }
    }
}
