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

        // Model footer shows the model used for the run on the rendered Select,
        // never "No model is configured", without repeating the value in label.
        let model_select = window.find("session-model-select");
        assert_eq!(model_select.label(), Some("Model for this session"));
        assert_eq!(model_select.value(), Some("Anthropic · claude-sonnet-4"));
        assert_ne!(model_select.value(), Some("No model is configured"));

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
            window.find("session-model-select").label(),
            Some("Model for this session")
        );
        assert_eq!(
            window.find("session-model-select").value(),
            Some("OpenAI · gpt-5")
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

        let session_id = view.active_session.id;
        let fallback = AgentRunSnapshotProjection {
            run: run_snapshot_with_times(
                session_id,
                run_id,
                anthropic_sonnet(),
                AgentRunState::Evaluating,
                started_at,
                paused_at,
                None,
            ),
            plan: Vec::new(),
            plan_progress: Default::default(),
            messages: Vec::new(),
            message_timeline_ordinals: Vec::new(),
            pending_approval: None,
            pending_input: None,
            usage: Default::default(),
            activities: Vec::new(),
        };
        let events = vec![ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(2),
            session_id,
            event: ServerEvent::AgentSessionStateChanged {
                previous: AgentSessionState::Evaluating,
                current: AgentSessionState::Paused,
            },
        }];

        // Replaying a historical pause that arrived as a session state event
        // keeps run state and session state in lockstep and preserves the
        // historical pause timestamp instead of climbing to wall-clock now.
        view.rebuild_projection_from_events_after(
            events,
            Some(EventSequence::new(2)),
            Some(EventSequence::new(1)),
            None,
            Some(fallback),
        );
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

        // Model footer shows the run's model on the rendered Select, not "No model is configured".
        assert_eq!(
            window.find("session-model-select").label(),
            Some("Model for this session")
        );
        assert_eq!(
            window.find("session-model-select").value(),
            Some("Anthropic · claude-sonnet-4")
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
            window.find("session-model-select").label(),
            Some("Model for this session")
        );
        assert_eq!(
            window.find("session-model-select").value(),
            Some("No model is configured")
        );
        let _ = view;
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

#[gpui_kit::test]
fn live_continuation_after_settled_run_keeps_polling_and_shows_running(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let now_ms = Timestamp::now().as_unix_millis();
    let started_at = Timestamp::from_unix_millis(now_ms.saturating_sub(60_000));
    let completed_at = Timestamp::from_unix_millis(started_at.as_unix_millis() + 10_000);
    let run_id = RunId::new();

    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let mut settled = run_snapshot_with_times(
            view.active_session.id,
            run_id,
            anthropic_sonnet(),
            AgentRunState::Completed,
            started_at,
            completed_at,
            Some(completed_at),
        );
        settled.summary = Some("Previous turn complete".to_owned());
        view.adopt_active_run(settled);
        assert!(!view.run_is_active());
        assert!(!view.run_should_poll());

        // Event-driven continuation (child wake, collaborator message, retry)
        // emits RunStateChanged without a new RunStarted snapshot.
        view.consume_agent_event(&AgentEvent::RunStateChanged {
            run_id,
            state: AgentRunState::Executing,
        });
        view
    });

    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);

        assert_eq!(window.find("run-status-label").label(), Some("Working"));
        assert!(window.try_find("loom-mark").is_some());
        assert_eq!(
            window.find("run-status-interrupt-hint").label(),
            Some("esc to interrupt")
        );
        assert!(window.try_find("pause-run").is_some());
        assert!(window.try_find("interrupt-run").is_some());
        assert!(window.try_find(("project-activity", 0usize)).is_some());

        view.update(cx, |view, _| {
            assert!(view.run_is_active());
            assert!(view.run_should_poll());
            assert_eq!(view.session_state, AgentSessionState::Executing);
            assert_eq!(view.active_session.state, AgentSessionState::Executing);
            let run = view.active_run.as_ref().expect("run remains attached");
            assert!(run.completed_at.is_none());
            assert!(run.summary.is_none());

            // Clear run so no poll timer lingers past the test.
            view.active_run_id = None;
            view.active_run = None;
            view.run_state = None;
            view.session_state = AgentSessionState::Idle;
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn archived_idle_and_queued_states_are_not_overwritten_by_previous_run(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let now_ms = Timestamp::now().as_unix_millis();
    let started_at = Timestamp::from_unix_millis(now_ms.saturating_sub(20_000));
    let settled_at = Timestamp::from_unix_millis(started_at.as_unix_millis() + 5_000);

    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let completed = run_snapshot_with_times(
            view.active_session.id,
            RunId::new(),
            anthropic_sonnet(),
            AgentRunState::Completed,
            started_at,
            settled_at,
            Some(settled_at),
        );
        view.adopt_active_run(completed);
        view
    });

    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();

        // 1. Queued session state is not overwritten by the previous Completed run
        // and does not render the previous run's elapsed timer.
        view.update(cx, |view, _| {
            view.consume_event(&ServerEvent::AgentSessionStateChanged {
                previous: AgentSessionState::Completed,
                current: AgentSessionState::Queued,
            });
        });
        window.render_frame(cx);
        assert_eq!(window.find("run-status-label").label(), Some("Queued"));
        assert!(window.try_find("run-status-elapsed").is_none());
        assert!(window.try_find("pause-run").is_none());
        assert!(window.try_find("interrupt-run").is_none());
        view.read_with(cx, |view, _| {
            assert_eq!(view.session_state, AgentSessionState::Queued);
            assert_eq!(view.active_session.state, AgentSessionState::Queued);
            assert_eq!(view.session_run_presentation().elapsed_ms, None);
            assert!(!view.run_should_poll());
        });

        // 2. Remote archive via workspace lifecycle feed clears a Paused run's
        // live state without dropping `active_run_id` (needed for transcript
        // paging and `run_model`) and does not get reverted on the next render.
        let paused_run_id = RunId::new();
        view.update(cx, |view, cx| {
            let paused = run_snapshot_with_times(
                view.active_session.id,
                paused_run_id,
                anthropic_sonnet(),
                AgentRunState::Paused,
                started_at,
                settled_at,
                None,
            );
            view.adopt_active_run(paused);
            let session_id = view.active_session.id;
            let node_id = view.default_backend_node_id.clone();
            view.apply_workspace_lifecycle_events(
                &node_id,
                vec![ServerEventEnvelope {
                    protocol_version: CURRENT_PROTOCOL_VERSION,
                    sequence: EventSequence::new(9),
                    session_id,
                    event: ServerEvent::AgentSessionArchived { session_id },
                }],
                cx,
            );
        });
        window.render_frame(cx);
        assert!(window.try_find("run-status-elapsed").is_none());
        assert!(window.try_find("pause-run").is_none());
        assert!(window.try_find("resume-run").is_none());
        assert!(window.try_find("interrupt-run").is_none());
        view.read_with(cx, |view, _| {
            let presentation = view.session_run_presentation();
            assert_eq!(view.session_state, AgentSessionState::Archived);
            assert_eq!(view.active_session.state, AgentSessionState::Archived);
            assert_eq!(view.sessions[0].state, AgentSessionState::Archived);
            assert_eq!(view.active_run_id, Some(paused_run_id));
            assert_eq!(presentation.elapsed_ms, None);
            assert_eq!(presentation.run_model, Some(anthropic_sonnet()));
            assert!(!view.run_should_poll());
        });

        // 3. Restored Idle state stays Idle across renders with no stale timer,
        // while keeping `active_run_id` / `active_run` intact for transcript
        // paging and follow-up messages.
        view.update(cx, |view, _| {
            let session_id = view.active_session.id;
            view.consume_event(&ServerEvent::AgentSessionRestored { session_id });
        });
        window.render_frame(cx);
        assert_eq!(window.find("run-status-label").label(), Some("Ready"));
        assert!(window.try_find("run-status-elapsed").is_none());
        view.read_with(cx, |view, _| {
            let presentation = view.session_run_presentation();
            assert_eq!(view.session_state, AgentSessionState::Idle);
            assert_eq!(view.active_session.state, AgentSessionState::Idle);
            assert_eq!(view.active_run_id, Some(paused_run_id));
            assert!(view.active_run.is_some());
            assert_eq!(presentation.elapsed_ms, None);
            assert_eq!(presentation.run_model, Some(anthropic_sonnet()));
            assert!(!view.run_should_poll());
        });

        // 4. Reloading an Archived session at head with a historical fallback
        // run projection preserves Archived state without clobbering it back
        // to Completed or dropping `active_run_id`.
        let historical_run_id = RunId::new();
        view.update(cx, |view, _| {
            let session_id = view.active_session.id;
            view.active_session.state = AgentSessionState::Archived;
            view.session_state = AgentSessionState::Archived;
            let archived_fallback = AgentRunSnapshotProjection {
                run: run_snapshot_with_times(
                    session_id,
                    historical_run_id,
                    anthropic_sonnet(),
                    AgentRunState::Completed,
                    started_at,
                    settled_at,
                    Some(settled_at),
                ),
                plan: Vec::new(),
                plan_progress: Default::default(),
                messages: Vec::new(),
                message_timeline_ordinals: Vec::new(),
                pending_approval: None,
                pending_input: None,
                usage: UsageSnapshot::default(),
                activities: Vec::new(),
            };
            view.rebuild_projection_from_events_after(
                Vec::new(),
                Some(EventSequence::new(10)),
                Some(EventSequence::new(10)),
                Some(AgentSessionState::Archived),
                Some(archived_fallback),
            );
        });
        window.render_frame(cx);
        assert!(window.try_find("run-status-elapsed").is_none());
        assert!(window.try_find("pause-run").is_none());
        assert!(window.try_find("resume-run").is_none());
        assert!(window.try_find("interrupt-run").is_none());
        view.read_with(cx, |view, _| {
            let presentation = view.session_run_presentation();
            assert_eq!(view.session_state, AgentSessionState::Archived);
            assert_eq!(view.active_session.state, AgentSessionState::Archived);
            assert_eq!(view.active_run_id, Some(historical_run_id));
            assert_eq!(presentation.run_state, None);
            assert_eq!(presentation.elapsed_ms, None);
            assert_eq!(presentation.run_model, Some(anthropic_sonnet()));
        });

        // 5. Reopening a restored Idle session at head with a historical Failed
        // fallback run must stay Idle ("Ready"), hide elapsed time and Retry,
        // and preserve `active_run_id` for transcript paging.
        let failed_run_id = RunId::new();
        view.update(cx, |view, _| {
            let session_id = view.active_session.id;
            view.active_session.state = AgentSessionState::Idle;
            view.session_state = AgentSessionState::Idle;
            let failed_fallback = AgentRunSnapshotProjection {
                run: run_snapshot_with_times(
                    session_id,
                    failed_run_id,
                    anthropic_sonnet(),
                    AgentRunState::Failed,
                    started_at,
                    settled_at,
                    Some(settled_at),
                ),
                plan: Vec::new(),
                plan_progress: Default::default(),
                messages: Vec::new(),
                message_timeline_ordinals: Vec::new(),
                pending_approval: None,
                pending_input: None,
                usage: UsageSnapshot::default(),
                activities: Vec::new(),
            };
            view.rebuild_projection_from_events_after(
                Vec::new(),
                Some(EventSequence::new(12)),
                Some(EventSequence::new(12)),
                Some(AgentSessionState::Idle),
                Some(failed_fallback),
            );
        });
        window.render_frame(cx);
        assert_eq!(window.find("run-status-label").label(), Some("Ready"));
        assert!(window.try_find("run-status-elapsed").is_none());
        assert!(window.try_find("retry-run").is_none());
        assert!(window.try_find(("project-status", 0usize)).is_none());
        view.read_with(cx, |view, _| {
            let presentation = view.session_run_presentation();
            assert_eq!(view.session_state, AgentSessionState::Idle);
            assert_eq!(view.active_session.state, AgentSessionState::Idle);
            assert_eq!(view.active_run_id, Some(failed_run_id));
            assert_eq!(presentation.run_state, None);
            assert_eq!(presentation.elapsed_ms, None);
        });

        // 6. Resyncing a Queued session via `apply_event_snapshot_with_durable`
        // at head (`latest_sequence == durable_sequence`) with a historical
        // Completed fallback run preserves Queued and hides the old timer.
        let queued_fallback_run_id = RunId::new();
        view.update(cx, |view, _| {
            let session_id = view.active_session.id;
            let completed_fallback = AgentRunSnapshotProjection {
                run: run_snapshot_with_times(
                    session_id,
                    queued_fallback_run_id,
                    anthropic_sonnet(),
                    AgentRunState::Completed,
                    started_at,
                    settled_at,
                    Some(settled_at),
                ),
                plan: Vec::new(),
                plan_progress: Default::default(),
                messages: Vec::new(),
                message_timeline_ordinals: Vec::new(),
                pending_approval: None,
                pending_input: None,
                usage: UsageSnapshot::default(),
                activities: Vec::new(),
            };
            view.apply_event_snapshot_with_durable(
                Vec::new(),
                EventSequence::new(15),
                Some(EventSequence::new(15)),
                Some(AgentSessionState::Queued),
                Some(completed_fallback),
                Some(AgentSessionState::Queued),
            );
        });
        window.render_frame(cx);
        assert_eq!(window.find("run-status-label").label(), Some("Queued"));
        assert!(window.try_find("run-status-elapsed").is_none());
        view.read_with(cx, |view, _| {
            assert_eq!(view.session_state, AgentSessionState::Queued);
            assert_eq!(view.active_session.state, AgentSessionState::Queued);
            assert_eq!(view.active_run_id, Some(queued_fallback_run_id));
            assert_eq!(view.session_run_presentation().elapsed_ms, None);
        });

        // 7. Archiving and restoring a NeedsInput or Paused run must route a
        // follow-up composer message via StartRun (never AnswerAgentQuestion or
        // DirectAgent against stale pre-archive run state).
        for pre_archive_state in [AgentRunState::NeedsInput, AgentRunState::Paused] {
            view.update(cx, |view, _| {
                let run = run_snapshot_with_times(
                    view.active_session.id,
                    RunId::new(),
                    anthropic_sonnet(),
                    pre_archive_state,
                    started_at,
                    settled_at,
                    None,
                );
                let run_revision = loom_protocol::RunRevision {
                    attempt_id: run.attempt_id,
                    control_revision: run.control_revision,
                    action_sequence: 1,
                };
                view.collaboration.observe_revision(run.id, run_revision);
                view.adopt_active_run(run);
                if pre_archive_state == AgentRunState::NeedsInput {
                    view.pending_input = Some("Which branch?".to_owned());
                }
                let session_id = view.active_session.id;
                view.consume_event(&ServerEvent::AgentSessionArchived { session_id });
                view.consume_event(&ServerEvent::AgentSessionRestored { session_id });
            });
            view.read_with(cx, |view, _| {
                assert_eq!(
                    view.composer_route_for("start fresh"),
                    crate::collaboration::ComposerRoute::StartRun,
                    "restored session after {pre_archive_state:?} must route via StartRun"
                );
                assert_eq!(
                    view.active_run.as_ref().map(|run| run.state),
                    Some(AgentRunState::Cancelled)
                );
            });
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn pending_input_and_approval_clear_when_run_goes_active_again(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let now = Timestamp::now();
    let run_id = RunId::new();

    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        view.review.open = true;
        view.review.tab = InspectorTab::Run;
        let run = run_snapshot_with_times(
            view.active_session.id,
            run_id,
            anthropic_sonnet(),
            AgentRunState::NeedsInput,
            now,
            now,
            None,
        );
        view.adopt_active_run(run);
        view.pending_input = Some("Which target?".to_owned());
        view
    });

    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        assert!(window.try_find("tab-attention").is_some());

        // Collaborator answers and the run transitions back to Executing.
        view.update(cx, |view, _| {
            let attempt_id = view.active_run.as_ref().unwrap().attempt_id;
            view.consume_agent_event(&AgentEvent::UserMessage {
                run_id,
                attempt_id,
                control_revision: 3,
                interaction_id: None,
                text: "Use main".to_owned(),
            });
            view.consume_agent_event(&AgentEvent::RunStateChanged {
                run_id,
                state: AgentRunState::Executing,
            });
        });
        window.render_frame(cx);
        assert_eq!(window.find("run-status-label").label(), Some("Working"));
        assert!(window.try_find("tab-attention").is_none());
        view.read_with(cx, |view, _| {
            assert!(view.pending_input.is_none());
            assert!(!view.session_run_presentation().has_pending_input);
        });

        // A pending approval clears as soon as the run goes active again.
        view.update(cx, |view, _| {
            let attempt_id = view.active_run.as_ref().unwrap().attempt_id;
            view.consume_agent_event(&AgentEvent::ToolApprovalRequired {
                run_id,
                attempt_id,
                control_revision: 4,
                interaction_id: loom_core::InteractionId::new(),
                call: ToolCall {
                    id: ToolCallId::new(),
                    name: "run_command".to_owned(),
                    arguments: serde_json::json!({"command": "cargo test"}),
                },
            });
            view.consume_agent_event(&AgentEvent::RunStateChanged {
                run_id,
                state: AgentRunState::AwaitingApproval,
            });
        });
        window.render_frame(cx);
        assert!(window.try_find("tab-attention").is_some());

        view.update(cx, |view, _| {
            view.consume_agent_event(&AgentEvent::RunStateChanged {
                run_id,
                state: AgentRunState::Executing,
            });
        });
        window.render_frame(cx);
        assert!(window.try_find("tab-attention").is_none());
        view.update(cx, |view, _| {
            assert!(view.pending_approval.is_none());
            assert!(!view.session_run_presentation().has_pending_approval);

            view.active_run_id = None;
            view.active_run = None;
            view.run_state = None;
            view.session_state = AgentSessionState::Idle;
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn elapsed_timer_never_snaps_backwards_on_live_pause_or_block_and_clears_cross_run_usage(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let now_ms = Timestamp::now().as_unix_millis();
    let started_at = Timestamp::from_unix_millis(now_ms.saturating_sub(30_000));
    let run_id = RunId::new();

    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        // At RunStarted, updated_at == started_at and step 1 recorded 5s usage.
        let run = run_snapshot_with_times(
            view.active_session.id,
            run_id,
            anthropic_sonnet(),
            AgentRunState::Executing,
            started_at,
            started_at,
            None,
        );
        view.adopt_active_run(run);
        view.consume_agent_event(&AgentEvent::RunUsageUpdated {
            run_id,
            usage: UsageSnapshot {
                elapsed_ms: 5_000,
                ..UsageSnapshot::default()
            },
        });
        view
    });

    view.update(cx, |view, _| {
        // Live transition to Paused must freeze at the live elapsed duration
        // (~30s), never snapping backwards to 0ms or step 1's 5s usage.
        view.consume_agent_event(&AgentEvent::RunStateChanged {
            run_id,
            state: AgentRunState::Paused,
        });
        let paused_elapsed = view
            .session_run_presentation()
            .elapsed_ms
            .expect("paused run has frozen elapsed time");
        assert!(
            paused_elapsed >= 25_000,
            "live pause snapped backwards to {paused_elapsed}ms"
        );

        // Starting a brand-new run via `adopt_active_run` (as `finish_send_response`
        // does before `AgentEvent::RunStarted` is polled) clears previous per-run
        // usage, plan, and activities immediately so stale elapsed_ms cannot leak
        // into the new run if it pauses before its first usage update.
        view.review.usage.run_provider = Some(loom_model::ProviderUsageSummary::default());
        view.plan = Some(PlanState::new(vec!["Old plan step".to_owned()]));
        view.activity_records.insert(
            loom_core::ActivityId::new(),
            AgentActivityRecord {
                id: loom_core::ActivityId::new(),
                run_id,
                timeline_ordinal: 1,
                parent_id: None,
                step_id: None,
                kind: loom_protocol::AgentActivityKind::ModelTurn,
                status: loom_protocol::AgentActivityStatus::Completed,
                started_at,
                completed_at: Some(started_at),
                elapsed_ms: Some(5_000),
                data: loom_protocol::AgentActivityData::ModelTurn {
                    model: anthropic_sonnet(),
                },
            },
        );
        let next_run_id = RunId::new();
        let next_started = Timestamp::now();
        let next_run = run_snapshot_with_times(
            view.active_session.id,
            next_run_id,
            anthropic_sonnet(),
            AgentRunState::Planning,
            next_started,
            next_started,
            None,
        );
        view.adopt_active_run(next_run.clone());
        assert!(view.review.usage.run.is_none());
        assert!(view.review.usage.run_provider.is_none());
        assert!(view.plan.is_none());
        assert!(view.activity_records.is_empty());

        view.consume_agent_event(&AgentEvent::RunStarted { snapshot: next_run });
        view.consume_agent_event(&AgentEvent::RunStateChanged {
            run_id: next_run_id,
            state: AgentRunState::Paused,
        });
        let next_paused_elapsed = view
            .session_run_presentation()
            .elapsed_ms
            .expect("newly paused run has elapsed time");
        assert!(
            next_paused_elapsed < 5_000,
            "previous run's 5s usage leaked into new run: {next_paused_elapsed}ms"
        );

        // Historical replay uses ActivityRecorded timestamps rather than
        // wall-clock now(), so an old paused run freezes at its activity time,
        // and `apply_run_projection` preserves the projection's own seeded plan
        // and usage after adopting the new run ID.
        let old_start = Timestamp::from_unix_millis(now_ms.saturating_sub(60 * 60 * 1_000));
        let old_activity_end = Timestamp::from_unix_millis(old_start.as_unix_millis() + 42_000);
        let old_run_id = RunId::new();
        let fallback = AgentRunSnapshotProjection {
            run: run_snapshot_with_times(
                view.active_session.id,
                old_run_id,
                anthropic_sonnet(),
                AgentRunState::Executing,
                old_start,
                old_start,
                None,
            ),
            plan: vec![loom_protocol::AgentPlanStep {
                id: "step-1".to_owned(),
                description: "Replayed step".to_owned(),
            }],
            plan_progress: Default::default(),
            messages: Vec::new(),
            message_timeline_ordinals: Vec::new(),
            pending_approval: None,
            pending_input: None,
            usage: UsageSnapshot {
                elapsed_ms: 12_000,
                ..UsageSnapshot::default()
            },
            activities: Vec::new(),
        };
        let activity = AgentActivityRecord {
            id: loom_core::ActivityId::new(),
            run_id: old_run_id,
            timeline_ordinal: 1,
            parent_id: None,
            step_id: None,
            kind: loom_protocol::AgentActivityKind::ModelTurn,
            status: loom_protocol::AgentActivityStatus::Completed,
            started_at: old_start,
            completed_at: Some(old_activity_end),
            elapsed_ms: Some(42_000),
            data: loom_protocol::AgentActivityData::ModelTurn {
                model: anthropic_sonnet(),
            },
        };
        let events = vec![
            ServerEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(2),
                session_id: view.active_session.id,
                event: ServerEvent::Agent {
                    event: AgentEvent::ActivityRecorded {
                        run_id: old_run_id,
                        activity,
                    },
                },
            },
            ServerEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence: EventSequence::new(3),
                session_id: view.active_session.id,
                event: ServerEvent::Agent {
                    event: AgentEvent::RunStateChanged {
                        run_id: old_run_id,
                        state: AgentRunState::Paused,
                    },
                },
            },
        ];
        view.rebuild_projection_from_events_after(
            events,
            Some(EventSequence::new(3)),
            Some(EventSequence::new(1)),
            None,
            Some(fallback),
        );
        assert_eq!(view.session_run_presentation().elapsed_ms, Some(42_000));
        assert!(view.plan.is_some());
        assert_eq!(
            view.review.usage.run.as_ref().map(|u| u.elapsed_ms),
            Some(12_000)
        );

        view.active_run_id = None;
        view.active_run = None;
        view.run_state = None;
        view.session_state = AgentSessionState::Idle;
    });
}

#[gpui_kit::test]
fn waiting_for_children_preserves_resume_on_child_and_root_and_never_overrides_failed(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let now = Timestamp::now();
    let root_id = AgentSessionId::new();

    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let child_id = view.active_session.id;
        let project_id = ProjectId::from_uuid(*root_id.as_uuid());
        let mut root_session = view.active_session.clone();
        root_session.id = root_id;
        root_session.name = "Root project".to_owned();
        view.sessions = vec![root_session, view.active_session.clone()];
        view.project_snapshot = Some(ProjectSnapshot {
            project_id,
            root_session_id: root_id,
            parent_project_id: None,
            agents: vec![
                ProjectAgentRecord {
                    session_id: root_id,
                    project_id,
                    parent_session_id: None,
                    depth: 1,
                    state: AgentSessionState::Paused,
                    task_summary: None,
                    output_cursor: EventSequence::default(),
                    updated_at: now,
                },
                ProjectAgentRecord {
                    session_id: child_id,
                    project_id,
                    parent_session_id: Some(root_id),
                    depth: 2,
                    state: AgentSessionState::Paused,
                    task_summary: None,
                    output_cursor: EventSequence::default(),
                    updated_at: now,
                },
            ],
            tasks: Vec::new(),
            worktrees: Vec::new(),
        });
        let child_run = run_snapshot_with_times(
            child_id,
            RunId::new(),
            anthropic_sonnet(),
            AgentRunState::Paused,
            now,
            now,
            None,
        );
        view.adopt_active_run(child_run);
        view
    });

    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        // 1. Paused delegated child session must say "Paused" and allow Resume.
        window.render_frame(cx);
        assert_eq!(window.find("run-status-label").label(), Some("Paused"));
        assert!(window.try_find("resume-run").is_some());
        assert!(view.read(cx).run_can_resume());

        // 2. Switch active session to the root while a child is still live:
        //    Paused root says "Waiting for sub-agents" AND keeps Resume enabled.
        view.update(cx, |view, _| {
            view.active_session.id = root_id;
            let root_run = run_snapshot_with_times(
                root_id,
                RunId::new(),
                anthropic_sonnet(),
                AgentRunState::Paused,
                now,
                now,
                None,
            );
            view.adopt_active_run(root_run);
        });
        window.render_frame(cx);
        assert_eq!(
            window.find("run-status-label").label(),
            Some("Waiting for sub-agents")
        );
        assert!(window.try_find("resume-run").is_some());
        assert!(view.read(cx).run_can_resume());

        // 3. Failed root with live children must report the failure, never
        //    "Waiting for sub-agents".
        view.update(cx, |view, _| {
            let mut failed_run = run_snapshot_with_times(
                root_id,
                RunId::new(),
                anthropic_sonnet(),
                AgentRunState::Failed,
                now,
                now,
                Some(now),
            );
            failed_run.summary = Some(format!(
                "{}provider timed out",
                loom_model::AGENT_RUN_FAILURE_SUMMARY_PREFIX
            ));
            view.adopt_active_run(failed_run);
        });
        window.render_frame(cx);
        assert_eq!(
            window.find("run-status-label").label(),
            Some("Something went wrong")
        );
        assert!(window.try_find("run-failure-reason").is_some());
        assert!(window.try_find("retry-run").is_some());

        view.update(cx, |view, _| {
            view.active_run_id = None;
            view.active_run = None;
            view.run_state = None;
            view.session_state = AgentSessionState::Idle;
            view.project_snapshot = None;
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn snapshot_tail_skips_covered_events_and_mid_run_model_switch_survives_refresh(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let now_ms = Timestamp::now().as_unix_millis();
    let historical_start = Timestamp::from_unix_millis(now_ms.saturating_sub(60 * 60 * 1_000));
    let historical_updated =
        Timestamp::from_unix_millis(historical_start.as_unix_millis() + 45_000);
    let run_id = RunId::new();
    let switched_model = ModelId::for_provider(
        &loom_model::ProviderId::new("openai"),
        &ModelId::new("gpt-5"),
    );

    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _| {
        let session_id = view.active_session.id;
        let fallback = AgentRunSnapshotProjection {
            run: run_snapshot_with_times(
                session_id,
                run_id,
                anthropic_sonnet(),
                AgentRunState::Evaluating,
                historical_start,
                historical_updated,
                None,
            ),
            plan: Vec::new(),
            plan_progress: Default::default(),
            messages: vec![
                loom_model::ModelMessage::new(loom_model::MessageRole::User, "check it"),
                loom_model::ModelMessage::new(loom_model::MessageRole::Assistant, "working on it"),
            ],
            message_timeline_ordinals: vec![0, 1],
            pending_approval: None,
            pending_input: None,
            usage: UsageSnapshot::default(),
            activities: Vec::new(),
        };
        // Tail window contains an already-covered UserMessage at seq 5 (<= durable 5)
        // AND a newer non-state event at seq 7 (so `applied > 0`), while the
        // pause transition at seq 6 aged out before the tail window and is
        // carried by `snapshot_session_state = Some(Paused)` at latest_sequence 8.
        let covered_event = ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(5),
            session_id,
            event: ServerEvent::Agent {
                event: AgentEvent::UserMessage {
                    run_id,
                    attempt_id: fallback.run.attempt_id,
                    control_revision: 2,
                    interaction_id: None,
                    text: "check it".to_owned(),
                },
            },
        };
        let non_state_tail_event = ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence: EventSequence::new(7),
            session_id,
            event: ServerEvent::AgentSessionRenamed {
                session_id,
                name: "Renamed in tail".to_owned(),
                source: loom_core::SessionTitleSource::Auto,
            },
        };
        view.apply_event_snapshot_with_durable(
            vec![covered_event, non_state_tail_event],
            EventSequence::new(8),
            Some(EventSequence::new(5)),
            Some(AgentSessionState::Evaluating),
            Some(fallback),
            Some(AgentSessionState::Paused),
        );
        // Covered UserMessage was not duplicated; non-state tail event applied;
        // newer snapshot session state reconciled even with applied > 0, and
        // preserved the historical elapsed duration rather than climbing to now().
        let user_rows = view
            .timeline
            .iter()
            .filter(|item| matches!(item, TimelineItem::User(_)))
            .count();
        assert_eq!(user_rows, 1);
        assert_eq!(view.active_session.name, "Renamed in tail");
        assert_eq!(view.run_state, Some(AgentRunState::Paused));
        assert_eq!(view.session_state, AgentSessionState::Paused);
        assert_eq!(view.session_run_presentation().elapsed_ms, Some(45_000));

        // Mid-run model switch queued in self.model must not revert to
        // active_run.model when self.models is cleared during refresh.
        view.model = switched_model.clone();
        view.models.clear();
        assert_eq!(view.model_picker_placeholder(), "OpenAI · gpt-5");

        view.active_run_id = None;
        view.active_run = None;
        view.run_state = None;
        view.session_state = AgentSessionState::Idle;
    });
}

#[gpui_kit::test]
fn poll_resync_snapshot_syncs_session_state_across_polling_footer_and_sidebar(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let now = Timestamp::now();

    let handle = cx.open_window(size(px(1280.), px(800.)), |_, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let run = run_snapshot_with_times(
            view.active_session.id,
            RunId::new(),
            anthropic_sonnet(),
            AgentRunState::Evaluating,
            now,
            now,
            None,
        );
        view.adopt_active_run(run);
        view
    });

    cx.update_window(handle.into(), |view, window, cx| {
        let view = view.downcast::<LoomView>().unwrap();
        window.render_frame(cx);
        assert_eq!(window.find("run-status-label").label(), Some("Reviewing"));
        assert!(window.try_find("loom-mark").is_some());

        // 1. Poll resync snapshot transitions Evaluating -> Idle while the
        // durable reload is in flight: must stop polling and stay Idle across
        // a render frame without sync_project_list reverting it to Evaluating.
        view.update(cx, |view, _| {
            let mut snapshot_session = view.active_session.clone();
            snapshot_session.state = AgentSessionState::Idle;
            view.reset_for_poll_resync(snapshot_session, Some("epoch-2".to_owned()));
            assert!(!view.run_is_active());
            assert!(!view.run_should_poll());
            assert_eq!(view.session_state, AgentSessionState::Idle);
        });
        window.render_frame(cx);
        assert_eq!(window.find("run-status-label").label(), Some("Ready"));
        assert!(window.try_find("loom-mark").is_none());
        assert!(window.try_find(("project-activity", 0usize)).is_none());
        view.read_with(cx, |view, _| {
            assert_eq!(view.session_state, AgentSessionState::Idle);
            assert_eq!(view.active_session.state, AgentSessionState::Idle);
            assert_eq!(view.sessions[0].state, AgentSessionState::Idle);
            assert!(!view.run_is_active());
            assert!(!view.run_should_poll());
        });

        // 2. Poll resync snapshot transitions Evaluating -> Completed across a
        // render frame: stops polling and shows Complete without loom-mark.
        view.update(cx, |view, _| {
            let run = run_snapshot_with_times(
                view.active_session.id,
                RunId::new(),
                anthropic_sonnet(),
                AgentRunState::Evaluating,
                now,
                now,
                None,
            );
            view.adopt_active_run(run);
            let mut completed_session = view.active_session.clone();
            completed_session.state = AgentSessionState::Completed;
            view.reset_for_poll_resync(completed_session, Some("epoch-3".to_owned()));
            assert!(!view.run_is_active());
            assert!(!view.run_should_poll());
        });
        window.render_frame(cx);
        assert_eq!(window.find("run-status-label").label(), Some("Complete"));
        assert!(window.try_find("loom-mark").is_none());
        assert!(window.try_find(("project-activity", 0usize)).is_none());
        view.read_with(cx, |view, _| {
            assert_eq!(view.session_state, AgentSessionState::Completed);
            assert_eq!(view.active_session.state, AgentSessionState::Completed);
            assert_eq!(view.sessions[0].state, AgentSessionState::Completed);
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn per_run_pending_usage_and_plan_state_do_not_leak_across_run_switches_or_stale_tail_replay(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let now_ms = Timestamp::now().as_unix_millis();
    let run1_started = Timestamp::from_unix_millis(now_ms.saturating_sub(60_000));
    let run2_started = Timestamp::from_unix_millis(now_ms.saturating_sub(5_000));
    let run1_id = RunId::new();
    let run2_id = RunId::new();

    let view = cx.new(|cx| LoomView::new_for_test(cx.focus_handle()));
    view.update(cx, |view, _| {
        let session_id = view.active_session.id;
        let run1 = run_snapshot_with_times(
            session_id,
            run1_id,
            anthropic_sonnet(),
            AgentRunState::AwaitingApproval,
            run1_started,
            run1_started,
            None,
        );
        view.adopt_active_run(run1.clone());
        view.pending_approval = Some(ToolCall {
            id: ToolCallId::new(),
            name: "run_command".to_owned(),
            arguments: serde_json::json!({"command": "rm -rf /tmp/old"}),
        });
        view.pending_input = Some("Old question".to_owned());
        view.approval_request_in_flight = true;
        view.plan = Some(PlanState::new(vec!["Old step".to_owned()]));
        view.review.usage.run = Some(UsageSnapshot {
            elapsed_ms: 15_000,
            input_tokens: 999,
            ..UsageSnapshot::default()
        });

        // Adopting a different run in Paused must clear the prior run's pending
        // approval, pending input, in-flight approval flag, plan, and usage.
        let run2 = run_snapshot_with_times(
            session_id,
            run2_id,
            anthropic_sonnet(),
            AgentRunState::Paused,
            run2_started,
            run2_started,
            None,
        );
        view.adopt_active_run(run2.clone());
        assert!(view.pending_approval.is_none());
        assert!(view.pending_input.is_none());
        assert!(!view.approval_request_in_flight);
        assert!(view.plan.is_none());
        assert!(view.review.usage.run.is_none());

        // Replaying stale tail events from run1 while run2 is active must not
        // mutate run2's plan, pending approval/input, usage, or run state.
        view.consume_agent_event(&AgentEvent::PlanProposed {
            run_id: run1_id,
            plan: loom_protocol::AgentPlan {
                title: Some("Stale plan".to_owned()),
                steps: vec![loom_protocol::AgentPlanStep {
                    id: "old".to_owned(),
                    description: "Stale run1 step".to_owned(),
                }],
            },
        });
        view.consume_agent_event(&AgentEvent::ToolApprovalRequired {
            run_id: run1_id,
            attempt_id: run1.attempt_id,
            control_revision: 2,
            interaction_id: loom_core::InteractionId::new(),
            call: ToolCall {
                id: ToolCallId::new(),
                name: "run_command".to_owned(),
                arguments: serde_json::json!({"command": "echo stale"}),
            },
        });
        view.consume_agent_event(&AgentEvent::NeedsInput {
            run_id: run1_id,
            attempt_id: run1.attempt_id,
            control_revision: 3,
            interaction_id: loom_core::InteractionId::new(),
            prompt: "Stale run1 prompt".to_owned(),
        });
        view.consume_agent_event(&AgentEvent::RunUsageUpdated {
            run_id: run1_id,
            usage: UsageSnapshot {
                elapsed_ms: 44_000,
                input_tokens: 1_234,
                ..UsageSnapshot::default()
            },
        });
        let run1_completed = run_snapshot_with_times(
            session_id,
            run1_id,
            anthropic_sonnet(),
            AgentRunState::Completed,
            run1_started,
            run1_started,
            Some(run1_started),
        );
        view.consume_agent_event(&AgentEvent::RunCompleted {
            snapshot: run1_completed,
        });

        assert_eq!(view.active_run_id, Some(run2_id));
        assert_eq!(view.run_state, Some(AgentRunState::Paused));
        assert!(view.plan.is_none());
        assert!(view.pending_approval.is_none());
        assert!(view.pending_input.is_none());
        assert!(view.review.usage.run.is_none());

        view.active_run_id = None;
        view.active_run = None;
        view.run_state = None;
        view.session_state = AgentSessionState::Idle;
    });
}
