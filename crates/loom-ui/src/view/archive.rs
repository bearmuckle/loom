//! The archived-session management surface.
//!
//! It lists the archived sessions of the workspaces this client already knows as
//! one row per project, because a project's member sessions are deleted with it,
//! plus a row for every archived session no project covers. Nothing here guesses
//! a hierarchy: the grouping uses `GetProjectSnapshotForSession`, the same
//! authoritative projection the sidebar uses, and the retention card edits the
//! policy the backend reports. Deleting a whole project goes through a
//! confirmation the surface and the project context menu share.

use super::*;
use loom_core::Capability;
use loom_protocol::ArchiveRetentionPolicy;

/// How many characters of a session id stand in for a project label when neither
/// the archived entries nor the client's session list name the project root.
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

/// One archived-session row.
///
/// A project contributes exactly one row, which carries the whole tree: the
/// member sessions are deleted with it and are never listed on their own. A
/// session no project covers is a plain row that deletes itself.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ArchivedSessionRow {
    /// The session a deletion targets: the project root, or the session itself.
    pub(crate) session_id: AgentSessionId,
    /// The heading: the project root's name, or the session's name.
    pub(crate) label: String,
    /// The worker the deletion is routed to.
    pub(crate) node_id: String,
    /// `Archived 3d ago`, from the newest archived member's archive timestamp.
    pub(crate) age: String,
    /// How many sessions a deletion removes: the whole tree for a project row,
    /// one for a plain row.
    pub(crate) session_count: usize,
    /// Whether the row deletes a whole project.
    pub(crate) is_project: bool,
}

/// Archived sessions grouped by project, with an explicit group for sessions
/// that belong to no project.
///
/// A project group holds exactly one row: the project. The group label repeats
/// the row's heading so the group reads as the tree it stands for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ArchivedSessionGroup {
    /// The project root, or `None` for the no-project group.
    pub(crate) project_root: Option<AgentSessionId>,
    pub(crate) label: String,
    pub(crate) rows: Vec<ArchivedSessionRow>,
}

impl ArchivedSessionGroup {
    /// The trailing detail of the group heading.
    pub(crate) fn heading_detail(&self) -> String {
        match self.rows.as_slice() {
            [row] if row.is_project => format!(
                "{} in the tree",
                match row.session_count {
                    1 => "1 session".to_owned(),
                    count => format!("{count} sessions"),
                }
            ),
            rows => format!(
                "{} archived",
                match rows.len() {
                    1 => "1 session".to_owned(),
                    count => format!("{count} sessions"),
                }
            ),
        }
    }
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

/// The windows the editor offers for archived sub-agent sessions, "Off" first.
pub(crate) const SUB_AGENT_RETENTION_WINDOW_PRESETS: [Option<Duration>; 7] = [
    None,
    Some(Duration::from_secs(60)),
    Some(Duration::from_secs(5 * 60)),
    Some(Duration::from_secs(10 * 60)),
    Some(Duration::from_secs(30 * 60)),
    Some(Duration::from_secs(3_600)),
    Some(Duration::from_secs(86_400)),
];

/// The sub-agent window choices and the one a policy selects; a stored window
/// no preset covers is offered as a `Custom (…)` entry.
pub(crate) fn sub_agent_retention_window_choices(
    policy: ArchiveRetentionPolicy,
) -> (Vec<String>, String) {
    let mut choices = SUB_AGENT_RETENTION_WINDOW_PRESETS
        .iter()
        .map(|window| retention_window_choice_label(*window))
        .collect::<Vec<_>>();
    let stored = policy.sub_agent_retention();
    match stored {
        Some(window) if !SUB_AGENT_RETENTION_WINDOW_PRESETS.contains(&Some(window)) => {
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

/// Reads a sub-agent choice label back to what it means for the draft.
pub(crate) fn sub_agent_retention_window_choice(label: &str) -> RetentionWindowChoice {
    if label == RETENTION_OFF_LABEL {
        return RetentionWindowChoice::Off;
    }
    SUB_AGENT_RETENTION_WINDOW_PRESETS
        .iter()
        .flatten()
        .find(|window| retention_window_label(**window) == label)
        .map_or(RetentionWindowChoice::Custom, |window| {
            RetentionWindowChoice::Window(*window)
        })
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

/// Whether a project snapshot describes a real project.
///
/// `GetProjectSnapshotForSession` answers with a one-member snapshot for an
/// ordinary session, so only a snapshot that owns another member or a delegated
/// task is a project: anything else is the session on its own.
pub(crate) fn is_project_tree(project: &loom_core::ProjectSnapshot) -> bool {
    project.agents.len() > 1 || !project.tasks.is_empty()
}

/// The worker that owns a project tree.
///
/// The routing follows what the surface holds: the root's own archived entry
/// when it is archived, otherwise the shallowest archived member, which lives on
/// the worker the whole tree is on.
pub(crate) fn project_tree_node(
    root_session_id: AgentSessionId,
    project: &loom_core::ProjectSnapshot,
    entries: &[ArchivedSessionEntry],
) -> Option<String> {
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.session.id == root_session_id)
    {
        return Some(entry.node_id.clone());
    }
    let mut members = project
        .agents
        .iter()
        .filter(|agent| agent.session_id != root_session_id)
        .filter_map(|agent| {
            entries
                .iter()
                .find(|entry| entry.session.id == agent.session_id)
                .map(|entry| (agent.depth, agent.session_id, entry.node_id.clone()))
        })
        .collect::<Vec<_>>();
    members.sort();
    members.first().map(|(_, _, node_id)| node_id.clone())
}

/// What a confirmed project deletion stops, phrased for the confirmation
/// dialog.
pub(crate) const PROJECT_DELETE_CANCELLATION: &str =
    "A running project manager and any running sub-agents are cancelled.";

/// What a project row's deletion removes, phrased for the row.
///
/// It names the whole tree rather than only the archived members, because the
/// deletion cascades to every session the project holds.
pub(crate) fn project_row_consequence(session_count: usize) -> String {
    let sub_agents = session_count.saturating_sub(1);
    match sub_agents {
        0 => "Deletes this project and its stored history.".to_owned(),
        1 => {
            "Deletes this project, 1 sub-agent session, and their history and worktrees (2 sessions in the tree)."
                .to_owned()
        }
        count => format!(
            "Deletes this project, {count} sub-agent sessions, and their history and worktrees ({} sessions in the tree).",
            session_count
        ),
    }
}

/// What a confirmed project deletion removes, phrased for the confirmation
/// dialog that names the project.
pub(crate) fn project_deletion_consequence(name: &str, session_count: usize) -> String {
    let sub_agents = session_count.saturating_sub(1);
    match sub_agents {
        0 => format!(
            "{name} is permanently removed with its stored history. It has no sub-agent sessions."
        ),
        1 => format!(
            "{name} is permanently removed with 1 sub-agent session, its history and its worktrees."
        ),
        count => format!(
            "{name} is permanently removed with its {count} sub-agent sessions, their history and their worktrees."
        ),
    }
}

/// The most complete projection of each project, in the order each project is
/// first seen.
///
/// One project can be projected more than once: the client's cached tree, the
/// sidebar's active snapshot, and the pane's own load all describe it. A cached
/// tree may have been captured before a member was delegated, and an archived
/// project's tree is never refreshed, while the projection a load fetches last
/// is the authoritative one. The projection naming the most agents therefore
/// decides the row, and every member any other projection names is kept, so no
/// sub-agent is ever grouped as a project of its own or listed as a plain row.
///
/// The kept member list is a union, so it can name a session that has since been
/// deleted: a row's `session_count` and its deletion wording are therefore an
/// upper bound. The worker re-reads the real tree when the deletion is
/// submitted, so this never widens what a deletion removes.
pub(crate) fn most_complete_snapshots(
    projects: &[loom_core::ProjectSnapshot],
) -> Vec<loom_core::ProjectSnapshot> {
    let mut kept: Vec<loom_core::ProjectSnapshot> = Vec::new();
    for project in projects {
        match kept
            .iter_mut()
            .find(|entry| entry.project_id == project.project_id)
        {
            Some(entry) if project.agents.len() > entry.agents.len() => {
                let mut replacement = project.clone();
                keep_members_named_by(&mut replacement, entry);
                *entry = replacement;
            }
            Some(entry) => keep_members_named_by(entry, project),
            None => kept.push(project.clone()),
        }
    }
    kept
}

/// Adds every agent `other` names that `snapshot` does not, in a deterministic
/// order.
fn keep_members_named_by(
    snapshot: &mut loom_core::ProjectSnapshot,
    other: &loom_core::ProjectSnapshot,
) {
    let missing = other
        .agents
        .iter()
        .filter(|agent| {
            !snapshot
                .agents
                .iter()
                .any(|kept| kept.session_id == agent.session_id)
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return;
    }
    snapshot.agents.extend(missing.into_iter().cloned());
    snapshot
        .agents
        .sort_by_key(|agent| (agent.depth, agent.session_id));
}

/// Groups archived sessions by the project the backend projects for them.
///
/// A project the client can resolve that covers at least one archived session
/// contributes exactly one row: its members are deleted with it and are never
/// listed on their own. A session that only covers itself is not a project, and
/// it joins the sessions no project covers in one explicit trailing group.
///
/// A session any known snapshot names as a project member is hidden, even when
/// the snapshot kept for its project does not name it: the pane never shows a
/// sub-agent as a plain row.
pub(crate) fn archived_session_groups(
    entries: &[ArchivedSessionEntry],
    projects: &[loom_core::ProjectSnapshot],
    session_names: &BTreeMap<AgentSessionId, String>,
    now_millis: u64,
) -> Vec<ArchivedSessionGroup> {
    let by_id = entries
        .iter()
        .map(|entry| (entry.session.id, entry))
        .collect::<BTreeMap<_, _>>();
    // Sub-agent sessions (project members below the root) are never listed:
    // they are removed with their project or by the sub-agent retention sweep.
    let sub_agents = projects
        .iter()
        .flat_map(|project| {
            project
                .agents
                .iter()
                .filter(|agent| agent.session_id != project.root_session_id || agent.depth > 1)
                .map(|agent| agent.session_id)
        })
        .collect::<BTreeSet<_>>();
    let mut grouped = BTreeSet::new();
    let mut groups = Vec::new();
    for project in most_complete_snapshots(projects)
        .iter()
        .filter(|project| is_project_tree(project))
    {
        // Only an archived project root yields a row.
        let mut members = project
            .agents
            .iter()
            .filter(|agent| agent.session_id == project.root_session_id)
            .filter(|agent| !grouped.contains(&agent.session_id))
            .filter_map(|agent| by_id.get(&agent.session_id).copied())
            .collect::<Vec<_>>();
        if members.is_empty() {
            continue;
        }
        // The newest archived member dates the whole row, so a row whose root
        // was archived long ago still reads as the work the tree holds.
        members.sort_by_key(|entry| (entry.session.updated_at, entry.session.id));
        let Some(newest) = members.last() else {
            continue;
        };
        let Some(node_id) = project_tree_node(project.root_session_id, project, entries) else {
            log::debug!(
                "[loom-ui] project {} has archived sessions but no worker to route a deletion to",
                project.project_id
            );
            continue;
        };
        let label = members
            .iter()
            .find(|entry| entry.session.id == project.root_session_id)
            .map(|entry| entry.session.name.clone())
            .or_else(|| session_names.get(&project.root_session_id).cloned())
            .unwrap_or_else(|| format!("Project {}", short_session_id(project.root_session_id)));
        groups.push(ArchivedSessionGroup {
            project_root: Some(project.root_session_id),
            label: label.clone(),
            rows: vec![ArchivedSessionRow {
                session_id: project.root_session_id,
                label,
                node_id,
                age: archived_age_label(newest.session.updated_at.as_unix_millis(), now_millis),
                session_count: project.agents.len(),
                is_project: true,
            }],
        });
        for agent in &project.agents {
            grouped.insert(agent.session_id);
        }
    }
    let mut loose = entries
        .iter()
        .filter(|entry| !grouped.contains(&entry.session.id))
        .filter(|entry| !sub_agents.contains(&entry.session.id))
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
                .map(|entry| ArchivedSessionRow {
                    session_id: entry.session.id,
                    label: entry.session.name.clone(),
                    node_id: entry.node_id.clone(),
                    age: archived_age_label(entry.session.updated_at.as_unix_millis(), now_millis),
                    session_count: 1,
                    is_project: false,
                })
                .collect(),
        });
    }
    groups
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
///
/// Whether the surface is shown is not part of this state: it is the view's
/// single covering surface, so the listing can be cached without being on
/// screen.
#[derive(Clone, Debug, Default)]
pub(crate) struct ArchivedSessionsState {
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
    /// Rows whose delete may discard dirty or locked worktrees, keyed by the
    /// session a row's deletion targets: the project root of a project row, the
    /// session itself for a plain row.
    pub(crate) force: BTreeSet<AgentSessionId>,
    /// The session with a delete request in flight, if any.
    pub(crate) delete_in_flight: Option<AgentSessionId>,
}

impl LoomView {
    /// The archived-session rows the surface draws, grouped by project.
    ///
    /// Only the projections the last load kept are consulted, because
    /// `reload_archived_sessions` already feeds every cached tree into that load
    /// and `finish_archived_sessions_load` stores the union of their members.
    /// Grouping here therefore hides a session any tree named as a project member
    /// without cloning every cached tree on each frame.
    pub(crate) fn grouped_archived_sessions(&self, now_millis: u64) -> Vec<ArchivedSessionGroup> {
        archived_session_groups(
            &self.archived_sessions.entries,
            &self.archived_sessions.projects,
            &self.known_session_names(),
            now_millis,
        )
    }

    /// Opens the archived-sessions surface and loads what it shows.
    pub(crate) fn open_archived_sessions(&mut self, cx: &mut Context<Self>) {
        // The surface covers the central area, so opening it closes whatever
        // else covered the conversation, including a phone's session drawer.
        self.show_surface(CoveringSurface::ArchivedSessions);
        self.archived_sessions.refusal = None;
        self.reload_archived_sessions(cx);
    }

    /// Closes the archived-sessions surface, which is the surface its own close
    /// control belongs to.
    pub(crate) fn close_archived_sessions(&mut self, cx: &mut Context<Self>) {
        if self.archived_sessions_open() {
            self.close_covering_surface();
        }
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

    /// Applies a sub-agent window choice of the card's editor to the draft.
    pub(crate) fn choose_archive_sub_agent_retention_window(
        &mut self,
        choice: &str,
        cx: &mut Context<Self>,
    ) {
        let window = match sub_agent_retention_window_choice(choice) {
            RetentionWindowChoice::Off => None,
            RetentionWindowChoice::Window(window) => Some(window.as_millis() as u64),
            RetentionWindowChoice::Custom => return,
        };
        let Some(draft) = self.archived_sessions.retention_draft.as_mut() else {
            return;
        };
        if draft.sub_agent_retention_ms == window {
            return;
        }
        draft.sub_agent_retention_ms = window;
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
        // One projection per project: the one naming the most agents, so a
        // stale cached tree never displaces the authoritative snapshot.
        self.archived_sessions.projects = most_complete_snapshots(&projects);
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
                || self
                    .archived_sessions
                    .projects
                    .iter()
                    .any(|project| project.root_session_id == *session_id)
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

    /// The project snapshot whose root is `session_id`, from every tree the
    /// client holds.
    pub(crate) fn project_root_snapshot(
        &self,
        session_id: AgentSessionId,
    ) -> Option<loom_core::ProjectSnapshot> {
        self.project_tree_snapshots
            .iter()
            .chain(self.project_snapshot.iter())
            .chain(self.archived_sessions.projects.iter())
            .find(|project| project.root_session_id == session_id)
            .cloned()
    }

    /// The name the client knows for a session, from what it lists and the
    /// archived entries it holds.
    fn known_session_name(&self, session_id: AgentSessionId) -> Option<String> {
        self.archived_sessions
            .entries
            .iter()
            .find(|entry| entry.session.id == session_id)
            .map(|entry| entry.session.name.clone())
            .or_else(|| {
                self.sessions
                    .iter()
                    .find(|session| session.id == session_id)
                    .map(|session| session.name.clone())
            })
            .or_else(|| {
                (self.active_session.id == session_id).then(|| self.active_session.name.clone())
            })
    }

    /// The names the client knows for the sessions it lists, so a project row
    /// can name its root even when the root itself is not archived.
    pub(crate) fn known_session_names(&self) -> BTreeMap<AgentSessionId, String> {
        let mut names = self
            .sessions
            .iter()
            .map(|session| (session.id, session.name.clone()))
            .collect::<BTreeMap<_, _>>();
        names
            .entry(self.active_session.id)
            .or_insert_with(|| self.active_session.name.clone());
        names
    }

    /// The worker a deletion of `session_id` is routed to: the session's own
    /// archived entry, a project's archived member when the root is not itself
    /// listed, and otherwise the node the client assigned the session to.
    pub(crate) fn archived_session_node(&self, session_id: AgentSessionId) -> Option<String> {
        let entries = &self.archived_sessions.entries;
        if let Some(entry) = entries.iter().find(|entry| entry.session.id == session_id) {
            return Some(entry.node_id.clone());
        }
        if let Some(project) = self.project_root_snapshot(session_id)
            && let Some(node_id) = project_tree_node(session_id, &project, entries)
        {
            return Some(node_id);
        }
        self.session_node_ids.get(&session_id).cloned()
    }

    /// The node and request a delete of `session_id` would use, or `None` when
    /// no worker owns the session.
    pub(crate) fn archived_session_delete_request(
        &self,
        session_id: AgentSessionId,
    ) -> Option<(String, ClientRequest)> {
        let node_id = self.archived_session_node(session_id)?;
        let force = self.archived_sessions.force.contains(&session_id);
        Some((node_id, delete_archived_session_request(session_id, force)))
    }

    /// Opens the confirmation a project deletion goes through.
    ///
    /// Nothing is submitted here: the project is only removed once the dialog is
    /// confirmed. A session no connected worker owns opens no dialog, because
    /// the request could not be routed anywhere.
    pub(crate) fn begin_project_delete(
        &mut self,
        session_id: AgentSessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(node_id) = self.archived_session_node(session_id) else {
            log::warn!(
                "[loom-ui] ignoring the delete of project {session_id}: no connected worker owns it"
            );
            self.record_status("Cannot delete the project: no worker owns it");
            cx.notify();
            return;
        };
        let session_count = self
            .project_root_snapshot(session_id)
            .map_or(1, |project| project.agents.len().max(1));
        let name = self
            .known_session_name(session_id)
            .unwrap_or_else(|| format!("Project {}", short_session_id(session_id)));
        log::debug!(
            "[loom-ui] asking to confirm the deletion of project {session_id} ({session_count} session(s)) on worker {node_id}"
        );
        self.project_delete_dialog = Some(ProjectDeleteDialogState {
            session_id,
            name,
            session_count,
            node_id,
            // The row's own force choice is the delete the user already
            // described, so the confirmation starts from it.
            force: self.archived_sessions.force.contains(&session_id),
            in_flight: false,
            refusal: None,
        });
        cx.notify();
    }

    /// Sets the confirmation's force choice.
    pub(crate) fn set_project_delete_force(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(dialog) = self.project_delete_dialog.as_mut() else {
            return;
        };
        if dialog.force == force {
            return;
        }
        dialog.force = force;
        cx.notify();
    }

    /// Closes the confirmation without submitting anything.
    pub(crate) fn cancel_project_delete(&mut self, cx: &mut Context<Self>) {
        if self.project_delete_dialog.take().is_some() {
            log::debug!("[loom-ui] cancelled a project deletion");
        }
        cx.notify();
    }

    /// The request the open confirmation would submit, or `None` when no
    /// confirmation is open.
    pub(crate) fn project_delete_request(&self) -> Option<(String, ClientRequest)> {
        let dialog = self.project_delete_dialog.as_ref()?;
        Some((
            dialog.node_id.clone(),
            delete_archived_session_request(dialog.session_id, dialog.force),
        ))
    }

    /// Submits the confirmed project deletion to the worker that owns the tree.
    pub(crate) fn confirm_project_delete(&mut self, cx: &mut Context<Self>) {
        let Some((node_id, request)) = self.project_delete_request() else {
            return;
        };
        let Some(dialog) = self.project_delete_dialog.as_mut() else {
            return;
        };
        if dialog.in_flight {
            log::info!(
                "[loom-ui] ignoring the confirmed project deletion {}: a request is already in flight",
                dialog.session_id
            );
            return;
        }
        let session_id = dialog.session_id;
        let force = dialog.force;
        dialog.in_flight = true;
        dialog.refusal = None;
        log::info!("[loom-ui] deleting project {session_id} on worker {node_id} (force={force})");
        self.dispatch_to_node(cx, node_id, request, move |view, response, cx| {
            view.finish_project_delete(session_id, response, cx);
        });
        cx.notify();
    }

    /// Applies the answer to a confirmed project deletion.
    ///
    /// A refusal keeps the dialog open with the backend's reason, so the
    /// deletion can be retried; success closes it and drops the whole tree.
    pub(crate) fn finish_project_delete(
        &mut self,
        session_id: AgentSessionId,
        response: ResponseEnvelope,
        cx: &mut Context<Self>,
    ) {
        let refusal = match response.result {
            Ok(ServerResponse::Session(SessionResponse::AgentSessionDeleted {
                session_id: deleted,
            })) => {
                if deleted != session_id {
                    log::warn!(
                        "[loom-ui] the delete of project {session_id} reported session {deleted} as deleted"
                    );
                }
                log::info!("[loom-ui] deleted project {deleted}");
                self.project_delete_dialog = None;
                self.finish_deleted_session(deleted, "Project deleted", cx);
                None
            }
            Err(error) => {
                log::warn!(
                    "[loom-ui] deleting project {session_id} was refused: {:?}: {}",
                    error.code,
                    error.message
                );
                Some(error.message)
            }
            Ok(response) => {
                log::warn!(
                    "[loom-ui] deleting project {session_id} returned an unexpected response"
                );
                Some(unexpected_response("project deletion", response).message)
            }
        };
        if let Some(refusal) = refusal
            && let Some(dialog) = self.project_delete_dialog.as_mut()
            && dialog.session_id == session_id
        {
            dialog.in_flight = false;
            dialog.refusal = Some(refusal);
        }
        cx.notify();
    }

    /// Deletes one plain archived session, carrying the row's force choice. The
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
        if !self
            .archived_sessions
            .entries
            .iter()
            .any(|entry| entry.session.id == session_id)
        {
            log::warn!(
                "[loom-ui] ignoring the delete of {session_id}: it is not in the archived-session list"
            );
            return;
        }
        let Some((node_id, request)) = self.archived_session_delete_request(session_id) else {
            log::warn!("[loom-ui] ignoring the delete of {session_id}: no worker owns it");
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

    /// Applies the answer to a plain delete request.
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
                self.finish_deleted_session(deleted, "Archived session deleted", cx);
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

    /// Applies a confirmed removal of `deleted` and every session the backend
    /// deleted with it: the sidebar state forgets the whole tree, the selection
    /// moves off it, and the open archived listing is refreshed.
    fn finish_deleted_session(
        &mut self,
        deleted: AgentSessionId,
        status: &str,
        cx: &mut Context<Self>,
    ) {
        self.archived_sessions.refusal = None;
        let was_active = self.forget_archived_session(deleted);
        if was_active {
            // The session on screen no longer exists, so move the selection
            // instead of leaving a deleted session active.
            if let Some(session) = self.sessions.first().cloned() {
                self.select_session(session, cx);
            } else {
                self.activate_session(empty_session_snapshot(self.workspace_id));
                self.review.open = false;
            }
        }
        self.record_status(status);
        self.reload_archived_sessions(cx);
        cx.notify();
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
