use super::*;

impl InProcessConnection {
    pub fn request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        let request_id = request.request_id;
        let _lifecycle = match self.backend.request_lifecycle.read() {
            Ok(lifecycle) if *lifecycle == 0 => lifecycle,
            Ok(_) => {
                return ResponseEnvelope::failure(
                    request_id,
                    LoomError::conflict("backend is shutting down"),
                );
            }
            Err(_) => {
                return ResponseEnvelope::failure(
                    request_id,
                    LoomError::new(
                        ErrorCode::Internal,
                        "backend request lifecycle lock was poisoned",
                        true,
                    ),
                );
            }
        };
        if let Some(auth) = &self.auth
            && let Err(error) = auth.verify()
        {
            return ResponseEnvelope::failure(request_id, error);
        }
        if !request
            .protocol_version
            .is_compatible_with(CURRENT_PROTOCOL_VERSION)
        {
            return ResponseEnvelope::failure(
                request_id,
                unsupported_version_error(request.protocol_version),
            );
        }
        let durable_mutation = request.request.is_retryable_mutation();
        // Child controls can wait for an in-flight model call to observe its
        // stop flag. Keep them out of the global mutation gate, as the direct
        // run controls are, so the request that stops a child is never queued
        // behind unrelated durable writes.
        let serialize_durable_request = durable_mutation
            && !matches!(&request.request, ClientRequest::ControlProjectChild { .. });
        let _durable_request_guard = if serialize_durable_request {
            match self.backend.idempotency_store.durable_gate() {
                Ok(guard) => Some(guard),
                Err(error) => return ResponseEnvelope::failure(request_id, error),
            }
        } else {
            None
        };
        if self.backend.persistence_failed.load(Ordering::SeqCst) {
            return ResponseEnvelope::failure(
                request_id,
                LoomError::new(
                    ErrorCode::Persistence,
                    "backend is unavailable after a durable state save failure; reopen it to recover",
                    true,
                ),
            );
        }

        let retryable = durable_mutation;
        if retryable
            && let Err(error) = self
                .backend
                .idempotency_store
                .validate_retry_horizon(request_id)
        {
            return ResponseEnvelope::failure(request_id, error);
        }
        let slot = if retryable {
            match self.backend.idempotency_store.request_slot(request_id) {
                Ok(slot) => Some(slot),
                Err(error) => return ResponseEnvelope::failure(request_id, error),
            }
        } else {
            None
        };
        let _request_guard = slot
            .as_ref()
            .map(|slot| slot.lock().unwrap_or_else(PoisonError::into_inner));
        let request_for_cache = request.request.clone();
        if retryable {
            if let Err(error) = self.authorize_request_access(&request_for_cache) {
                return ResponseEnvelope::failure(request_id, error);
            }
            match self
                .backend
                .idempotency_store
                .cached_response(request_id, &request_for_cache)
            {
                Ok(Some(response)) => return response,
                Ok(None) => {}
                Err(error) => return ResponseEnvelope::failure(request_id, error),
            }
        }

        let result = match request.request {
            ClientRequest::Negotiate {
                client_version,
                capabilities,
            } => self.negotiate(client_version, capabilities),
            ClientRequest::DiscoverCapabilities => self.discover_capabilities(),
            request => self.handle_after_negotiation(request, request_id),
        };
        let result = match result {
            Ok(response) => {
                if retryable {
                    let response_envelope = ResponseEnvelope::success(request_id, response.clone());
                    let record =
                        IdempotencyRecord::new(request_id, request_for_cache, response_envelope);
                    if durable_mutation {
                        self.backend
                            .persist_state_with_idempotency_candidate((request_id, record.clone()))
                            .and_then(|()| {
                                self.backend.idempotency_store.publish(request_id, record)?;
                                Ok(response)
                            })
                    } else {
                        self.backend
                            .idempotency_store
                            .publish(request_id, record)
                            .map(|()| response)
                    }
                } else {
                    if durable_mutation {
                        self.backend.persist_state().map(|()| response)
                    } else {
                        Ok(response)
                    }
                }
            }
            Err(error) => Err(error),
        };

        let response = match result {
            Ok(response) => ResponseEnvelope::success(request_id, response),
            Err(error) => ResponseEnvelope::failure(request_id, error),
        };
        if retryable {
            drop(_request_guard);
            drop(slot);
            self.backend
                .idempotency_store
                .release_request_slot(request_id);
        }
        response
    }

    pub(crate) fn negotiate(
        &self,
        client_version: ProtocolVersion,
        capabilities: CapabilitySet,
    ) -> Result<ServerResponse> {
        if !client_version.is_compatible_with(CURRENT_PROTOCOL_VERSION) {
            return Err(unsupported_version_error(client_version));
        }
        let negotiated = capabilities
            .intersection(&self.backend.supported_capabilities)
            .intersection(&self.authorized_capabilities());
        *self.negotiated_capabilities()? = Some(negotiated.clone());

        Ok(ServerResponse::Negotiated(NegotiationResult {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            capabilities: negotiated,
        }))
    }

    pub(crate) fn discover_capabilities(&self) -> Result<ServerResponse> {
        let capabilities = self
            .backend
            .supported_capabilities
            .intersection(&self.authorized_capabilities());
        Ok(ServerResponse::Capabilities(NegotiationResult {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            capabilities,
        }))
    }

    pub(crate) fn authorize_request_access(&self, request: &ClientRequest) -> Result<()> {
        self.authorize_request(request)?;
        let capabilities = self
            .negotiated_capabilities()?
            .clone()
            .ok_or_else(|| LoomError::invalid_request("connection must negotiate first"))?;
        if let Some(required) = request.required_capability()
            && !capabilities.contains(required)
        {
            return Err(LoomError::new(
                ErrorCode::CapabilityDenied,
                format!("connection did not negotiate capability {required:?}"),
                false,
            ));
        }
        Ok(())
    }

    pub(crate) fn handle_after_negotiation(
        &self,
        request: ClientRequest,
        request_id: RequestId,
    ) -> Result<ServerResponse> {
        self.authorize_request_access(&request)?;
        if self.backend.persistence.is_none()
            && matches!(
                &request,
                ClientRequest::SendProjectAgentMessage { .. }
                    | ClientRequest::ListProjectAgentMessages { .. }
                    | ClientRequest::ControlProjectChild { .. }
                    | ClientRequest::GetProjectChildReview { .. }
                    | ClientRequest::IntegrateProjectChild { .. }
                    | ClientRequest::CleanupProjectChildWorktree { .. }
            )
        {
            return Err(LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project agents and messages require durable storage",
                false,
            ));
        }
        self.dispatch_request(request, request_id)
    }

    pub(crate) fn worker_node_status(&self) -> Result<WorkerNodeStatus> {
        let storage_root = self
            .backend
            .session_filesystems()?
            .values()
            .next()
            .map(|filesystem| filesystem.root().to_path_buf())
            .or_else(|| {
                self.backend
                    .session_root_base
                    .parent()
                    .map(Path::to_path_buf)
            })
            .unwrap_or_else(|| self.backend.session_root_base.clone());
        let (disk_total_bytes, disk_available_bytes) = Self::disk_resources(&storage_root);
        let resources = self
            .backend
            .resource_monitor()?
            .sample(disk_total_bytes, disk_available_bytes);
        Ok(WorkerNodeStatus {
            name: self.backend.node_name.clone(),
            node_id: self.backend.node_id.clone(),
            online: true,
            capabilities: self.backend.supported_capabilities.clone(),
            resources,
        })
    }

    pub(crate) fn start_github_copilot_login(&self) -> Result<ServerResponse> {
        const MAX_PENDING_LOGINS: usize = 8;
        const COMPLETED_LOGIN_RETENTION: Duration = Duration::from_secs(300);

        let now = Instant::now();
        let login_id = uuid::Uuid::new_v4().to_string();
        self.backend.credentials.begin_pending(
            login_id.clone(),
            now,
            COMPLETED_LOGIN_RETENTION,
            MAX_PENDING_LOGINS,
            now + Duration::from_secs(3600),
        )?;
        let device = match GitHubCopilotAuthenticator::default().begin() {
            Ok(device) => device,
            Err(error) => {
                self.backend.credentials.remove(&login_id);
                return Err(error);
            }
        };
        self.backend.credentials.set_expires_at(
            &login_id,
            now + Duration::from_secs(device.expires_in.min(3600)),
        );

        let backend = self.backend.clone();
        let device_for_poll = device.clone();
        let worker_login_id = login_id.clone();
        let spawn_result = thread::Builder::new()
            .name("github-copilot-login".to_owned())
            .spawn(move || {
                let status = match GitHubCopilotAuthenticator::default()
                    .poll(&device_for_poll)
                    .and_then(|token| {
                        backend.providers.configure_github_copilot(token)?;
                        backend.persist_state()
                    }) {
                    Ok(()) => GitHubCopilotLoginStatus::Configured,
                    Err(error) => GitHubCopilotLoginStatus::Failed {
                        message: error.message,
                    },
                };
                backend.credentials.finish(&worker_login_id, status);
            });
        if let Err(error) = spawn_result {
            self.backend.credentials.remove(&login_id);
            return Err(LoomError::new(
                ErrorCode::Internal,
                format!("could not start GitHub Copilot sign-in: {error}"),
                false,
            ));
        }

        Ok(ServerResponse::GitHubCopilotLoginStarted {
            login_id,
            user_code: device.user_code,
            verification_uri: device.verification_uri,
            expires_in: device.expires_in,
            interval: device.interval,
        })
    }

    pub(crate) fn github_copilot_login_status(&self, login_id: &str) -> Result<ServerResponse> {
        let status = self.backend.credentials.status(login_id, Instant::now())?;
        Ok(ServerResponse::GitHubCopilotLoginStatus { status })
    }

    pub(crate) fn authorize_request(&self, request: &ClientRequest) -> Result<()> {
        let Some(auth) = &self.auth else {
            return Ok(());
        };
        if let Some(capability) = request.required_capability()
            && !auth.scope().allows_capability(capability)
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                format!("token is not authorized for capability {capability:?}"),
                false,
            ));
        }

        let mut workspace_id = None;
        let mut session_id = None;
        let mut run_id = None;
        match request {
            ClientRequest::CreateWorkspace { .. } => {
                if auth.scope().workspaces.is_some() || auth.scope().sessions.is_some() {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "workspace-scoped tokens cannot create workspaces",
                        false,
                    ));
                }
            }
            ClientRequest::GetProjectSnapshot { project_id } => {
                let root_session_id = AgentSessionId::from_uuid(*project_id.as_uuid());
                if !auth.scope().allows_session(root_session_id) {
                    return Err(unauthorized_session(root_session_id));
                }
                if let Ok(root) = self.backend.sessions()?.get(root_session_id)
                    && !auth.scope().allows_workspace(root.workspace_id)
                {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "token is not authorized for the project's workspace",
                        false,
                    ));
                }
                let snapshot = self.load_project_snapshot(*project_id)?;
                self.authorize_project_snapshot(auth, &snapshot)?;
            }
            ClientRequest::GetProjectSnapshotForSession { session_id } => {
                if !auth.scope().allows_session(*session_id) {
                    return Err(unauthorized_session(*session_id));
                }
                let session = self.backend.sessions()?.get(*session_id)?;
                if !auth.scope().allows_workspace(session.workspace_id) {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "token is not authorized for the project's workspace",
                        false,
                    ));
                }
                let snapshot = self.load_project_snapshot_for_session(*session_id)?;
                self.authorize_project_snapshot(auth, &snapshot)?;
            }
            ClientRequest::RegisterWorkspace { workspace } => {
                if auth.scope().sessions.is_some() {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "session-scoped tokens cannot register workspaces",
                        false,
                    ));
                }
                workspace_id = Some(workspace.id);
            }
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: requested_workspace,
                ..
            } => {
                if auth.scope().sessions.is_some() {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "session-scoped tokens cannot create sessions",
                        false,
                    ));
                }
                workspace_id = Some(*requested_workspace);
            }
            ClientRequest::RenameWorkspace {
                workspace_id: requested_workspace,
                ..
            }
            | ClientRequest::SetWorkspaceConfigForWorkspace {
                workspace_id: requested_workspace,
                ..
            } => {
                if auth.scope().sessions.is_some() {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "session-scoped tokens cannot modify workspace configuration",
                        false,
                    ));
                }
                workspace_id = Some(*requested_workspace);
            }
            ClientRequest::ListWorkspaceSessions {
                workspace_id: requested_workspace,
                ..
            }
            | ClientRequest::GetWorkspaceConfigForWorkspace {
                workspace_id: requested_workspace,
            } => workspace_id = Some(*requested_workspace),
            ClientRequest::GetAgentSession {
                session_id: requested_session,
            }
            | ClientRequest::GetAgentSessionSnapshot {
                session_id: requested_session,
            }
            | ClientRequest::GetAgentSessionSnapshotMetadata {
                session_id: requested_session,
            }
            | ClientRequest::GetAgentSessionInitialState {
                session_id: requested_session,
            }
            | ClientRequest::RenameAgentSession {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ArchiveAgentSession {
                session_id: requested_session,
            }
            | ClientRequest::GetSessionUsage {
                session_id: requested_session,
            }
            | ClientRequest::ForkAgentSession {
                session_id: requested_session,
                ..
            } => session_id = Some(*requested_session),
            ClientRequest::GetSessionEvents {
                session_id: requested_session,
                workspace_id: None,
                ..
            } => session_id = *requested_session,
            ClientRequest::GetSessionEvents {
                workspace_id: Some(requested_workspace),
                ..
            } => workspace_id = Some(*requested_workspace),
            ClientRequest::GetRecentSessionEvents {
                session_id: requested_session,
                ..
            } => session_id = Some(*requested_session),
            ClientRequest::ListProjectAgentMessages {
                session_id: requested_session,
                ..
            } => session_id = Some(*requested_session),
            ClientRequest::ControlProjectChild {
                manager_session_id,
                task_id,
                ..
            }
            | ClientRequest::GetProjectChildReview {
                manager_session_id,
                task_id,
                ..
            }
            | ClientRequest::IntegrateProjectChild {
                manager_session_id,
                task_id,
                ..
            }
            | ClientRequest::CleanupProjectChildWorktree {
                manager_session_id,
                task_id,
                ..
            } => {
                session_id = Some(*manager_session_id);
                let Some(task) = self
                    .backend
                    .persistence
                    .as_ref()
                    .map(|persistence| persistence.load_delegated_task(*task_id))
                    .transpose()?
                    .flatten()
                else {
                    return Err(LoomError::not_found("delegated task", task_id));
                };
                if !auth.scope().allows_session(task.target_session_id) {
                    return Err(unauthorized_session(task.target_session_id));
                }
                let target = self.backend.sessions()?.get(task.target_session_id)?;
                if !auth.scope().allows_workspace(target.workspace_id) {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "token is not authorized for the child task's workspace",
                        false,
                    ));
                }
            }
            ClientRequest::SendProjectAgentMessage { message } => {
                if !auth.scope().allows_session(message.sender_session_id) {
                    return Err(unauthorized_session(message.sender_session_id));
                }
                if !auth.scope().allows_session(message.target_session_id) {
                    return Err(unauthorized_session(message.target_session_id));
                }
            }
            ClientRequest::StartSessionAgentRun {
                session_id: requested_session,
                ..
            }
            | ClientRequest::StartSessionAgentRunWithOptions {
                session_id: requested_session,
                ..
            }
            | ClientRequest::AttachSessionRepository {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ImportSessionDirectory {
                session_id: requested_session,
                ..
            }
            | ClientRequest::AttachSessionDirectory {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ListSessionDirectories {
                session_id: requested_session,
            }
            | ClientRequest::DetachSessionDirectory {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ListSessionRepositories {
                session_id: requested_session,
            }
            | ClientRequest::DetachSessionRepository {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionFilesystemSnapshot {
                session_id: requested_session,
            }
            | ClientRequest::GetSessionFilesystemChanges {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ReadSessionFile {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ApplySessionFilesystemEdit {
                session_id: requested_session,
                ..
            }
            | ClientRequest::TakeSessionFilesystemControl {
                session_id: requested_session,
                ..
            }
            | ClientRequest::CreateSessionCheckpoint {
                session_id: requested_session,
                ..
            }
            | ClientRequest::RevertSessionCheckpoint {
                session_id: requested_session,
                ..
            }
            | ClientRequest::UndoSessionEdit {
                session_id: requested_session,
            }
            | ClientRequest::GetSessionContextFiles {
                session_id: requested_session,
            }
            | ClientRequest::GetSessionVcsStatus {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionVcsDiff {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionVcsBranches {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionVcsConflicts {
                session_id: requested_session,
                ..
            }
            | ClientRequest::OpenSessionTerminal {
                session_id: requested_session,
                ..
            }
            | ClientRequest::WriteSessionTerminalInput {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ResizeSessionTerminal {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionTerminalEvents {
                session_id: requested_session,
                ..
            }
            | ClientRequest::CancelSessionTerminal {
                session_id: requested_session,
                ..
            }
            | ClientRequest::StartSessionTask {
                session_id: requested_session,
                ..
            }
            | ClientRequest::ListSessionTasks {
                session_id: requested_session,
            }
            | ClientRequest::GetSessionTask {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionTaskEvents {
                session_id: requested_session,
                ..
            }
            | ClientRequest::CancelSessionTask {
                session_id: requested_session,
                ..
            }
            | ClientRequest::GetSessionTaskEvidence {
                session_id: requested_session,
                ..
            }
            | ClientRequest::SetSessionApprovalPolicy {
                session_id: requested_session,
                ..
            } => session_id = Some(*requested_session),
            ClientRequest::GetAgentRun {
                run_id: requested_run,
            }
            | ClientRequest::GetAgentRunMessagePage {
                run_id: requested_run,
                ..
            }
            | ClientRequest::GetAgentRunTranscriptPage {
                run_id: requested_run,
                ..
            }
            | ClientRequest::GetAgentRunMessageContentRange {
                run_id: requested_run,
                ..
            }
            | ClientRequest::GetAgentRunSnapshot {
                run_id: requested_run,
            }
            | ClientRequest::GetRunCheckpoint {
                run_id: requested_run,
            }
            | ClientRequest::ApproveAgentAction {
                run_id: requested_run,
                ..
            }
            | ClientRequest::RejectAgentAction {
                run_id: requested_run,
                ..
            }
            | ClientRequest::InterruptAgentRun {
                run_id: requested_run,
            }
            | ClientRequest::RetryAgentStep {
                run_id: requested_run,
            }
            | ClientRequest::PauseAgentRun {
                run_id: requested_run,
            }
            | ClientRequest::ResumeAgentRun {
                run_id: requested_run,
            }
            | ClientRequest::RetryAgentFromCheckpoint {
                run_id: requested_run,
                ..
            }
            | ClientRequest::GetRunUsage {
                run_id: requested_run,
            }
            | ClientRequest::InspectAgentContext {
                run_id: requested_run,
            } => run_id = Some(*requested_run),
            ClientRequest::SendAgentMessage {
                run_id: requested_run,
                ..
            } => run_id = Some(*requested_run),
            ClientRequest::AttachRunEvidence {
                run_id: requested_run,
                ..
            } => run_id = Some(*requested_run),
            ClientRequest::Negotiate { .. }
            | ClientRequest::DiscoverCapabilities
            | ClientRequest::ListWorkspaces
            | ClientRequest::ListModels
            | ClientRequest::GetWorkerNodeStatus
            | ClientRequest::ListProviders
            | ClientRequest::ListGitHubRepositories
            | ClientRequest::StartGitHubCopilotLogin
            | ClientRequest::GetGitHubCopilotLoginStatus { .. }
            | ClientRequest::DiscoverProviderModels { .. }
            | ClientRequest::GetProviderHealth { .. } => {}
            ClientRequest::ConfigureGitHubCopilot { .. }
            | ClientRequest::ConfigureApiKeyProvider { .. } => {}
        }

        if let ClientRequest::AttachSessionRepository { source, .. }
        | ClientRequest::ImportSessionDirectory { source, .. }
        | ClientRequest::AttachSessionDirectory { source, .. } = request
            && Path::new(source).is_absolute()
            && !auth.scope().allows_repository_source(Path::new(source))
        {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "token is not authorized to access that local source path",
                false,
            ));
        }

        if let Some(session_id) = session_id {
            if !auth.scope().allows_session(session_id) {
                return Err(unauthorized_session(session_id));
            }
            let session = self.backend.sessions()?.get(session_id)?;
            if !auth.scope().allows_workspace(session.workspace_id) {
                return Err(LoomError::new(
                    ErrorCode::AuthorizationDenied,
                    "token is not authorized for the session's workspace",
                    false,
                ));
            }
            if workspace_id.is_some_and(|requested| requested != session.workspace_id) {
                return Err(LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    "session does not belong to the requested workspace",
                    false,
                ));
            }
            workspace_id = Some(session.workspace_id);
        } else if run_id.is_none()
            && workspace_id.is_none()
            && matches!(
                request,
                ClientRequest::GetSessionEvents {
                    session_id: None,
                    workspace_id: None,
                    ..
                }
            )
            && (auth.scope().sessions.is_some() || auth.scope().workspaces.is_some())
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "an unrestricted token is required to list events across sessions",
                false,
            ));
        }

        if let Some(run_id) = run_id {
            let session_id = self.run_summary(run_id)?.snapshot.session_id;
            if !auth.scope().allows_session(session_id) {
                return Err(unauthorized_session(session_id));
            }
            let session = self.backend.sessions()?.get(session_id)?;
            if !auth.scope().allows_workspace(session.workspace_id) {
                return Err(LoomError::new(
                    ErrorCode::AuthorizationDenied,
                    "token is not authorized for the run's workspace",
                    false,
                ));
            }
            workspace_id = Some(session.workspace_id);
        }

        if let Some(workspace_id) = workspace_id
            && !auth.scope().allows_workspace(workspace_id)
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "token is not authorized for the requested workspace",
                false,
            ));
        }
        Ok(())
    }

    pub(crate) fn negotiated_capabilities(&self) -> Result<MutexGuard<'_, Option<CapabilitySet>>> {
        self.negotiated_capabilities.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "connection state lock was poisoned",
                true,
            )
        })
    }
}
