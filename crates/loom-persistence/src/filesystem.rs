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
pub(crate) fn save_filesystem_records(
    transaction: &Transaction<'_>,
    records: &[DurableFilesystemRecord],
) -> Result<()> {
    for record in records {
        let filesystem = record.payload.get("filesystem").ok_or_else(|| {
            LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem record has no workspace snapshot",
                false,
            )
        })?;
        let next_sequence = filesystem
            .get("next_sequence")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    "filesystem record has no change sequence high-water mark",
                    false,
                )
            })?;
        if filesystem.get("root").and_then(Value::as_str) != Some(record.root.as_str())
            || filesystem.get("control").and_then(Value::as_str)
                != Some(workspace_control_name(record.control))
            || filesystem.get("session_id").and_then(Value::as_str)
                != Some(record.session_id.to_string().as_str())
            || filesystem
                .get("checkpoints")
                .and_then(Value::as_array)
                .is_none_or(|checkpoints| !checkpoints.is_empty())
            || filesystem
                .get("edits")
                .and_then(Value::as_array)
                .is_none_or(|edits| !edits.is_empty())
            || filesystem
                .get("changes")
                .and_then(Value::as_array)
                .is_none_or(|changes| !changes.is_empty())
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem record index fields or checkpoint payload are invalid",
                false,
            ));
        }
        let raw = serde_json::to_vec(&record.payload).map_err(|error| {
            persistence_error(
                format!("could not encode filesystem record: {error}"),
                false,
            )
        })?;
        if raw.len() > MAX_CONTENT_BYTES {
            return Err(LoomError::new(
                ErrorCode::Persistence,
                "filesystem record exceeds the maximum supported size",
                false,
            ));
        }
        let hash = Sha256::digest(&raw);
        let unchanged = transaction
            .query_row(
                "SELECT payload_hash FROM session_filesystems WHERE session_id=?1",
                [record.session_id.as_uuid().as_bytes().as_slice()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|error| {
                persistence_error(
                    format!("could not compare filesystem record: {error}"),
                    true,
                )
            })?
            .is_some_and(|existing| existing == hash.as_slice());
        if !unchanged {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(&raw).map_err(|error| {
                persistence_error(
                    format!("could not compress filesystem record: {error}"),
                    false,
                )
            })?;
            let compressed = encoder.finish().map_err(|error| {
                persistence_error(
                    format!("could not compress filesystem record: {error}"),
                    false,
                )
            })?;
            let (codec, payload) = if compressed.len() < raw.len() {
                (1_i64, compressed)
            } else {
                (0_i64, raw.clone())
            };
            transaction
                .execute(
                    "INSERT INTO session_filesystems(
                        session_id, root, control, payload_hash, raw_size, payload_codec, payload
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(session_id) DO UPDATE SET
                        root=excluded.root,
                        control=excluded.control,
                        payload_hash=excluded.payload_hash,
                        raw_size=excluded.raw_size,
                        payload_codec=excluded.payload_codec,
                        payload=excluded.payload
                     WHERE session_filesystems.root IS NOT excluded.root
                        OR session_filesystems.control IS NOT excluded.control
                        OR session_filesystems.payload_hash IS NOT excluded.payload_hash",
                    params![
                        record.session_id.as_uuid().as_bytes().as_slice(),
                        record.root,
                        workspace_control_name(record.control),
                        hash.as_slice(),
                        raw.len() as i64,
                        codec,
                        payload,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!(
                            "could not save filesystem record for {}: {error}",
                            record.session_id
                        ),
                        true,
                    )
                })?;
        }
        let next_sequence = encode_counter(next_sequence, "filesystem sequence")?;
        transaction
            .execute(
                "INSERT INTO filesystem_change_state(session_id, next_sequence)
                 VALUES (?1, ?2)
                 ON CONFLICT(session_id) DO UPDATE SET
                    next_sequence=MAX(filesystem_change_state.next_sequence, excluded.next_sequence)",
                params![record.session_id.as_uuid().as_bytes().as_slice(), next_sequence],
            )
            .map_err(|error| {
                persistence_error(format!("could not save filesystem sequence: {error}"), true)
            })?;
        if let Some(delta) = &record.delta {
            save_checkpoint_delta_rows(
                transaction,
                record.session_id,
                &record.checkpoints,
                &delta.deleted_checkpoints,
            )?;
            save_filesystem_edit_delta_rows(
                transaction,
                record.session_id,
                &record.edits,
                &delta.deleted_edits,
            )?;
            save_filesystem_change_rows(transaction, record.session_id, &record.changes)?;
            for sequence in &delta.deleted_changes {
                transaction
                    .execute(
                        "DELETE FROM filesystem_changes WHERE session_id=?1 AND sequence=?2",
                        params![
                            record.session_id.as_uuid().as_bytes().as_slice(),
                            encode_counter(sequence.value(), "filesystem sequence")?
                        ],
                    )
                    .map_err(|error| {
                        persistence_error(
                            format!("could not delete filesystem change: {error}"),
                            true,
                        )
                    })?;
            }
        } else {
            save_checkpoint_rows(transaction, record.session_id, &record.checkpoints)?;
            save_filesystem_edit_rows(transaction, record.session_id, &record.edits)?;
            save_filesystem_change_rows(transaction, record.session_id, &record.changes)?;
        }
        save_session_repository_rows(transaction, record.session_id, &record.repositories)?;
        save_session_directory_rows(transaction, record.session_id, &record.directories)?;
    }
    collect_unused_content(transaction, MAX_CONTENT_GC_CANDIDATES_PER_WRITE)?;
    Ok(())
}

pub(crate) fn save_filesystem_edit_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    edits: &[DurableFilesystemEdit],
) -> Result<()> {
    transaction
        .execute(
            "DELETE FROM filesystem_edits WHERE session_id=?1",
            [session_id.as_uuid().as_bytes().as_slice()],
        )
        .map_err(|error| {
            persistence_error(format!("could not replace filesystem edits: {error}"), true)
        })?;
    for edit in edits {
        if edit.path.is_empty()
            || edit.path.len() > 4096
            || edit.after_revision.len() > 256
            || edit.before.as_ref().is_some_and(|text| {
                edit.before_bytes
                    .as_ref()
                    .is_some_and(|bytes| bytes != text.as_bytes())
            })
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem edit metadata is invalid",
                false,
            ));
        }
        let before = edit
            .before_bytes
            .as_deref()
            .or_else(|| edit.before.as_ref().map(String::as_bytes));
        let before_hash = before
            .map(|bytes| store_content(transaction, bytes))
            .transpose()?;
        transaction
            .execute(
                "INSERT INTO filesystem_edits(
                    session_id, edit_id, path, before_hash, after_revision, source
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(session_id, edit_id) DO UPDATE SET
                    path=excluded.path,
                    before_hash=excluded.before_hash,
                    after_revision=excluded.after_revision,
                    source=excluded.source
                 WHERE filesystem_edits.path IS NOT excluded.path
                    OR filesystem_edits.before_hash IS NOT excluded.before_hash
                    OR filesystem_edits.after_revision IS NOT excluded.after_revision
                    OR filesystem_edits.source IS NOT excluded.source",
                params![
                    session_id.as_uuid().as_bytes().as_slice(),
                    i64::try_from(edit.id).map_err(|_| LoomError::new(
                        ErrorCode::Persistence,
                        "invalid filesystem edit id",
                        false
                    ))?,
                    edit.path,
                    before_hash,
                    edit.after_revision,
                    workspace_control_name(edit.source),
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save filesystem edit: {error}"), true)
            })?;
    }
    Ok(())
}

pub(crate) fn save_filesystem_edit_delta_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    edits: &[DurableFilesystemEdit],
    deleted: &[u64],
) -> Result<()> {
    for id in deleted {
        transaction
            .execute(
                "DELETE FROM filesystem_edits WHERE session_id=?1 AND edit_id=?2",
                params![
                    session_id.as_uuid().as_bytes().as_slice(),
                    i64::try_from(*id)
                        .map_err(|_| LoomError::invalid_request("invalid filesystem edit id"))?
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not delete filesystem edit: {error}"), true)
            })?;
    }
    for edit in edits {
        if edit.id == 0
            || edit.path.is_empty()
            || edit.path.len() > 4096
            || edit.after_revision.len() > 256
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem edit metadata is invalid",
                false,
            ));
        }
        let before = edit
            .before_bytes
            .as_deref()
            .or_else(|| edit.before.as_ref().map(String::as_bytes));
        let before_hash = before
            .map(|bytes| store_content(transaction, bytes))
            .transpose()?;
        transaction.execute("INSERT INTO filesystem_edits(session_id,edit_id,path,before_hash,after_revision,source) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session_id,edit_id) DO UPDATE SET path=excluded.path,before_hash=excluded.before_hash,after_revision=excluded.after_revision,source=excluded.source", params![session_id.as_uuid().as_bytes().as_slice(), i64::try_from(edit.id).map_err(|_| LoomError::invalid_request("invalid filesystem edit id"))?, edit.path, before_hash, edit.after_revision, workspace_control_name(edit.source)]).map_err(|error| persistence_error(format!("could not save filesystem edit delta: {error}"), true))?;
    }
    Ok(())
}

pub(crate) fn save_checkpoint_delta_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    checkpoints: &[Checkpoint],
    deleted: &[CheckpointId],
) -> Result<()> {
    for checkpoint in checkpoints {
        if checkpoint.session_id != session_id || checkpoint.label.trim().is_empty() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "checkpoint identity or label is invalid",
                false,
            ));
        }
        let sid = session_id.as_uuid().as_bytes();
        let cid = checkpoint.id.as_uuid().as_bytes();
        transaction.execute("INSERT INTO checkpoints(session_id,checkpoint_id,label,created_at) VALUES(?1,?2,?3,?4) ON CONFLICT(session_id,checkpoint_id) DO UPDATE SET label=excluded.label,created_at=excluded.created_at WHERE checkpoints.label IS NOT excluded.label OR checkpoints.created_at IS NOT excluded.created_at", params![sid.as_slice(),cid.as_slice(),checkpoint.label,encode_timestamp(checkpoint.created_at)?]).map_err(|error| persistence_error(format!("could not save checkpoint delta: {error}"), true))?;
        let mut wanted = BTreeSet::new();
        for (path, file) in &checkpoint.files {
            if path.trim().is_empty() || file.content.len() > MAX_CONTENT_BYTES {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "checkpoint file is invalid",
                    false,
                ));
            }
            wanted.insert(path.clone());
            let hash = store_content(transaction, file.content.as_bytes())?;
            transaction.execute("INSERT INTO checkpoint_files(session_id,checkpoint_id,path,existed,revision,expected_revision,content_hash) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(session_id,checkpoint_id,path) DO UPDATE SET existed=excluded.existed,revision=excluded.revision,expected_revision=excluded.expected_revision,content_hash=excluded.content_hash WHERE checkpoint_files.existed IS NOT excluded.existed OR checkpoint_files.revision IS NOT excluded.revision OR checkpoint_files.expected_revision IS NOT excluded.expected_revision OR checkpoint_files.content_hash IS NOT excluded.content_hash",params![sid.as_slice(),cid.as_slice(),path,if file.existed{1_i64}else{0_i64},file.revision,file.expected_revision,hash]).map_err(|error|persistence_error(format!("could not save checkpoint file delta: {error}"),true))?;
        }
        let mut statement = transaction
            .prepare("SELECT path FROM checkpoint_files WHERE session_id=?1 AND checkpoint_id=?2")
            .map_err(|error| {
                persistence_error(format!("could not inspect checkpoint files: {error}"), true)
            })?;
        let rows = statement
            .query_map(params![sid.as_slice(), cid.as_slice()], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| {
                persistence_error(format!("could not inspect checkpoint files: {error}"), true)
            })?;
        let existing = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                persistence_error(format!("could not inspect checkpoint files: {error}"), true)
            })?;
        drop(statement);
        for path in existing {
            if !wanted.contains(&path) {
                transaction.execute("DELETE FROM checkpoint_files WHERE session_id=?1 AND checkpoint_id=?2 AND path=?3",params![sid.as_slice(),cid.as_slice(),path]).map_err(|error|persistence_error(format!("could not delete checkpoint file: {error}"),true))?;
            }
        }
    }
    for id in deleted {
        transaction
            .execute(
                "DELETE FROM checkpoints WHERE session_id=?1 AND checkpoint_id=?2",
                params![
                    session_id.as_uuid().as_bytes().as_slice(),
                    id.as_uuid().as_bytes().as_slice()
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not delete checkpoint: {error}"), true)
            })?;
    }
    Ok(())
}

pub(crate) fn save_filesystem_change_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    changes: &[SessionFilesystemChange],
) -> Result<()> {
    let mut max_sequence = transaction
        .query_row(
            "SELECT MAX(sequence) FROM filesystem_changes WHERE session_id=?1",
            [session_id.as_uuid().as_bytes().as_slice()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map_err(|error| {
            persistence_error(
                format!("could not inspect filesystem change high-water mark: {error}"),
                true,
            )
        })?;
    let mut previous = None;
    for change in changes {
        if change.session_id != session_id
            || change.path.is_empty()
            || change.path.len() > 4096
            || change
                .revision
                .as_ref()
                .is_some_and(|revision| revision.len() > 256)
            || previous.is_some_and(|sequence| sequence >= change.sequence)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "filesystem change identity or ordering is invalid",
                false,
            ));
        }
        previous = Some(change.sequence);
        let sequence = encode_counter(change.sequence.value(), "filesystem sequence")?;
        if max_sequence.is_some_and(|current| sequence <= current) {
            continue;
        }
        let session_id_bytes = session_id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO filesystem_changes(session_id, sequence, path, kind, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id, sequence) DO UPDATE SET
                    path=excluded.path,
                    kind=excluded.kind,
                    revision=excluded.revision
                 WHERE filesystem_changes.path IS NOT excluded.path
                    OR filesystem_changes.kind IS NOT excluded.kind
                    OR filesystem_changes.revision IS NOT excluded.revision",
                params![
                    session_id_bytes.as_slice(),
                    sequence,
                    change.path,
                    workspace_change_kind_name(change.kind),
                    change.revision,
                ],
            )
            .map_err(|error| {
                persistence_error(format!("could not save filesystem change: {error}"), true)
            })?;
        max_sequence = Some(sequence);
    }
    transaction
        .execute(
            "DELETE FROM filesystem_changes
             WHERE session_id=?1 AND sequence < COALESCE((
                SELECT sequence FROM filesystem_changes
                WHERE session_id=?1 ORDER BY sequence DESC
                LIMIT 1 OFFSET ?2
             ), -1)",
            params![
                session_id.as_uuid().as_bytes().as_slice(),
                (MAX_FILESYSTEM_CHANGE_HISTORY - 1) as i64
            ],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune filesystem changes: {error}"), true)
        })?;
    Ok(())
}

pub(crate) fn save_session_repository_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    repositories: &BTreeMap<RepositoryId, SessionRepository>,
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_session_repositories (
            session_id BLOB NOT NULL, repository_id BLOB NOT NULL,
            PRIMARY KEY(session_id, repository_id)
        ) WITHOUT ROWID;",
        )
        .map_err(|error| {
            persistence_error(format!("could not stage repositories: {error}"), true)
        })?;
    let session_bytes = session_id.as_uuid().as_bytes();
    transaction
        .execute(
            "DELETE FROM _loom_wanted_session_repositories WHERE session_id=?1",
            [session_bytes.as_slice()],
        )
        .map_err(|error| {
            persistence_error(format!("could not reset repository staging: {error}"), true)
        })?;
    for (id, repository) in repositories {
        if *id != repository.id || repository.path.is_empty() || repository.path.len() > 4096 {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "session repository metadata is invalid",
                false,
            ));
        }
        let repository_bytes = id.as_uuid().as_bytes();
        let attached_at = encode_timestamp(repository.attached_at)?;
        transaction.execute(
            "INSERT INTO _loom_wanted_session_repositories(session_id, repository_id) VALUES (?1, ?2)",
            params![session_bytes.as_slice(), repository_bytes.as_slice()],
        ).map_err(|error| persistence_error(format!("could not stage repository: {error}"), true))?;
        transaction.execute(
            "INSERT INTO session_repositories(session_id, repository_id, source, path, revision, attached_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(session_id, repository_id) DO UPDATE SET
                source=excluded.source, path=excluded.path, revision=excluded.revision, attached_at=excluded.attached_at
             WHERE session_repositories.source IS NOT excluded.source
                OR session_repositories.path IS NOT excluded.path
                OR session_repositories.revision IS NOT excluded.revision
                OR session_repositories.attached_at IS NOT excluded.attached_at",
            params![session_bytes.as_slice(), repository_bytes.as_slice(), repository.source, repository.path,
                repository.revision, attached_at],
        ).map_err(|error| persistence_error(format!("could not save session repository: {error}"), true))?;
    }
    transaction
        .execute(
            "DELETE FROM session_repositories WHERE session_id=?1 AND NOT EXISTS (
            SELECT 1 FROM _loom_wanted_session_repositories wanted
            WHERE wanted.session_id=session_repositories.session_id
              AND wanted.repository_id=session_repositories.repository_id
        )",
            [session_bytes.as_slice()],
        )
        .map_err(|error| {
            persistence_error(
                format!("could not prune session repositories: {error}"),
                true,
            )
        })?;
    Ok(())
}

pub(crate) fn save_session_directory_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    directories: &[SessionDirectory],
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_session_directories (
            session_id BLOB NOT NULL, ordinal INTEGER NOT NULL,
            PRIMARY KEY(session_id, ordinal)
        ) WITHOUT ROWID;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not stage mounted directories: {error}"),
                true,
            )
        })?;
    let session_bytes = session_id.as_uuid().as_bytes();
    transaction
        .execute(
            "DELETE FROM _loom_wanted_session_directories WHERE session_id=?1",
            [session_bytes.as_slice()],
        )
        .map_err(|error| {
            persistence_error(format!("could not reset directory staging: {error}"), true)
        })?;
    let mut unique_paths = BTreeSet::new();
    for (ordinal, directory) in directories.iter().enumerate() {
        if directory.path.is_empty()
            || directory.path.len() > 4096
            || !unique_paths.insert(&directory.path)
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "mounted directory metadata is invalid",
                false,
            ));
        }
        let ordinal = i64::try_from(ordinal).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "mounted directory count exceeds SQLite's integer range",
                false,
            )
        })?;
        transaction
            .execute(
                "INSERT INTO _loom_wanted_session_directories(session_id, ordinal) VALUES (?1, ?2)",
                params![session_bytes.as_slice(), ordinal],
            )
            .map_err(|error| {
                persistence_error(format!("could not stage mounted directory: {error}"), true)
            })?;
        transaction.execute(
            "INSERT INTO session_directories(session_id, ordinal, source, path) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, ordinal) DO UPDATE SET source=excluded.source, path=excluded.path
             WHERE session_directories.source IS NOT excluded.source OR session_directories.path IS NOT excluded.path",
            params![session_bytes.as_slice(), ordinal, directory.source, directory.path],
        ).map_err(|error| persistence_error(format!("could not save mounted directory: {error}"), true))?;
    }
    transaction.execute(
        "DELETE FROM session_directories WHERE session_id=?1 AND NOT EXISTS (
            SELECT 1 FROM _loom_wanted_session_directories wanted
            WHERE wanted.session_id=session_directories.session_id AND wanted.ordinal=session_directories.ordinal
        )", [session_bytes.as_slice()],
    ).map_err(|error| persistence_error(format!("could not prune mounted directories: {error}"), true))?;
    Ok(())
}

pub(crate) fn save_checkpoint_rows(
    transaction: &Transaction<'_>,
    session_id: AgentSessionId,
    checkpoints: &[Checkpoint],
) -> Result<()> {
    transaction
        .execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_checkpoints (
                session_id BLOB NOT NULL,
                checkpoint_id BLOB NOT NULL,
                PRIMARY KEY(session_id, checkpoint_id)
             ) WITHOUT ROWID, STRICT;
             CREATE TEMP TABLE IF NOT EXISTS _loom_wanted_checkpoint_files (
                session_id BLOB NOT NULL,
                checkpoint_id BLOB NOT NULL,
                path TEXT NOT NULL,
                PRIMARY KEY(session_id, checkpoint_id, path)
             ) WITHOUT ROWID, STRICT;
             DELETE FROM _loom_wanted_checkpoints;
             DELETE FROM _loom_wanted_checkpoint_files;",
        )
        .map_err(|error| {
            persistence_error(
                format!("could not stage filesystem checkpoints: {error}"),
                true,
            )
        })?;
    let session_id_bytes = session_id.as_uuid().as_bytes();
    for checkpoint in checkpoints {
        if checkpoint.session_id != session_id || checkpoint.label.trim().is_empty() {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                "checkpoint identity or label is invalid",
                false,
            ));
        }
        let checkpoint_id_bytes = checkpoint.id.as_uuid().as_bytes();
        transaction
            .execute(
                "INSERT INTO _loom_wanted_checkpoints(session_id, checkpoint_id)
                 VALUES (?1, ?2)",
                params![session_id_bytes.as_slice(), checkpoint_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not stage checkpoint {}: {error}", checkpoint.id),
                    true,
                )
            })?;
        transaction
            .execute(
                "INSERT INTO checkpoints(session_id, checkpoint_id, label, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id, checkpoint_id) DO UPDATE SET
                    label=excluded.label, created_at=excluded.created_at
                 WHERE checkpoints.label IS NOT excluded.label
                    OR checkpoints.created_at IS NOT excluded.created_at",
                params![
                    session_id_bytes.as_slice(),
                    checkpoint_id_bytes.as_slice(),
                    checkpoint.label,
                    encode_timestamp(checkpoint.created_at)?,
                ],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not save checkpoint {}: {error}", checkpoint.id),
                    true,
                )
            })?;
        for (path, file) in &checkpoint.files {
            if path.trim().is_empty() || file.content.len() > MAX_CONTENT_BYTES {
                return Err(LoomError::new(
                    ErrorCode::Persistence,
                    "checkpoint file path or content is invalid",
                    false,
                ));
            }
            let content_hash = store_content(transaction, file.content.as_bytes())?;
            transaction
                .execute(
                    "INSERT INTO _loom_wanted_checkpoint_files(session_id, checkpoint_id, path)
                     VALUES (?1, ?2, ?3)",
                    params![
                        session_id_bytes.as_slice(),
                        checkpoint_id_bytes.as_slice(),
                        path
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not stage checkpoint file '{path}': {error}"),
                        true,
                    )
                })?;
            transaction
                .execute(
                    "INSERT INTO checkpoint_files(
                        session_id, checkpoint_id, path, existed, revision,
                        expected_revision, content_hash
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(session_id, checkpoint_id, path) DO UPDATE SET
                        existed=excluded.existed,
                        revision=excluded.revision,
                        expected_revision=excluded.expected_revision,
                        content_hash=excluded.content_hash
                     WHERE checkpoint_files.existed IS NOT excluded.existed
                        OR checkpoint_files.revision IS NOT excluded.revision
                        OR checkpoint_files.expected_revision IS NOT excluded.expected_revision
                        OR checkpoint_files.content_hash IS NOT excluded.content_hash",
                    params![
                        session_id_bytes.as_slice(),
                        checkpoint_id_bytes.as_slice(),
                        path,
                        if file.existed { 1_i64 } else { 0_i64 },
                        file.revision,
                        file.expected_revision,
                        content_hash,
                    ],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not save checkpoint file '{path}': {error}"),
                        true,
                    )
                })?;
        }
        transaction
            .execute(
                "DELETE FROM checkpoint_files
                 WHERE session_id=?1 AND checkpoint_id=?2 AND NOT EXISTS (
                    SELECT 1 FROM _loom_wanted_checkpoint_files wanted
                    WHERE wanted.session_id=checkpoint_files.session_id
                      AND wanted.checkpoint_id=checkpoint_files.checkpoint_id
                      AND wanted.path=checkpoint_files.path
                 )",
                params![session_id_bytes.as_slice(), checkpoint_id_bytes.as_slice()],
            )
            .map_err(|error| {
                persistence_error(format!("could not prune checkpoint files: {error}"), true)
            })?;
    }
    transaction
        .execute(
            "DELETE FROM checkpoints
             WHERE session_id=?1 AND NOT EXISTS (
                SELECT 1 FROM _loom_wanted_checkpoints wanted
                WHERE wanted.session_id=checkpoints.session_id
                  AND wanted.checkpoint_id=checkpoints.checkpoint_id
             )",
            [session_id_bytes.as_slice()],
        )
        .map_err(|error| {
            persistence_error(format!("could not prune checkpoints: {error}"), true)
        })?;
    Ok(())
}
