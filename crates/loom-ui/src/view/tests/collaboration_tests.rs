//! Multi-user view state: conflict handling keeps the draft, observers cannot
//! send or act, and attributed entries reach the timeline.

use super::*;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{TestAppContext, px, size};
use loom_core::{
    Author, CollaborationEntry, CollaborationEntryKind, ParticipantId, ParticipantIdentity,
    PresenceStatus, Recipient, RosterEntry, RunAttemptId, RunRevision, SessionRole,
};
use loom_protocol::{CollaborationResponse, RunActionOutcome};

fn run_snapshot(view: &LoomView, state: AgentRunState) -> AgentRunSnapshot {
    AgentRunSnapshot {
        id: RunId::new(),
        attempt_id: RunAttemptId::new(),
        control_revision: 1,
        session_id: view.active_session.id,
        task: "task".to_owned(),
        model: ModelId::new("deterministic/demo"),
        state,
        started_at: Timestamp::now(),
        updated_at: Timestamp::now(),
        completed_at: None,
        summary: None,
        evidence: Vec::new(),
        approval_policy: None,
    }
}

fn person(name: &str) -> ParticipantIdentity {
    ParticipantIdentity {
        id: ParticipantId::new(),
        display_name: name.to_owned(),
    }
}

fn roster(person: &ParticipantIdentity, role: SessionRole) -> RosterEntry {
    RosterEntry {
        participant: person.clone(),
        role,
        presence: PresenceStatus::Online,
        connections: 1,
        last_active_at: None,
    }
}

#[gpui_kit::test]
fn conflict_keeps_the_draft_and_refreshes_the_revision(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let run = run_snapshot(&view, AgentRunState::Completed);
        view.active_run_id = Some(run.id);
        view.active_run = Some(run.clone());
        let expected = RunRevision {
            attempt_id: run.attempt_id,
            control_revision: 1,
            action_sequence: 1,
        };
        let current = RunRevision {
            action_sequence: 2,
            ..expected
        };
        view.timeline
            .push(TimelineItem::User("resume please".to_owned()));
        view.optimistic_messages.push("resume please".to_owned());
        view.finish_ordered_send(
            ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Collaboration(CollaborationResponse::RunAction(Box::new(
                    RunActionOutcome::Conflict {
                        expected,
                        current,
                        run: run.clone(),
                    },
                ))),
            ),
            "resume please".to_owned(),
            cx,
        );
        assert_eq!(
            view.collaboration.restore_draft.as_deref(),
            Some("resume please")
        );
        assert!(view.collaboration.conflict.is_some());
        assert_eq!(view.collaboration.revision_for(&run), Some(current));
        assert!(view.optimistic_messages.is_empty());
        assert!(
            !view
                .timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::User(text) if text == "resume please")),
            "the refused message is withdrawn from the transcript"
        );
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("run-conflict-notice").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn observers_cannot_send_or_act(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let me = person("Nadia");
        view.collaboration.identity = Some(me.clone());
        view.collaboration.replace_roster(
            view.active_session.id,
            vec![roster(&me, SessionRole::Observer)],
        );
        assert!(!view.send_composer_text("hello".to_owned(), cx));
        assert!(view.timeline.is_empty());
        assert!(!view.can_manage_sources());
        view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
        assert!(view.source_dialog.is_none());
        view.approve_pending_action(cx);
        assert!(!view.approval_request_in_flight);
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn participants_search_only_for_sessions_they_own(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let me = person("Sam");
        view.collaboration.identity = Some(me.clone());
        view.collaboration.replace_roster(
            view.active_session.id,
            vec![roster(&me, SessionRole::Owner)],
        );
        view.begin_source_dialog(SessionSourceDialogPurpose::AddToSession, cx);
        assert_eq!(
            view.repository_search_session(),
            Some(Some(view.active_session.id))
        );
        view.begin_source_dialog(SessionSourceDialogPurpose::StartSession, cx);
        assert_eq!(view.repository_search_session(), None);
        view.collaboration.identity = None;
        assert_eq!(view.repository_search_session(), Some(None));
        view
    });
}

#[gpui_kit::test]
fn collaboration_events_reach_the_timeline_and_roster(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let sam = person("Sam Rivera");
        let author = Author::Participant {
            participant: sam.clone(),
            role: SessionRole::Participant,
        };
        let peer = CollaborationEntry {
            session_id: view.active_session.id,
            run_id: None,
            author: author.clone(),
            recipient: Recipient::People,
            kind: CollaborationEntryKind::PeerMessage {
                text: "I can take the reconnect test".to_owned(),
            },
            revision: None,
            created_at: Timestamp::now(),
        };
        view.consume_event(&ServerEvent::Collaboration {
            entry: peer.clone(),
        });
        // A re-delivered entry is not shown twice.
        view.consume_event(&ServerEvent::Collaboration { entry: peer });
        let direction = CollaborationEntry {
            session_id: view.active_session.id,
            run_id: None,
            author,
            recipient: Recipient::Agent,
            kind: CollaborationEntryKind::AgentDirection {
                text: "cover reconnects".to_owned(),
            },
            revision: None,
            created_at: Timestamp::now(),
        };
        view.consume_event(&ServerEvent::Collaboration { entry: direction });
        view.timeline
            .push(TimelineItem::User("cover reconnects".to_owned()));
        assert_eq!(
            view.timeline
                .iter()
                .filter(|item| matches!(item, TimelineItem::Collaboration(_)))
                .count(),
            1
        );
        view.consume_event(&ServerEvent::SessionRosterChanged {
            entries: vec![roster(&sam, SessionRole::Participant)],
        });
        assert_eq!(view.collaboration.roster.len(), 1);
        view.review.open = true;
        view.review.tab = InspectorTab::People;
        view
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}
