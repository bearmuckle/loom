//! Regression tests for issue #449: session header/inspector, sidebar pills,
//! footer status, elapsed timer, composer model line, and Pause/Stop controls
//! must derive from a single source of run state and agree.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::{EventSequence, ProjectAgentRecord, ProjectId, ProjectSnapshot};
use loom_protocol::{AgentRunSnapshot, AgentRunSnapshotProjection, ServerEventEnvelope};

fn tools_turn(total_ms: u64) -> TimelineItem {
    let first = total_ms / 2;
    let second = total_ms - first;
    TimelineItem::Assistant(AssistantTurn {
        parts: vec![
            AssistantPart::Tool(Box::new(ToolPart {
                id: ToolCallId::new(),
                name: "run_command".to_owned(),
                title: "cargo test".to_owned(),
                status: ToolPartStatus::Completed,
                detail: Some("cargo test".to_owned()),
                output: Some("ok".to_owned()),
                elapsed_ms: Some(first),
                approval_pending: false,
                policy: None,
            })),
            AssistantPart::Tool(Box::new(ToolPart {
                id: ToolCallId::new(),
                name: "apply_patch".to_owned(),
                title: "Edit view.rs".to_owned(),
                status: ToolPartStatus::Completed,
                detail: Some("crates/loom-ui/src/view.rs".to_owned()),
                output: Some("updated".to_owned()),
                elapsed_ms: Some(second),
                approval_pending: false,
                policy: None,
            })),
        ],
        streaming: false,
    })
}

fn run_snapshot_with_times(
    session_id: AgentSessionId,
    run_id: RunId,
    model: ModelId,
    state: AgentRunState,
    started_at: Timestamp,
    updated_at: Timestamp,
    completed_at: Option<Timestamp>,
) -> AgentRunSnapshot {
    AgentRunSnapshot {
        id: run_id,
        attempt_id: loom_core::RunAttemptId::new(),
        control_revision: 2,
        session_id,
        task: "Harden loom-mcp against review findings".to_owned(),
        jev: None,
        model,
        state,
        started_at,
        updated_at,
        completed_at,
        summary: None,
        evidence: Vec::new(),
        approval_policy: None,
    }
}

fn anthropic_sonnet() -> ModelId {
    ModelId::for_provider(
        &loom_model::ProviderId::new("anthropic"),
        &ModelId::new("claude-sonnet-4"),
    )
}

#[gpui_kit::test]
fn finished_after_tools_state_agrees_across_footer_pills_timer_composer_and_controls(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let now_ms = Timestamp::now().as_unix_millis();
    let started_at = Timestamp::from_unix_millis(now_ms.saturating_sub(41 * 60 * 1_000));
    let completed_at = Timestamp::from_unix_millis(started_at.as_unix_millis() + 103_000);
    let run_id = RunId::new();

    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.models.clear();
        view.default_models.clear();
        view.review.open = true;
        view.review.tab = InspectorTab::Run;

        let session_id = view.active_session.id;
        // Repro the race: the durable snapshot was read while the run was still
        // Evaluating (and even had stale pending input), and the event batch
        // after its cursor delivered RunCompleted.
        let stale_fallback = AgentRunSnapshotProjection {
            run: run_snapshot_with_times(
                session_id,
                run_id,
                anthropic_sonnet(),
                AgentRunState::Evaluating,
                started_at,
                completed_at,
                None,
            ),
            plan: Vec::new(),
            plan_progress: Default::default(),
            messages: Vec::new(),
            message_timeline_ordinals: Vec::new(),
            pending_approval: None,
            pending_input: Some("Old question".to_owned()),
            usage: Default::default(),
            activities: Vec::new(),
        };
        let completed_snapshot = run_snapshot_with_times(
            session_id,
            run_id,
            anthropic_sonnet(),
            AgentRunState::Completed,
            started_at,
            completed_at,
            Some(completed_at),
        );
        let events = vec![ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(10),
            session_id,
            event: ServerEvent::Agent {
                event: AgentEvent::RunCompleted {
                    snapshot: completed_snapshot,
                },
            },
        }];

        view.rebuild_projection_from_events(
            events,
            Some(EventSequence::new(10)),
            Some(stale_fallback),
        );
        view.timeline.push(tools_turn(103_000));
        view
    });

    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);

        assert_eq!(window.find("run-status-label").label(), Some("Complete"));
        assert_eq!(window.find("run-status-elapsed").label(), Some("1m 43s"));
        assert!(window.try_find("loom-mark").is_none());
        assert!(window.try_find("run-status-interrupt-hint").is_none());
        assert!(window.try_find("pause-run").is_none());
        assert!(window.try_find("resume-run").is_none());
        assert!(window.try_find("retry-run").is_none());
        assert!(window.try_find("interrupt-run").is_none());

        // Inspector header and Run status card agree on Done + frozen duration.
        assert_eq!(
            window.find("inspector-status-summary").label(),
            Some("Done")
        );
        assert_eq!(window.find("run-card-status").label(), Some("Done"));
        assert_eq!(window.find("run-card-elapsed").label(), Some("1m 43s"));

        // Sidebar row has neither active dots nor a Paused pill.
        assert!(window.try_find(("project-activity", 0usize)).is_none());
        assert!(window.try_find(("project-status", 0usize)).is_none());

        // Model footer shows the model used for the run, never "No model is configured".
        let model_label = view.read(cx).model_picker_display_label(cx);
        assert_eq!(model_label, "Anthropic · claude-sonnet-4");
        assert_ne!(model_label, "No model is configured");

        view.read_with(cx, |view, _| {
            assert!(!view.run_can_pause());
            assert!(!view.run_can_interrupt());
            assert!(!view.run_can_resume());
            assert!(!view.run_can_retry());
            assert!(view.pending_input.is_none());
            assert_eq!(view.active_session.state, AgentSessionState::Completed);
            assert_eq!(view.session_state, AgentSessionState::Completed);
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn genuinely_running_state_shows_active_status_timer_hint_and_controls(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let now_ms = Timestamp::now().as_unix_millis();
    let started_at = Timestamp::from_unix_millis(now_ms.saturating_sub(15_000));
    let run_id = RunId::new();
    let openai = ModelId::for_provider(
        &loom_model::ProviderId::new("openai"),
        &ModelId::new("gpt-5"),
    );

    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.models = vec![openai.clone()];
        view.default_models = vec![openai.clone()];
        view.review.open = true;
        view.review.tab = InspectorTab::Run;
        let snapshot = run_snapshot_with_times(
            view.active_session.id,
            run_id,
            openai.clone(),
            AgentRunState::Evaluating,
            started_at,
            started_at,
            None,
        );
        view.adopt_active_run(snapshot);
        view
    });

    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);

        assert_eq!(window.find("run-status-label").label(), Some("Reviewing"));
        assert!(window.try_find("loom-mark").is_some());
        assert_eq!(
            window.find("run-status-interrupt-hint").label(),
            Some("esc to interrupt")
        );
        assert!(window.try_find("run-status-elapsed").is_some());
        assert!(window.try_find("pause-run").is_some());
        assert!(window.try_find("interrupt-run").is_some());
        assert!(window.try_find("resume-run").is_none());
        assert!(window.try_find("retry-run").is_none());

        // Sidebar shows activity dots and no attention pill; inspector agrees.
        assert!(window.try_find(("project-activity", 0usize)).is_some());
        assert!(window.try_find(("project-status", 0usize)).is_none());
        assert_eq!(
            window.find("inspector-status-summary").label(),
            Some("Reviewing")
        );
        assert_eq!(window.find("run-card-status").label(), Some("Reviewing"));
        assert_eq!(
            view.read(cx).model_picker_display_label(cx),
            "OpenAI · gpt-5"
        );

        view.read_with(cx, |view, _| {
            let presentation = view.session_run_presentation();
            assert!(presentation.is_active);
            assert!(presentation.elapsed_ms.is_some_and(|ms| ms >= 10_000));
            assert!(view.run_can_pause());
            assert!(view.run_can_interrupt());
            assert!(!view.run_can_resume());
        });

        // Clear the active run so no poll timer lingers past the test.
        view.update(cx, |view, _| {
            view.active_run_id = None;
            view.active_run = None;
            view.run_state = None;
            view.session_state = AgentSessionState::Idle;
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn paused_state_freezes_timer_hides_pause_and_agrees_across_all_surfaces(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let now_ms = Timestamp::now().as_unix_millis();
    let started_at = Timestamp::from_unix_millis(now_ms.saturating_sub(41 * 60 * 1_000));
    let paused_at = Timestamp::from_unix_millis(started_at.as_unix_millis() + 103_000);
    let run_id = RunId::new();

    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.models.clear();
        view.review.open = true;
        view.review.tab = InspectorTab::Run;

        let snapshot = run_snapshot_with_times(
            view.active_session.id,
            run_id,
            anthropic_sonnet(),
            AgentRunState::Evaluating,
            started_at,
            paused_at,
            None,
        );
        view.adopt_active_run(snapshot);

        // Even when the pause arrives as a session state event rather than a
        // run event, run state and session state must stay in lockstep.
        view.consume_event(&ServerEvent::AgentSessionStateChanged {
            previous: AgentSessionState::Evaluating,
            current: AgentSessionState::Paused,
        });
        view
    });

    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);

        assert_eq!(window.find("run-status-label").label(), Some("Paused"));
        assert_eq!(window.find("run-status-elapsed").label(), Some("1m 43s"));
        assert!(window.try_find("loom-mark").is_none());
        assert!(window.try_find("run-status-interrupt-hint").is_none());
        assert!(window.try_find("pause-run").is_none());
        assert!(window.try_find("resume-run").is_some());
        assert!(window.try_find("interrupt-run").is_some());
        assert!(window.try_find("retry-run").is_none());

        // Sidebar pill and inspector agree on Paused (no active dots).
        assert!(window.try_find(("project-activity", 0usize)).is_none());
        assert_eq!(
            window.find(("project-status", 0usize)).label(),
            Some("Paused")
        );
        assert_eq!(
            window.find("inspector-status-summary").label(),
            Some("Paused")
        );
        assert_eq!(window.find("run-card-status").label(), Some("Paused"));
        assert_eq!(window.find("run-card-elapsed").label(), Some("1m 43s"));

        // Model footer shows the run's model, not "No model is configured".
        assert_eq!(
            view.read(cx).model_picker_display_label(cx),
            "Anthropic · claude-sonnet-4"
        );

        view.read_with(cx, |view, _| {
            assert!(!view.run_can_pause());
            assert!(view.run_can_resume());
            assert!(view.run_can_interrupt());
        });

        // Clear the paused run so no poll timer lingers past the test.
        view.update(cx, |view, _| {
            view.active_run_id = None;
            view.active_run = None;
            view.run_state = None;
            view.session_state = AgentSessionState::Idle;
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn no_model_state_shows_no_model_configured_and_no_run_controls(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.models.clear();
        view.default_models.clear();
        view.model = ModelId::new("default");
        view.default_model = ModelId::new("default");
        view.review.open = true;
        view.review.tab = InspectorTab::Run;
        view
    });

    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);

        assert_eq!(window.find("run-status-label").label(), Some("Ready"));
        assert!(window.try_find("loom-mark").is_none());
        assert!(window.try_find("run-status-elapsed").is_none());
        assert!(window.try_find("run-status-interrupt-hint").is_none());
        assert!(window.try_find("pause-run").is_none());
        assert!(window.try_find("resume-run").is_none());
        assert!(window.try_find("retry-run").is_none());
        assert!(window.try_find("interrupt-run").is_none());
        assert!(window.try_find(("project-activity", 0usize)).is_none());
        assert!(window.try_find(("project-status", 0usize)).is_none());
        assert_eq!(
            window.find("inspector-status-summary").label(),
            Some("Ready")
        );
        assert_eq!(window.find("run-card-status").label(), Some("Ready"));
        assert_eq!(
            view.read(cx).model_picker_display_label(cx),
            "No model is configured"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn run_state_mutations_keep_project_snapshots_and_completed_runs_consistent(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _| {
        let session_id = view.active_session.id;
        let project_id = ProjectId::from_uuid(*session_id.as_uuid());
        view.sessions = vec![view.active_session.clone()];
        view.project_snapshot = Some(ProjectSnapshot {
            project_id,
            root_session_id: session_id,
            parent_project_id: None,
            agents: vec![ProjectAgentRecord {
                session_id,
                project_id,
                parent_session_id: None,
                depth: 1,
                state: AgentSessionState::Executing,
                task_summary: None,
                output_cursor: EventSequence::default(),
                updated_at: Timestamp::from_unix_millis(1),
            }],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        });
        view.project_tree_snapshots = vec![view.project_snapshot.clone().unwrap()];

        // Adopting a run and transitioning it to Completed must update the
        // cached project snapshot too, and a completed timestamp must beat any
        // stale non-terminal run_state.
        let run_id = RunId::new();
        let started_at = Timestamp::from_unix_millis(10_000);
        let completed_at = Timestamp::from_unix_millis(45_000);
        let mut snapshot = run_snapshot_with_times(
            session_id,
            run_id,
            ModelId::new("gpt-5"),
            AgentRunState::Evaluating,
            started_at,
            completed_at,
            Some(completed_at),
        );
        view.adopt_active_run(snapshot.clone());
        let presentation = view.session_run_presentation();
        assert_eq!(presentation.run_state, Some(AgentRunState::Completed));
        assert_eq!(presentation.session_state, AgentSessionState::Completed);
        assert!(!presentation.can_pause);
        assert!(!presentation.can_stop);
        assert!(!presentation.show_interrupt_hint);
        assert_eq!(presentation.elapsed_ms, Some(35_000));

        // Transitioning via set_run_state updates project snapshots in lockstep.
        snapshot.state = AgentRunState::Completed;
        view.adopt_active_run(snapshot);
        assert_eq!(
            view.project_snapshot.as_ref().unwrap().agents[0].state,
            AgentSessionState::Completed
        );
        assert_eq!(
            view.project_tree_snapshots[0].agents[0].state,
            AgentSessionState::Completed
        );
    });
}
