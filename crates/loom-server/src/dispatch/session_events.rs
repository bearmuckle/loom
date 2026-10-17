use super::*;

impl InProcessConnection {
    pub(super) fn session_events_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Events(EventsRequest::GetSessionEvents {
                session_id,
                workspace_id,
                after_sequence,
                stream_epoch,
            }) => {
                if session_id.is_some() && workspace_id.is_some() {
                    return Err(LoomError::invalid_request(
                        "session_id and workspace_id cannot both scope an event stream",
                    ));
                }
                let current_stream_epoch = Some(self.backend.node_id.clone());
                let stream_epoch_changed = (session_id.is_some() || workspace_id.is_some())
                    && stream_epoch
                        .as_deref()
                        .is_some_and(|epoch| Some(epoch) != current_stream_epoch.as_deref());
                let after_sequence = if stream_epoch_changed {
                    None
                } else {
                    after_sequence
                };
                if let Some(workspace_id) = workspace_id {
                    if !self
                        .negotiated_capabilities()?
                        .as_ref()
                        .is_some_and(|capabilities| {
                            capabilities.contains(Capability::SubscribeWorkspaceEvents)
                        })
                    {
                        return Err(LoomError::new(
                            ErrorCode::CapabilityDenied,
                            "connection did not negotiate capability SubscribeWorkspaceEvents",
                            false,
                        ));
                    }
                    self.backend.workspace_records()?.get(workspace_id)?;
                    let events = self.workspace_events_since(workspace_id, after_sequence)?;
                    let durable_cursor = self.feed_workspace_cursor(workspace_id)?;
                    let latest_sequence = self.latest_workspace_event_sequence(workspace_id)?;
                    let journal = self.backend.journal()?;
                    let session_ids = self
                        .backend
                        .sessions()?
                        .list_in_workspace(Some(workspace_id), true)
                        .into_iter()
                        .map(|session| session.id)
                        .collect::<BTreeSet<_>>();
                    // EventJournal.next_sequence stores the last assigned global sequence.
                    let global_head_sequence = journal.next_sequence;
                    let history_missing = after_sequence.is_none()
                        && session_ids.iter().any(|session_id| {
                            !events.iter().any(|event| {
                                matches!(event,
                                    WorkspaceFeedEvent::Session(event)
                                        if event.session_id == *session_id
                                            && matches!(
                                                &event.event,
                                                loom_protocol::ServerEvent::AgentSessionCreated { .. }
                                                    | loom_protocol::ServerEvent::AgentSessionForked { .. }
                                            )
                                )
                            })
                        });
                    let cursor_stale = stream_epoch_changed
                        || history_missing
                        || match (durable_cursor, after_sequence) {
                            (Some(cursor), Some(after)) => {
                                after < cursor.pruned_through
                                    || after > global_head_sequence
                                    || (after > cursor.latest_sequence
                                        && journal.workspace_cursor_is_stale(
                                            &session_ids,
                                            workspace_id,
                                            after_sequence,
                                        ))
                            }
                            (Some(cursor), None) => cursor.pruned_through.value() > 0,
                            (None, Some(after)) => {
                                after > global_head_sequence
                                    || journal.workspace_cursor_is_stale(
                                        &session_ids,
                                        workspace_id,
                                        after_sequence,
                                    )
                            }
                            (None, None) => false,
                        };
                    if cursor_stale {
                        let oldest_sequence = durable_cursor
                            .and_then(|cursor| cursor.oldest_retained_sequence)
                            .or_else(|| {
                                journal.workspace_oldest_sequence(&session_ids, workspace_id)
                            })
                            .or_else(|| {
                                durable_cursor
                                    .filter(|cursor| cursor.pruned_through.value() > 0)
                                    .map(|cursor| cursor.pruned_through.next())
                            })
                            .unwrap_or_else(|| latest_sequence.next());
                        // A snapshot cannot page, so the feed window it carries
                        // is bounded to its newest events and `oldest_sequence`
                        // says how far back this response reaches; the caller
                        // rebuilds the older part from the workspace catalog the
                        // same snapshot carries.
                        let window = self.bound_workspace_snapshot(events)?;
                        let oldest_sequence = window.oldest_sequence.unwrap_or(oldest_sequence);
                        let readable = self.readable_sessions()?;
                        return Ok(ServerResponse::Events(
                            EventsResponse::WorkspaceEventsSnapshot {
                                workspace_id,
                                sessions: self
                                    .backend
                                    .sessions()?
                                    .list_in_workspace(Some(workspace_id), true)
                                    .into_iter()
                                    .filter(|session| {
                                        readable
                                            .as_ref()
                                            .is_none_or(|readable| readable.contains(&session.id))
                                    })
                                    .collect(),
                                events: self.filter_private_workspace_events(
                                    filter_workspace_feed(window.events, readable.as_ref()),
                                )?,
                                oldest_sequence,
                                latest_sequence,
                                stream_epoch: current_stream_epoch,
                            },
                        ));
                    }
                    // The feed grows with activity, so it is bounded like a
                    // session batch: the oldest events of the backlog are kept
                    // and the caller continues from the cursor this page names.
                    let page = self.bound_workspace_events(events)?;
                    return Ok(ServerResponse::Events(EventsResponse::WorkspaceEvents {
                        workspace_id,
                        events: self.filter_private_workspace_events(filter_workspace_feed(
                            page.events,
                            self.readable_sessions()?.as_ref(),
                        ))?,
                        stream_epoch: current_stream_epoch,
                        next_after_sequence: page.next_after_sequence,
                    }));
                }
                let (events, session_latest_sequence) = match session_id {
                    Some(session_id) => {
                        let (events, latest) =
                            self.session_events_with_safe_cursor(session_id, after_sequence)?;
                        (events, Some(latest))
                    }
                    None => (self.session_events_since(None, after_sequence)?, None),
                };
                let journal = self.backend.journal()?;
                let history_missing = session_id.is_some()
                    && after_sequence.is_none()
                    && !events.iter().any(|event| {
                        matches!(
                            &event.event,
                            loom_protocol::ServerEvent::AgentSessionCreated { .. }
                                | loom_protocol::ServerEvent::AgentSessionForked { .. }
                        )
                    });
                if let Some(session_id) = session_id {
                    let durable_cursor = self.feed_session_cursor(session_id)?;
                    let cursor_stale = stream_epoch_changed
                        || match (durable_cursor, after_sequence) {
                            (Some(cursor), Some(after)) => {
                                after < cursor.pruned_through
                                    || after > cursor.latest_sequence
                                        && journal.is_cursor_stale(Some(session_id), after_sequence)
                            }
                            (None, _) => journal.is_cursor_stale(Some(session_id), after_sequence),
                            (_, None) => false,
                        };
                    if cursor_stale || history_missing {
                        let oldest_sequence = durable_cursor
                            .and_then(|cursor| cursor.oldest_retained_sequence)
                            .or_else(|| journal.oldest_sequence(Some(session_id)))
                            .or_else(|| {
                                durable_cursor
                                    .filter(|cursor| cursor.pruned_through.value() > 0)
                                    .map(|cursor| cursor.pruned_through.next())
                            })
                            .unwrap_or_else(|| {
                                session_latest_sequence
                                    .unwrap_or(journal.next_sequence)
                                    .next()
                            });
                        // A snapshot cannot page, so the window it carries is
                        // bounded to its newest events: the caller resumes into
                        // that state, and the durable rebuild the same snapshot
                        // triggers recovers everything older than
                        // `oldest_sequence`.
                        let window = self.bound_event_snapshot(events)?;
                        let oldest_sequence = window.oldest_sequence.unwrap_or(oldest_sequence);
                        return Ok(ServerResponse::Events(
                            EventsResponse::SessionEventsSnapshot {
                                session: self.backend.sessions()?.get(session_id)?,
                                events: self.filter_private_events(window.events)?,
                                oldest_sequence,
                                latest_sequence: session_latest_sequence
                                    .unwrap_or(journal.next_sequence),
                                stream_epoch: current_stream_epoch,
                            },
                        ));
                    }
                }
                let batch = self.bound_event_batch(events)?;
                // A person reading the cross-session stream sees only the
                // sessions they hold a grant in.
                let readable = if session_id.is_none() {
                    self.readable_sessions()?
                } else {
                    None
                };
                let own_request = |event: &ServerEventEnvelope| {
                    self.participant()
                        .is_some_and(|participant| requested_by(event, participant.id))
                };
                let events = batch
                    .events
                    .into_iter()
                    .filter(|event| {
                        readable
                            .as_ref()
                            .is_none_or(|readable| readable.contains(&event.session_id))
                            || own_request(event)
                    })
                    .collect();
                Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                    events: self.filter_private_events(events)?,
                    stream_epoch: session_id.map(|_| self.backend.node_id.clone()),
                    next_after_sequence: batch.next_after_sequence,
                }))
            }
            ClientRequest::Events(EventsRequest::GetRecentSessionEvents { session_id, limit }) => {
                // This batch is a tail the caller sized with `limit`, not a batch
                // that grows with the journal, so its own bound is the count the
                // caller asked for; the resync path is the one that pages.
                Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                    events: self.filter_private_events(
                        self.recent_session_events(session_id, limit as usize)?,
                    )?,
                    stream_epoch: Some(self.backend.node_id.clone()),
                    next_after_sequence: None,
                }))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}

/// Keeps workspace-level events and the session events a person may read.
fn filter_workspace_feed(
    events: Vec<WorkspaceFeedEvent>,
    readable: Option<&BTreeSet<AgentSessionId>>,
) -> Vec<WorkspaceFeedEvent> {
    let Some(readable) = readable else {
        return events;
    };
    events
        .into_iter()
        .filter(|event| match event {
            WorkspaceFeedEvent::Session(event) => readable.contains(&event.session_id),
            WorkspaceFeedEvent::Workspace(_) => true,
        })
        .collect()
}

/// Whether `event` is an access-request event filed by `participant`.
fn requested_by(event: &ServerEventEnvelope, participant: loom_core::ParticipantId) -> bool {
    match &event.event {
        loom_protocol::ServerEvent::SessionAccessRequested { request }
        | loom_protocol::ServerEvent::SessionAccessRequestResolved { request, .. } => {
            request.participant.id == participant
        }
        _ => false,
    }
}

impl InProcessConnection {
    /// Drops access-request events a person may not see: they go to the
    /// session's owners and to the requester only. Operator connections see
    /// every event.
    fn filter_private_events(
        &self,
        events: Vec<ServerEventEnvelope>,
    ) -> Result<Vec<ServerEventEnvelope>> {
        let Some(participant) = self.participant() else {
            return Ok(events);
        };
        let mut kept = Vec::with_capacity(events.len());
        for event in events {
            if self.private_event_visible(&event, participant.id)? {
                kept.push(event);
            }
        }
        Ok(kept)
    }

    fn filter_private_workspace_events(
        &self,
        events: Vec<WorkspaceFeedEvent>,
    ) -> Result<Vec<WorkspaceFeedEvent>> {
        let Some(participant) = self.participant() else {
            return Ok(events);
        };
        let mut kept = Vec::with_capacity(events.len());
        for event in events {
            let visible = match &event {
                WorkspaceFeedEvent::Session(event) => {
                    self.private_event_visible(event, participant.id)?
                }
                WorkspaceFeedEvent::Workspace(_) => true,
            };
            if visible {
                kept.push(event);
            }
        }
        Ok(kept)
    }

    fn private_event_visible(
        &self,
        event: &ServerEventEnvelope,
        participant: loom_core::ParticipantId,
    ) -> Result<bool> {
        if !matches!(
            event.event,
            loom_protocol::ServerEvent::SessionAccessRequested { .. }
                | loom_protocol::ServerEvent::SessionAccessRequestResolved { .. }
        ) {
            return Ok(true);
        }
        Ok(requested_by(event, participant)
            || self.session_role(participant, event.session_id)?
                == Some(loom_core::SessionRole::Owner))
    }
}
