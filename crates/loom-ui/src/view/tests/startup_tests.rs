//! Startup landing: the project the browser client opens, and the screen it
//! shows while it gets there.
//!
//! `try_new_browser` is compiled only for wasm, where these tests cannot run,
//! so the decisions it makes are pure helpers that are checked here.

use super::*;
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, EventSequence, ProjectAgentRecord,
    ProjectId, ProjectSnapshot, Timestamp, WorkspaceId,
};

fn session(id: AgentSessionId, name: &str, updated_at: u64) -> AgentSessionSnapshot {
    AgentSessionSnapshot {
        id,
        workspace_id: WorkspaceId::new(),
        name: name.to_owned(),
        goal: None,
        state: AgentSessionState::Idle,
        created_at: Timestamp::from_unix_millis(1),
        updated_at: Timestamp::from_unix_millis(updated_at),
        title_source: loom_core::SessionTitleSource::Auto,
    }
}

/// A project snapshot with `root` at depth 1 and the other ids as children.
fn project(root: AgentSessionId, children: &[AgentSessionId]) -> ProjectSnapshot {
    let project_id = ProjectId::from_uuid(*root.as_uuid());
    let agent = |session_id: AgentSessionId, parent_session_id: Option<AgentSessionId>| {
        ProjectAgentRecord {
            session_id,
            project_id,
            parent_session_id,
            depth: if parent_session_id.is_some() { 2 } else { 1 },
            state: AgentSessionState::Idle,
            task_summary: None,
            output_cursor: EventSequence::default(),
            updated_at: Timestamp::from_unix_millis(1),
        }
    };
    ProjectSnapshot {
        project_id,
        root_session_id: root,
        parent_project_id: None,
        agents: std::iter::once(agent(root, None))
            .chain(children.iter().map(|child| agent(*child, Some(root))))
            .collect(),
        tasks: Vec::new(),
        worktrees: Vec::new(),
    }
}

/// The landing session is the project whose activity is newest, never the
/// sub-agent that produced that activity.
#[test]
fn the_most_recent_project_is_opened_rather_than_its_sub_agent() {
    let older_root = AgentSessionId::new();
    let newer_root = AgentSessionId::new();
    let newer_agent = AgentSessionId::new();
    // The worker lists newest activity first, and a sub-agent's update is
    // usually the newest of all.
    let sessions = vec![
        session(newer_agent, "Researcher", 40),
        session(newer_root, "Checkout flow", 30),
        session(older_root, "Billing", 20),
    ];
    let projects = vec![
        project(newer_root, &[newer_agent]),
        project(older_root, &[]),
    ];

    let landing = landing_session(&sessions, &projects).expect("a landing session");

    assert_eq!(
        landing.id, newer_root,
        "the client opens the sub-agent's project, not its transcript"
    );
    assert_eq!(landing.name, "Checkout flow");
}

/// A root session is already the project, so it lands unchanged.
#[test]
fn the_most_recent_session_lands_when_it_is_a_root() {
    let root = AgentSessionId::new();
    let other = AgentSessionId::new();
    let sessions = vec![session(root, "Latest", 30), session(other, "Other", 20)];
    let projects = vec![project(root, &[]), project(other, &[])];

    assert_eq!(
        landing_session(&sessions, &projects).map(|session| session.id),
        Some(root)
    );
}

/// A session no snapshot covers keeps its own row, so it lands as itself.
#[test]
fn a_session_outside_every_project_lands_as_itself() {
    let standalone = AgentSessionId::new();
    let root = AgentSessionId::new();
    let sessions = vec![
        session(standalone, "Standalone", 30),
        session(root, "Project", 20),
    ];

    assert_eq!(
        landing_session(&sessions, &[project(root, &[])]).map(|session| session.id),
        Some(standalone)
    );
}

/// A sub-agent whose project root is not in the loaded list cannot be replaced,
/// so the most recent session is still better than nothing.
#[test]
fn a_sub_agent_without_its_root_lands_as_itself() {
    let agent = AgentSessionId::new();
    let sessions = vec![session(agent, "Researcher", 30)];
    let projects = vec![project(agent, &[])];

    assert_eq!(
        landing_session(&sessions, &projects).map(|session| session.id),
        Some(agent)
    );
    assert!(landing_session(&[], &projects).is_none());
}

/// The startup screen never reads as a failure: no reconnect action, no raw
/// details, and copy that says what the client is doing.
#[test]
fn the_startup_screen_offers_progress_instead_of_a_settings_prompt() {
    let connecting = startup_screen(false);
    assert_eq!(connecting.heading, "Connecting to your worker");
    assert!(!connecting.reconnect);
    assert!(connecting.details.is_none());
    assert!(connecting.progress.is_none());
    assert_eq!(connecting.footer, "Connecting to the worker");

    let opening = startup_screen(true);
    assert_eq!(opening.heading, "Opening your most recent project");
    assert!(!opening.reconnect);
    assert!(opening.details.is_none());
    assert_eq!(
        opening.footer,
        "Connected  ·  opening your most recent project"
    );
    // The two phases say different things, so neither can be mistaken for the
    // disconnected screen's "connect a worker" or "connection lost" copy.
    assert_ne!(connecting.detail, opening.detail);
    assert_ne!(connecting.empty, opening.empty);
    assert_ne!(connecting.heading, disconnected_screen(None).heading);
    assert_ne!(
        opening.heading,
        disconnected_screen(Some("the connection closed (code 1006)")).heading
    );
}

/// The first browser frame has no covering surface. A page with a worker URL
/// bootstraps at once, so the startup screen must not have the settings dialog
/// on top of it; a page without one is onboarded by the welcome dialog, whose
/// button opens Settings.
#[test]
fn the_browser_client_starts_without_a_covering_surface() {
    assert!(
        browser_startup_covering_surface().is_none(),
        "the first browser view covers itself with the settings dialog"
    );
}

/// The startup screen paints the settings dialog only once no bootstrap is in
/// flight: an automatic startup that reaches Settings holds the whole screen
/// with its own progress, and a settings dialog the view still holds waits
/// until the disconnected screen that offers it is on screen.
#[test]
fn the_startup_screen_hides_the_settings_dialog_while_a_bootstrap_runs() {
    let cases = [
        // settings open, bootstrap in flight, settings dialog painted
        (true, true, false),
        (true, false, true),
        (false, true, false),
        (false, false, false),
    ];
    for (settings_open, browser_starting, painted) in cases {
        assert_eq!(
            startup_screen_shows_settings(settings_open, browser_starting),
            painted,
            "settings open {settings_open}, bootstrap in flight {browser_starting}"
        );
    }
}
