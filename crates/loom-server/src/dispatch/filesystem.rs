use super::*;

impl InProcessConnection {
    pub(super) fn filesystem_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Filesystem(FilesystemRequest::ImportSessionDirectory {
                session_id,
                source,
                path,
            }) => self.import_session_directory(session_id, source, path),
            ClientRequest::Filesystem(FilesystemRequest::AttachSessionDirectory {
                session_id,
                source,
                path,
            }) => self.attach_session_directory(session_id, source, path),
            ClientRequest::Filesystem(FilesystemRequest::ListSessionDirectories { session_id }) => {
                self.backend.sessions()?.get(session_id)?;
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::SessionDirectories {
                        directories: self
                            .session_filesystem(session_id)?
                            .mounted_directories()?
                            .into_iter()
                            .map(|(path, source)| SessionDirectory {
                                path,
                                source: source.display().to_string(),
                            })
                            .collect(),
                    },
                ))
            }
            ClientRequest::Filesystem(FilesystemRequest::DetachSessionDirectory {
                session_id,
                path,
            }) => {
                self.detach_session_directory(session_id, path)?;
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::SessionDirectoryDetached,
                ))
            }
            ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemSnapshot {
                session_id,
            }) => {
                let mut snapshot = self.session_filesystem(session_id)?.snapshot()?;
                snapshot.root = ".".to_owned();
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::SessionFilesystemSnapshot(SessionFilesystemSnapshot {
                        session_id,
                        root: snapshot.root,
                        captured_at: snapshot.captured_at,
                        entries: snapshot.entries,
                    }),
                ))
            }
            ClientRequest::Filesystem(FilesystemRequest::GetSessionFilesystemChanges {
                session_id,
                after_sequence,
            }) => {
                let filesystem = self.session_filesystem(session_id)?;
                if let Some(persistence) = &self.backend.persistence {
                    let new_changes = filesystem.poll_changes()?;
                    let next_sequence = filesystem.state()?.next_sequence;
                    persistence.save_filesystem_changes(session_id, next_sequence, &new_changes)?;
                    let page = persistence.load_filesystem_changes_page(
                        session_id,
                        after_sequence,
                        MAX_REVIEW_CHANGES,
                    )?;
                    return Ok(ServerResponse::Filesystem(
                        FilesystemResponse::SessionFilesystemChanges {
                            changes: page.changes,
                            truncated: page.truncated,
                        },
                    ));
                }
                let mut changes = filesystem.changes_since(after_sequence)?;
                let history_pruned = filesystem_history_pruned(after_sequence, &changes);
                let truncated = history_pruned || changes.len() > MAX_REVIEW_CHANGES;
                if changes.len() > MAX_REVIEW_CHANGES {
                    changes = changes.split_off(changes.len() - MAX_REVIEW_CHANGES);
                }
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::SessionFilesystemChanges {
                        changes: changes
                            .into_iter()
                            .map(|change| SessionFilesystemChange {
                                sequence: change.sequence,
                                session_id,
                                path: change.path,
                                kind: change.kind,
                                revision: change.revision,
                            })
                            .collect(),
                        truncated,
                    },
                ))
            }
            ClientRequest::Filesystem(FilesystemRequest::ReadSessionFile { session_id, path }) => {
                let mut file = self.session_filesystem(session_id)?.read_file(&path)?;
                file.content = bounded_review_text(&file.content, MAX_REVIEW_FILE_BYTES);
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::SessionFilesystemFile(SessionFilesystemFile {
                        session_id,
                        path: file.path,
                        content: file.content,
                        revision: file.revision,
                    }),
                ))
            }
            ClientRequest::Filesystem(FilesystemRequest::ApplySessionFilesystemEdit {
                session_id,
                edit,
            }) => Ok(ServerResponse::Filesystem(
                FilesystemResponse::WorkspaceEditApplied(
                    self.session_filesystem(session_id)?.apply_user_edit(edit)?,
                ),
            )),
            ClientRequest::Filesystem(FilesystemRequest::TakeSessionFilesystemControl {
                session_id,
                control,
            }) => {
                self.session_filesystem(session_id)?.take_control(control)?;
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::WorkspaceControl(control),
                ))
            }
            ClientRequest::Filesystem(FilesystemRequest::CreateSessionCheckpoint {
                session_id,
                label,
            }) => Ok(ServerResponse::Filesystem(
                FilesystemResponse::CheckpointCreated(
                    self.session_filesystem(session_id)?
                        .create_checkpoint(label)?,
                ),
            )),
            ClientRequest::Filesystem(FilesystemRequest::RevertSessionCheckpoint {
                session_id,
                checkpoint_id,
            }) => Ok(ServerResponse::Filesystem(
                FilesystemResponse::CheckpointReverted(
                    self.session_filesystem(session_id)?
                        .revert_checkpoint(checkpoint_id)?,
                ),
            )),
            ClientRequest::Filesystem(FilesystemRequest::UndoSessionEdit { session_id }) => Ok(
                ServerResponse::Filesystem(FilesystemResponse::WorkspaceUndo(
                    self.session_filesystem(session_id)?
                        .undo_last_agent_edit()?,
                )),
            ),
            ClientRequest::Filesystem(FilesystemRequest::GetSessionContextFiles { session_id }) => {
                Ok(ServerResponse::Filesystem(
                    FilesystemResponse::ContextFiles {
                        files: self.session_filesystem(session_id)?.context_files()?,
                    },
                ))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
