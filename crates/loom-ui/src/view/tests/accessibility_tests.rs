//! Accessibility node role, name, value, and state tests for Loom's UI.

use super::*;
use crate::collaboration::CollaborationState;
use crate::state::{
    AssistantPart, AssistantTurn, EvidenceText, SystemNote, SystemTone, TimelineItem, ToolPart,
    ToolPartStatus,
};
use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, ElementId, Role, TestAppContext, px, size};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, Author, CollaborationEntry,
    CollaborationEntryKind, ParticipantIdentity, Recipient, Timestamp, ToolCallId, WorkspaceId,
};

fn session_snapshot(
    workspace_id: WorkspaceId,
    name: &str,
    state: AgentSessionState,
) -> AgentSessionSnapshot {
    let timestamp = Timestamp::from_unix_millis(1);
    AgentSessionSnapshot {
        id: AgentSessionId::new(),
        workspace_id,
        name: name.to_owned(),
        goal: None,
        state,
        created_at: timestamp,
        updated_at: timestamp,
        title_source: loom_core::SessionTitleSource::Manual,
    }
}

#[gpui_kit::test]
fn settings_switches_report_role_label_and_checked_state(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    for auto_approve in [true, false] {
        let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
            let view = cx.new(|cx| {
                let mut view = LoomView::new_for_test(cx.focus_handle());
                view.auto_approve_actions = auto_approve;
                view.ask_before_destructive_actions = !auto_approve;
                view.show_reasoning = !auto_approve;
                view.show_issue_references = auto_approve;
                view.show_pull_request_references = auto_approve;
                view.workspace_config.auto_archive_completed_agents = auto_approve;
                view.workspace_config.auto_title_projects = auto_approve;
                view.show_surface(CoveringSurface::Settings(SettingsSection::Agents));
                view
            });
            Root::new(view, window, cx)
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let settings = window.within("settings-dialog");

            let auto_approve_switch = settings.find("session-auto-approve-toggle");
            assert_eq!(auto_approve_switch.role(), Some(Role::Switch));
            assert_eq!(
                auto_approve_switch.label(),
                Some("Auto-approve non-destructive actions")
            );
            assert_eq!(auto_approve_switch.checked(), Some(auto_approve));

            let destructive_switch = settings.find("session-ask-before-destructive-toggle");
            assert_eq!(destructive_switch.role(), Some(Role::Switch));
            assert_eq!(
                destructive_switch.label(),
                Some("Ask before destructive actions")
            );
            assert_eq!(destructive_switch.checked(), Some(!auto_approve));

            let archive_switch = settings.find("auto-archive-completed-agents-toggle");
            assert_eq!(archive_switch.role(), Some(Role::Switch));
            assert_eq!(archive_switch.checked(), Some(auto_approve));

            let title_switch = settings.find("auto-title-projects-toggle");
            assert_eq!(title_switch.role(), Some(Role::Switch));
            assert_eq!(title_switch.checked(), Some(auto_approve));

            let reasoning_switch = settings.find("show-reasoning-toggle");
            assert_eq!(reasoning_switch.role(), Some(Role::Switch));
            assert_eq!(reasoning_switch.checked(), Some(!auto_approve));

            let issue_links_switch = settings.find("show-issue-references-toggle");
            assert_eq!(issue_links_switch.role(), Some(Role::Switch));
            assert_eq!(issue_links_switch.checked(), Some(auto_approve));

            let pr_links_switch = settings.find("show-pull-request-references-toggle");
            assert_eq!(pr_links_switch.role(), Some(Role::Switch));
            assert_eq!(pr_links_switch.checked(), Some(auto_approve));
        })
        .unwrap();
    }
}

#[gpui_kit::test]
fn settings_steppers_expose_descriptive_labels_and_value_readouts(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.workspace_config.cpu_pulse_threshold_percent = 5;
            view.workspace_config.project_agent_concurrency = 4;
            view.workspace_config.run_admission_timeout_secs = 30 * 60;
            view.show_surface(CoveringSurface::Settings(SettingsSection::Agents));
            view
        });
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let settings = window.within("settings-dialog");

        for (id, expected_label) in [
            (
                "cpu-pulse-threshold-decrease",
                "Decrease session indicator pulse threshold",
            ),
            (
                "cpu-pulse-threshold-increase",
                "Increase session indicator pulse threshold",
            ),
            (
                "project-agent-concurrency-decrease",
                "Decrease parallel project agents",
            ),
            (
                "project-agent-concurrency-increase",
                "Increase parallel project agents",
            ),
            (
                "run-admission-timeout-decrease",
                "Decrease run queue timeout",
            ),
            (
                "run-admission-timeout-increase",
                "Increase run queue timeout",
            ),
        ] {
            let button = settings.find(id);
            assert_eq!(button.role(), Some(Role::Button), "{id} role");
            assert_eq!(button.label(), Some(expected_label), "{id} label");
            assert_eq!(
                settings.find_by_label(expected_label).path(),
                button.path(),
                "{id} discoverable by accessible label"
            );
        }

        for (id, expected_value) in [
            ("cpu-pulse-threshold-value", "5%"),
            ("project-agent-concurrency-value", "4"),
            ("run-admission-timeout-value", "30 min"),
        ] {
            let value = settings.find(id);
            assert_eq!(value.role(), Some(Role::Label), "{id} role");
            assert_eq!(value.value(), Some(expected_value), "{id} value");
        }
    })
    .unwrap();

    let appearance = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.show_surface(CoveringSurface::Settings(SettingsSection::Appearance));
            view
        });
        Root::new(view, window, cx)
    });
    cx.update_window(appearance.into(), |_, window, cx| {
        window.render_frame(cx);
        let settings = window.within("settings-dialog");

        for (id, expected_label) in [
            ("font-scale-decrease", "Decrease font size"),
            ("font-scale-increase", "Increase font size"),
            ("font-scale-reset", "Reset font size"),
        ] {
            let button = settings.find(id);
            assert_eq!(button.role(), Some(Role::Button), "{id} role");
            assert_eq!(button.label(), Some(expected_label), "{id} label");
            assert_eq!(settings.find_by_label(expected_label).path(), button.path());
        }
        let readout = settings.find("font-scale-value");
        assert_eq!(readout.role(), Some(Role::Label));
        assert_eq!(readout.value(), Some("100%"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn project_list_items_and_sections_expose_accessible_names_and_roles(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            let workspace_id = view.workspace_id;
            let first = session_snapshot(workspace_id, "Alpha Project", AgentSessionState::Idle);
            let second = session_snapshot(workspace_id, "Beta Project", AgentSessionState::Paused);
            view.session_node_ids
                .insert(first.id, view.default_backend_node_id.clone());
            view.session_node_ids
                .insert(second.id, view.default_backend_node_id.clone());
            view.session_sources.insert(
                first.id,
                "https://github.com/bearmuckle/loom.git".to_owned(),
            );
            view.active_session = first.clone();
            view.sessions = vec![first, second];
            view
        });
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);

        let region = window.find("session-list");
        assert_eq!(region.role(), Some(Role::Region));
        assert_eq!(region.label(), Some("Projects"));

        let first_row = window.find(("project-row", 0usize));
        assert_eq!(first_row.label(), Some("Alpha Project"));
        let first_name = window.find(("project-name", 0usize));
        assert_eq!(first_name.role(), Some(Role::Label));
        assert_eq!(first_name.value(), Some("Alpha Project"));
        let first_detail = window.find(("project-detail", 0usize));
        assert_eq!(first_detail.role(), Some(Role::Label));
        assert_eq!(
            first_detail.value(),
            Some("https://github.com/bearmuckle/loom.git")
        );

        let second_row = window.find(("project-row", 1usize));
        assert_eq!(second_row.label(), Some("Beta Project"));
        let second_name = window.find(("project-name", 1usize));
        assert_eq!(second_name.role(), Some(Role::Label));
        assert_eq!(second_name.value(), Some("Beta Project"));
        let second_status = window.find(("project-status", 1usize));
        assert_eq!(second_status.role(), Some(Role::Label));
        assert_eq!(second_status.value(), Some("Paused"));

        let headings = window
            .within("session-list")
            .find_all_by_role(Role::Heading);
        assert!(
            !headings.is_empty(),
            "worker section header exposes a heading role in the project list"
        );
        assert!(headings[0].label().is_some_and(|label| !label.is_empty()));
    })
    .unwrap();
}

#[gpui_kit::test]
fn session_transcript_exposes_log_and_structured_message_articles(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let empty_window = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view
        });
        Root::new(view, window, cx)
    });
    cx.update_window(empty_window.into(), |_, window, cx| {
        window.render_frame(cx);
        let empty = window.find("timeline-empty");
        assert_eq!(empty.role(), Some(Role::Log));
        assert_eq!(empty.label(), Some("Session transcript"));
        let status = window.find("timeline-empty-status");
        assert_eq!(status.role(), Some(Role::Status));
        assert_eq!(status.label(), Some("Ready when you are"));
    })
    .unwrap();

    let opening_window = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view.conversation_opening = true;
            view
        });
        Root::new(view, window, cx)
    });
    cx.update_window(opening_window.into(), |_, window, cx| {
        window.render_frame(cx);
        let opening = window.find("timeline-opening");
        assert_eq!(opening.role(), Some(Role::Status));
        assert_eq!(opening.label(), Some("Opening the conversation"));
    })
    .unwrap();

    let populated = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            view.timeline = vec![
                TimelineItem::User("Inspect the settings accessibility tree".to_owned()),
                TimelineItem::Assistant(AssistantTurn {
                    parts: vec![
                        AssistantPart::Tool(Box::new(ToolPart {
                            id: ToolCallId::new(),
                            name: "read_file".to_owned(),
                            title: "read_file crates/loom-ui/src/view.rs".to_owned(),
                            detail: None,
                            status: ToolPartStatus::Completed,
                            output: Some("ok".to_owned()),
                            elapsed_ms: Some(12),
                            approval_pending: false,
                            policy: None,
                        })),
                        AssistantPart::Text("Updated the labels and message roles.".to_owned()),
                    ],
                    streaming: false,
                }),
                TimelineItem::System(SystemNote {
                    heading: Some("Notice".to_owned()),
                    text: "Worker reconnected".to_owned(),
                    details: None,
                    progress: None,
                    tone: SystemTone::Neutral,
                    retryable: false,
                }),
            ];
            view
        });
        Root::new(view, window, cx)
    });
    cx.update_window(populated.into(), |_, window, cx| {
        window.render_frame(cx);

        let viewport = window.find((ElementId::from("timeline-scroller"), "viewport"));
        assert_eq!(viewport.role(), Some(Role::Log));
        assert_eq!(viewport.label(), Some("Session transcript"));

        let articles = window
            .within("timeline-scroller")
            .find_all_by_role(Role::Article);
        assert_eq!(articles.len(), 3);

        let user_item = window.find(("timeline-item", 0usize));
        assert_eq!(user_item.role(), Some(Role::Article));
        assert_eq!(
            user_item.label(),
            Some("You: Inspect the settings accessibility tree")
        );
        let user_speaker = window.find(("timeline-speaker", 0usize));
        assert_eq!(user_speaker.role(), Some(Role::Label));
        assert_eq!(user_speaker.value(), Some("You"));

        let assistant_item = window.find(("timeline-item", 1usize));
        assert_eq!(assistant_item.role(), Some(Role::Article));
        assert_eq!(
            assistant_item.label(),
            Some("Agent: Updated the labels and message roles.")
        );
        let assistant_speaker = window.find(("timeline-speaker", 1usize));
        assert_eq!(assistant_speaker.role(), Some(Role::Label));
        assert_eq!(assistant_speaker.value(), Some("Agent"));

        let system_item = window.find(("timeline-item", 2usize));
        assert_eq!(system_item.role(), Some(Role::Article));
        assert_eq!(
            system_item.label(),
            Some("System: Notice: Worker reconnected")
        );
        let system_heading = window.find(("timeline-speaker", 2usize));
        assert_eq!(system_heading.role(), Some(Role::Label));
        assert_eq!(system_heading.value(), Some("Notice"));
    })
    .unwrap();
}

#[test]
fn timeline_item_accessible_name_summarizes_every_entry_kind() {
    let collaboration = CollaborationState::default();

    assert_eq!(
        timeline::timeline_item_accessible_name(
            &TimelineItem::User("  first line\n\tsecond line  ".to_owned()),
            &collaboration,
        ),
        "You: first line second line"
    );

    assert_eq!(
        timeline::timeline_item_accessible_name(
            &TimelineItem::User("   ".to_owned()),
            &collaboration,
        ),
        "You"
    );

    let long_prompt = "word ".repeat(40);
    let long_name =
        timeline::timeline_item_accessible_name(&TimelineItem::User(long_prompt), &collaboration);
    assert!(long_name.starts_with("You: "));
    assert!(long_name.ends_with("..."));
    assert!(long_name.len() <= "You: ".len() + 123);

    let tool_only = TimelineItem::Assistant(AssistantTurn {
        parts: vec![AssistantPart::Tool(Box::new(ToolPart {
            id: ToolCallId::new(),
            name: "read_file".to_owned(),
            title: "read_file README.md".to_owned(),
            detail: None,
            status: ToolPartStatus::Completed,
            output: None,
            elapsed_ms: None,
            approval_pending: false,
            policy: None,
        }))],
        streaming: false,
    });
    let tool_only_name = timeline::timeline_item_accessible_name(&tool_only, &collaboration);
    assert!(tool_only_name.starts_with("Agent: Tools used: "));
    assert!(
        tool_only_name.ends_with(&format!(", {}", ToolPartStatus::Completed.label())),
        "unexpected tool summary name: {tool_only_name}"
    );

    let reasoning_only = TimelineItem::Assistant(AssistantTurn {
        parts: vec![AssistantPart::Reasoning("Checking the layout".to_owned())],
        streaming: false,
    });
    assert_eq!(
        timeline::timeline_item_accessible_name(&reasoning_only, &collaboration),
        "Agent: Reasoning: Checking the layout"
    );

    let evidence_only = TimelineItem::Assistant(AssistantTurn {
        parts: vec![AssistantPart::Evidence(vec![EvidenceText {
            label: "diff".to_owned(),
            uri: "file:///tmp/diff".to_owned(),
        }])],
        streaming: false,
    });
    assert_eq!(
        timeline::timeline_item_accessible_name(&evidence_only, &collaboration),
        "Agent: Evidence"
    );

    let empty_assistant = TimelineItem::Assistant(AssistantTurn {
        parts: Vec::new(),
        streaming: true,
    });
    assert_eq!(
        timeline::timeline_item_accessible_name(&empty_assistant, &collaboration),
        "Agent"
    );

    let peer_entry = TimelineItem::Collaboration(Box::new(CollaborationEntry {
        session_id: AgentSessionId::new(),
        run_id: None,
        author: Author::Participant {
            participant: ParticipantIdentity {
                id: loom_core::ParticipantId::new(),
                display_name: "Alice".to_owned(),
            },
            role: loom_core::SessionRole::Participant,
        },
        recipient: Recipient::People,
        kind: CollaborationEntryKind::PeerMessage {
            text: "Looks good to me".to_owned(),
        },
        revision: None,
        created_at: Timestamp::from_unix_millis(1),
    }));
    let peer_name = timeline::timeline_item_accessible_name(&peer_entry, &collaboration);
    assert!(peer_name.contains("Alice"));
    assert!(peer_name.contains("(Peer chat): Looks good to me"));

    let plain_system = TimelineItem::System(SystemNote {
        heading: None,
        text: "Connection restored".to_owned(),
        details: None,
        progress: None,
        tone: SystemTone::Neutral,
        retryable: false,
    });
    assert_eq!(
        timeline::timeline_item_accessible_name(&plain_system, &collaboration),
        "System: Connection restored"
    );
}
