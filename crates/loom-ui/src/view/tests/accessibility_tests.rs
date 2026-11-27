//! Accessibility node role, name, value, and state tests for Loom's UI.

use super::loom_view_render_tests::nested_project_view;
use super::*;
use crate::collaboration::CollaborationState;
use crate::state::{
    ASK_USER_TOOL, AssistantPart, AssistantTurn, EvidenceText, OutputDetail, SystemNote,
    SystemTone, TimelineItem, ToolPart, ToolPartStatus,
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

            for (id, expected_label, expected_checked) in [
                (
                    "session-auto-approve-toggle",
                    "Auto-approve non-destructive actions",
                    auto_approve,
                ),
                (
                    "session-ask-before-destructive-toggle",
                    "Ask before destructive actions",
                    !auto_approve,
                ),
                (
                    "auto-archive-completed-agents-toggle",
                    "Auto-archive completed sub agents",
                    auto_approve,
                ),
                (
                    "auto-title-projects-toggle",
                    "Automatic project titles",
                    auto_approve,
                ),
                (
                    "show-reasoning-toggle",
                    "Show model reasoning",
                    !auto_approve,
                ),
                (
                    "show-issue-references-toggle",
                    "Session issue links",
                    auto_approve,
                ),
                (
                    "show-pull-request-references-toggle",
                    "Session pull request links",
                    auto_approve,
                ),
            ] {
                let switch = settings.find(id);
                assert!(switch.role().is_some(), "{id} must publish an a11y role");
                assert_eq!(switch.role(), Some(Role::Switch), "{id} role");
                assert_eq!(switch.label(), Some(expected_label), "{id} label");
                assert_eq!(switch.checked(), Some(expected_checked), "{id} checked");
            }
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
            assert!(button.role().is_some(), "{id} must publish an a11y role");
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
            assert!(value.role().is_some(), "{id} must publish an a11y role");
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
            assert!(button.role().is_some(), "{id} must publish an a11y role");
            assert_eq!(button.role(), Some(Role::Button), "{id} role");
            assert_eq!(button.label(), Some(expected_label), "{id} label");
            assert_eq!(settings.find_by_label(expected_label).path(), button.path());
        }
        let readout = settings.find("font-scale-value");
        assert!(readout.role().is_some());
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
        assert!(region.role().is_some());
        assert_eq!(region.role(), Some(Role::Region));
        assert_eq!(region.label(), Some("Projects"));

        let first_row = window.find(("project-row", 0usize));
        assert!(
            first_row.role().is_some(),
            "project-row must publish a role so GPUI writes its label to AccessKit"
        );
        assert_eq!(first_row.role(), Some(Role::ListItem));
        assert_eq!(first_row.label(), Some("Alpha Project"));

        let first_name = window.find(("project-name", 0usize));
        assert!(first_name.role().is_some());
        assert_eq!(first_name.role(), Some(Role::Label));
        assert_eq!(first_name.value(), Some("Alpha Project"));

        let first_detail = window.find(("project-detail", 0usize));
        assert!(first_detail.role().is_some());
        assert_eq!(first_detail.role(), Some(Role::Label));
        assert_eq!(
            first_detail.value(),
            Some("https://github.com/bearmuckle/loom.git")
        );

        let second_row = window.find(("project-row", 1usize));
        assert!(second_row.role().is_some());
        assert_eq!(second_row.role(), Some(Role::ListItem));
        assert_eq!(second_row.label(), Some("Beta Project"));

        let second_name = window.find(("project-name", 1usize));
        assert!(second_name.role().is_some());
        assert_eq!(second_name.role(), Some(Role::Label));
        assert_eq!(second_name.value(), Some("Beta Project"));

        let second_status = window.find(("project-status", 1usize));
        assert!(second_status.role().is_some());
        assert_eq!(second_status.role(), Some(Role::Label));
        assert_eq!(second_status.value(), Some("Paused"));

        let headings = window
            .within("session-list")
            .find_all_by_role(Role::Heading);
        assert!(
            !headings.is_empty(),
            "worker section header exposes a heading role in the project list"
        );
        assert_eq!(headings[0].role(), Some(Role::Heading));
        assert!(headings[0].label().is_some_and(|label| !label.is_empty()));
        assert!(
            headings[0]
                .description()
                .is_some_and(|description| !description.is_empty()),
            "worker section heading carries its subtitle as aria_description"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn delegated_agent_popover_rows_separate_open_and_actions_buttons(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |window, cx| {
        let view = cx.new(|cx| nested_project_view(cx.focus_handle()));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(("project-agents", 0usize), cx);
        window.render_frame(cx);

        for (index, name, status) in [(0usize, "Manager", "Working"), (1usize, "Worker", "Done")] {
            let row = window.find(("project-agent-row", index));
            assert_eq!(row.role(), Some(Role::Group));
            assert_eq!(row.label(), Some(name));

            let open = window.find(("project-agent-open", index));
            assert_eq!(open.role(), Some(Role::Button));
            assert_eq!(open.label(), Some(format!("{name} · {status}").as_str()));

            let actions = window.find(("project-agent-actions", index));
            assert_eq!(actions.role(), Some(Role::Button));
            assert_eq!(actions.label(), Some("Agent actions"));
            assert!(
                !actions.path().starts_with(open.path()),
                "Agent actions button must be a sibling of project-agent-open, not nested inside it"
            );
        }
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
        let log = window.find("timeline-opening-log");
        assert_eq!(log.role(), Some(Role::Log));
        assert_eq!(log.label(), Some("Session transcript"));
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
                TimelineItem::Collaboration(Box::new(CollaborationEntry {
                    session_id: view.active_session.id,
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
                        text: "Checking this together".to_owned(),
                    },
                    revision: None,
                    created_at: Timestamp::from_unix_millis(1),
                })),
                TimelineItem::System(SystemNote {
                    heading: Some("Notice".to_owned()),
                    text: "Worker reconnected".to_owned(),
                    details: None,
                    progress: None,
                    tone: SystemTone::Neutral,
                    retryable: true,
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
        assert_eq!(articles.len(), 4);

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

        let peer_item = window.find(("timeline-item", 2usize));
        assert_eq!(peer_item.role(), Some(Role::Article));
        assert!(
            peer_item
                .label()
                .is_some_and(|label| label.contains("(Peer chat): Checking this together"))
        );
        let peer_disclaimer = window.find(("peer-chat-disclaimer", 2usize));
        assert_eq!(peer_disclaimer.role(), Some(Role::Label));
        assert_eq!(
            peer_disclaimer.value(),
            Some("Peer chat · not sent to the agent")
        );

        let system_item = window.find(("timeline-item", 3usize));
        assert_eq!(system_item.role(), Some(Role::Article));
        assert_eq!(
            system_item.label(),
            Some("System: Notice: Worker reconnected")
        );
        let system_heading = window.find(("timeline-speaker", 3usize));
        assert_eq!(system_heading.role(), Some(Role::Label));
        assert_eq!(system_heading.value(), Some("Notice"));
        let retry_hint = window.find(("system-note-retry", 3usize));
        assert_eq!(retry_hint.role(), Some(Role::Label));
        assert_eq!(
            retry_hint.value(),
            Some("Retry by sending another message.")
        );
    })
    .unwrap();
}

#[test]
fn timeline_item_accessible_name_matches_painted_content() {
    let collaboration = CollaborationState::default();
    let name_for = |item: &TimelineItem, output_detail: OutputDetail, show_reasoning: bool| {
        timeline::timeline_item_accessible_name_with(
            item,
            &collaboration,
            output_detail,
            show_reasoning,
            |part| part.detail.clone(),
        )
    };

    assert_eq!(
        name_for(
            &TimelineItem::User("  first line\n\tsecond line  ".to_owned()),
            OutputDetail::Medium,
            false,
        ),
        "You: first line second line"
    );
    assert_eq!(
        name_for(
            &TimelineItem::User("   ".to_owned()),
            OutputDetail::Medium,
            false
        ),
        "You"
    );

    // ask_user turns announce "Agent asks: <preview>" and never count as Tools used.
    let ask_turn = TimelineItem::Assistant(AssistantTurn {
        parts: vec![AssistantPart::Tool(Box::new(ToolPart {
            id: ToolCallId::new(),
            name: ASK_USER_TOOL.to_owned(),
            title: "ask_user".to_owned(),
            detail: Some("Which target should I build?".to_owned()),
            status: ToolPartStatus::AwaitingInput,
            output: None,
            elapsed_ms: None,
            approval_pending: false,
            policy: None,
        }))],
        streaming: false,
    });
    assert_eq!(
        name_for(&ask_turn, OutputDetail::Low, false),
        "Agent asks: Which target should I build?"
    );

    // Regular tools respect output_detail unless they need attention.
    let read_tool = TimelineItem::Assistant(AssistantTurn {
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
    let medium_tool_name = name_for(&read_tool, OutputDetail::Medium, false);
    assert!(medium_tool_name.starts_with("Agent: Tools used: "));
    assert!(
        medium_tool_name.ends_with(&format!(", {}", ToolPartStatus::Completed.label())),
        "unexpected tool summary name: {medium_tool_name}"
    );
    assert_eq!(
        name_for(&read_tool, OutputDetail::Low, false),
        "Agent",
        "Low output detail hides completed tool activity"
    );

    let approval_tool = TimelineItem::Assistant(AssistantTurn {
        parts: vec![AssistantPart::Tool(Box::new(ToolPart {
            id: ToolCallId::new(),
            name: "run_command".to_owned(),
            title: "run_command rm -rf".to_owned(),
            detail: None,
            status: ToolPartStatus::AwaitingApproval,
            output: None,
            elapsed_ms: None,
            approval_pending: true,
            policy: None,
        }))],
        streaming: false,
    });
    assert!(
        name_for(&approval_tool, OutputDetail::Low, false).starts_with("Agent: Tools used: "),
        "Low output detail keeps attention-needing tool runs visible"
    );

    // Reasoning respects both show_reasoning and output_detail.
    let reasoning_only = TimelineItem::Assistant(AssistantTurn {
        parts: vec![AssistantPart::Reasoning("Checking the layout".to_owned())],
        streaming: false,
    });
    assert_eq!(
        name_for(&reasoning_only, OutputDetail::Medium, true),
        "Agent: Reasoning: Checking the layout"
    );
    assert_eq!(
        name_for(&reasoning_only, OutputDetail::Medium, false),
        "Agent"
    );
    assert_eq!(name_for(&reasoning_only, OutputDetail::Low, true), "Agent");

    let evidence_only = TimelineItem::Assistant(AssistantTurn {
        parts: vec![AssistantPart::Evidence(vec![EvidenceText {
            label: "diff".to_owned(),
            uri: "file:///tmp/diff".to_owned(),
        }])],
        streaming: false,
    });
    assert_eq!(
        name_for(&evidence_only, OutputDetail::Medium, false),
        "Agent: Evidence"
    );

    let empty_assistant = TimelineItem::Assistant(AssistantTurn {
        parts: Vec::new(),
        streaming: true,
    });
    assert_eq!(
        name_for(&empty_assistant, OutputDetail::Medium, true),
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
    let peer_name = name_for(&peer_entry, OutputDetail::Medium, false);
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
        name_for(&plain_system, OutputDetail::Medium, false),
        "System: Connection restored"
    );
}

#[test]
fn accessible_preview_truncates_cleanly_without_dangling_spaces_or_mid_word_slices() {
    // Exact 120-character single token fits without ellipsis.
    let exact_120 = "a".repeat(120);
    assert_eq!(timeline::accessible_preview(&exact_120), exact_120);

    // Single >120-character token truncates to 117 chars + "...".
    let single_long = "x".repeat(150);
    let single_preview = timeline::accessible_preview(&single_long);
    assert_eq!(single_preview.chars().count(), 120);
    assert!(single_preview.ends_with("..."));
    assert!(!single_preview.ends_with(" ..."));

    // First token of 118 chars followed by another word must not emit " ...".
    let first_118_plus_next = format!("{} next", "b".repeat(118));
    let preview_118 = timeline::accessible_preview(&first_118_plus_next);
    assert_eq!(preview_118.chars().count(), 120);
    assert!(preview_118.ends_with("..."));
    assert!(!preview_118.ends_with(" ..."));

    // Multi-word input hitting 118 chars + next word stops at the prior word boundary.
    let words_118_plus_next = format!("{} {} next", "c".repeat(100), "d".repeat(17));
    let multi_118 = timeline::accessible_preview(&words_118_plus_next);
    assert_eq!(multi_118, format!("{}...", "c".repeat(100)));
    assert!(!multi_118.ends_with(" ..."));

    // Repeated short words truncate on a whole word boundary without mid-word cuts.
    let repeated = "word ".repeat(40);
    let repeated_preview = timeline::accessible_preview(&repeated);
    assert!(repeated_preview.chars().count() <= 120);
    assert!(repeated_preview.ends_with("word..."));
    assert!(!repeated_preview.ends_with(" ..."));
}
