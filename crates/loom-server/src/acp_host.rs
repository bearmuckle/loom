//! Thin Agent Client Protocol (ACP v1) host inside `loom-server`.
//!
//! Manages one supervised `agy acp` (or configured ACP harness) subprocess per
//! ACP session, maps ACP `session/update` notifications and live
//! `session/request_permission` requests onto Loom's existing `AgentEvent`,
//! `AgentActivityRecord`, `AgentPlan`, `UsageSnapshot`, and approval surfaces,
//! and enforces Antigravity's interactive-only guard.
//!
//! Loom never injects its system prompt, tool list, or context assembler into
//! an ACP harness, and never reads, logs, persists, or echoes Google / Gemini
//! credentials or `GEMINI_API_KEY`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError,
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use loom_agent::{AgentEvent, AgentRunSnapshot, AgentRunState, AgentRuntimeState, AgentTask};
use loom_core::{
    ANTIGRAVITY_AUTH_OAUTH_PERSONAL, ANTIGRAVITY_HARNESS_ID, AcpCapabilities, ActivityId,
    AgentSessionId, ApprovalPolicy, Capability, ErrorCode, InteractionId, LoomError, Result,
    RunAttemptId, RunId, SessionBackendDescriptor, Timestamp, ToolCallId, UsageSnapshot,
    WorkspaceId,
};
use loom_harness::acp::{
    ANTIGRAVITY_COMMAND_ENV, AcpClient, AcpCommandSpec, AcpEvent, AcpInitializeResult, AcpNext,
    AcpOptions, AcpPermissionOutcome, AcpPermissionRequest, AcpPlanEntry, AcpSessionInfo,
    AcpToolCall, AcpToolCallUpdate, AcpUsageUpdate, AntigravityRunContext, antigravity_notices,
    check_interactive_only, discover_antigravity_command,
};
use loom_model::{
    AgentActivityData, AgentActivityKind, AgentActivityRecord, AgentActivityStatus,
    AgentInteractionKind, AgentInteractionRecord, AgentInteractionStatus, AgentPlan, AgentPlanStep,
    AgentRunAttemptRecord, ApprovalDecision, FileActivityOperation, MessageRole, ModelId,
    ModelMessage, ToolCall, ToolResult,
};
use loom_protocol::{AcpHarnessProbeResult, ProviderResponse, RunResponse, ServerResponse};
use serde_json::json;

use crate::{InProcessBackend, InProcessConnection, RunHandle};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const AUTH_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// Operator resolution for an active ACP `session/request_permission` prompt.
#[derive(Clone, Debug)]
pub(crate) enum AcpPermissionDecision {
    Allow,
    Deny { reason: Option<String> },
    Cancel,
}

/// Live permission request waiting on Loom's approval UI.
struct PendingAcpPermission {
    attempt_id: RunAttemptId,
    control_revision: u64,
    interaction_id: InteractionId,
    tool_call_id: ToolCallId,
    call: ToolCall,
    tx: mpsc::Sender<AcpPermissionDecision>,
}

/// One live ACP harness process bound to a Loom session.
struct LiveAcpSession {
    client: AcpClient,
    init: AcpInitializeResult,
    session: AcpSessionInfo,
    command_display: String,
    auth_method: String,
}

struct AcpSessionEntry {
    live: Mutex<Option<LiveAcpSession>>,
}

/// Worker-wide registry of live ACP harness subprocesses and pending permission prompts.
#[derive(Default)]
pub(crate) struct AcpSessionManager {
    sessions: Mutex<BTreeMap<AgentSessionId, Arc<AcpSessionEntry>>>,
    pending_permissions: Mutex<BTreeMap<RunId, PendingAcpPermission>>,
}

impl AcpSessionManager {
    fn entry(&self, session_id: AgentSessionId) -> Arc<AcpSessionEntry> {
        let mut guard = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(guard.entry(session_id).or_insert_with(|| {
            Arc::new(AcpSessionEntry {
                live: Mutex::new(None),
            })
        }))
    }

    fn register_permission(&self, run_id: RunId, pending: PendingAcpPermission) {
        self.pending_permissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run_id, pending);
    }

    fn take_permission(&self, run_id: RunId) -> Option<PendingAcpPermission> {
        self.pending_permissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&run_id)
    }

    /// Cancels any pending ACP permission for `run_id` so an interrupted turn unblocks immediately.
    pub(crate) fn cancel_pending_permission(&self, run_id: RunId) {
        if let Some(pending) = self.take_permission(run_id) {
            let _ = pending.tx.send(AcpPermissionDecision::Cancel);
        }
    }

    /// Terminates and removes the live ACP subprocess for `session_id`, if any.
    pub(crate) fn close_session(&self, session_id: AgentSessionId) {
        let entry = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&session_id);
        if let Some(entry) = entry
            && let Some(mut live) = entry
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            live.client.kill();
        }
    }

    /// Terminates all live ACP subprocesses and cancels all pending permissions on shutdown.
    pub(crate) fn shutdown_all(&self) {
        let pending = std::mem::take(
            &mut *self
                .pending_permissions
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for (_, item) in pending {
            let _ = item.tx.send(AcpPermissionDecision::Cancel);
        }
        let entries =
            std::mem::take(&mut *self.sessions.lock().unwrap_or_else(PoisonError::into_inner));
        for (_, entry) in entries {
            if let Some(mut live) = entry
                .live
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                live.client.kill();
            }
        }
    }
}

impl Drop for AcpSessionManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

/// Validates that `harness` is supported by this ACP host slice.
fn validate_harness_name(harness: &str) -> Result<String> {
    let trimmed = harness.trim();
    if trimmed == ANTIGRAVITY_HARNESS_ID {
        return Ok(ANTIGRAVITY_HARNESS_ID.to_owned());
    }
    Err(LoomError::invalid_request(format!(
        "unsupported ACP harness '{trimmed}'; currently supported: '{ANTIGRAVITY_HARNESS_ID}'"
    )))
}

fn resolve_command_for_harness(
    harness: &str,
    command_override: Option<&str>,
) -> Result<AcpCommandSpec> {
    let _ = validate_harness_name(harness)?;
    let env_override = std::env::var(ANTIGRAVITY_COMMAND_ENV).ok();
    let path_env = loom_core::child_path();
    discover_antigravity_command(
        command_override,
        env_override.as_deref(),
        path_env.as_deref(),
    )
}

fn session_command_override(command: &str) -> Option<&str> {
    let trimmed = command.trim();
    if trimmed.is_empty() || trimmed == "agy acp" || trimmed == "antigravity-acp" {
        None
    } else {
        Some(trimmed)
    }
}

fn profile_dir_for_session(session_root_base: &Path, session_id: AgentSessionId) -> PathBuf {
    session_root_base
        .join("acp-profiles")
        .join(session_id.to_string())
}

fn next_timeline_ordinal(state: &AgentRuntimeState) -> u64 {
    let max_msg = state.message_timeline_ordinals.iter().copied().max();
    let max_act = state.activities.iter().map(|a| a.timeline_ordinal).max();
    match (max_msg, max_act) {
        (Some(a), Some(b)) => a.max(b).saturating_add(1),
        (Some(a), None) | (None, Some(a)) => a.saturating_add(1),
        (None, None) => 0,
    }
}

fn build_env_for_acp(build_cache: Option<&Path>) -> Vec<(String, String)> {
    let mut env = vec![
        ("CARGO_INCREMENTAL".to_owned(), "0".to_owned()),
        ("CARGO_PROFILE_DEV_DEBUG".to_owned(), "0".to_owned()),
    ];
    if let Some(dir) = build_cache {
        env.push((
            "CARGO_TARGET_DIR".to_owned(),
            dir.to_string_lossy().into_owned(),
        ));
    }
    env
}

impl InProcessConnection {
    /// Probes the Antigravity ACP harness on the worker without reading or
    /// persisting any credential files or API keys.
    pub(crate) fn probe_acp_harness(
        &self,
        harness: String,
        command: Option<String>,
        auth_method: Option<String>,
    ) -> Result<ServerResponse> {
        let harness = validate_harness_name(&harness)?;
        let notices = antigravity_notices();
        let spec = match resolve_command_for_harness(&harness, command.as_deref()) {
            Ok(spec) => spec,
            Err(error) => {
                return Ok(ServerResponse::Provider(ProviderResponse::AcpHarnessProbe(
                    AcpHarnessProbeResult {
                        harness,
                        discovered_command: None,
                        available: false,
                        protocol_version: None,
                        agent_name: None,
                        agent_version: None,
                        capabilities: AcpCapabilities::default(),
                        models: Vec::new(),
                        interactive_only: true,
                        notices,
                        error: Some(error.message),
                    },
                )));
            }
        };

        let probe_dir = self
            .backend
            .session_root_base
            .join("acp-probe")
            .join(loom_core::RequestId::new().to_string());
        let profile_dir = probe_dir.join("profile");
        let _ = std::fs::create_dir_all(&probe_dir);

        let mut client = match AcpClient::spawn(&AcpOptions {
            command: spec.clone(),
            cwd: probe_dir.clone(),
            profile_dir: Some(profile_dir),
            env: Vec::new(),
        }) {
            Ok(client) => client,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&probe_dir);
                return Ok(ServerResponse::Provider(ProviderResponse::AcpHarnessProbe(
                    AcpHarnessProbeResult {
                        harness,
                        discovered_command: Some(spec.display),
                        available: false,
                        protocol_version: None,
                        agent_name: None,
                        agent_version: None,
                        capabilities: AcpCapabilities::default(),
                        models: Vec::new(),
                        interactive_only: true,
                        notices,
                        error: Some(error.message),
                    },
                )));
            }
        };

        let init = match client.initialize(HANDSHAKE_TIMEOUT) {
            Ok(init) => init,
            Err(error) => {
                client.kill();
                let _ = std::fs::remove_dir_all(&probe_dir);
                return Ok(ServerResponse::Provider(ProviderResponse::AcpHarnessProbe(
                    AcpHarnessProbeResult {
                        harness,
                        discovered_command: Some(spec.display),
                        available: false,
                        protocol_version: None,
                        agent_name: None,
                        agent_version: None,
                        capabilities: AcpCapabilities::default(),
                        models: Vec::new(),
                        interactive_only: true,
                        notices,
                        error: Some(error.message),
                    },
                )));
            }
        };

        let mut models = Vec::new();
        let mut probe_error = None;
        if let Some(method) = auth_method
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            match client.authenticate(
                Some(method),
                &init.auth_methods,
                HANDSHAKE_TIMEOUT,
                None,
                &mut |_| {},
            ) {
                Ok(_) => match client.session_new(&probe_dir, HANDSHAKE_TIMEOUT) {
                    Ok(session) => models = session.models,
                    Err(error) => probe_error = Some(error.message),
                },
                Err(error) => probe_error = Some(error.message),
            }
        }

        client.kill();
        let _ = std::fs::remove_dir_all(&probe_dir);

        Ok(ServerResponse::Provider(ProviderResponse::AcpHarnessProbe(
            AcpHarnessProbeResult {
                harness,
                discovered_command: Some(spec.display),
                available: probe_error.is_none(),
                protocol_version: Some(init.protocol_version),
                agent_name: init.agent_name,
                agent_version: init.agent_version,
                capabilities: init.capabilities,
                models,
                interactive_only: true,
                notices,
                error: probe_error,
            },
        )))
    }

    /// Creates a new session backed by an external ACP harness.
    pub(crate) fn create_acp_session_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        name: String,
        harness: String,
        command: Option<String>,
        auth_method: Option<String>,
        interactive: bool,
    ) -> Result<loom_core::AgentSessionSnapshot> {
        self.backend.workspace_records()?.get(workspace_id)?;
        if name.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent session name must not be empty",
            ));
        }
        let harness = validate_harness_name(&harness)?;
        check_interactive_only(
            &harness,
            AntigravityRunContext {
                interactive,
                auto_approve_actions: false,
                delegated_or_scheduled: false,
            },
        )?;

        let command_display = command
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                resolve_command_for_harness(&harness, None)
                    .ok()
                    .map(|spec| spec.display)
            })
            .unwrap_or_else(|| "agy acp".to_owned());

        let selected_auth = auth_method
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(ANTIGRAVITY_AUTH_OAUTH_PERSONAL)
            .to_owned();

        let backend_descriptor = SessionBackendDescriptor::acp(loom_core::AcpSessionBackend {
            harness,
            command: command_display,
            protocol_version: None,
            auth_method: Some(selected_auth),
            agent_name: None,
            agent_version: None,
            capabilities: AcpCapabilities::default(),
            remote_session_id: None,
            available_models: Vec::new(),
        });

        let session_id = AgentSessionId::new();
        let filesystem = self
            .backend
            .create_session_filesystem(workspace_id, session_id)?;
        let (snapshot, record) = self.backend.sessions()?.create_in_workspace_with_backend(
            workspace_id,
            session_id,
            name,
            Some(backend_descriptor),
        )?;
        self.backend
            .session_filesystems()?
            .insert(session_id, filesystem);
        self.backend
            .session_repositories()?
            .insert(session_id, BTreeMap::new());
        // Antigravity ACP is strictly interactive-only: never start with auto-approve.
        self.backend
            .session_policies()?
            .insert(session_id, ApprovalPolicy::default());
        self.backend
            .auto_approve_actions()?
            .insert(session_id, false);
        self.backend.journal()?.append_session(record);
        self.grant_creator_ownership(session_id);
        Ok(snapshot)
    }

    /// Starts an ACP run for a session whose backend descriptor is ACP.
    pub(crate) fn start_acp_run(
        &self,
        session: &loom_core::AgentSessionSnapshot,
        input: crate::StartRunInput,
    ) -> Result<ServerResponse> {
        let negotiated = self
            .negotiated_capabilities()?
            .clone()
            .ok_or_else(|| LoomError::invalid_request("connection must negotiate first"))?;
        if !negotiated.contains(Capability::AcpAgentSessions) {
            return Err(LoomError::new(
                ErrorCode::CapabilityDenied,
                "connection did not negotiate capability AcpAgentSessions",
                false,
            ));
        }

        let Some(acp) = session
            .backend
            .as_ref()
            .and_then(SessionBackendDescriptor::as_acp)
        else {
            return Err(LoomError::invalid_state(
                "session does not use an ACP backend",
            ));
        };
        let harness = acp.harness.clone();
        let command = acp.command.clone();
        let auth_method = acp.auth_method.clone();

        let policy = self.policy(session.id)?;
        let auto_approve =
            self.auto_approve_actions(session.id)? || policy == ApprovalPolicy::auto_approve();
        check_interactive_only(
            &harness,
            AntigravityRunContext {
                interactive: true,
                auto_approve_actions: auto_approve,
                delegated_or_scheduled: input.project_task_id.is_some(),
            },
        )?;

        if input.task.trim().is_empty() {
            return Err(LoomError::invalid_request("agent task must not be empty"));
        }

        let workspace = self.session_filesystem(session.id)?;
        let cwd = workspace.root().to_path_buf();
        let now = Timestamp::now();
        let run_id = RunId::new();
        let attempt_id = RunAttemptId::new();
        let run = AgentRunSnapshot {
            id: run_id,
            attempt_id,
            control_revision: 0,
            session_id: session.id,
            task: input.task.clone(),
            model: input.model.clone(),
            state: AgentRunState::Planning,
            started_at: now,
            updated_at: now,
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
            approval_policy: Some(policy.clone()),
            jev: None,
        };
        let attempt = AgentRunAttemptRecord {
            run_id,
            session_id: session.id,
            id: attempt_id,
            number: 1,
            state: AgentRunState::Planning,
            checkpoint_id: None,
            started_at: now,
            completed_at: None,
        };
        let task = AgentTask::new(input.task.clone(), input.model.clone())?;
        let user_message = ModelMessage::new(MessageRole::User, input.task.clone());
        let initial_state = AgentRuntimeState {
            session_id: session.id,
            task,
            run: run.clone(),
            attempts: vec![attempt],
            plan: AgentPlan {
                steps: Vec::new(),
                title: None,
            },
            messages: vec![user_message],
            message_timeline_ordinals: vec![0],
            last_project_message_sequence: 0,
            last_queued_direction_sequence: 0,
            pending_approval: None,
            pending_tool_execution: None,
            pending_project_join: None,
            pending_input: None,
            last_failed_call: None,
            next_message_id: 0,
            active_message_id: None,
            approval_policy: policy,
            options: input.options,
            usage: UsageSnapshot::default(),
            context_inspection: None,
            context_checkpoint: None,
            provider_cursor: 0,
            step_id: None,
            step_index: 0,
            activities: Vec::new(),
            interactions: Vec::new(),
        };

        let handle = Arc::new(RunHandle::new_acp(initial_state));
        self.backend.runs()?.insert(run_id, Arc::clone(&handle));
        self.backend.emit_acp_event(
            &handle,
            AgentEvent::RunStarted {
                snapshot: run.clone(),
            },
        )?;
        self.backend.emit_acp_event(
            &handle,
            AgentEvent::RunStateChanged {
                run_id,
                state: AgentRunState::Executing,
            },
        )?;
        self.backend.persist_state()?;

        self.backend.spawn_acp_run_worker(
            Arc::clone(&handle),
            AcpTurnSpec {
                harness,
                command,
                auth_method,
                cwd,
                prompt: input.task,
                requested_model: input.model,
            },
        )?;

        Ok(ServerResponse::Run(RunResponse::AgentRunStarted(
            handle.snapshot(),
        )))
    }

    /// Sends a follow-up prompt on an idle/completed ACP run using the same live harness session.
    pub(crate) fn send_acp_message(
        &self,
        handle: &Arc<RunHandle>,
        attempt_id: RunAttemptId,
        expected_control_revision: u64,
        message: String,
    ) -> Result<ServerResponse> {
        if message.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent message must not be empty",
            ));
        }
        let session = self.backend.sessions()?.get(handle.session_id)?;
        let Some(acp) = session
            .backend
            .as_ref()
            .and_then(SessionBackendDescriptor::as_acp)
        else {
            return Err(LoomError::invalid_state(
                "session does not use an ACP backend",
            ));
        };
        let harness = acp.harness.clone();
        let command = acp.command.clone();
        let auth_method = acp.auth_method.clone();
        let policy = self.policy(session.id)?;
        let auto_approve =
            self.auto_approve_actions(session.id)? || policy == ApprovalPolicy::auto_approve();
        check_interactive_only(
            &harness,
            AntigravityRunContext {
                interactive: true,
                auto_approve_actions: auto_approve,
                delegated_or_scheduled: false,
            },
        )?;
        if handle.is_running() {
            return Err(LoomError::invalid_state(
                "ACP run is currently processing a turn",
            ));
        }
        let (run_id, new_revision, model) = {
            let mut state = handle.locked_state();
            if state.run.attempt_id != attempt_id
                || state.run.control_revision != expected_control_revision
            {
                return Err(LoomError::conflict("run control revision has changed"));
            }
            if state.pending_approval.is_some() {
                return Err(LoomError::invalid_state(
                    "resolve the pending permission request before sending a message",
                ));
            }
            state.run.control_revision = state.run.control_revision.saturating_add(1);
            state.run.completed_at = None;
            state.run.summary = None;
            let ordinal = next_timeline_ordinal(&state);
            state
                .messages
                .push(ModelMessage::new(MessageRole::User, message.clone()));
            state.message_timeline_ordinals.push(ordinal);
            (
                state.run.id,
                state.run.control_revision,
                state.run.model.clone(),
            )
        };
        self.backend.emit_acp_event(
            handle,
            AgentEvent::UserMessage {
                run_id,
                attempt_id,
                control_revision: new_revision,
                interaction_id: None,
                text: message.clone(),
            },
        )?;
        self.backend.emit_acp_event(
            handle,
            AgentEvent::RunStateChanged {
                run_id,
                state: AgentRunState::Executing,
            },
        )?;
        let workspace = self.session_filesystem(session.id)?;
        let cwd = workspace.root().to_path_buf();
        self.backend.spawn_acp_run_worker(
            Arc::clone(handle),
            AcpTurnSpec {
                harness,
                command,
                auth_method,
                cwd,
                prompt: message,
                requested_model: model,
            },
        )?;
        Ok(ServerResponse::Run(RunResponse::AgentRun(
            handle.snapshot(),
        )))
    }

    /// Resolves a live ACP `session/request_permission` prompt from Loom's approval UI.
    pub(crate) fn resolve_acp_permission(
        &self,
        handle: &Arc<RunHandle>,
        attempt_id: RunAttemptId,
        expected_control_revision: u64,
        tool_call_id: ToolCallId,
        decision: AcpPermissionDecision,
    ) -> Result<ServerResponse> {
        let pending = self
            .backend
            .acp_sessions
            .take_permission(handle.run_id)
            .ok_or_else(|| LoomError::invalid_state("ACP run is not waiting for tool approval"))?;
        if pending.attempt_id != attempt_id
            || pending.control_revision != expected_control_revision
            || pending.tool_call_id != tool_call_id
        {
            // Restore if validation failed so the operator can retry with the right revision.
            self.backend
                .acp_sessions
                .register_permission(handle.run_id, pending);
            return Err(LoomError::conflict(
                "approval request does not match the current pending ACP permission",
            ));
        }

        let (approval_decision, interaction_status) = match &decision {
            AcpPermissionDecision::Allow => {
                (ApprovalDecision::Approved, AgentInteractionStatus::Approved)
            }
            AcpPermissionDecision::Deny { .. } | AcpPermissionDecision::Cancel => {
                (ApprovalDecision::Rejected, AgentInteractionStatus::Rejected)
            }
        };

        let new_revision = {
            let mut state = handle.locked_state();
            state.run.control_revision = state.run.control_revision.saturating_add(1);
            state.pending_approval = None;
            if let Some(interaction) = state
                .interactions
                .iter_mut()
                .find(|item| item.id == pending.interaction_id)
            {
                interaction.status = interaction_status;
                interaction.decision = Some(approval_decision);
                interaction.resolved_at = Some(Timestamp::now());
            }
            state.run.control_revision
        };

        self.backend.emit_acp_event(
            handle,
            AgentEvent::ToolApprovalDecided {
                run_id: handle.run_id,
                attempt_id,
                control_revision: new_revision,
                interaction_id: pending.interaction_id,
                tool_call_id,
                decision: approval_decision,
            },
        )?;
        self.backend.emit_acp_event(
            handle,
            AgentEvent::RunStateChanged {
                run_id: handle.run_id,
                state: AgentRunState::Executing,
            },
        )?;
        let _ = pending.tx.send(decision);
        let _ = pending.call;
        Ok(ServerResponse::Run(RunResponse::AgentRun(
            handle.snapshot(),
        )))
    }
}

pub(crate) struct AcpTurnSpec {
    harness: String,
    command: String,
    auth_method: Option<String>,
    cwd: PathBuf,
    prompt: String,
    requested_model: ModelId,
}

impl InProcessBackend {
    /// Emits an `AgentEvent` on an ACP `RunHandle`, updating journal, session state,
    /// cached handle state, and message fragments identically to native runs.
    pub(crate) fn emit_acp_event(
        self: &Arc<Self>,
        handle: &Arc<RunHandle>,
        event: AgentEvent,
    ) -> Result<()> {
        let _event_guard = handle
            .event_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let (AgentEvent::AssistantMessageDelta { text, .. }, Some(persistence)) =
            (&event, self.persistence.as_ref())
        {
            handle.append_message_delta(persistence.as_ref(), text)?;
        }
        if let (AgentEvent::RunCompleted { .. }, Some(persistence)) =
            (&event, self.persistence.as_ref())
        {
            handle.flush_message_fragments(persistence.as_ref())?;
        }
        self.record_agent_event(handle.session_id, event.clone())?;
        handle.apply_event(&event);
        Ok(())
    }

    /// Runs one ACP `session/prompt` turn on the bounded run executor.
    pub(crate) fn spawn_acp_run_worker(
        self: &Arc<Self>,
        handle: Arc<RunHandle>,
        spec: AcpTurnSpec,
    ) -> Result<()> {
        handle.join_worker()?;
        handle.set_running(true);
        let backend = Arc::clone(self);
        let reject_backend = Arc::clone(self);
        let task_handle = Arc::clone(&handle);
        let reject_handle = Arc::clone(&handle);
        let run_id = handle.run_id;
        let admission_timeout = self.run_admission_timeout(handle.session_id);
        let worker = self.run_executor.spawn_bounded(
            format!("loom-acp-run-{run_id}"),
            admission_timeout,
            move || {
                backend.drive_acp_turn(&task_handle, spec);
                if let Err(error) = backend.persist_run_checkpoint(&task_handle) {
                    task_handle.record_failure(error);
                }
                if let Err(error) = backend.persist_worker_state() {
                    task_handle.record_failure(error);
                }
                task_handle.set_running(false);
            },
            move |error| {
                let _ = reject_backend.fail_acp_run(&reject_handle, error);
                reject_handle.set_running(false);
            },
        );
        *handle.worker.lock().unwrap_or_else(PoisonError::into_inner) = Some(worker);
        Ok(())
    }

    fn drive_acp_turn(self: &Arc<Self>, handle: &Arc<RunHandle>, spec: AcpTurnSpec) {
        let entry = self.acp_sessions.entry(handle.session_id);
        let mut live_guard = entry.live.lock().unwrap_or_else(PoisonError::into_inner);

        // Reuse existing live subprocess when healthy; otherwise spawn + handshake + auth.
        let need_spawn = match live_guard.as_mut() {
            Some(live) => live.client.has_exited(),
            None => true,
        };
        if need_spawn {
            *live_guard = None;
            let live = match self.start_live_acp_session(handle, &spec) {
                Ok(live) => live,
                Err(error) => {
                    let _ = self.fail_acp_run(handle, error);
                    return;
                }
            };
            *live_guard = Some(live);
        }

        let Some(live) = live_guard.as_mut() else {
            return;
        };

        // Honor a specific model from the harness catalog when requested.
        let requested = spec.requested_model.as_str().trim();
        if !requested.is_empty()
            && requested != "default"
            && requested != ANTIGRAVITY_HARNESS_ID
            && live.session.current_model_id.as_deref() != Some(requested)
        {
            if let Err(error) = live.client.set_model(
                &live.session.session_id,
                requested,
                live.session.model_config_option_id.as_deref(),
                HANDSHAKE_TIMEOUT,
            ) {
                self.record_acp_notice_activity(
                    handle,
                    "acp_set_model",
                    format!(
                        "model selection '{requested}' was rejected by harness: {}",
                        error.message
                    ),
                    AgentActivityStatus::Failed,
                );
            } else {
                live.session.current_model_id = Some(requested.to_owned());
            }
        }

        let acp_session_id = live.session.session_id.clone();
        if let Err(error) = live.client.start_prompt(&acp_session_id, &spec.prompt) {
            *live_guard = None;
            let _ = self.fail_acp_run(handle, error);
            return;
        }

        let mut tool_activities: BTreeMap<
            String,
            (ActivityId, ToolCall, Timestamp, Option<String>),
        > = BTreeMap::new();
        let mut cancel_sent_at: Option<Instant> = None;
        let assistant_message_id = {
            let mut state = handle.locked_state();
            let id = state.next_message_id;
            state.next_message_id = state.next_message_id.saturating_add(1);
            state.active_message_id = Some(id);
            id
        };
        let mut assistant_msg_index: Option<usize> = None;

        loop {
            if handle.control.is_interrupt_requested() {
                if cancel_sent_at.is_none() {
                    let _ = live.client.cancel_session(&acp_session_id);
                    cancel_sent_at = Some(Instant::now());
                } else if cancel_sent_at.is_some_and(|at| at.elapsed() >= CANCEL_GRACE) {
                    let _ = self.cancel_acp_run(handle);
                    return;
                }
            }

            match live.client.next_event(POLL_INTERVAL) {
                AcpNext::Idle => continue,
                AcpNext::Exited {
                    exit_code,
                    stderr_tail,
                } => {
                    *live_guard = None;
                    let detail = if stderr_tail.trim().is_empty() {
                        format!(
                            "ACP harness '{}' exited mid-run (code {:?})",
                            spec.command, exit_code
                        )
                    } else {
                        format!(
                            "ACP harness '{}' exited mid-run (code {:?}): {}",
                            spec.command,
                            exit_code,
                            stderr_tail.trim()
                        )
                    };
                    let _ = self.fail_acp_run(
                        handle,
                        LoomError::new(ErrorCode::ProviderUnavailable, detail, true),
                    );
                    return;
                }
                AcpNext::Event(event) => match *event {
                    AcpEvent::MessageChunk(text) if !text.is_empty() => {
                        self.append_acp_assistant_text(handle, &mut assistant_msg_index, &text);
                        let _ = self.emit_acp_event(
                            handle,
                            AgentEvent::AssistantMessageDelta {
                                run_id: handle.run_id,
                                message_id: assistant_message_id,
                                text,
                            },
                        );
                    }
                    AcpEvent::ThoughtChunk(text) if !text.is_empty() => {
                        self.append_acp_assistant_reasoning(
                            handle,
                            &mut assistant_msg_index,
                            &text,
                        );
                        let _ = self.emit_acp_event(
                            handle,
                            AgentEvent::ReasoningDelta {
                                run_id: handle.run_id,
                                message_id: assistant_message_id,
                                text,
                            },
                        );
                    }
                    AcpEvent::ToolCall(tc) => {
                        let activity = self.build_acp_tool_activity(handle, &tc);
                        let tool_call = activity_tool_call(&activity);
                        tool_activities.insert(
                            tc.tool_call_id.clone(),
                            (activity.id, tool_call, activity.started_at, tc.kind.clone()),
                        );
                        {
                            let mut state = handle.locked_state();
                            state.usage.add_tool_call();
                        }
                        let _ = self.emit_acp_event(
                            handle,
                            AgentEvent::ActivityRecorded {
                                run_id: handle.run_id,
                                activity,
                            },
                        );
                    }
                    AcpEvent::ToolCallUpdate(update) => {
                        if let Some(activity) =
                            self.update_acp_tool_activity(handle, &mut tool_activities, &update)
                        {
                            let _ = self.emit_acp_event(
                                handle,
                                AgentEvent::ActivityRecorded {
                                    run_id: handle.run_id,
                                    activity,
                                },
                            );
                        }
                    }
                    AcpEvent::Plan(entries) => {
                        let plan = map_acp_plan(&entries);
                        {
                            handle.locked_state().plan = plan.clone();
                        }
                        let _ = self.emit_acp_event(
                            handle,
                            AgentEvent::PlanProposed {
                                run_id: handle.run_id,
                                plan,
                            },
                        );
                    }
                    AcpEvent::UsageUpdate(usage) => {
                        self.apply_acp_usage(handle, &usage);
                    }
                    AcpEvent::ModelsUpdated {
                        models,
                        current_model_id,
                        model_config_option_id,
                    } => {
                        live.session.models = models.clone();
                        if current_model_id.is_some() {
                            live.session.current_model_id = current_model_id;
                        }
                        if model_config_option_id.is_some() {
                            live.session.model_config_option_id = model_config_option_id;
                        }
                        self.sync_acp_session_backend(handle.session_id, live);
                    }
                    AcpEvent::StderrLine(line) => {
                        self.record_acp_notice_activity(
                            handle,
                            "acp_stderr",
                            line,
                            AgentActivityStatus::Completed,
                        );
                    }
                    AcpEvent::PermissionRequest(req) => {
                        let outcome =
                            self.handle_acp_permission_request(handle, live, &acp_session_id, &req);
                        match outcome {
                            PermissionWaitResult::Answered => {}
                            PermissionWaitResult::Cancelled => {
                                cancel_sent_at = Some(Instant::now());
                            }
                            PermissionWaitResult::HarnessExited { detail } => {
                                *live_guard = None;
                                let _ = self.fail_acp_run(
                                    handle,
                                    LoomError::new(ErrorCode::ProviderUnavailable, detail, true),
                                );
                                return;
                            }
                        }
                    }
                    AcpEvent::PromptFinished(result) => {
                        if let Some(usage) = &result.usage {
                            self.apply_acp_usage(handle, usage);
                        }
                        if result.stop_reason == "cancelled"
                            || handle.control.is_interrupt_requested()
                        {
                            let _ = self.cancel_acp_run(handle);
                        } else {
                            let _ = self.complete_acp_run(handle);
                        }
                        return;
                    }
                    AcpEvent::PromptFailed { code, message, .. } => {
                        let _ = self.fail_acp_run(
                            handle,
                            LoomError::new(
                                ErrorCode::ProviderUnavailable,
                                format!("ACP prompt failed (code {code}): {message}"),
                                false,
                            ),
                        );
                        return;
                    }
                    AcpEvent::MessageChunk(_)
                    | AcpEvent::ThoughtChunk(_)
                    | AcpEvent::Other { .. } => {}
                },
            }
        }
    }

    fn start_live_acp_session(
        self: &Arc<Self>,
        handle: &Arc<RunHandle>,
        spec: &AcpTurnSpec,
    ) -> Result<LiveAcpSession> {
        let prior_remote_session_id = self
            .sessions()
            .ok()
            .and_then(|s| s.get(handle.session_id).ok())
            .and_then(|snapshot| {
                snapshot
                    .backend
                    .as_ref()
                    .and_then(SessionBackendDescriptor::as_acp)
                    .and_then(|acp| acp.remote_session_id.clone())
            });

        let command_spec =
            resolve_command_for_harness(&spec.harness, session_command_override(&spec.command))?;
        let profile_dir = profile_dir_for_session(&self.session_root_base, handle.session_id);
        let build_cache = self.build_cache_root();
        let mut client = AcpClient::spawn(&AcpOptions {
            command: command_spec.clone(),
            cwd: spec.cwd.clone(),
            profile_dir: Some(profile_dir),
            env: build_env_for_acp(build_cache.as_deref()),
        })?;

        let init = client.initialize(HANDSHAKE_TIMEOUT)?;

        let cancel_check = || handle.control.is_interrupt_requested();
        let mut on_auth_event = |event: AcpEvent| {
            if let AcpEvent::StderrLine(line) = event {
                self.record_acp_notice_activity(
                    handle,
                    "acp_auth",
                    line,
                    AgentActivityStatus::Completed,
                );
            }
        };
        let auth_method = client.authenticate(
            spec.auth_method.as_deref(),
            &init.auth_methods,
            AUTH_TIMEOUT,
            Some(&cancel_check),
            &mut on_auth_event,
        )?;

        // Honest resume: only call session/load or session/resume when the
        // harness explicitly advertised it. Otherwise start fresh without
        // claiming replay.
        let session = match prior_remote_session_id.as_deref() {
            Some(remote_id) if init.capabilities.load_session => {
                match client.session_load(remote_id, &spec.cwd, HANDSHAKE_TIMEOUT) {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        self.record_acp_notice_activity(
                            handle,
                            "acp_resume",
                            format!(
                                "ACP session/load failed ({}); starting a new harness session without claiming replay",
                                error.message
                            ),
                            AgentActivityStatus::Completed,
                        );
                        client.session_new(&spec.cwd, HANDSHAKE_TIMEOUT)?
                    }
                }
            }
            Some(remote_id) if init.capabilities.resume_session => {
                match client.session_resume(remote_id, &spec.cwd, HANDSHAKE_TIMEOUT) {
                    Ok(resumed) => resumed,
                    Err(error) => {
                        self.record_acp_notice_activity(
                            handle,
                            "acp_resume",
                            format!(
                                "ACP session/resume failed ({}); starting a new harness session without claiming replay",
                                error.message
                            ),
                            AgentActivityStatus::Completed,
                        );
                        client.session_new(&spec.cwd, HANDSHAKE_TIMEOUT)?
                    }
                }
            }
            Some(_) => {
                self.record_acp_notice_activity(
                    handle,
                    "acp_resume",
                    "ACP harness does not advertise loadSession/resume; starting a new harness session (no conversation replay claimed)".to_owned(),
                    AgentActivityStatus::Completed,
                );
                client.session_new(&spec.cwd, HANDSHAKE_TIMEOUT)?
            }
            None => client.session_new(&spec.cwd, HANDSHAKE_TIMEOUT)?,
        };

        let live = LiveAcpSession {
            client,
            init,
            session,
            command_display: command_spec.display,
            auth_method,
        };
        self.sync_acp_session_backend(handle.session_id, &live);
        Ok(live)
    }

    fn sync_acp_session_backend(
        self: &Arc<Self>,
        session_id: AgentSessionId,
        live: &LiveAcpSession,
    ) {
        let descriptor = SessionBackendDescriptor::acp(loom_core::AcpSessionBackend {
            harness: ANTIGRAVITY_HARNESS_ID.to_owned(),
            command: live.command_display.clone(),
            protocol_version: Some(live.init.protocol_version),
            auth_method: Some(live.auth_method.clone()),
            agent_name: live.init.agent_name.clone(),
            agent_version: live.init.agent_version.clone(),
            capabilities: live.init.capabilities.clone(),
            remote_session_id: Some(live.session.session_id.clone()),
            available_models: live.session.models.clone(),
        });
        if let Ok(mut sessions) = self.sessions()
            && let Ok((_, record)) = sessions.update_backend(session_id, descriptor)
            && let Ok(mut journal) = self.journal()
        {
            journal.append_session(record);
        }
    }

    fn handle_acp_permission_request(
        self: &Arc<Self>,
        handle: &Arc<RunHandle>,
        live: &mut LiveAcpSession,
        acp_session_id: &str,
        req: &AcpPermissionRequest,
    ) -> PermissionWaitResult {
        let tool_call_id = ToolCallId::new();
        let interaction_id = InteractionId::new();
        let options_json = req
            .options
            .iter()
            .map(|opt| {
                json!({
                    "option_id": opt.option_id,
                    "name": opt.name,
                    "kind": opt.kind,
                })
            })
            .collect::<Vec<_>>();
        let call = ToolCall {
            id: tool_call_id,
            name: "acp_permission".to_owned(),
            arguments: json!({
                "title": req.title,
                "kind": req.kind,
                "tool_call_id": req.tool_call_id,
                "input": req.raw_input,
                "options": options_json,
            }),
        };

        let now = Timestamp::now();
        let (attempt_id, control_revision, timeline_ordinal) = {
            let mut state = handle.locked_state();
            state.run.control_revision = state.run.control_revision.saturating_add(1);
            let rev = state.run.control_revision;
            let attempt = state.run.attempt_id;
            let ord = next_timeline_ordinal(&state);
            state.pending_approval = Some(call.clone());
            state.interactions.push(AgentInteractionRecord {
                id: interaction_id,
                run_id: handle.run_id,
                session_id: handle.session_id,
                attempt_id: attempt,
                control_revision: rev,
                kind: AgentInteractionKind::ToolApproval,
                status: AgentInteractionStatus::Pending,
                tool_call_id: Some(tool_call_id),
                prompt: req.title.clone(),
                decision: None,
                created_at: now,
                resolved_at: None,
            });
            (attempt, rev, ord)
        };

        let activity_id = ActivityId::new();
        let _ = self.emit_acp_event(
            handle,
            AgentEvent::ActivityRecorded {
                run_id: handle.run_id,
                activity: AgentActivityRecord {
                    id: activity_id,
                    run_id: handle.run_id,
                    timeline_ordinal,
                    parent_id: None,
                    step_id: None,
                    kind: AgentActivityKind::ToolCall,
                    status: AgentActivityStatus::AwaitingApproval,
                    started_at: now,
                    completed_at: None,
                    elapsed_ms: None,
                    data: AgentActivityData::ToolCall {
                        call: call.clone(),
                        result: None,
                    },
                },
            },
        );
        let _ = self.emit_acp_event(
            handle,
            AgentEvent::RunStateChanged {
                run_id: handle.run_id,
                state: AgentRunState::AwaitingApproval,
            },
        );
        let _ = self.emit_acp_event(
            handle,
            AgentEvent::ToolApprovalRequired {
                run_id: handle.run_id,
                attempt_id,
                control_revision,
                interaction_id,
                call: call.clone(),
            },
        );
        let _ = self.persist_run_checkpoint(handle);

        let (tx, rx) = mpsc::channel();
        self.acp_sessions.register_permission(
            handle.run_id,
            PendingAcpPermission {
                attempt_id,
                control_revision,
                interaction_id,
                tool_call_id,
                call: call.clone(),
                tx,
            },
        );

        loop {
            if handle.control.is_interrupt_requested() {
                let _ = self.acp_sessions.take_permission(handle.run_id);
                let _ = live
                    .client
                    .respond_permission(&req.request_id, &AcpPermissionOutcome::Cancelled);
                let _ = live.client.cancel_session(acp_session_id);
                self.finish_permission_activity(
                    handle,
                    activity_id,
                    timeline_ordinal,
                    now,
                    call,
                    AgentActivityStatus::Cancelled,
                    "cancelled",
                    false,
                );
                return PermissionWaitResult::Cancelled;
            }
            if live.client.has_exited() {
                let _ = self.acp_sessions.take_permission(handle.run_id);
                return PermissionWaitResult::HarnessExited {
                    detail: format!(
                        "ACP harness exited while waiting for permission: {}",
                        live.client.stderr_tail().trim()
                    ),
                };
            }
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok(AcpPermissionDecision::Allow) => {
                    let outcome = match req.allow_option_id() {
                        Some(option_id) => AcpPermissionOutcome::Selected {
                            option_id: option_id.to_owned(),
                        },
                        None => AcpPermissionOutcome::Cancelled,
                    };
                    let _ = live.client.respond_permission(&req.request_id, &outcome);
                    self.finish_permission_activity(
                        handle,
                        activity_id,
                        timeline_ordinal,
                        now,
                        call,
                        AgentActivityStatus::Completed,
                        "approved",
                        true,
                    );
                    return PermissionWaitResult::Answered;
                }
                Ok(AcpPermissionDecision::Deny { reason }) => {
                    let outcome = match req.reject_option_id() {
                        Some(option_id) => AcpPermissionOutcome::Selected {
                            option_id: option_id.to_owned(),
                        },
                        None => AcpPermissionOutcome::Cancelled,
                    };
                    let _ = live.client.respond_permission(&req.request_id, &outcome);
                    self.finish_permission_activity(
                        handle,
                        activity_id,
                        timeline_ordinal,
                        now,
                        call,
                        AgentActivityStatus::Failed,
                        reason.as_deref().unwrap_or("rejected"),
                        false,
                    );
                    return PermissionWaitResult::Answered;
                }
                Ok(AcpPermissionDecision::Cancel) => {
                    let _ = live
                        .client
                        .respond_permission(&req.request_id, &AcpPermissionOutcome::Cancelled);
                    let _ = live.client.cancel_session(acp_session_id);
                    self.finish_permission_activity(
                        handle,
                        activity_id,
                        timeline_ordinal,
                        now,
                        call,
                        AgentActivityStatus::Cancelled,
                        "cancelled",
                        false,
                    );
                    return PermissionWaitResult::Cancelled;
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = live
                        .client
                        .respond_permission(&req.request_id, &AcpPermissionOutcome::Cancelled);
                    return PermissionWaitResult::Cancelled;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_permission_activity(
        self: &Arc<Self>,
        handle: &Arc<RunHandle>,
        activity_id: ActivityId,
        timeline_ordinal: u64,
        started_at: Timestamp,
        call: ToolCall,
        status: AgentActivityStatus,
        output: &str,
        success: bool,
    ) {
        let now = Timestamp::now();
        let elapsed_ms = now
            .as_unix_millis()
            .saturating_sub(started_at.as_unix_millis());
        let result = ToolResult {
            tool_call_id: call.id,
            name: call.name.clone(),
            success,
            output: output.to_owned(),
        };
        let _ = self.emit_acp_event(
            handle,
            AgentEvent::ActivityRecorded {
                run_id: handle.run_id,
                activity: AgentActivityRecord {
                    id: activity_id,
                    run_id: handle.run_id,
                    timeline_ordinal,
                    parent_id: None,
                    step_id: None,
                    kind: AgentActivityKind::ToolCall,
                    status,
                    started_at,
                    completed_at: Some(now),
                    elapsed_ms: Some(elapsed_ms),
                    data: AgentActivityData::ToolCall {
                        call,
                        result: Some(result),
                    },
                },
            },
        );
    }

    fn append_acp_assistant_text(
        &self,
        handle: &Arc<RunHandle>,
        assistant_msg_index: &mut Option<usize>,
        text: &str,
    ) {
        let mut state = handle.locked_state();
        let idx = match *assistant_msg_index {
            Some(idx) => idx,
            None => {
                let ord = next_timeline_ordinal(&state);
                state
                    .messages
                    .push(ModelMessage::new(MessageRole::Assistant, String::new()));
                state.message_timeline_ordinals.push(ord);
                let idx = state.messages.len() - 1;
                *assistant_msg_index = Some(idx);
                idx
            }
        };
        if let Some(msg) = state.messages.get_mut(idx) {
            msg.content.push_str(text);
        }
    }

    fn append_acp_assistant_reasoning(
        &self,
        handle: &Arc<RunHandle>,
        assistant_msg_index: &mut Option<usize>,
        text: &str,
    ) {
        let mut state = handle.locked_state();
        let idx = match *assistant_msg_index {
            Some(idx) => idx,
            None => {
                let ord = next_timeline_ordinal(&state);
                state
                    .messages
                    .push(ModelMessage::new(MessageRole::Assistant, String::new()));
                state.message_timeline_ordinals.push(ord);
                let idx = state.messages.len() - 1;
                *assistant_msg_index = Some(idx);
                idx
            }
        };
        if let Some(msg) = state.messages.get_mut(idx) {
            msg.reasoning_content
                .get_or_insert_with(String::new)
                .push_str(text);
        }
    }

    fn build_acp_tool_activity(
        &self,
        handle: &Arc<RunHandle>,
        tc: &AcpToolCall,
    ) -> AgentActivityRecord {
        let timeline_ordinal = {
            let state = handle.locked_state();
            next_timeline_ordinal(&state)
        };
        let call = ToolCall {
            id: ToolCallId::new(),
            name: tc.title.clone(),
            arguments: tc.raw_input.clone(),
        };
        let status = map_acp_tool_status(tc.status.as_deref());
        let (kind, data) = map_acp_tool_data(
            tc.kind.as_deref(),
            call,
            &tc.raw_input,
            tc.locations.first().cloned(),
            None,
        );
        let now = Timestamp::now();
        AgentActivityRecord {
            id: ActivityId::new(),
            run_id: handle.run_id,
            timeline_ordinal,
            parent_id: None,
            step_id: None,
            kind,
            status,
            started_at: now,
            completed_at: is_terminal_activity_status(status).then_some(now),
            elapsed_ms: is_terminal_activity_status(status).then_some(0),
            data,
        }
    }

    fn update_acp_tool_activity(
        &self,
        handle: &Arc<RunHandle>,
        activities: &mut BTreeMap<String, (ActivityId, ToolCall, Timestamp, Option<String>)>,
        update: &AcpToolCallUpdate,
    ) -> Option<AgentActivityRecord> {
        let (activity_id, call, started_at, original_kind) =
            match activities.get(&update.tool_call_id).cloned() {
                Some(existing) => existing,
                None => {
                    let call = ToolCall {
                        id: ToolCallId::new(),
                        name: update
                            .title
                            .clone()
                            .unwrap_or_else(|| update.tool_call_id.clone()),
                        arguments: update.raw_input.clone().unwrap_or(serde_json::Value::Null),
                    };
                    let id = ActivityId::new();
                    let now = Timestamp::now();
                    activities.insert(
                        update.tool_call_id.clone(),
                        (id, call.clone(), now, update.kind.clone()),
                    );
                    (id, call, now, update.kind.clone())
                }
            };
        let status = map_acp_tool_status(update.status.as_deref());
        let now = Timestamp::now();
        let completed = is_terminal_activity_status(status);
        let result = update.raw_output.as_ref().map(|output| ToolResult {
            tool_call_id: call.id,
            name: call.name.clone(),
            success: status != AgentActivityStatus::Failed
                && status != AgentActivityStatus::Cancelled,
            output: output.clone(),
        });
        let timeline_ordinal = {
            let state = handle.locked_state();
            state
                .activities
                .iter()
                .find(|a| a.id == activity_id)
                .map(|a| a.timeline_ordinal)
                .unwrap_or_else(|| next_timeline_ordinal(&state))
        };
        let effective_kind = update.kind.as_deref().or(original_kind.as_deref());
        let (kind, data) = map_acp_tool_data(
            effective_kind,
            call.clone(),
            update.raw_input.as_ref().unwrap_or(&call.arguments),
            None,
            result,
        );
        Some(AgentActivityRecord {
            id: activity_id,
            run_id: handle.run_id,
            timeline_ordinal,
            parent_id: None,
            step_id: None,
            kind,
            status,
            started_at,
            completed_at: completed.then_some(now),
            elapsed_ms: completed.then_some(
                now.as_unix_millis()
                    .saturating_sub(started_at.as_unix_millis()),
            ),
            data,
        })
    }

    fn apply_acp_usage(self: &Arc<Self>, handle: &Arc<RunHandle>, usage: &AcpUsageUpdate) {
        let snapshot = {
            let mut state = handle.locked_state();
            let input = usage.input_tokens.unwrap_or(0);
            let output = usage.output_tokens.unwrap_or(0);
            let cached = usage.cached_input_tokens.unwrap_or(0);
            if input > 0 || output > 0 || cached > 0 {
                state.usage.add_tokens(input, output, cached);
            } else if let Some(used) = usage.context_used {
                state.usage.input_tokens = state.usage.input_tokens.max(used);
            }
            if let Some(cost_usd) = usage.cost_usd
                && cost_usd.is_finite()
                && cost_usd >= 0.0
            {
                let micros = (cost_usd * 1_000_000.0).round() as u64;
                state.usage.cost_micros = state.usage.cost_micros.max(micros);
            }
            state.usage.clone()
        };
        let _ = self.emit_acp_event(
            handle,
            AgentEvent::RunUsageUpdated {
                run_id: handle.run_id,
                usage: snapshot,
            },
        );
    }

    fn record_acp_notice_activity(
        self: &Arc<Self>,
        handle: &Arc<RunHandle>,
        name: &str,
        message: String,
        status: AgentActivityStatus,
    ) {
        let now = Timestamp::now();
        let timeline_ordinal = {
            let state = handle.locked_state();
            next_timeline_ordinal(&state)
        };
        let call = ToolCall {
            id: ToolCallId::new(),
            name: name.to_owned(),
            arguments: json!({ "source": "acp_harness" }),
        };
        let result = ToolResult {
            tool_call_id: call.id,
            name: name.to_owned(),
            success: status == AgentActivityStatus::Completed,
            output: message,
        };
        let _ = self.emit_acp_event(
            handle,
            AgentEvent::ActivityRecorded {
                run_id: handle.run_id,
                activity: AgentActivityRecord {
                    id: ActivityId::new(),
                    run_id: handle.run_id,
                    timeline_ordinal,
                    parent_id: None,
                    step_id: None,
                    kind: AgentActivityKind::ToolCall,
                    status,
                    started_at: now,
                    completed_at: Some(now),
                    elapsed_ms: Some(0),
                    data: AgentActivityData::ToolCall {
                        call,
                        result: Some(result),
                    },
                },
            },
        );
    }

    fn complete_acp_run(self: &Arc<Self>, handle: &Arc<RunHandle>) -> Result<()> {
        let now = Timestamp::now();
        let snapshot = {
            let mut state = handle.locked_state();
            state.run.state = AgentRunState::Completed;
            state.run.completed_at = Some(now);
            state.run.updated_at = now;
            state.pending_approval = None;
            state.active_message_id = None;
            state.usage.elapsed_ms = now
                .as_unix_millis()
                .saturating_sub(state.run.started_at.as_unix_millis());
            state.run.clone()
        };
        self.emit_acp_event(
            handle,
            AgentEvent::RunStateChanged {
                run_id: handle.run_id,
                state: AgentRunState::Completed,
            },
        )?;
        self.emit_acp_event(handle, AgentEvent::RunCompleted { snapshot })
    }

    fn cancel_acp_run(self: &Arc<Self>, handle: &Arc<RunHandle>) -> Result<()> {
        handle.control.clear_request();
        let now = Timestamp::now();
        let snapshot = {
            let mut state = handle.locked_state();
            state.run.state = AgentRunState::Cancelled;
            state.run.completed_at = Some(now);
            state.run.updated_at = now;
            state.run.summary = Some("ACP run interrupted by the user".to_owned());
            state.pending_approval = None;
            state.active_message_id = None;
            state.run.clone()
        };
        self.emit_acp_event(
            handle,
            AgentEvent::RunStateChanged {
                run_id: handle.run_id,
                state: AgentRunState::Cancelled,
            },
        )?;
        self.emit_acp_event(handle, AgentEvent::RunCompleted { snapshot })
    }

    fn fail_acp_run(self: &Arc<Self>, handle: &Arc<RunHandle>, error: LoomError) -> Result<()> {
        self.record_acp_notice_activity(
            handle,
            "acp_harness_error",
            error.message.clone(),
            AgentActivityStatus::Failed,
        );
        self.emit_acp_event(
            handle,
            AgentEvent::ProviderError {
                run_id: handle.run_id,
                error: error.clone(),
            },
        )?;
        let now = Timestamp::now();
        let snapshot = {
            let mut state = handle.locked_state();
            state.run.state = AgentRunState::Failed;
            state.run.completed_at = Some(now);
            state.run.updated_at = now;
            state.run.summary = Some(format!(
                "{}{}",
                loom_model::AGENT_RUN_FAILURE_SUMMARY_PREFIX,
                error.message
            ));
            state.pending_approval = None;
            state.active_message_id = None;
            state.run.clone()
        };
        self.emit_acp_event(
            handle,
            AgentEvent::RunStateChanged {
                run_id: handle.run_id,
                state: AgentRunState::Failed,
            },
        )?;
        self.emit_acp_event(handle, AgentEvent::RunCompleted { snapshot })
    }
}

enum PermissionWaitResult {
    Answered,
    Cancelled,
    HarnessExited { detail: String },
}

fn map_acp_tool_status(status: Option<&str>) -> AgentActivityStatus {
    match status.unwrap_or("in_progress") {
        "completed" | "done" => AgentActivityStatus::Completed,
        "failed" | "error" => AgentActivityStatus::Failed,
        "cancelled" | "canceled" => AgentActivityStatus::Cancelled,
        _ => AgentActivityStatus::Started,
    }
}

fn is_terminal_activity_status(status: AgentActivityStatus) -> bool {
    matches!(
        status,
        AgentActivityStatus::Completed
            | AgentActivityStatus::Failed
            | AgentActivityStatus::Cancelled
    )
}

fn map_acp_tool_data(
    acp_kind: Option<&str>,
    call: ToolCall,
    raw_input: &serde_json::Value,
    location: Option<String>,
    result: Option<ToolResult>,
) -> (AgentActivityKind, AgentActivityData) {
    let path = location.or_else(|| {
        raw_input
            .get("path")
            .or_else(|| raw_input.get("file_path"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    });
    match acp_kind {
        Some("read") => (
            AgentActivityKind::File,
            AgentActivityData::File {
                call,
                operation: FileActivityOperation::Read,
                path,
                result,
            },
        ),
        Some("edit" | "delete" | "move") => (
            AgentActivityKind::File,
            AgentActivityData::File {
                call,
                operation: FileActivityOperation::Write,
                path,
                result,
            },
        ),
        Some("search") => (
            AgentActivityKind::Search,
            AgentActivityData::Search {
                call,
                query: raw_input
                    .get("query")
                    .or_else(|| raw_input.get("pattern"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                path,
                result,
            },
        ),
        Some("execute") => (
            AgentActivityKind::Command,
            AgentActivityData::Command {
                call,
                command: raw_input
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                args: Vec::new(),
                cwd: None,
                result,
            },
        ),
        _ => (
            AgentActivityKind::ToolCall,
            AgentActivityData::ToolCall { call, result },
        ),
    }
}

fn activity_tool_call(activity: &AgentActivityRecord) -> ToolCall {
    match &activity.data {
        AgentActivityData::ToolCall { call, .. }
        | AgentActivityData::File { call, .. }
        | AgentActivityData::Search { call, .. }
        | AgentActivityData::Command { call, .. } => call.clone(),
        AgentActivityData::ModelTurn { .. } => ToolCall {
            id: ToolCallId::new(),
            name: "acp".to_owned(),
            arguments: serde_json::Value::Null,
        },
    }
}

fn map_acp_plan(entries: &[AcpPlanEntry]) -> AgentPlan {
    let steps = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| AgentPlanStep {
            id: format!("acp-step-{}", index + 1),
            description: entry.content.clone(),
        })
        .collect();
    AgentPlan { steps, title: None }
}
