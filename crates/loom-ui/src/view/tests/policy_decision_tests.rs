//! Tool policy decisions in the conversation view.
//!
//! The backend journals `AgentEvent::ToolPolicyEvaluated` before it acts on a
//! decision, and completes a denied call as a failed result carrying the same
//! reason. The projection records the evaluation against the call, so a denial
//! renders as a policy decision with the action kind it was evaluated under and
//! the reason the policy gave, while a failed call the policy never evaluated
//! reports the reason its result carried in a note under the row.

use super::{LoomView, tool_element_id, tool_failure_reason, tool_policy_note, tool_shows_output};
use crate::state::{AssistantPart, TimelineItem, ToolPart, ToolPartStatus};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, AppContext, Entity, TestAppContext, px, size};
use loom_core::{ActionKind, PolicyDecision, PolicyEvaluation, RunId, ToolCallId};
use loom_model::ToolCall;
use loom_protocol::{AgentEvent, ToolResult};
use std::cell::RefCell;
use std::rc::Rc;

/// The reason the policy evaluation carries for a denied push, matching
/// `loom_core::PolicyEvaluation::evaluate`.
const DENIAL_REASON: &str = "github_push_branch is denied by the workspace policy";

fn push_branch_call() -> ToolCall {
    ToolCall {
        id: ToolCallId::new(),
        name: "github_push_branch".to_owned(),
        arguments: serde_json::json!({"repository": "owner/name", "branch": "fix"}),
    }
}

/// A call whose result exists nowhere else, so its output is shown as well as
/// summarized by the failure note.
fn web_search_call() -> ToolCall {
    ToolCall {
        id: ToolCallId::new(),
        name: "web_search".to_owned(),
        arguments: serde_json::json!({"query": "needle"}),
    }
}

/// Consumes the two events the backend journals before a policy decision: the
/// call the model requested, then the evaluation itself.
fn request_and_evaluate(view: &mut LoomView, call: &ToolCall) -> RunId {
    let run_id = RunId::new();
    view.consume_agent_event(&AgentEvent::ToolCallRequested {
        run_id,
        call: call.clone(),
    });
    view.consume_agent_event(&AgentEvent::ToolPolicyEvaluated {
        run_id,
        call: call.clone(),
        evaluation: PolicyEvaluation {
            action: ActionKind::Destructive,
            decision: PolicyDecision::Deny,
            reason: DENIAL_REASON.to_owned(),
        },
    });
    run_id
}

/// Consumes the failed result the backend journals for a denied call.
fn complete_as_failed(view: &mut LoomView, run_id: RunId, call: &ToolCall) {
    view.consume_agent_event(&AgentEvent::ToolCallCompleted {
        run_id,
        result: ToolResult::failure(call, DENIAL_REASON),
    });
}

/// Every tool block the projection holds, in timeline order.
fn projected_parts(view: &LoomView) -> Vec<ToolPart> {
    view.timeline
        .iter()
        .flat_map(|item| match item {
            TimelineItem::Assistant(turn) => turn
                .parts
                .iter()
                .filter_map(|part| match part {
                    AssistantPart::Tool(tool) => Some(tool.as_ref().clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

/// Opens the view on the timeline `scenario` builds and renders it, returning
/// the window and the view so a test can assert on both.
fn open_scenario(
    cx: &mut TestAppContext,
    scenario: impl FnOnce(&mut LoomView),
) -> (AnyWindowHandle, Entity<LoomView>) {
    let rendered = Rc::new(RefCell::new(None));
    let rendered_for_window = rendered.clone();
    let handle = cx.open_window(size(px(1280.), px(800.)), move |window, cx| {
        let view = cx.new(|cx| {
            let mut view = LoomView::new_for_test(cx.focus_handle());
            view.sessions = vec![view.active_session.clone()];
            scenario(&mut view);
            // Tool blocks sit behind the collapsed usage summary.
            view.expanded_tool_usage.insert(0);
            view
        });
        *rendered_for_window.borrow_mut() = Some(view.clone());
        gpui_kit::component::Root::new(view, window, cx)
    });
    let view = rendered.borrow_mut().take().expect("the view opened");
    (handle.into(), view)
}

#[gpui_kit::test]
fn a_policy_decision_is_recorded_against_the_call(cx: &mut TestAppContext) {
    let view = cx.new(|cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let call = push_branch_call();
        let run_id = request_and_evaluate(&mut view, &call);

        // The decision alone, before any result exists, already reads as a
        // policy denial, and it stays in the call's own timeline entry.
        let parts = projected_parts(&view);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].id, call.id);
        assert_eq!(parts[0].status, ToolPartStatus::Denied);
        assert_eq!(parts[0].status.label(), "denied by policy");
        assert!(parts[0].output.is_none());
        assert_eq!(view.timeline.len(), 1);

        complete_as_failed(&mut view, run_id, &call);
        let parts = projected_parts(&view);
        assert_eq!(parts.len(), 1);
        let part = &parts[0];
        assert_eq!(part.status, ToolPartStatus::Denied);
        assert_eq!(
            part.policy.as_ref().map(|policy| policy.action),
            Some(ActionKind::Destructive)
        );
        assert_eq!(
            part.policy.as_ref().map(|policy| policy.decision),
            Some(PolicyDecision::Deny)
        );
        assert_eq!(
            part.policy.as_ref().map(|policy| policy.reason.as_str()),
            Some(DENIAL_REASON)
        );
        assert_eq!(part.output.as_deref(), Some(DENIAL_REASON));
        assert_eq!(
            view.timeline.len(),
            1,
            "the evaluation must not fork the timeline"
        );
        assert_eq!(
            tool_policy_note(part).as_deref(),
            Some(format!("destructive action · {DENIAL_REASON}").as_str())
        );
        view
    });
    let _ = view;
}

#[gpui_kit::test]
fn a_policy_denial_renders_its_action_kind_and_reason(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_scenario(cx, |view| {
        let call = push_branch_call();
        let run_id = request_and_evaluate(view, &call);
        complete_as_failed(view, run_id, &call);
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let part = projected_parts(view.read(cx))
            .pop()
            .expect("the denied call is projected");
        assert_eq!(part.status, ToolPartStatus::Denied);
        let key = tool_element_id(0, 0);

        // The row's own status line reports the decision, not a failure.
        let header = window.find(("tool-header", key));
        assert!(header.visible());
        let label = header.label().unwrap_or_default().to_owned();
        assert!(label.contains("denied by policy"), "header was {label:?}");
        assert!(!label.contains("failed"), "header was {label:?}");

        // The action kind and the reason are the point of the row, so they
        // render without opening the disclosure.
        let note = window.find(("tool-policy-note", key));
        assert!(note.visible());
        assert_eq!(note.label(), tool_policy_note(&part).as_deref());
        let note = note.label().unwrap_or_default().to_owned();
        assert!(note.contains("destructive action"), "note was {note:?}");
        assert!(note.contains(DENIAL_REASON), "note was {note:?}");
        // The failed result's output body stays hidden: the reason is the
        // policy's own statement rather than tool output.
        assert!(window.try_find(("copy-tool-output", key)).is_none());
        assert!(
            window.try_find(("tool-failure-note", key)).is_none(),
            "a denial is a decision, not a failure with a reason to report"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_failure_without_a_policy_evaluation_still_renders_as_a_failure(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_scenario(cx, |view| {
        let call = ToolCall {
            id: ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": "src/missing.rs"}),
        };
        let run_id = RunId::new();
        view.consume_agent_event(&AgentEvent::ToolCallRequested {
            run_id,
            call: call.clone(),
        });
        view.consume_agent_event(&AgentEvent::ToolCallCompleted {
            run_id,
            result: ToolResult::failure(&call, "No such file or directory"),
        });
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let part = projected_parts(view.read(cx))
            .pop()
            .expect("the failed call is projected");
        assert_eq!(part.status, ToolPartStatus::Failed);
        assert_eq!(part.policy, None);
        assert_eq!(tool_policy_note(&part), None);

        let key = tool_element_id(0, 0);
        let label = window
            .find(("tool-header", key))
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(label.contains("failed"), "header was {label:?}");
        assert!(!label.contains("denied"), "header was {label:?}");
        assert!(
            window.try_find(("tool-policy-note", key)).is_none(),
            "a failure the policy never evaluated has no policy note"
        );

        // The reason is what the row lacked: it is visible without opening the
        // disclosure.
        let note = window.find(("tool-failure-note", key));
        assert!(note.visible());
        assert_eq!(note.label(), tool_failure_reason(&part).as_deref());
        let note = note.label().unwrap_or_default().to_owned();
        assert!(
            note.contains("No such file or directory"),
            "note was {note:?}"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_failed_call_without_a_reason_renders_no_note(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (handle, view) = open_scenario(cx, |view| {
        let call = web_search_call();
        let run_id = RunId::new();
        view.consume_agent_event(&AgentEvent::ToolCallRequested {
            run_id,
            call: call.clone(),
        });
        view.consume_agent_event(&AgentEvent::ToolCallCompleted {
            run_id,
            result: ToolResult::failure(&call, ""),
        });
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let part = projected_parts(view.read(cx))
            .pop()
            .expect("the failed call is projected");
        assert_eq!(part.status, ToolPartStatus::Failed);
        assert!(part.output.is_none(), "an empty result carries no reason");
        assert_eq!(tool_failure_reason(&part), None);

        let key = tool_element_id(0, 0);
        assert!(
            window.try_find(("tool-failure-note", key)).is_none(),
            "a failure with no reason must not render an empty note"
        );
        let label = window
            .find(("tool-header", key))
            .label()
            .unwrap_or_default()
            .to_owned();
        assert!(label.contains("failed"), "header was {label:?}");
    })
    .unwrap();
}

#[gpui_kit::test]
fn a_long_failure_reads_as_a_summary_while_its_output_stays_reachable(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let reason = format!(
        "search backend refused the query\n{}",
        "retry later ".repeat(40)
    );
    let (handle, view) = open_scenario(cx, |view| {
        let call = web_search_call();
        let run_id = RunId::new();
        view.consume_agent_event(&AgentEvent::ToolCallRequested {
            run_id,
            call: call.clone(),
        });
        view.consume_agent_event(&AgentEvent::ToolCallCompleted {
            run_id,
            result: ToolResult::failure(&call, reason.clone()),
        });
        // The note summarizes the reason, so the disclosure has to hold the
        // rest of it.
        view.expanded_tools.insert(call.id);
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let part = projected_parts(view.read(cx))
            .pop()
            .expect("the failed call is projected");
        assert!(tool_shows_output(&part));
        let key = tool_element_id(0, 0);

        let note = window.find(("tool-failure-note", key));
        assert!(note.visible());
        let note = note.label().unwrap_or_default().to_owned();
        assert!(
            note.contains("search backend refused the query"),
            "note was {note:?}"
        );
        assert!(!note.contains('\n'), "the note is one line: {note:?}");
        assert!(note.contains("[output truncated]"), "note was {note:?}");

        // The whole reason stays reachable by expanding the row.
        assert!(window.try_find(("copy-tool-output", key)).is_some());
    })
    .unwrap();
}
