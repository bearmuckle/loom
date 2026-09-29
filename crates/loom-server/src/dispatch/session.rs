use super::*;

impl InProcessConnection {
    pub(super) fn session_dispatch(
        &self,
        request: ClientRequest,
        _request_id: RequestId,
    ) -> Result<ServerResponse> {
        match request {
            ClientRequest::Session(SessionRequest::GetAgentSession { session_id }) => {
                let snapshot = self.backend.sessions()?.get(session_id)?;
                Ok(ServerResponse::Session(SessionResponse::AgentSession(
                    snapshot,
                )))
            }
            ClientRequest::Session(SessionRequest::GetAgentSessionSnapshot { session_id }) => Ok(
                ServerResponse::Session(SessionResponse::AgentSessionSnapshot(
                    self.session_snapshot_projection(session_id, true)?,
                )),
            ),
            ClientRequest::Session(SessionRequest::GetAgentSessionSnapshotMetadata {
                session_id,
            }) => Ok(ServerResponse::Session(
                SessionResponse::AgentSessionSnapshot(
                    self.session_snapshot_projection(session_id, false)?,
                ),
            )),
            ClientRequest::Session(SessionRequest::GetAgentSessionInitialState { session_id }) => {
                Ok(ServerResponse::Session(
                    SessionResponse::AgentSessionInitialState(
                        self.session_initial_state(session_id)?,
                    ),
                ))
            }
            ClientRequest::Session(SessionRequest::RenameAgentSession { session_id, name }) => {
                let (snapshot, record) = self.backend.sessions()?.rename(session_id, name)?;
                self.backend.journal()?.append_session(record);
                Ok(ServerResponse::Session(
                    SessionResponse::AgentSessionRenamed(snapshot),
                ))
            }
            ClientRequest::Session(SessionRequest::ArchiveAgentSession { session_id }) => {
                self.archive_session(session_id)
            }
            ClientRequest::Session(SessionRequest::ForkAgentSession { session_id, name }) => {
                let source = self.backend.sessions()?.get(session_id)?;
                if name.trim().is_empty() {
                    return Err(LoomError::invalid_request(
                        "forked agent session name must not be empty",
                    ));
                }
                let approval_policy = self.policy(session_id)?;
                let auto_approve_actions = self.auto_approve_actions(session_id)?;
                let source_filesystem = self.session_filesystem(session_id)?;
                let target_id = AgentSessionId::new();
                let target_root = self
                    .backend
                    .session_root_base
                    .join(source.workspace_id.to_string())
                    .join(target_id.to_string())
                    .join("fs");
                fs::create_dir_all(&target_root).map_err(|error| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not create forked session filesystem: {error}"),
                        false,
                    )
                })?;
                if let Err(error) = copy_filesystem_tree(source_filesystem.root(), &target_root) {
                    let _ = fs::remove_dir_all(&target_root);
                    return Err(error);
                }
                for (path, source) in source_filesystem.mounted_directories()? {
                    let destination = target_root.join(&path);
                    fs::remove_file(&destination).map_err(|error| {
                        LoomError::new(
                            ErrorCode::WorkspaceAccessDenied,
                            format!("could not prepare forked directory copy: {error}"),
                            false,
                        )
                    })?;
                    copy_directory_contents(&source, &destination)?;
                }
                let target_filesystem = Workspace::open(target_id, &target_root)?;
                let mut target_repositories = BTreeMap::new();
                let source_repositories = self
                    .backend
                    .session_repositories()?
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_default();
                let mut target_vcs = BTreeMap::new();
                for repository in source_repositories.values() {
                    let repository_path = checked_session_path(&target_root, &repository.path)?;
                    let service = GitService::open(&repository_path)?;
                    let id = RepositoryId::new();
                    let forked_repository = SessionRepository {
                        id,
                        source: repository.source.clone(),
                        path: repository.path.clone(),
                        revision: service.status()?.head,
                        attached_at: Timestamp::now(),
                    };
                    target_vcs.insert((target_id, id), service);
                    target_repositories.insert(id, forked_repository);
                }
                let (snapshot, record) = match self
                    .backend
                    .sessions()?
                    .fork_with_id(session_id, name, target_id)
                {
                    Ok(fork) => fork,
                    Err(error) => {
                        let _ = fs::remove_dir_all(&target_root);
                        return Err(error);
                    }
                };
                self.backend
                    .session_policies()?
                    .insert(target_id, approval_policy);
                self.backend
                    .auto_approve_actions()?
                    .insert(target_id, auto_approve_actions);
                self.backend
                    .session_filesystems()?
                    .insert(target_id, target_filesystem);
                self.backend
                    .session_repositories()?
                    .insert(target_id, target_repositories);
                self.backend.session_vcs()?.extend(target_vcs);
                self.backend.journal()?.append_session(record);
                let history = self
                    .backend
                    .journal()?
                    .events
                    .iter()
                    .filter(|event| {
                        event.session_id == session_id
                            && !matches!(
                                &event.event,
                                loom_protocol::ServerEvent::AgentSessionCreated { .. }
                                    | loom_protocol::ServerEvent::AgentSessionForked { .. }
                            )
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let mut journal = self.backend.journal()?;
                for event in history {
                    let sequence = journal.next();
                    journal.append_event(ServerEventEnvelope {
                        protocol_version: event.protocol_version,
                        sequence,
                        session_id: target_id,
                        event: event.event,
                    });
                }
                Ok(ServerResponse::Session(
                    SessionResponse::AgentSessionForked(snapshot),
                ))
            }
            ClientRequest::Session(SessionRequest::SetSessionApprovalPolicy {
                session_id,
                policy,
                auto_approve_actions,
            }) => {
                self.backend.sessions()?.get(session_id)?;
                self.backend
                    .session_policies()?
                    .insert(session_id, policy.clone());
                if let Some(auto_approve_actions) = auto_approve_actions {
                    self.backend
                        .auto_approve_actions()?
                        .insert(session_id, auto_approve_actions);
                }
                Ok(ServerResponse::Session(SessionResponse::ApprovalPolicy(
                    policy,
                )))
            }
            _ => unreachable!("request was routed to the wrong dispatch domain"),
        }
    }
}
