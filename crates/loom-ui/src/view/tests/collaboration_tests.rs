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

#[gpui_kit::test]
fn control_conflict_adopts_the_run_and_does_not_apply_the_action(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let run = run_snapshot(&view, AgentRunState::Paused);
        view.active_run_id = Some(run.id);
        view.active_run = Some(run.clone());
        view.approval_request_in_flight = true;
        let expected = RunRevision {
            attempt_id: run.attempt_id,
            control_revision: 1,
            action_sequence: 1,
        };
        let current = RunRevision {
            control_revision: 2,
            action_sequence: 2,
            ..expected
        };
        let moved = AgentRunSnapshot {
            control_revision: 2,
            state: AgentRunState::Cancelled,
            ..run.clone()
        };
        let action = loom_core::RunControlAction::Approve {
            tool_call_id: loom_core::ToolCallId::new(),
        };
        view.finish_ordered_control(
            ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Collaboration(CollaborationResponse::RunAction(Box::new(
                    RunActionOutcome::Conflict {
                        expected,
                        current,
                        run: moved.clone(),
                    },
                ))),
            ),
            &run,
            action,
            cx,
        );
        assert!(!view.approval_request_in_flight);
        assert_eq!(view.active_run.as_ref(), Some(&moved));
        assert_eq!(view.run_state, Some(AgentRunState::Cancelled));
        assert_eq!(view.collaboration.revision_for(&moved), Some(current));
        assert!(view.collaboration.restore_draft.is_none());
        assert!(
            view.collaboration
                .conflict
                .as_ref()
                .is_some_and(|notice| notice.control.is_some())
        );
        assert!(
            !view
                .timeline
                .iter()
                .any(|item| matches!(item, TimelineItem::Collaboration(_))),
            "a refused control leaves no history row"
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
fn accepted_control_records_the_attributed_entry(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let run = run_snapshot(&view, AgentRunState::Executing);
        view.active_run_id = Some(run.id);
        view.active_run = Some(run.clone());
        let revision = RunRevision {
            attempt_id: run.attempt_id,
            control_revision: 2,
            action_sequence: 3,
        };
        let paused = AgentRunSnapshot {
            state: AgentRunState::Completed,
            control_revision: 2,
            ..run.clone()
        };
        let entry = CollaborationEntry {
            session_id: view.active_session.id,
            run_id: Some(run.id),
            author: Author::Operator,
            recipient: Recipient::Agent,
            kind: CollaborationEntryKind::RunControl {
                action: loom_core::RunControlAction::Pause,
            },
            revision: Some(revision),
            created_at: Timestamp::now(),
        };
        view.finish_ordered_control(
            ResponseEnvelope::success(
                loom_core::RequestId::new(),
                ServerResponse::Collaboration(CollaborationResponse::RunAction(Box::new(
                    RunActionOutcome::Accepted {
                        entry: entry.clone(),
                        revision,
                        run: paused.clone(),
                    },
                ))),
            ),
            &run,
            loom_core::RunControlAction::Pause,
            cx,
        );
        assert_eq!(view.run_state, Some(AgentRunState::Completed));
        assert!(view.collaboration.conflict.is_none());
        // The live event for the same entry does not add a second row.
        view.consume_event(&ServerEvent::Collaboration { entry });
        assert_eq!(
            view.timeline
                .iter()
                .filter(|item| matches!(item, TimelineItem::Collaboration(_)))
                .count(),
            1
        );
        view
    });
}

#[gpui_kit::test]
fn unsupported_ordered_control_falls_back_to_the_legacy_request(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let run = run_snapshot(&view, AgentRunState::Executing);
        view.active_run_id = Some(run.id);
        view.active_run = Some(run.clone());
        view.collaboration.support = crate::collaboration::CollaborationSupport::Supported;
        view.finish_ordered_control(
            ResponseEnvelope::failure(
                loom_core::RequestId::new(),
                loom_core::LoomError::new(ErrorCode::UnsupportedCapability, "old worker", false),
            ),
            &run,
            loom_core::RunControlAction::Interrupt,
            cx,
        );
        assert!(!view.collaboration.uses_ordered_path());
        assert!(view.collaboration.conflict.is_none());
        view
    });
}

#[gpui_kit::test]
fn history_rebuild_restores_peer_chat_once_after_a_reset(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        let sam = person("Sam Rivera");
        let author = Author::Participant {
            participant: sam,
            role: SessionRole::Participant,
        };
        let peer = CollaborationEntry {
            session_id: view.active_session.id,
            run_id: None,
            author: author.clone(),
            recipient: Recipient::People,
            kind: CollaborationEntryKind::PeerMessage {
                text: "earlier chat".to_owned(),
            },
            revision: None,
            created_at: Timestamp::now(),
        };
        let direction = CollaborationEntry {
            recipient: Recipient::Agent,
            kind: CollaborationEntryKind::AgentDirection {
                text: "cover reconnects".to_owned(),
            },
            ..peer.clone()
        };
        let peer_rows = |view: &LoomView| {
            view.timeline
                .iter()
                .filter(|item| matches!(item, TimelineItem::Collaboration(_)))
                .count()
        };
        // A live event arrives before the history read lands.
        view.consume_event(&ServerEvent::Collaboration {
            entry: peer.clone(),
        });
        view.apply_collaboration_history([peer.clone(), direction.clone()]);
        assert_eq!(peer_rows(&view), 1);
        assert_eq!(
            view.collaboration.author_of("cover reconnects"),
            Some(&author)
        );

        // A reload clears the timeline; history puts the rows back once.
        view.reset_projection();
        assert_eq!(peer_rows(&view), 0);
        view.apply_collaboration_history([peer.clone(), direction]);
        view.consume_event(&ServerEvent::Collaboration { entry: peer });
        assert_eq!(peer_rows(&view), 1);
        assert_eq!(
            view.collaboration.author_of("cover reconnects"),
            Some(&author)
        );
        view
    });
}

fn access_request(
    view: &LoomView,
    who: &ParticipantIdentity,
    role: SessionRole,
) -> loom_protocol::SessionAccessRequest {
    loom_protocol::SessionAccessRequest {
        id: loom_protocol::AccessRequestId::new(),
        session_id: view.active_session.id,
        participant: who.clone(),
        requested_role: role,
        note: Some("please".to_owned()),
        created_at: Timestamp::now(),
    }
}

#[gpui_kit::test]
fn observer_request_shows_pending_then_the_outcome(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let me = person("Nadia");
        view.collaboration.identity = Some(me.clone());
        view.collaboration.support = crate::collaboration::CollaborationSupport::Supported;
        view.collaboration.replace_roster(
            view.active_session.id,
            vec![roster(&me, SessionRole::Observer)],
        );
        let request = access_request(&view, &me, SessionRole::Participant);
        view.consume_event(&ServerEvent::SessionAccessRequested {
            request: request.clone(),
        });
        assert_eq!(view.collaboration.access.own.as_ref(), Some(&request));
        view.review.open = true;
        view.review.tab = InspectorTab::People;
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("access-request-pending").visible());
    })
    .unwrap();
    handle
        .update(cx, |view, _window, cx| {
            let request = view.collaboration.access.own.clone().unwrap();
            view.finish_access_decision(
                "deny request",
                view.active_session.id,
                ResponseEnvelope::success(
                    loom_core::RequestId::new(),
                    ServerResponse::Access(
                        loom_protocol::AccessResponse::SessionAccessRequestResolved {
                            request,
                            decision: loom_protocol::AccessRequestDecision::Denied {
                                reason: Some("not now".to_owned()),
                            },
                            grant: None,
                        },
                    ),
                ),
                cx,
            );
            assert!(view.collaboration.access.own.is_none());
        })
        .unwrap();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("access-request-outcome").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn owner_sees_pending_requests_until_resolved(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1280.), px(800.)), |_window, cx| {
        let mut view = LoomView::new_for_test(cx.focus_handle());
        view.sessions = vec![view.active_session.clone()];
        let owner = person("Björn");
        let nadia = person("Nadia");
        view.collaboration.identity = Some(owner.clone());
        view.collaboration.support = crate::collaboration::CollaborationSupport::Supported;
        view.collaboration.replace_roster(
            view.active_session.id,
            vec![roster(&owner, SessionRole::Owner)],
        );
        let request = access_request(&view, &nadia, SessionRole::Owner);
        view.consume_event(&ServerEvent::SessionAccessRequested {
            request: request.clone(),
        });
        view.consume_event(&ServerEvent::SessionAccessRequested {
            request: request.clone(),
        });
        assert_eq!(view.collaboration.access.pending.len(), 1);
        // A failed decision is reported through the status banner.
        view.finish_access_decision(
            "approve request",
            view.active_session.id,
            ResponseEnvelope::failure(
                loom_core::RequestId::new(),
                loom_core::LoomError::invalid_state("already decided"),
            ),
            cx,
        );
        assert!(view.status_banner.is_some());
        view.review.open = true;
        view.review.tab = InspectorTab::People;
        view
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("access-deny-reason").visible());
    })
    .unwrap();
    handle
        .update(cx, |view, _window, _cx| {
            let request = view.collaboration.access.pending[0].clone();
            view.consume_event(&ServerEvent::SessionAccessRequestResolved {
                request,
                decision: loom_protocol::AccessRequestDecision::Approved {
                    role: SessionRole::Participant,
                },
            });
            assert!(view.collaboration.access.pending.is_empty());
            assert!(view.collaboration.access.outcome.is_none());
        })
        .unwrap();
}
