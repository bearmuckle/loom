//! Attributed, ordered collaboration: peer chat, agent direction, answers,
//! and run control by people sharing a session.

use loom_core::{
    Author, CollaborationEntry, CollaborationEntryKind, MAX_COLLABORATION_TEXT_BYTES, Recipient,
    RunControlAction, RunRevision,
};

use super::*;

/// The result of an action taken through a run's ordering gate.
#[allow(clippy::large_enum_variant)]
pub(crate) enum GatedRunAction {
    Applied {
        response: ServerResponse,
        entry: CollaborationEntry,
        revision: RunRevision,
    },
    Conflict {
        current: RunRevision,
    },
}

impl InProcessConnection {
    /// Who this connection acts as in `session_id`. The identity always comes
    /// from the authenticated credential; operator connections that are not
    /// bound to a person are reported as [`Author::Operator`].
    pub(crate) fn author(&self, session_id: AgentSessionId) -> Result<Author> {
        let Some(participant) = self.participant() else {
            return Ok(Author::Operator);
        };
        let role = self
            .session_role(participant.id, session_id)?
            .unwrap_or(loom_core::SessionRole::Observer);
        Ok(Author::Participant {
            participant: participant.clone(),
            role,
        })
    }

    /// The run's current revision, read under its ordering gate.
    pub(crate) fn current_run_revision(&self, run_id: loom_core::RunId) -> Result<RunRevision> {
        let handle = self.run_handle(run_id)?;
        let gate = self.backend.collaboration.gate(run_id);
        let sequence = gate.lock();
        Ok(run_revision(&handle.snapshot(), *sequence))
    }

    /// Applies one person's run action in order.
    ///
    /// The run's gate is held from the revision check through the action, so
    /// actions on one run are applied one at a time. With `expected`, the
    /// action is applied only when the run is still at that revision;
    /// otherwise nothing changes and the current revision is returned. Every
    /// applied action advances the action sequence and is recorded in the
    /// session history with the connection's identity as its author.
    pub(crate) fn gated_run_action(
        &self,
        run_id: loom_core::RunId,
        expected: Option<RunRevision>,
        kind: CollaborationEntryKind,
        apply: impl FnOnce(&Self) -> Result<ServerResponse>,
    ) -> Result<GatedRunAction> {
        let handle = self.run_handle(run_id)?;
        let author = self.author(handle.session_id)?;
        let gate = self.backend.collaboration.gate(run_id);
        let mut sequence = gate.lock();
        let current = run_revision(&handle.snapshot(), *sequence);
        if expected.is_some_and(|expected| expected != current) {
            return Ok(GatedRunAction::Conflict { current });
        }
        let response = apply(self)?;
        *sequence += 1;
        let revision = run_revision(&handle.snapshot(), *sequence);
        drop(sequence);
        let entry = CollaborationEntry {
            session_id: handle.session_id,
            run_id: Some(run_id),
            author,
            recipient: Recipient::Agent,
            kind,
            revision: Some(revision),
            created_at: Timestamp::now(),
        };
        self.backend.record_collaboration(entry.clone())?;
        self.backend.persist_state()?;
        Ok(GatedRunAction::Applied {
            response,
            entry,
            revision,
        })
    }

    /// Runs a legacy run request through the ordering gate without a
    /// revision check, so it is still serialized with people's actions and
    /// attributed to its author.
    pub(crate) fn attributed_run_request(
        &self,
        run_id: loom_core::RunId,
        kind: CollaborationEntryKind,
        apply: impl FnOnce(&Self) -> Result<ServerResponse>,
    ) -> Result<ServerResponse> {
        match self.gated_run_action(run_id, None, kind, apply)? {
            GatedRunAction::Applied { response, .. } => Ok(response),
            GatedRunAction::Conflict { .. } => {
                unreachable!("an action without an expected revision never conflicts")
            }
        }
    }

    /// The history entry for a user message: an answer when the agent is
    /// waiting on a question, a direction otherwise.
    pub(crate) fn message_entry_kind(
        &self,
        run_id: loom_core::RunId,
        text: &str,
    ) -> Result<CollaborationEntryKind> {
        let state = self.run_handle(run_id)?.state();
        Ok(if state.pending_input.is_some() {
            CollaborationEntryKind::AgentAnswer {
                interaction_id: pending_question(&state),
                text: text.to_owned(),
            }
        } else {
            CollaborationEntryKind::AgentDirection {
                text: text.to_owned(),
            }
        })
    }

    /// One page of the attributed entries retained in `session_id`'s feed.
    pub(crate) fn session_collaboration_page(
        &self,
        session_id: AgentSessionId,
        after_sequence: Option<EventSequence>,
        limit: u32,
    ) -> Result<loom_protocol::CollaborationResponse> {
        // A deleted session is reported as missing, not as an empty history.
        self.backend.sessions()?.get(session_id)?;
        let limit = limit.clamp(1, loom_protocol::MAX_SESSION_COLLABORATION_PAGE_SIZE) as usize;
        let mut entries = self
            .session_events_since(Some(session_id), after_sequence)?
            .into_iter()
            .filter_map(|event| match event.event {
                ServerEvent::Collaboration { entry } => Some((event.sequence, entry)),
                _ => None,
            })
            .take(limit + 1)
            .collect::<Vec<_>>();
        let next_after_sequence = if entries.len() > limit {
            entries.truncate(limit);
            entries.last().map(|(sequence, _)| *sequence)
        } else {
            None
        };
        Ok(loom_protocol::CollaborationResponse::SessionCollaboration {
            session_id,
            entries,
            next_after_sequence,
        })
    }

    pub(crate) fn collaboration_request(
        &self,
        request: loom_protocol::CollaborationRequest,
    ) -> Result<loom_protocol::CollaborationResponse> {
        use loom_protocol::{CollaborationRequest, CollaborationResponse, RunActionOutcome};
        let (run_id, expected, kind, action) = match request {
            CollaborationRequest::SendPeerMessage { session_id, text } => {
                validate_text(&text)?;
                self.backend.sessions()?.get(session_id)?;
                let entry = CollaborationEntry {
                    session_id,
                    run_id: None,
                    author: self.author(session_id)?,
                    recipient: Recipient::People,
                    kind: CollaborationEntryKind::PeerMessage { text },
                    revision: None,
                    created_at: Timestamp::now(),
                };
                // Peer chat is history only: it is not delivered to any run
                // and never starts or continues one.
                self.backend.record_collaboration(entry.clone())?;
                self.backend.persist_state()?;
                return Ok(CollaborationResponse::PeerMessageSent {
                    entry: Box::new(entry),
                });
            }
            CollaborationRequest::GetRunRevision { run_id } => {
                return Ok(CollaborationResponse::RunRevision {
                    run_id,
                    revision: self.current_run_revision(run_id)?,
                });
            }
            CollaborationRequest::ListSessionCollaboration {
                session_id,
                after_sequence,
                limit,
            } => return self.session_collaboration_page(session_id, after_sequence, limit),
            CollaborationRequest::DirectAgent {
                run_id,
                expected,
                text,
            } => {
                validate_text(&text)?;
                (
                    run_id,
                    expected,
                    CollaborationEntryKind::AgentDirection { text: text.clone() },
                    PersonAction::Message(text),
                )
            }
            CollaborationRequest::AnswerAgentQuestion {
                run_id,
                expected,
                text,
            } => {
                validate_text(&text)?;
                let state = self.run_handle(run_id)?.state();
                if state.pending_input.is_none() {
                    return Err(LoomError::invalid_state(
                        "the agent is not waiting for an answer",
                    ));
                }
                (
                    run_id,
                    expected,
                    CollaborationEntryKind::AgentAnswer {
                        interaction_id: pending_question(&state),
                        text: text.clone(),
                    },
                    PersonAction::Message(text),
                )
            }
            CollaborationRequest::ControlRun {
                run_id,
                expected,
                action,
            } => (
                run_id,
                expected,
                CollaborationEntryKind::RunControl {
                    action: action.clone(),
                },
                PersonAction::Control(action),
            ),
        };
        let outcome = self.gated_run_action(run_id, Some(expected), kind, |connection| {
            connection.apply_person_action(run_id, expected, action)
        })?;
        let run = self.run_handle(run_id)?.snapshot();
        Ok(CollaborationResponse::RunAction(Box::new(match outcome {
            GatedRunAction::Applied {
                entry, revision, ..
            } => RunActionOutcome::Accepted {
                entry,
                revision,
                run,
            },
            GatedRunAction::Conflict { current } => RunActionOutcome::Conflict {
                expected,
                current,
                run,
            },
        })))
    }

    fn apply_person_action(
        &self,
        run_id: loom_core::RunId,
        expected: RunRevision,
        action: PersonAction,
    ) -> Result<ServerResponse> {
        let RunRevision {
            attempt_id,
            control_revision,
            ..
        } = expected;
        match action {
            PersonAction::Message(text) => {
                self.send_agent_message(run_id, attempt_id, control_revision, text)
            }
            PersonAction::Control(RunControlAction::Approve { tool_call_id }) => self
                .continue_run(run_id, |run| {
                    run.approve_entry(tool_call_id, attempt_id, control_revision)
                }),
            PersonAction::Control(RunControlAction::Reject {
                tool_call_id,
                reason,
            }) => self.continue_run(run_id, |run| {
                run.reject_entry(tool_call_id, reason, attempt_id, control_revision)
            }),
            PersonAction::Control(RunControlAction::Pause) => self.stop_run(run_id, RunStop::Pause),
            PersonAction::Control(RunControlAction::Interrupt) => {
                self.stop_run(run_id, RunStop::Interrupt)
            }
            PersonAction::Control(RunControlAction::Resume) => self.resume_agent_run(run_id),
            PersonAction::Control(RunControlAction::Retry) => {
                self.continue_run(run_id, AgentRuntime::retry_entry)
            }
            PersonAction::Control(RunControlAction::RetryFromCheckpoint { checkpoint_id }) => {
                self.retry_from_checkpoint(run_id, checkpoint_id)
            }
        }
    }
}

enum PersonAction {
    Message(String),
    Control(RunControlAction),
}

fn pending_question(state: &AgentRuntimeState) -> Option<loom_core::InteractionId> {
    state
        .interactions
        .iter()
        .rev()
        .find(|interaction| {
            interaction.kind == loom_model::AgentInteractionKind::UserInput
                && interaction.status == loom_model::AgentInteractionStatus::Pending
        })
        .map(|interaction| interaction.id)
}

fn validate_text(text: &str) -> Result<()> {
    if text.trim().is_empty() {
        return Err(LoomError::invalid_request("message must not be empty"));
    }
    if text.len() > MAX_COLLABORATION_TEXT_BYTES {
        return Err(LoomError::invalid_request(format!(
            "message exceeds {MAX_COLLABORATION_TEXT_BYTES} bytes"
        )));
    }
    Ok(())
}
