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
pub(crate) fn save_feed_rows(transaction: &Transaction<'_>, feed: &DurableFeedState) -> Result<()> {
    save_feed_rows_with_limits(
        transaction,
        feed,
        MAX_DURABLE_FEED_SESSION_BYTES,
        MAX_DURABLE_FEED_TOTAL_BYTES,
    )
}

pub(crate) fn save_feed_rows_with_limits(
    transaction: &Transaction<'_>,
    feed: &DurableFeedState,
    session_byte_limit: usize,
    total_byte_limit: usize,
) -> Result<()> {
    save_feed_rows_with_limits_and_pruning(
        transaction,
        feed,
        session_byte_limit,
        total_byte_limit,
        true,
    )
}

pub(crate) fn save_feed_rows_incremental(
    transaction: &Transaction<'_>,
    feed: &DurableFeedState,
) -> Result<()> {
    save_feed_rows_with_limits_and_pruning(
        transaction,
        feed,
        MAX_DURABLE_FEED_SESSION_BYTES,
        MAX_DURABLE_FEED_TOTAL_BYTES,
        false,
    )
}

pub(crate) fn save_feed_rows_with_limits_and_pruning(
    transaction: &Transaction<'_>,
    feed: &DurableFeedState,
    session_byte_limit: usize,
    total_byte_limit: usize,
    prune: bool,
) -> Result<()> {
    let next_sequence = i64::try_from(feed.next_sequence.value()).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            "event sequence exceeds SQLite's integer range",
            false,
        )
    })?;
    let retention_limit = i64::try_from(feed.retention_limit).map_err(|_| {
        LoomError::new(
            ErrorCode::Persistence,
            "event retention limit exceeds SQLite's integer range",
            false,
        )
    })?;
    let mut previous = 0;
    for event in &feed.events {
        let sequence = i64::try_from(event.sequence.value()).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "event sequence exceeds SQLite's integer range",
                false,
            )
        })?;
        if sequence <= previous || sequence > next_sequence {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "event feed sequences are invalid: sequence {sequence} after {previous} with cursor {next_sequence}"
                ),
                false,
            ));
        }
        previous = sequence;
        let workspace_id = transaction
            .query_row(
                "SELECT workspace_id FROM sessions WHERE id=?1",
                [event.session_id.as_uuid().as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not resolve event workspace: {error}"), true)
            })?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!(
                        "cannot persist event for unknown session {}",
                        event.session_id
                    ),
                    false,
                )
            })?;
        let raw_payload = serde_json::to_vec(event).map_err(|error| {
            persistence_error(format!("could not encode event feed entry: {error}"), false)
        })?;
        if raw_payload.len() > MAX_FEED_EVENT_BYTES {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "event feed entry exceeds the maximum supported size",
                false,
            ));
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&raw_payload).map_err(|error| {
            persistence_error(
                format!("could not compress event feed entry: {error}"),
                false,
            )
        })?;
        let compressed = encoder.finish().map_err(|error| {
            persistence_error(
                format!("could not compress event feed entry: {error}"),
                false,
            )
        })?;
        let (payload_codec, payload) = if compressed.len() < raw_payload.len() {
            (1_i64, compressed)
        } else {
            (0_i64, raw_payload)
        };
        transaction
            .execute(
                "INSERT INTO feed_events(sequence, session_id, workspace_id, payload_codec, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(sequence) DO NOTHING",
                params![
                    sequence,
                    event.session_id.as_uuid().as_bytes().as_slice(),
                    workspace_id.as_slice(),
                    payload_codec,
                    payload
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save event feed entry: {error}"), true)
            })?;
        transaction
            .execute(
                "INSERT INTO feed_workspace_meta(
                    workspace_id, first_sequence, latest_sequence, pruned_through
                 ) VALUES (?1, ?2, ?2, 0)
                 ON CONFLICT(workspace_id) DO UPDATE SET
                    latest_sequence=MAX(feed_workspace_meta.latest_sequence, excluded.latest_sequence)",
                params![workspace_id.as_slice(), sequence],
            )
            .map_err(|error| persistence_error(format!("could not save workspace feed cursor: {error}"), true))?;
        transaction
            .execute(
                "INSERT INTO feed_session_meta(
                    session_id, first_sequence, latest_sequence, pruned_through
                 ) VALUES (?1, ?2, ?2, 0)
                 ON CONFLICT(session_id) DO UPDATE SET
                    latest_sequence=MAX(feed_session_meta.latest_sequence, excluded.latest_sequence)",
                params![
                    event.session_id.as_uuid().as_bytes().as_slice(),
                    sequence
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save session feed cursor: {error}"), true)
            })?;
    }
    let mut previous_workspace = 0;
    for event in &feed.workspace_events {
        let sequence = i64::try_from(event.sequence.value()).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "workspace event sequence exceeds SQLite's integer range",
                false,
            )
        })?;
        if sequence <= previous_workspace || sequence > next_sequence {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!(
                    "workspace event feed sequences are invalid: sequence {sequence} after {previous_workspace} with cursor {next_sequence}"
                ),
                false,
            ));
        }
        previous_workspace = sequence;
        let duplicate: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM feed_events WHERE sequence=?1)",
                [sequence],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not validate workspace event sequence: {error}"),
                    true,
                )
            })?;
        if duplicate {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "workspace and session event sequences overlap",
                false,
            ));
        }
        let raw_payload = serde_json::to_vec(event).map_err(|error| {
            persistence_error(
                format!("could not encode workspace feed entry: {error}"),
                false,
            )
        })?;
        if raw_payload.len() > MAX_FEED_EVENT_BYTES {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "workspace event exceeds the maximum supported size",
                false,
            ));
        }
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&raw_payload).map_err(|error| {
            persistence_error(
                format!("could not compress workspace event: {error}"),
                false,
            )
        })?;
        let compressed = encoder.finish().map_err(|error| {
            persistence_error(
                format!("could not compress workspace event: {error}"),
                false,
            )
        })?;
        let (codec, payload) = if compressed.len() < raw_payload.len() {
            (1_i64, compressed)
        } else {
            (0_i64, raw_payload)
        };
        transaction
            .execute(
                "INSERT INTO workspace_feed_events(sequence, workspace_id, payload_codec, payload)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT(sequence) DO NOTHING",
                params![
                    sequence,
                    event.workspace_id.as_uuid().as_bytes().as_slice(),
                    codec,
                    payload
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save workspace event: {error}"), true)
            })?;
        transaction.execute(
            "INSERT INTO feed_workspace_meta(workspace_id, first_sequence, latest_sequence, pruned_through)
             VALUES (?1, ?2, ?2, 0)
             ON CONFLICT(workspace_id) DO UPDATE SET latest_sequence=MAX(feed_workspace_meta.latest_sequence, excluded.latest_sequence)",
            params![event.workspace_id.as_uuid().as_bytes().as_slice(), sequence],
        ).map_err(|error| persistence_error(format!("could not save workspace feed cursor: {error}"), true))?;
    }
    save_feed_store_meta(transaction, next_sequence, retention_limit)?;
    if !prune {
        return Ok(());
    }
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_pruned_feed (
                sequence INTEGER PRIMARY KEY,
                session_id BLOB NOT NULL,
                workspace_id BLOB NOT NULL
             ) WITHOUT ROWID;
             DELETE FROM _loom_pruned_feed;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prepare reconnect feed pruning: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "WITH ranked AS (
                SELECT sequence, session_id, workspace_id,
                       SUM(length(payload)) OVER (
                           PARTITION BY session_id ORDER BY sequence DESC
                       ) AS session_bytes,
                       ROW_NUMBER() OVER (
                           PARTITION BY session_id ORDER BY sequence DESC
                       ) AS session_position,
                       SUM(length(payload)) OVER (
                           ORDER BY sequence DESC
                       ) + (SELECT COALESCE(SUM(length(payload)), 0)
                            FROM workspace_feed_events) AS total_bytes
                FROM feed_events
             )
             INSERT INTO _loom_pruned_feed(sequence, session_id, workspace_id)
             SELECT sequence, session_id, workspace_id FROM ranked
             WHERE session_bytes > ?1 OR session_position > ?2 OR total_bytes > ?3",
            params![
                session_byte_limit as i64,
                retention_limit,
                total_byte_limit as i64
            ],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not select reconnect feed retention: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "WITH pruned AS (
                SELECT workspace_id, MAX(sequence) AS pruned_through
                FROM _loom_pruned_feed GROUP BY workspace_id
             )
             UPDATE feed_workspace_meta
             SET pruned_through=MAX(
                    pruned_through,
                    (SELECT pruned.pruned_through FROM pruned
                     WHERE pruned.workspace_id=feed_workspace_meta.workspace_id)
                 )
             WHERE workspace_id IN (SELECT workspace_id FROM pruned)",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not update pruned workspace feed cursors: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "WITH pruned AS (
                SELECT session_id, MAX(sequence) AS pruned_through
                FROM _loom_pruned_feed GROUP BY session_id
             )
             UPDATE feed_session_meta
             SET pruned_through=MAX(
                    pruned_through,
                    (SELECT pruned.pruned_through FROM pruned
                     WHERE pruned.session_id=feed_session_meta.session_id)
                 )
             WHERE session_id IN (SELECT session_id FROM pruned)",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not update pruned feed cursors: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM feed_events WHERE sequence IN (
                SELECT sequence FROM _loom_pruned_feed
             )",
            [],
        )
        .map_err(|error| persistence_error(format!("could not prune event feed: {error}"), true))?;
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_pruned_workspace_feed (
            sequence INTEGER PRIMARY KEY,
            workspace_id BLOB NOT NULL
         ) WITHOUT ROWID, STRICT;
         DELETE FROM _loom_pruned_workspace_feed;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prepare workspace feed pruning: {error}"),
                true,
            )
        })?;
    transaction.execute(
        "WITH ranked AS (
            SELECT sequence, workspace_id,
                   SUM(length(payload)) OVER (PARTITION BY workspace_id ORDER BY sequence DESC) AS stream_bytes,
                   ROW_NUMBER() OVER (PARTITION BY workspace_id ORDER BY sequence DESC) AS stream_position,
                   SUM(length(payload)) OVER (ORDER BY sequence DESC)
                       + (SELECT COALESCE(SUM(length(payload)), 0) FROM feed_events) AS total_bytes
            FROM workspace_feed_events
         )
         INSERT INTO _loom_pruned_workspace_feed(sequence, workspace_id)
         SELECT sequence, workspace_id FROM ranked
         WHERE stream_bytes > ?1 OR stream_position > ?2 OR total_bytes > ?3",
        params![session_byte_limit as i64, retention_limit, total_byte_limit as i64],
    ).map_err(|error| persistence_error(format!("could not select workspace feed retention: {error}"), true))?;
    transaction
        .execute(
            "WITH pruned AS (
            SELECT workspace_id, MAX(sequence) AS pruned_through
            FROM _loom_pruned_workspace_feed GROUP BY workspace_id
         )
         UPDATE feed_workspace_meta
         SET pruned_through=MAX(pruned_through,
             (SELECT pruned.pruned_through FROM pruned
              WHERE pruned.workspace_id=feed_workspace_meta.workspace_id))
         WHERE workspace_id IN (SELECT workspace_id FROM pruned)",
            [],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not update pruned workspace-only cursors: {error}"),
                true,
            )
        })?;
    transaction
        .execute(
            "DELETE FROM workspace_feed_events WHERE sequence IN (
            SELECT sequence FROM _loom_pruned_workspace_feed
         )",
            [],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune workspace feed: {error}"), true)
        })?;
    Ok(())
}

pub(crate) fn save_feed_store_meta(
    transaction: &Transaction<'_>,
    next_sequence: i64,
    retention_limit: i64,
) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO feed_store_meta(singleton, next_sequence, retention_limit)
             VALUES (1, ?1, ?2)
             ON CONFLICT(singleton) DO UPDATE SET
                next_sequence=MAX(feed_store_meta.next_sequence, excluded.next_sequence),
                retention_limit=excluded.retention_limit
             WHERE feed_store_meta.next_sequence < excluded.next_sequence
                OR feed_store_meta.retention_limit IS NOT excluded.retention_limit",
            params![next_sequence, retention_limit],
        )
        .map_err(|error| {
            persistence_error(format!("could not save event feed cursor: {error}"), true)
        })?;
    Ok(())
}

pub(crate) fn decode_feed_event(
    sequence: i64,
    session_id: Vec<u8>,
    payload_codec: i64,
    payload: Vec<u8>,
) -> Result<ServerEventEnvelope> {
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
    if sequence <= 0 || payload.len() > MAX_FEED_EVENT_BYTES {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted event feed entry exceeds its supported size or sequence range",
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
    if event.sequence.value() != sequence as u64
        || event.session_id.as_uuid().as_bytes().as_slice() != session_id.as_slice()
    {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted event feed index does not match its payload",
            false,
        ));
    }
    Ok(event)
}

pub(crate) fn decode_feed_payload(payload_codec: i64, payload: Vec<u8>) -> Result<Vec<u8>> {
    let decoded = match payload_codec {
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
    if decoded.len() > MAX_FEED_EVENT_BYTES {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted event feed entry exceeds the maximum supported size",
            false,
        ));
    }
    Ok(decoded)
}

#[cfg(test)]
pub(crate) fn workspace_feed_event_sequence(event: &WorkspaceFeedEvent) -> u64 {
    match event {
        WorkspaceFeedEvent::Session(event) => event.sequence.value(),
        WorkspaceFeedEvent::Workspace(event) => event.sequence.value(),
    }
}

pub(crate) fn load_all_workspace_events(
    connection: &Connection,
) -> Result<Vec<WorkspaceEventEnvelope>> {
    let mut statement = connection
        .prepare(
            "SELECT sequence, workspace_id, payload_codec, payload
         FROM workspace_feed_events ORDER BY sequence",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prepare workspace event feed: {error}"),
                true,
            )
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
            persistence_error(
                format!("could not read workspace event feed: {error}"),
                true,
            )
        })?;
    rows.map(|row| {
        let (sequence, workspace_id, codec, payload) = row.map_err(|error| {
            persistence_error(format!("could not read workspace event: {error}"), true)
        })?;
        let payload = decode_feed_payload(codec, payload)?;
        let event: WorkspaceEventEnvelope = serde_json::from_slice(&payload).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted workspace event is malformed: {error}"),
                false,
            )
        })?;
        if sequence <= 0
            || event.sequence.value() != sequence as u64
            || event.workspace_id.as_uuid().as_bytes().as_slice() != workspace_id.as_slice()
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted workspace event index does not match its payload",
                false,
            ));
        }
        Ok(event)
    })
    .collect()
}
