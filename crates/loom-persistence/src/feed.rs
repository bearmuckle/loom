use super::*;

impl FilePersistence {
    /// Loads the bounded reconnect feed from sequence-indexed records.
    pub fn load_feed_state(&self) -> Result<Option<DurableFeedState>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let meta = connection
            .query_row(
                "SELECT next_sequence, retention_limit FROM feed_store_meta WHERE singleton=1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read event feed: {error}"), true)
            })?;
        let Some((next_sequence, retention_limit)) = meta else {
            return Ok(None);
        };
        let largest_stream_bytes: i64 = connection
            .query_row(
                "SELECT COALESCE(MAX(stream_bytes), 0) FROM (
                    SELECT SUM(length(payload)) AS stream_bytes
                    FROM feed_events GROUP BY session_id
                    UNION ALL
                    SELECT SUM(length(payload)) AS stream_bytes
                    FROM workspace_feed_events GROUP BY workspace_id
                 )",
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(format!("could not validate event feed size: {error}"), true)
            })?;
        let total_feed_bytes: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(length(payload)), 0) FROM feed_events",
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not validate total event feed size: {error}"),
                    true,
                )
            })?;
        if largest_stream_bytes < 0
            || largest_stream_bytes > MAX_DURABLE_FEED_SESSION_BYTES as i64
            || total_feed_bytes < 0
            || total_feed_bytes > MAX_DURABLE_FEED_TOTAL_BYTES as i64
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted event feed exceeds its byte limit",
                false,
            ));
        }
        let mut statement = connection
            .prepare(
                "SELECT sequence, session_id, payload_codec, payload
                 FROM feed_events ORDER BY sequence",
            )
            .map_err(|error| {
                persistence_error(format!("could not prepare event feed: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read event feed: {error}"), true)
            })?;
        let mut events = Vec::new();
        for row in rows {
            let (sequence, session_id, payload_codec, payload) = row.map_err(|error| {
                persistence_error(format!("could not read event feed: {error}"), true)
            })?;
            let payload = match payload_codec {
                0 => payload,
                1 => {
                    let mut decoded = Vec::new();
                    ZlibDecoder::new(payload.as_slice())
                        .take(MAX_FEED_EVENT_BYTES as u64 + 1)
                        .read_to_end(&mut decoded)
                        .map_err(|error| {
                            LoomError::new(
                                ErrorCode::MalformedPayload,
                                format!("persisted event feed entry is malformed: {error}"),
                                false,
                            )
                        })?;
                    decoded
                }
                _ => {
                    return Err(LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted event feed entry uses an unsupported codec",
                        false,
                    ));
                }
            };
            if payload.len() > MAX_FEED_EVENT_BYTES {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted event feed entry exceeds the maximum supported size",
                    false,
                ));
            }
            let event: ServerEventEnvelope = serde_json::from_slice(&payload).map_err(|error| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted event feed entry is malformed: {error}"),
                    false,
                )
            })?;
            if sequence < 0
                || event.sequence.value() != sequence as u64
                || event.session_id.as_uuid().as_bytes().as_slice() != session_id.as_slice()
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted event feed index does not match its payload",
                    false,
                ));
            }
            events.push(event);
        }
        let next_sequence = u64::try_from(next_sequence).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted event cursor is negative",
                false,
            )
        })?;
        let retention_limit = usize::try_from(retention_limit).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted event retention is invalid",
                false,
            )
        })?;
        let mut events_per_session = BTreeMap::<AgentSessionId, usize>::new();
        for event in &events {
            *events_per_session.entry(event.session_id).or_default() += 1;
        }
        let workspace_events = load_all_workspace_events(&connection)?;
        let mut events_per_workspace = BTreeMap::<WorkspaceId, usize>::new();
        let mut seen_sequences = events
            .iter()
            .map(|event| event.sequence)
            .collect::<BTreeSet<_>>();
        for event in &workspace_events {
            *events_per_workspace.entry(event.workspace_id).or_default() += 1;
            if !seen_sequences.insert(event.sequence) || event.sequence.value() > next_sequence {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted session and workspace event sequences overlap or exceed the cursor",
                    false,
                ));
            }
        }
        if events_per_session
            .values()
            .any(|count| *count > retention_limit)
            || events_per_workspace
                .values()
                .any(|count| *count > retention_limit)
            || events
                .last()
                .is_some_and(|event| event.sequence.value() > next_sequence)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted event feed exceeds its cursor or retention limit",
                false,
            ));
        }
        Ok(Some(DurableFeedState {
            next_sequence: EventSequence::new(next_sequence),
            retention_limit,
            events,
            workspace_events,
        }))
    }

    /// Loads only the global feed cursor metadata for startup.
    pub fn load_feed_header(&self) -> Result<Option<DurableFeedHeader>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let meta = connection
            .query_row(
                "SELECT next_sequence, retention_limit FROM feed_store_meta WHERE singleton=1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read event feed header: {error}"), true)
            })?;
        meta.map(|(next_sequence, retention_limit)| {
            Ok(DurableFeedHeader {
                next_sequence: EventSequence::new(decode_counter(next_sequence, "event cursor")?),
                retention_limit: usize::try_from(retention_limit).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted event retention is invalid",
                        false,
                    )
                })?,
            })
        })
        .transpose()
    }

    /// Loads lightweight retained-boundary metadata for one session stream.
    pub fn load_feed_session_cursor(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFeedSessionCursor>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        Self::load_feed_session_cursor_on(&connection, session_id)
    }

    pub(crate) fn load_feed_session_cursor_on(
        connection: &Connection,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFeedSessionCursor>> {
        let cursor = connection
            .query_row(
                "SELECT first_sequence, latest_sequence, pruned_through,
                        (SELECT MIN(sequence) FROM feed_events WHERE session_id=?1)
                 FROM feed_session_meta WHERE session_id=?1",
                [session_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read session feed cursor: {error}"), true)
            })?;
        cursor
            .map(|(first, latest, pruned, oldest)| {
                Ok(DurableFeedSessionCursor {
                    first_sequence: EventSequence::new(decode_counter(
                        first,
                        "first event sequence",
                    )?),
                    latest_sequence: EventSequence::new(decode_counter(
                        latest,
                        "latest event sequence",
                    )?),
                    pruned_through: EventSequence::new(decode_counter(
                        pruned,
                        "pruned event sequence",
                    )?),
                    oldest_retained_sequence: oldest
                        .map(|sequence| decode_counter(sequence, "oldest event sequence"))
                        .transpose()?
                        .map(EventSequence::new),
                })
            })
            .transpose()
    }

    /// Loads lightweight retained-boundary metadata for one workspace stream.
    pub fn load_feed_workspace_cursor(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<DurableFeedWorkspaceCursor>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let cursor = connection
            .query_row(
                "SELECT first_sequence, latest_sequence, pruned_through,
                        (SELECT MIN(sequence) FROM (
                            SELECT sequence FROM feed_events WHERE workspace_id=?1
                            UNION ALL
                            SELECT sequence FROM workspace_feed_events WHERE workspace_id=?1
                        ))
                 FROM feed_workspace_meta WHERE workspace_id=?1",
                [workspace_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read workspace feed cursor: {error}"),
                    true,
                )
            })?;
        cursor
            .map(|(first, latest, pruned, oldest)| {
                Ok(DurableFeedWorkspaceCursor {
                    first_sequence: EventSequence::new(decode_counter(
                        first,
                        "first event sequence",
                    )?),
                    latest_sequence: EventSequence::new(decode_counter(
                        latest,
                        "latest event sequence",
                    )?),
                    pruned_through: EventSequence::new(decode_counter(
                        pruned,
                        "pruned event sequence",
                    )?),
                    oldest_retained_sequence: oldest
                        .map(|sequence| decode_counter(sequence, "oldest event sequence"))
                        .transpose()?
                        .map(EventSequence::new),
                })
            })
            .transpose()
    }

    /// Loads retained events belonging to one workspace after a global sequence cursor.
    pub fn load_feed_workspace_events_since(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<WorkspaceFeedEvent>> {
        let mut events = self
            .load_feed_events_by_workspace(workspace_id, after_sequence)?
            .into_iter()
            .map(WorkspaceFeedEvent::Session)
            .collect::<Vec<_>>();
        events.extend(self.load_workspace_only_events_since(workspace_id, after_sequence)?);
        events.sort_by_key(|event| match event {
            WorkspaceFeedEvent::Session(event) => event.sequence,
            WorkspaceFeedEvent::Workspace(event) => event.sequence,
        });
        Ok(events)
    }

    pub(crate) fn load_workspace_only_events_since(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<WorkspaceFeedEvent>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let after = after_sequence
            .map(|sequence| sequence.value() as i64)
            .unwrap_or(0);
        let mut statement = connection
            .prepare(
                "SELECT sequence, payload_codec, payload FROM workspace_feed_events
             WHERE workspace_id=?1 AND sequence>?2 ORDER BY sequence ASC",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare workspace-only feed page: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map(
                params![workspace_id.as_uuid().as_bytes().as_slice(), after],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not read workspace-only feed page: {error}"),
                    true,
                )
            })?;
        rows.map(|row| {
            let (sequence, codec, payload) = row.map_err(|error| {
                persistence_error(
                    format!("could not read workspace-only feed row: {error}"),
                    true,
                )
            })?;
            let payload = decode_feed_payload(codec, payload)?;
            let event: WorkspaceEventEnvelope =
                serde_json::from_slice(&payload).map_err(|error| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        format!("persisted workspace event is malformed: {error}"),
                        false,
                    )
                })?;
            if sequence <= 0
                || event.sequence.value() != sequence as u64
                || event.workspace_id != workspace_id
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted workspace event index does not match its payload",
                    false,
                ));
            }
            Ok(WorkspaceFeedEvent::Workspace(event))
        })
        .collect()
    }

    pub(crate) fn load_feed_events_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<ServerEventEnvelope>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let after = after_sequence
            .map(|sequence| {
                i64::try_from(sequence.value()).map_err(|_| {
                    LoomError::invalid_request("event cursor exceeds SQLite's integer range")
                })
            })
            .transpose()?;
        let mut statement = connection
            .prepare(
                "SELECT sequence, session_id, payload_codec, payload FROM feed_events
             WHERE workspace_id=?1 AND sequence>?2 ORDER BY sequence ASC",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare workspace event feed page: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map(
                params![
                    workspace_id.as_uuid().as_bytes().as_slice(),
                    after.unwrap_or(0)
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not read workspace event feed page: {error}"),
                    true,
                )
            })?;
        rows.map(|row| {
            let (sequence, session_id, codec, payload) = row.map_err(|error| {
                persistence_error(
                    format!("could not read workspace event feed row: {error}"),
                    true,
                )
            })?;
            decode_feed_event(sequence, session_id, codec, payload)
        })
        .collect()
    }

    /// Loads a session's retained events after a cursor without hydrating other streams.
    pub fn load_feed_events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<ServerEventEnvelope>> {
        self.load_feed_events_query(session_id, after_sequence, None, false)
    }

    /// Loads a bounded tail of one session's retained events.
    pub fn load_recent_feed_events(
        &self,
        session_id: AgentSessionId,
        limit: usize,
    ) -> Result<Vec<ServerEventEnvelope>> {
        self.load_feed_events_query(Some(session_id), None, Some(limit), true)
    }

    pub(crate) fn load_feed_events_query(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
        limit: Option<usize>,
        descending: bool,
    ) -> Result<Vec<ServerEventEnvelope>> {
        if !self.path.exists() || limit == Some(0) {
            return Ok(Vec::new());
        }
        let limit = limit
            .map(|limit| {
                i64::try_from(limit).map_err(|_| {
                    LoomError::invalid_request("event feed limit exceeds SQLite's integer range")
                })
            })
            .transpose()?
            .unwrap_or(-1);
        let connection = self.connection()?;
        let mut bindings = Vec::with_capacity(3);
        let mut predicates = Vec::with_capacity(2);
        if let Some(session_id) = session_id {
            bindings.push(SqlValue::Blob(session_id.as_uuid().as_bytes().to_vec()));
            predicates.push(format!("session_id=?{}", bindings.len()));
        }
        if let Some(sequence) = after_sequence {
            let sequence = i64::try_from(sequence.value()).map_err(|_| {
                LoomError::invalid_request("event cursor exceeds SQLite's integer range")
            })?;
            bindings.push(SqlValue::Integer(sequence));
            predicates.push(format!("sequence>?{}", bindings.len()));
        }
        let where_clause = if predicates.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", predicates.join(" AND "))
        };
        bindings.push(SqlValue::Integer(limit));
        let ordering = if descending { "DESC" } else { "ASC" };
        let sql = format!(
            "SELECT sequence, session_id, payload_codec, payload FROM feed_events
             {where_clause} ORDER BY sequence {ordering} LIMIT ?{}",
            bindings.len()
        );
        let mut statement = connection.prepare(&sql).map_err(|error| {
            persistence_error(format!("could not prepare event feed page: {error}"), true)
        })?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(bindings.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(|error| {
                persistence_error(format!("could not read event feed page: {error}"), true)
            })?;
        let mut events = rows
            .map(|row| {
                let (sequence, session_id, codec, payload) = row.map_err(|error| {
                    persistence_error(format!("could not read event feed row: {error}"), true)
                })?;
                decode_feed_event(sequence, session_id, codec, payload)
            })
            .collect::<Result<Vec<_>>>()?;
        if descending {
            events.reverse();
        }
        Ok(events)
    }
}
