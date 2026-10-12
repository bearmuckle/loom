//! The agent's clarification question in the conversation view.
//!
//! An `ask_user` request is a question to answer, so it renders as its own
//! component: the heading and the whole question are expanded (no disclosure
//! to open), the text wraps to the available width, and the answer action
//! takes a phone user to the composer where the reply is typed. The error and
//! neutral notes keep the generic note card.

use super::loom_view_render_tests::{render_scenario, render_scenario_at};
use crate::state::{SystemNote, SystemTone, TimelineItem};
use crate::view::LoomView;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, Entity, TestAppContext, prelude::*, px, size};
use std::cell::RefCell;
use std::rc::Rc;

/// A question long enough to wrap onto several lines at a phone width, so a
/// fixed-width or truncated rendering would not fit it.
const QUESTION: &str = "Which branch should I use for the release? I can cut the \
    version from main now, or wait for the trailing documentation fixes to land and \
    cut it from the release branch once they pass review.";

fn question_note() -> TimelineItem {
    TimelineItem::System(SystemNote {
        tone: SystemTone::Input,
        heading: Some("Agent needs input".to_owned()),
        text: QUESTION.to_owned(),
        retryable: false,
    })
}

fn error_note(text: &str) -> TimelineItem {
    TimelineItem::System(SystemNote {
        tone: SystemTone::Error,
        heading: Some("save · persistence".to_owned()),
        text: text.to_owned(),
        retryable: false,
    })
}

fn neutral_note(text: &str) -> TimelineItem {
    TimelineItem::System(SystemNote::status(text.to_owned()))
}

/// The scenario the shared harness renders: a session with an unanswered
/// question, matching the state `AgentEvent::NeedsInput` leaves behind.
fn question_scenario(view: &mut LoomView) {
    view.sessions = vec![view.active_session.clone()];
    view.timeline = vec![question_note()];
    view.pending_input = Some(QUESTION.to_owned());
}

/// Opens the view on a timeline and returns the window and its entity, so a
/// test can click and read focus; the shared `render_scenario` helper renders a
/// frame but hands neither back.
fn open_view(
    cx: &mut TestAppContext,
    window_size: gpui_kit::Size<gpui_kit::Pixels>,
    timeline: Vec<TimelineItem>,
) -> (AnyWindowHandle, Entity<LoomView>) {
    let rendered_view = Rc::new(RefCell::new(None));
    let rendered_view_for_window = rendered_view.clone();
    let handle = cx.open_window(window_size, move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            // The composer only renders for a session.
            view.sessions = vec![view.active_session.clone()];
            // A pending question is what makes the answer action meaningful.
            view.pending_input = timeline.iter().find_map(|item| match item {
                TimelineItem::System(note) if note.tone == SystemTone::Input => {
                    Some(note.text.clone())
                }
                _ => None,
            });
            view.timeline = timeline;
            view
        });
        *rendered_view_for_window.borrow_mut() = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = rendered_view.borrow_mut().take().expect("view opened");
    (handle.into(), view)
}

/// The height of the question text block in a freshly opened window.
fn question_text_height(
    cx: &mut TestAppContext,
    window_size: gpui_kit::Size<gpui_kit::Pixels>,
) -> gpui_kit::Pixels {
    let (handle, _view) = open_view(cx, window_size, vec![question_note()]);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.find("agent-question-text").bounds().size.height
    })
    .unwrap()
}

#[gpui_kit::test]
fn input_note_renders_the_whole_question_without_a_disclosure(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    // The shared harness renders the question at a desktop and a phone width.
    render_scenario(cx, question_scenario);
    render_scenario_at(cx, size(px(390.), px(844.)), question_scenario);

    let wide_height = question_text_height(cx, size(px(1280.), px(800.)));
    let (handle, _view) = open_view(cx, size(px(390.), px(844.)), vec![question_note()]);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        // The heading, the question, and the answer action are all in the first
        // frame: there is no disclosure or truncation control to open.
        assert!(window.find("agent-question").visible());
        assert!(window.find("agent-question-heading").visible());
        assert!(window.find("agent-question-answer").visible());

        let text = window.find("agent-question-text");
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
    })
    .unwrap();
}

#[gpui_kit::test]
fn question_answer_action_is_finger_sized_on_a_phone_only(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (phone_handle, _phone_view) =
        open_view(cx, size(px(390.), px(844.)), vec![question_note()]);
    let phone_height = cx
        .update_window(phone_handle, |_, window, cx| {
            window.render_frame(cx);
            let answer = window.find("agent-question-answer");
            assert!(answer.visible());
            assert!(
                answer.bounds().right() <= window.viewport_size().width,
                "the answer action runs off the phone screen: {:?}",
                answer.bounds()
            );
            answer.bounds().size.height
        })
        .unwrap();

    let (desktop_handle, _desktop_view) =
        open_view(cx, size(px(1280.), px(800.)), vec![question_note()]);
    let desktop_height = cx
        .update_window(desktop_handle, |_, window, cx| {
            window.render_frame(cx);
            window.find("agent-question-answer").bounds().size.height
        })
        .unwrap();

    assert!(
        phone_height >= px(44.),
        "the answer action is below the touch target: {phone_height:?}"
    );
    assert!(
        phone_height > desktop_height,
        "only a phone lays the action out as a finger-sized target: {phone_height:?} vs {desktop_height:?}"
    );
}

#[gpui_kit::test]
fn clicking_the_answer_action_focuses_the_composer(cx: &mut TestAppContext) {
    use gpui_kit::Focusable as _;

    cx.update(gpui_kit::init);
    let (handle, view) = open_view(cx, size(px(390.), px(844.)), vec![question_note()]);
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

        window.click("agent-question-answer", cx);
        window.render_frame(cx);
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert!(
                input.read(cx).focus_handle(cx).is_focused(window),
                "the answer action must focus the composer"
            );
        });

        // Typing afterwards lands in the composer, which is where the answer to
        // the question is typed.
        window.input("use the release branch", cx);
        view.update(cx, |view, cx| {
            let input = view.composer_input.clone().expect("composer input");
            assert_eq!(input.read(cx).value().to_string(), "use the release branch");
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn error_and_neutral_notes_keep_the_generic_note_card(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, _view) = open_view(
        cx,
        size(px(390.), px(844.)),
        vec![error_note("write failed"), neutral_note("Working")],
    );
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(("system-note", 0usize)).visible());
        assert!(window.find(("system-note", 1usize)).visible());
        assert!(
            window.try_find("agent-question").is_none(),
            "only a request for input renders the question component"
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
    let (handle, _view) = open_view(cx, size(px(1280.), px(800.)), vec![question_note()]);
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("agent-question").visible());
    })
    .unwrap();
}
