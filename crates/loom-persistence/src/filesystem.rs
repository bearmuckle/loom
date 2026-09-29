use super::*;

impl FilePersistence {
    pub fn list_filesystem_sessions(&self) -> Result<Vec<AgentSessionId>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT session_id FROM session_filesystems ORDER BY session_id")
            .map_err(|error| {
                persistence_error(format!("could not prepare filesystem index: {error}"), true)
            })?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|error| {
                persistence_error(format!("could not read filesystem index: {error}"), true)
            })?;
        rows.map(|row| {
            row.map_err(|error| {
                persistence_error(format!("could not read filesystem index: {error}"), true)
            })
            .and_then(|id| decode_uuid(&id, "filesystem session id"))
            .map(AgentSessionId::from_uuid)
        })
        .collect()
    }

    /// Appends newly observed filesystem changes and enforces bounded retention.
    pub fn save_filesystem_changes(
        &self,
        session_id: AgentSessionId,
        next_sequence: EventSequence,
        changes: &[SessionFilesystemChange],
    ) -> Result<()> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin filesystem change write: {error}"),
                true,
            )
        })?;
        let next_sequence = encode_counter(next_sequence.value(), "filesystem sequence")?;
        transaction
            .execute(
                "INSERT INTO filesystem_change_state(session_id, next_sequence)
                 VALUES (?1, ?2)
                 ON CONFLICT(session_id) DO UPDATE SET
                    next_sequence=MAX(filesystem_change_state.next_sequence, excluded.next_sequence)",
                params![session_id.as_uuid().as_bytes().as_slice(), next_sequence],
            )
            .map_err(|error| {
                persistence_error(format!("could not update filesystem sequence: {error}"), true)
            })?;
        save_filesystem_change_rows(&transaction, session_id, changes)?;
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit filesystem changes: {error}"),
                true,
            )
        })
    }

    /// Loads a bounded filesystem change page without restoring the workspace.
    pub fn load_filesystem_changes_page(
        &self,
        session_id: AgentSessionId,
        after: Option<EventSequence>,
        limit: usize,
    ) -> Result<DurableFilesystemChangesPage> {
        if !(1..=MAX_FILESYSTEM_CHANGE_PAGE_SIZE).contains(&limit) {
            return Err(LoomError::invalid_request(format!(
                "filesystem change page size must be between 1 and {MAX_FILESYSTEM_CHANGE_PAGE_SIZE}"
            )));
        }
        if !self.path.exists() {
            return Ok(DurableFilesystemChangesPage {
                changes: Vec::new(),
                truncated: false,
            });
        }
        let after = after
            .map(|sequence| encode_counter(sequence.value(), "filesystem sequence"))
            .transpose()?;
        let connection = self.connection()?;
        let first_retained: Option<i64> = connection
            .query_row(
                "SELECT MIN(sequence) FROM filesystem_changes WHERE session_id=?1",
                [session_id.as_uuid().as_bytes().as_slice()],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not inspect filesystem change retention: {error}"),
                    true,
                )
            })?;
        let limit_plus_one = i64::try_from(limit + 1).map_err(|_| {
            LoomError::invalid_request("filesystem change page size is out of range")
        })?;
        let mut statement = connection
            .prepare(
                "SELECT sequence, path, kind, revision FROM filesystem_changes
                 WHERE session_id=?1 AND (?2 IS NULL OR sequence>?2)
                 ORDER BY sequence DESC LIMIT ?3",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare filesystem change page: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map(
                params![
                    session_id.as_uuid().as_bytes().as_slice(),
                    after,
                    limit_plus_one
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not query filesystem change page: {error}"),
                    true,
                )
            })?;
        let mut rows = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(
                    format!("could not read filesystem change page: {error}"),
                    true,
                )
            })?;
        let has_more = rows.len() > limit;
        if has_more {
            rows.truncate(limit);
        }
        rows.reverse();
        let history_pruned = after
            .zip(first_retained)
            .is_some_and(|(cursor, first)| first > cursor.saturating_add(1));
        let mut changes = Vec::with_capacity(rows.len());
        for (sequence, path, kind, revision) in rows {
            changes.push(SessionFilesystemChange {
                sequence: EventSequence::new(decode_counter(sequence, "filesystem sequence")?),
                session_id,
                path,
                kind: parse_workspace_change_kind(&kind)?,
                revision,
            });
        }
        Ok(DurableFilesystemChangesPage {
            changes,
            truncated: history_pruned || has_more,
        })
    }

    pub fn load_filesystem_record(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFilesystemRecord>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let connection = self.connection()?;
        let row = connection
            .query_row(
                "SELECT root, control, payload_hash, raw_size, payload_codec, payload
                 FROM session_filesystems WHERE session_id=?1",
                [session_id.as_uuid().as_bytes().as_slice()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| {
                persistence_error(format!("could not read filesystem record: {error}"), true)
            })?;
        let Some((root, control, hash, raw_size, codec, payload)) = row else {
            return Ok(None);
        };
        let control = parse_workspace_control(&control)?;
        let raw_size = usize::try_from(raw_size).map_err(|_| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted filesystem payload has an invalid size",
                false,
            )
        })?;
        if raw_size > MAX_CONTENT_BYTES {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted filesystem payload exceeds the maximum supported size",
                false,
            ));
        }
        let raw = match codec {
            0 => payload,
            1 => {
                let mut decoded = Vec::with_capacity(raw_size);
                ZlibDecoder::new(payload.as_slice())
                    .take((raw_size as u64).saturating_add(1))
                    .read_to_end(&mut decoded)
                    .map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("persisted filesystem payload is malformed: {error}"),
                            false,
                        )
                    })?;
                decoded
            }
            _ => {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted filesystem payload uses an unsupported codec",
                    false,
                ));
            }
        };
        if raw.len() != raw_size || Sha256::digest(&raw).as_slice() != hash {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted filesystem payload failed its length or hash check",
                false,
            ));
        }
        let mut payload: Value = serde_json::from_slice(&raw).map_err(|error| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem payload is malformed: {error}"),
                false,
            )
        })?;
        let filesystem = payload.get("filesystem").ok_or_else(|| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "persisted filesystem payload has no workspace snapshot",
                false,
            )
        })?;
        let payload_root = filesystem.get("root").and_then(Value::as_str);
        let payload_control = filesystem.get("control").and_then(Value::as_str);
        if payload_root != Some(root.as_str())
            || payload_control != Some(workspace_control_name(control))
            || filesystem.get("session_id").and_then(Value::as_str)
                != Some(session_id.to_string().as_str())
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem index columns do not match the payload",
                false,
            ));
        }
        let sequence = connection
            .query_row(
                "SELECT next_sequence FROM filesystem_change_state WHERE session_id=?1",
                [session_id.as_uuid().as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not read filesystem sequence state: {error}"),
                    true,
                )
            })?
            .map(|sequence| decode_counter(sequence, "filesystem sequence"))
            .transpose()?
            .unwrap_or(0);
        let payload_sequence = filesystem
            .get("next_sequence")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted filesystem sequence is missing",
                    false,
                )
            })?;
        if sequence < payload_sequence {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem sequence index is behind its payload",
                false,
            ));
        }
        if sequence > payload_sequence {
            payload["filesystem"]["next_sequence"] = serde_json::json!(sequence);
        }
        let checkpoint_rows = {
            let mut statement = connection
                .prepare(
                    "SELECT checkpoint_id, label, created_at FROM checkpoints
                     WHERE session_id=?1 ORDER BY created_at, checkpoint_id",
                )
                .map_err(|error| {
                    persistence_error(format!("could not prepare checkpoint list: {error}"), true)
                })?;
            let rows = statement
                .query_map([session_id.as_uuid().as_bytes().as_slice()], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|error| {
                    persistence_error(format!("could not read checkpoint list: {error}"), true)
                })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    persistence_error(format!("could not read checkpoint list: {error}"), true)
                })?
        };
        let mut checkpoints = Vec::with_capacity(checkpoint_rows.len());
        for (checkpoint_id, label, created_at) in checkpoint_rows {
            let checkpoint_id =
                CheckpointId::from_uuid(decode_uuid(&checkpoint_id, "checkpoint id")?);
            let file_rows = {
                let mut statement = connection
                    .prepare(
                        "SELECT path, existed, revision, expected_revision, content_hash
                         FROM checkpoint_files WHERE session_id=?1 AND checkpoint_id=?2
                         ORDER BY path",
                    )
                    .map_err(|error| {
                        persistence_error(
                            format!("could not prepare checkpoint files: {error}"),
                            true,
                        )
                    })?;
                let rows = statement
                    .query_map(
                        params![
                            session_id.as_uuid().as_bytes().as_slice(),
                            checkpoint_id.as_uuid().as_bytes().as_slice()
                        ],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, Vec<u8>>(4)?,
                            ))
                        },
                    )
                    .map_err(|error| {
                        persistence_error(format!("could not read checkpoint files: {error}"), true)
                    })?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|error| {
                        persistence_error(format!("could not read checkpoint files: {error}"), true)
                    })?
            };
            let mut files = BTreeMap::new();
            for (path, existed, revision, expected_revision, content_hash) in file_rows {
                let content = decode_content(&connection, &content_hash)?;
                files.insert(
                    path,
                    CheckpointFile {
                        existed: existed != 0,
                        content,
                        revision,
                        expected_revision,
                    },
                );
            }
            checkpoints.push(Checkpoint {
                id: checkpoint_id,
                session_id,
                label,
                created_at: decode_timestamp(created_at)?,
                files,
            });
        }
        let edit_rows = {
            let mut statement = connection
                .prepare(
                    "SELECT edit_id, path, before_hash, after_revision, source FROM filesystem_edits
                     WHERE session_id=?1 ORDER BY edit_id",
                )
                .map_err(|error| {
                    persistence_error(format!("could not prepare filesystem edits: {error}"), true)
                })?;
            let rows = statement
                .query_map([session_id.as_uuid().as_bytes().as_slice()], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .map_err(|error| {
                    persistence_error(format!("could not read filesystem edits: {error}"), true)
                })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    persistence_error(format!("could not read filesystem edits: {error}"), true)
                })?
        };
        let mut edits = Vec::with_capacity(edit_rows.len());
        for (edit_id, path, before_hash, after_revision, source) in edit_rows {
            let before_bytes = before_hash
                .as_deref()
                .map(|hash| load_content_range(&connection, hash, 0, MAX_CONTENT_BYTES))
                .transpose()?;
            let before = before_bytes
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok());
            edits.push(DurableFilesystemEdit {
                id: u64::try_from(edit_id).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "persisted edit id is invalid",
                        false,
                    )
                })?,
                path,
                before,
                before_bytes,
                after_revision,
                source: parse_workspace_control(&source)?,
            });
        }
        let changes = Vec::new();
        let repositories = {
            let mut statement = connection
                .prepare(
                    "SELECT repository_id, source, path, revision, attached_at
                     FROM session_repositories WHERE session_id=?1 ORDER BY repository_id",
                )
                .map_err(|error| {
                    persistence_error(format!("could not prepare repository rows: {error}"), true)
                })?;
            let rows = statement
                .query_map([session_id.as_uuid().as_bytes().as_slice()], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })
                .map_err(|error| {
                    persistence_error(format!("could not read repository rows: {error}"), true)
                })?;
            let rows = rows
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    persistence_error(format!("could not read repository rows: {error}"), true)
                })?;
            let mut repositories = BTreeMap::new();
            for (id, source, path, revision, attached_at) in rows {
                let id = RepositoryId::from_uuid(decode_uuid(&id, "repository id")?);
                repositories.insert(
                    id,
                    SessionRepository {
                        id,
                        source,
                        path,
                        revision,
                        attached_at: decode_timestamp(attached_at)?,
                    },
                );
            }
            repositories
        };
        let directories = {
            let mut statement = connection
                .prepare("SELECT source, path FROM session_directories WHERE session_id=?1 ORDER BY ordinal")
                .map_err(|error| persistence_error(format!("could not prepare mounted directories: {error}"), true))?;
            let rows = statement
                .query_map([session_id.as_uuid().as_bytes().as_slice()], |row| {
                    Ok(SessionDirectory {
                        source: row.get(0)?,
                        path: row.get(1)?,
                    })
                })
                .map_err(|error| {
                    persistence_error(format!("could not read mounted directories: {error}"), true)
                })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|error| {
                    persistence_error(format!("could not read mounted directories: {error}"), true)
                })?
        };
        Ok(Some(DurableFilesystemRecord {
            session_id,
            root,
            control,
            checkpoints,
            edits,
            changes,
            repositories,
            directories,
            payload,
            delta: None,
        }))
    }
}
