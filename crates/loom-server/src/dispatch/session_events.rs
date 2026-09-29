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
                        return Ok(ServerResponse::Events(
                            EventsResponse::WorkspaceEventsSnapshot {
                                workspace_id,
                                sessions: self
                                    .backend
                                    .sessions()?
                                    .list_in_workspace(Some(workspace_id), true),
                                events,
                                oldest_sequence,
                                latest_sequence,
                                stream_epoch: current_stream_epoch,
                            },
                        ));
                    }
                    return Ok(ServerResponse::Events(EventsResponse::WorkspaceEvents {
                        workspace_id,
                        events,
                        stream_epoch: current_stream_epoch,
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
                        return Ok(ServerResponse::Events(
                            EventsResponse::SessionEventsSnapshot {
                                session: self.backend.sessions()?.get(session_id)?,
                                events,
                                oldest_sequence,
                                latest_sequence: session_latest_sequence
                                    .unwrap_or(journal.next_sequence),
                                stream_epoch: current_stream_epoch,
                            },
                        ));
                    }
                }
                Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                    events,
                    stream_epoch: session_id.map(|_| self.backend.node_id.clone()),
                }))
            }
            ClientRequest::Events(EventsRequest::GetRecentSessionEvents { session_id, limit }) => {
                Ok(ServerResponse::Events(EventsResponse::SessionEvents {
                    events: self.recent_session_events(session_id, limit as usize)?,
                    stream_epoch: Some(self.backend.node_id.clone()),
                }))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
