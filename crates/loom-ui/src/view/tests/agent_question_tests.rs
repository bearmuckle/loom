//! The agent's clarification question in the conversation view.
//!
//! An `ask_user` call is a question to answer, so the transcript renders the
//! call as its own component instead of tool activity: the heading and the
//! whole question are expanded (no disclosure to open), the text wraps to the
//! available width, and the choices the call offers answer with one click while
//! the free-text route stays one tap away in the composer. The error and neutral
//! notes keep the generic note card.

use super::loom_view_render_tests::{render_scenario, render_scenario_at};
use super::{
    AgentActivityData, AgentActivityRecord, AgentActivityStatus, LoomView, question_choice_id,
    tool_detail, tool_element_id, tool_title,
};
use crate::state::{
    ASK_USER_TOOL, AssistantPart, AssistantTurn, OutputDetail, SystemNote, SystemTone,
    TimelineItem, ToolPart, ToolPartStatus,
};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, Entity, TestAppContext, prelude::*, px, size};
use loom_core::{ActivityId, RunAttemptId, RunId, Timestamp, ToolCallId};
use loom_model::{ModelId, ToolCall};
use loom_protocol::AgentActivityKind;
use std::cell::RefCell;
use std::rc::Rc;

/// A question long enough to wrap onto several lines at a phone width, so a
/// fixed-width or truncated rendering would not fit it.
const QUESTION: &str = "Which branch should I use for the release? I can cut the \
    version from main now, or wait for the trailing documentation fixes to land and \
    cut it from the release branch once they pass review.";

/// The answers the call offers. A click on one sends exactly its text.
const CHOICES: [&str; 2] = ["Cut it from main now", "Wait for the documentation fixes"];

fn question_call() -> ToolCall {
    ToolCall {
        id: ToolCallId::new(),
        name: ASK_USER_TOOL.to_owned(),
        arguments: serde_json::json!({"prompt": QUESTION, "options": CHOICES}),
    }
}

/// The activity the run records for the call, which is where the question's
/// arguments stay available after a reload.
fn question_activity(call: &ToolCall) -> AgentActivityRecord {
    AgentActivityRecord {
        id: ActivityId::new(),
        run_id: RunId::new(),
        timeline_ordinal: 0,
        parent_id: None,
        step_id: None,
        kind: AgentActivityKind::ToolCall,
        status: AgentActivityStatus::AwaitingInput,
        started_at: Timestamp::from_unix_millis(0),
        completed_at: None,
        elapsed_ms: None,
        data: AgentActivityData::ToolCall {
            call: call.clone(),
            result: None,
        },
    }
}

/// The tool block the event stream produces for the call.
fn question_part(call: &ToolCall) -> ToolPart {
    ToolPart {
        id: call.id,
        name: call.name.clone(),
        title: tool_title(&call.name, &call.arguments),
        status: ToolPartStatus::AwaitingInput,
        detail: tool_detail(call),
        output: None,
        elapsed_ms: None,
        approval_pending: false,
        policy: None,
    }
}

/// The agent turn that holds the question, as the stream leaves it.
fn question_timeline(call: &ToolCall) -> Vec<TimelineItem> {
    vec![TimelineItem::Assistant(AssistantTurn {
        parts: vec![AssistantPart::Tool(Box::new(question_part(call)))],
        streaming: false,
    })]
}

fn error_note(text: &str) -> TimelineItem {
    TimelineItem::System(SystemNote {
        tone: SystemTone::Error,
        heading: Some("save · persistence".to_owned()),
        text: text.to_owned(),
        details: None,
        progress: None,
        retryable: false,
    })
}

fn neutral_note(text: &str) -> TimelineItem {
    TimelineItem::System(SystemNote::status(text.to_owned()))
}

/// The scenario the shared harness renders: a session with an unanswered
/// question, matching the state `AgentEvent::NeedsInput` leaves behind.
fn question_scenario(view: &mut LoomView) {
    let call = question_call();
    let activity = question_activity(&call);
    view.sessions = vec![view.active_session.clone()];
    view.activity_records.insert(activity.id, activity);
    view.timeline = question_timeline(&call);
    view.pending_input = Some(QUESTION.to_owned());
}

/// Opens the view on a timeline, the recorded activities, and the pending
/// question, and returns the window and its entity so a test can click and read
/// focus; the shared `render_scenario` helper renders a frame but hands neither
/// back.
fn open_timeline_view(
    cx: &mut TestAppContext,
    window_size: gpui_kit::Size<gpui_kit::Pixels>,
    timeline: Vec<TimelineItem>,
    activities: Vec<AgentActivityRecord>,
    pending_input: Option<String>,
) -> (AnyWindowHandle, Entity<LoomView>) {
    let rendered_view = Rc::new(RefCell::new(None));
    let rendered_view_for_window = rendered_view.clone();
    let handle = cx.open_window(window_size, move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            // The composer only renders for a session.
            view.sessions = vec![view.active_session.clone()];
            for activity in activities {
                view.activity_records.insert(activity.id, activity);
            }
            view.pending_input = pending_input;
            view.timeline = timeline;
            view
        });
        *rendered_view_for_window.borrow_mut() = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = rendered_view.borrow_mut().take().expect("view opened");
    (handle.into(), view)
}

/// Opens the view on the one question the run is waiting on, with the call's
/// arguments recorded as the stream and the store leave them.
fn open_question_view(
    cx: &mut TestAppContext,
    window_size: gpui_kit::Size<gpui_kit::Pixels>,
) -> (AnyWindowHandle, Entity<LoomView>, ToolCall) {
    let call = question_call();
    let (handle, view) = open_timeline_view(
        cx,
        window_size,
        question_timeline(&call),
        vec![question_activity(&call)],
        Some(QUESTION.to_owned()),
    );
    (handle, view, call)
}

/// The height of the question text block in a freshly opened window.
fn question_text_height(
    cx: &mut TestAppContext,
    window_size: gpui_kit::Size<gpui_kit::Pixels>,
) -> gpui_kit::Pixels {
    let (handle, _view, _call) = open_question_view(cx, window_size);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window
            .find(("agent-question-text", tool_element_id(0, 0)))
            .bounds()
            .size
            .height
    })
    .unwrap()
}

#[gpui_kit::test]
fn the_call_renders_the_whole_question_with_its_choices(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    // The shared harness renders the question at a desktop and a phone width.
    render_scenario(cx, question_scenario);
    render_scenario_at(cx, size(px(390.), px(844.)), question_scenario);

    let wide_height = question_text_height(cx, size(px(1280.), px(800.)));
    let (handle, _view, _call) = open_question_view(cx, size(px(390.), px(844.)));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let key = tool_element_id(0, 0);
        // The heading, the question, the choices, and the free-text action are
        // all in the first frame: there is no disclosure or truncation control
        // to open.
        assert!(window.find(("agent-question", key)).visible());
        assert!(window.find(("agent-question-heading", key)).visible());

        let text = window.find(("agent-question-text", key));
        assert!(text.visible(), "the question text must render expanded");
        assert_eq!(
            text.label(),
            Some(QUESTION),
            "the component must carry the complete question"
        );
        let bounds = text.bounds();
        assert!(
            bounds.size.width > px(0.) && bounds.size.height > px(0.),
            "the question text must not collapse: {bounds:?}"
        );
        assert!(
            bounds.right() <= window.viewport_size().width,
            "the question runs off the phone screen: {bounds:?}"
        );
        // Wrapping to the available width is what proves the question is laid
        // out in full instead of being clipped at a fixed width.
        assert!(
            bounds.size.height > wide_height,
            "the question must wrap onto more lines at a phone width: {bounds:?} vs {wide_height:?}"
        );

        // Every choice is a target of its own and announces the answer it
        // sends.
        for (choice_index, choice) in CHOICES.iter().enumerate() {
            let button = window.find((
                "agent-question-choice",
                question_choice_id(0, 0, choice_index),
            ));
            assert!(button.visible(), "the choice {choice} must render");
            let announced = format!("Answer the agent question with {choice}");
            assert_eq!(button.label(), Some(announced.as_str()));
        }
        assert!(window.find(("agent-question-answer", key)).visible());

        // The call is a question, not tool activity: no tool row carries it and
        // no usage summary counts it.
        assert!(
            window.try_find(("tool", key)).is_none(),
            "the question must not be presented as a tool row"
        );
        assert!(window.try_find(("tool-header", key)).is_none());
        assert!(
            window.try_find(("tool-usage", key)).is_none(),
            "the question must not be counted as tool usage"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn choices_are_finger_sized_on_a_phone_only(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (phone_handle, _phone_view, _call) = open_question_view(cx, size(px(390.), px(844.)));
    let phone = cx
        .update_window(phone_handle, |_, window, cx| {
            window.render_frame(cx);
            let choice = window.find(("agent-question-choice", question_choice_id(0, 0, 0)));
            assert!(choice.visible());
            assert!(
                choice.bounds().right() <= window.viewport_size().width,
                "the choice runs off the phone screen: {:?}",
                choice.bounds()
            );
            let answer = window.find(("agent-question-answer", tool_element_id(0, 0)));
            assert!(answer.visible());
            assert!(
                answer.bounds().right() <= window.viewport_size().width,
                "the free-text action runs off the phone screen: {:?}",
                answer.bounds()
            );
            (choice.bounds().size.height, answer.bounds().size.height)
        })
        .unwrap();

    let (desktop_handle, _desktop_view, _call) = open_question_view(cx, size(px(1280.), px(800.)));
    let desktop = cx
        .update_window(desktop_handle, |_, window, cx| {
            window.render_frame(cx);
            (
                window
                    .find(("agent-question-choice", question_choice_id(0, 0, 0)))
                    .bounds()
                    .size
                    .height,
                window
                    .find(("agent-question-answer", tool_element_id(0, 0)))
                    .bounds()
                    .size
                    .height,
            )
        })
        .unwrap();

    assert!(
        phone.0 >= px(44.),
        "a choice is below the touch target: {:?}",
        phone.0
    );
    assert!(
        phone.1 >= px(44.),
        "the free-text action is below the touch target: {:?}",
        phone.1
    );
    assert!(
        phone.0 > desktop.0,
        "only a phone lays a choice out as a finger-sized target: {:?} vs {:?}",
        phone.0,
        desktop.0
    );
    assert!(
        phone.1 > desktop.1,
        "only a phone lays the free-text action out as a finger-sized target: {:?} vs {:?}",
        phone.1,
        desktop.1
    );
}

#[gpui_kit::test]
fn choosing_a_choice_answers_the_question(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view, _call) = open_question_view(cx, size(px(1280.), px(800.)));
    let run_id = RunId::new();
    cx.update_window(handle, |_, window, cx| {
        // The answer goes to the run that is parked on the question, so the
        // test stands in for that run and for the worker that serves it.
        view.update(cx, |view, cx| {
            view.session_node_ids
                .insert(view.active_session.id, "test-node".to_owned());
            view.active_run_id = Some(run_id);
            view.active_run = Some(loom_protocol::AgentRunSnapshot {
                id: run_id,
                attempt_id: RunAttemptId::new(),
                control_revision: 1,
                session_id: view.active_session.id,
                task: "cut the release".to_owned(),
                model: ModelId::new("deterministic/demo"),
                state: loom_protocol::AgentRunState::NeedsInput,
                started_at: Timestamp::from_unix_millis(1),
                updated_at: Timestamp::from_unix_millis(2),
                completed_at: None,
                summary: None,
                evidence: Vec::new(),
                approval_policy: None,
            });
            view.run_state = Some(loom_protocol::AgentRunState::NeedsInput);
            cx.notify();
        });
        window.render_frame(cx);

        window.click(("agent-question-choice", question_choice_id(0, 0, 1)), cx);
        window.render_frame(cx);
        view.update(cx, |view, _| {
            assert!(
                view.timeline
                    .iter()
                    .any(|item| matches!(item, TimelineItem::User(text) if text == CHOICES[1])),
                "the chosen answer is sent as the user's reply"
            );
            view.active_run_id = None;
            view.run_state = None;
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_choice_sends_nothing_once_the_question_is_answered(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let call = question_call();
    let (handle, view) = open_timeline_view(
        cx,
        size(px(1280.), px(800.)),
        question_timeline(&call),
        vec![question_activity(&call)],
        // The run has moved on, so the question is history.
        None,
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // The question stays readable, but nothing it offered can be sent.
        assert!(
            window
                .find(("agent-question", tool_element_id(0, 0)))
                .visible()
        );
        window.click(("agent-question-choice", question_choice_id(0, 0, 0)), cx);
        window.render_frame(cx);
        view.update(cx, |view, _| {
            assert!(
                !view
                    .timeline
                    .iter()
                    .any(|item| matches!(item, TimelineItem::User(_))),
                "a question the run is no longer waiting on must not send an answer"
            );
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn clicking_the_answer_action_focuses_the_composer(cx: &mut TestAppContext) {
    use gpui_kit::Focusable as _;

    cx.update(gpui_kit::init);
    let (handle, view, _call) = open_question_view(cx, size(px(390.), px(844.)));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // The view focuses the composer as soon as it creates it, so focus has
        // to move away first for the click to be the cause of the refocus.
        view.update(cx, |view, cx| {
            if let Some(filter) = view.session_filter_input.clone() {
                filter.update(cx, |state, cx| state.focus(window, cx));
            }
        });
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert!(!input.read(cx).focus_handle(cx).is_focused(window));
        });

        window.click(("agent-question-answer", tool_element_id(0, 0)), cx);
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert!(
                input.read(cx).focus_handle(cx).is_focused(window),
                "the answer action must focus the composer"
            );
        });

        // Typing afterwards lands in the composer, which is where an answer of
        // the user's own wording is written.
        window.input("use the release branch", cx);
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert_eq!(input.read(cx).value().to_string(), "use the release branch");
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_question_without_choices_offers_the_free_text_answer(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    // A call whose arguments were not recorded keeps the prompt its detail
    // carries and offers no choices at all.
    let call = ToolCall {
        id: ToolCallId::new(),
        name: ASK_USER_TOOL.to_owned(),
        arguments: serde_json::Value::Null,
    };
    let mut part = question_part(&call);
    part.detail = Some(QUESTION.to_owned());
    let (handle, _view) = open_timeline_view(
        cx,
        size(px(1280.), px(800.)),
        vec![TimelineItem::Assistant(AssistantTurn {
            parts: vec![AssistantPart::Tool(Box::new(part))],
            streaming: false,
        })],
        Vec::new(),
        Some(QUESTION.to_owned()),
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window
                .find(("agent-question-text", tool_element_id(0, 0)))
                .label(),
            Some(QUESTION)
        );
        assert!(
            window
                .try_find(("agent-question-choice", question_choice_id(0, 0, 0)))
                .is_none(),
            "a question that offers no choices renders no choice"
        );
        assert!(
            window
                .find(("agent-question-answer", tool_element_id(0, 0)))
                .visible()
        );
    })
    .unwrap();
}

/// A run blocked on the user stays reachable at every output detail level, so
/// the question is not hidden with the rest of the agent's work.
#[gpui_kit::test]
fn the_question_stays_reachable_at_the_low_output_detail(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view, _call) = open_question_view(cx, size(px(1280.), px(800.)));
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.output_detail = OutputDetail::Low;
            cx.notify();
        });
        window.render_frame(cx);
        assert!(
            window
                .find(("agent-question", tool_element_id(0, 0)))
                .visible()
        );
        assert!(
            window
                .find(("agent-question-choice", question_choice_id(0, 0, 0)))
                .visible()
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn error_and_neutral_notes_keep_the_generic_note_card(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, _view) = open_timeline_view(
        cx,
        size(px(390.), px(844.)),
        vec![error_note("write failed"), neutral_note("Working")],
        Vec::new(),
        None,
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(("system-note", 0usize)).visible());
        assert!(window.find(("system-note", 1usize)).visible());
        assert!(
            window
                .try_find(("agent-question", tool_element_id(0, 0)))
                .is_none(),
            "only a question renders the question component"
        );
    })
    .unwrap();
}

/// The same scenario at a desktop width renders the component as well, so the
/// assertions above are not an artefact of the phone branch.
#[gpui_kit::test]
fn question_scenario_renders_the_component(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    render_scenario(cx, question_scenario);
    let (handle, _view, _call) = open_question_view(cx, size(px(1280.), px(800.)));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .find(("agent-question", tool_element_id(0, 0)))
                .visible()
        );
    })
    .unwrap();
}
