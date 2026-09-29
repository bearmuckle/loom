use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock, Weak},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use loom_agent::{
    AgentEvent, AgentEventObserver, AgentRunSnapshot, AgentRunState, AgentRuntime,
    AgentRuntimeOptions, AgentRuntimeState, AgentTask, RunControl, RunProgress,
};
use loom_core::{
    ActivityId, AgentSessionId, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy,
    Capability, CapabilitySet, DelegatedTaskSpec, ErrorCode, EventSequence, LoomError,
    MAX_PROJECT_AGENT_DEPTH, ProjectAgentRecord, ProjectId, ProjectSnapshot,
    ProjectWorktreeCleanupDisposition, ProjectWorktreeRecord, ProjectWorktreeStatus,
    ProtocolVersion, RepositoryId, RequestId, Result, RunAttemptId, SessionEventRecord,
    TaskContextReference, Timestamp, UsageSnapshot, WorkspaceId, WorkspaceRecord,
};
use loom_model::{
    ModelCapabilities, ModelDescriptor, ModelId, ModelMessage, ProviderId, ToolCall, ToolDefinition,
};
use loom_persistence::{
    DurableFeedSessionCursor, DurableFeedState, DurableFeedWorkspaceCursor, DurableFilesystemDelta,
    DurableFilesystemEdit, DurableFilesystemRecord, DurableIdempotencyRecord, DurableProviderState,
    DurableRunCheckpointWrite, DurableRunContextCheckpoint, DurableRunMessage,
    DurableRunMessageDelta, DurableRunRuntimeConfig, DurableRunSummary,
    DurableSessionProjectionRead, DurableSessionSettings, DurableStateWrite, FilePersistence,
    ProjectCancellationCascadeRecord,
};
use loom_process::{TaskSupervisor, TerminalManager};
use loom_protocol::{
    AgentActivityRecord, AgentExecutionStateRecord, AgentRunMessageHeader,
    AgentRunSnapshotProjection, AgentRunTranscriptMessage, AgentSessionInitialState,
    AgentSessionSnapshotProjection, CURRENT_PROTOCOL_VERSION, ClientRequest,
    GitHubCopilotLoginStatus, GitHubRepository, MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES,
    MAX_AGENT_RUN_MESSAGE_PAGE_SIZE, MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES,
    MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE, NegotiationResult, ProjectChildControlAction,
    RequestEnvelope, ResponseEnvelope, ServerEvent, ServerEventEnvelope, ServerResponse,
    SessionDirectory, SessionFilesystemChange, SessionFilesystemFile, SessionFilesystemSnapshot,
    SessionRepository, WorkerNodeResources, WorkerNodeStatus, WorkspaceConfig, WorkspaceEvent,
    WorkspaceEventEnvelope, WorkspaceFeedEvent, unsupported_version_error,
};
use loom_providers::{
    CredentialRef, FileCredentialStore, GITHUB_COPILOT_CREDENTIAL_REF, GitHubCopilotAuthenticator,
    ModelProvider, ProviderConfig, ProviderHealth, ProviderRegistry, UnavailableProvider,
    UsageLedger, deterministic_descriptor,
};
use loom_session::{SessionManager, WorkspaceManager};
use loom_tools::{ToolExecutor, ToolExtension, ToolResult};
use loom_vcs::GitService;
use loom_workspace::Workspace;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sysinfo::System;

mod auth;
mod backend;
mod connection;
mod dispatch;
mod event_journal;
mod project_tools;
mod remote;
mod resource_monitor;
mod run_handle;
mod services;

use services::admission::AdmissionService;
use services::credential::CredentialService;
#[cfg(test)]
use services::idempotency::{
    IDEMPOTENCY_RETENTION, LEGACY_IDEMPOTENCY_RETENTION, trim_idempotency_cache,
};
use services::idempotency::{IdempotencyRecord, IdempotencyStore};

fn json_value<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|error| {
        LoomError::new(
            ErrorCode::Persistence,
            format!("could not serialize persistence section: {error}"),
            false,
        )
    })
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

const FEED_PRUNE_AFTER_NEW_SEQUENCES: u64 = 64;
const FEED_PRUNE_AFTER_NEW_BYTES: usize = 4 * 1024 * 1024;
const MAX_NONTERMINAL_PROJECT_TASKS: usize = 50;

fn should_prune_worker_feed(
    next_sequence: u64,
    last_pruned_sequence: u64,
    accumulated_bytes: usize,
    pending_bytes: usize,
) -> bool {
    next_sequence.saturating_sub(last_pruned_sequence) >= FEED_PRUNE_AFTER_NEW_SEQUENCES
        || accumulated_bytes.saturating_add(pending_bytes) >= FEED_PRUNE_AFTER_NEW_BYTES
}

#[derive(Deserialize)]
struct GitHubApiRepository {
    full_name: String,
    description: Option<String>,
    clone_url: String,
    private: bool,
    default_branch: String,
}

fn fetch_github_repositories(token: &str, endpoint: &str) -> Result<Vec<GitHubRepository>> {
    let mut repositories = Vec::new();
    for page in 1..=100 {
        let url = format!("{endpoint}?per_page=100&sort=updated&page={page}");
        let mut response = ureq::get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("Authorization", &format!("Bearer {token}"))
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "Loom")
            .call()
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::ProviderAuthentication,
                    format!("could not list GitHub repositories: {error}"),
                    true,
                )
            })?;
        let page_repositories: Vec<GitHubApiRepository> =
            response.body_mut().read_json().map_err(|error| {
                LoomError::new(
                    ErrorCode::ProviderInvalidResponse,
                    format!("GitHub returned an invalid repository list: {error}"),
                    false,
                )
            })?;
        let page_len = page_repositories.len();
        repositories.extend(
            page_repositories
                .into_iter()
                .map(|repository| GitHubRepository {
                    full_name: repository.full_name,
                    description: repository.description,
                    clone_url: repository.clone_url,
                    private: repository.private,
                    default_branch: repository.default_branch,
                }),
        );
        if page_len < 100 {
            break;
        }
    }
    repositories.sort_by(|left, right| left.full_name.cmp(&right.full_name));
    Ok(repositories)
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<()> {
    let source = fs::canonicalize(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not resolve local directory: {error}"),
            false,
        )
    })?;
    if !source.is_dir() {
        return Err(LoomError::invalid_request(
            "local import source must be a directory",
        ));
    }
    let destination_parent = destination
        .parent()
        .ok_or_else(|| LoomError::invalid_request("local import destination must have a parent"))?;
    fs::create_dir_all(destination_parent).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not create local import destination: {error}"),
            false,
        )
    })?;
    let destination_parent = fs::canonicalize(destination_parent).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not resolve local import destination: {error}"),
            false,
        )
    })?;
    if destination_parent.starts_with(&source) {
        return Err(LoomError::invalid_request(
            "cannot import a directory that contains the session filesystem",
        ));
    }
    let destination = destination_parent.join(
        destination
            .file_name()
            .ok_or_else(|| LoomError::invalid_request("invalid import destination"))?,
    );
    if destination.exists() {
        return Err(LoomError::conflict(
            "local import destination already exists",
        ));
    }
    let temporary = destination.with_file_name(format!(".loom-import-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&temporary).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not create temporary import directory: {error}"),
            false,
        )
    })?;

    fn copy_tree(
        root: &Path,
        source: &Path,
        destination: &Path,
        ancestors: &mut BTreeSet<PathBuf>,
    ) -> Result<()> {
        let source = fs::canonicalize(source).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve imported path: {error}"),
                false,
            )
        })?;
        if !source.starts_with(root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "local import contains a symbolic link outside its source directory",
                false,
            ));
        }
        let metadata = fs::metadata(&source).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not inspect imported path: {error}"),
                false,
            )
        })?;
        if metadata.is_dir() {
            if !ancestors.insert(source.clone()) {
                return Err(LoomError::invalid_request(
                    "local import contains a directory link cycle",
                ));
            }
            fs::create_dir_all(destination).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not create imported directory: {error}"),
                    false,
                )
            })?;
            for entry in fs::read_dir(&source).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not read imported directory: {error}"),
                    false,
                )
            })? {
                let entry = entry.map_err(|error| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not read imported entry: {error}"),
                        false,
                    )
                })?;
                copy_tree(
                    root,
                    &entry.path(),
                    &destination.join(entry.file_name()),
                    ancestors,
                )?;
            }
            ancestors.remove(&source);
        } else if metadata.is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    LoomError::new(
                        ErrorCode::WorkspaceAccessDenied,
                        format!("could not create imported file parent: {error}"),
                        false,
                    )
                })?;
            }
            fs::copy(source, destination).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not copy imported file: {error}"),
                    false,
                )
            })?;
        } else {
            return Err(LoomError::invalid_request(
                "local import contains an unsupported special file",
            ));
        }
        Ok(())
    }

    let result = copy_tree(&source, &source, &temporary, &mut BTreeSet::new());
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, &destination).map_err(|error| {
        let _ = fs::remove_dir_all(&temporary);
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not install imported directory: {error}"),
            false,
        )
    })
}

fn from_json<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|error| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            format!("persisted state contains malformed JSON: {error}"),
            false,
        )
    })
}

fn worker_node_url_is_safe(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    if !matches!(url.scheme(), "ws" | "wss")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    !url.query_pairs().any(|(key, _)| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "token"
                | "access_token"
                | "auth"
                | "authorization"
                | "password"
                | "api_key"
                | "secret"
                | "credential"
                | "bearer"
        )
    })
}

pub use auth::{AuthSession, AuthTokenStore, AuthorizationScope, IssuedToken};
pub use remote::{
    RemoteServer, RemoteServerConfig, RunningRemoteServer, WebSocketConnection, WebSocketTransport,
};

const DEFAULT_EVENT_RETENTION: usize = 4096;
const MAX_REVIEW_CHANGES: usize = 512;

fn filesystem_history_pruned(
    after: Option<loom_core::EventSequence>,
    changes: &[SessionFilesystemChange],
) -> bool {
    after.is_some_and(|after| {
        changes
            .first()
            .is_some_and(|first| first.sequence.value() > after.value().saturating_add(1))
    })
}
const MAX_REVIEW_DIFF_BYTES: usize = 64 * 1024;
const MAX_REVIEW_FILE_BYTES: usize = 128 * 1024;
const MAX_RUN_MESSAGE_BYTES: usize = 32 * 1024;

fn checked_session_relative_path(relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if relative.trim().is_empty()
        || relative.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(LoomError::invalid_request(
            "session repository path must be a normalized relative path",
        ));
    }
    Ok(path.to_path_buf())
}

fn copy_filesystem_tree(source: &Path, destination: &Path) -> Result<()> {
    let entries = fs::read_dir(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not read source session filesystem: {error}"),
            false,
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not inspect source session filesystem: {error}"),
                false,
            )
        })?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type().map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not inspect session filesystem entry: {error}"),
                false,
            )
        })?;
        if file_type.is_dir() {
            fs::create_dir(&destination_path).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not create copied session directory: {error}"),
                    false,
                )
            })?;
            copy_filesystem_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not copy session file: {error}"),
                    false,
                )
            })?;
        } else if file_type.is_symlink() {
            copy_session_symlink(&source_path, &destination_path)?;
        } else {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!(
                    "cannot copy unsupported filesystem entry '{}'",
                    source_path.display()
                ),
                false,
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_session_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not read session symlink: {error}"),
            false,
        )
    })?;
    std::os::unix::fs::symlink(target, destination).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not copy session symlink: {error}"),
            false,
        )
    })
}

#[cfg(windows)]
fn copy_session_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not read session symlink: {error}"),
            false,
        )
    })?;
    let target_is_dir = fs::metadata(source).is_ok_and(|metadata| metadata.is_dir());
    let result = if target_is_dir {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    };
    result.map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not copy session symlink: {error}"),
            false,
        )
    })
}

fn checked_session_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = checked_session_relative_path(relative)?;
    let root = fs::canonicalize(root).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not resolve session filesystem root: {error}"),
            false,
        )
    })?;
    let path = root.join(relative);
    let canonical = fs::canonicalize(&path).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not resolve session repository path: {error}"),
            false,
        )
    })?;
    if !canonical.starts_with(&root) || canonical == root {
        return Err(LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            "session repository path escapes its filesystem root",
            false,
        ));
    }
    Ok(canonical)
}

fn repository_display_name(source: &str) -> Result<String> {
    if Path::new(source).is_absolute() {
        let repository = GitService::open(source)?;
        return Ok(repository
            .root()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "local repository".to_owned()));
    }
    let parsed = url::Url::parse(source).map_err(|_| {
        LoomError::invalid_request("repository source must be an absolute path or URL")
    })?;
    if !matches!(parsed.scheme(), "https" | "ssh")
        || parsed.host_str().is_none_or(str::is_empty)
        || !parsed.password().unwrap_or_default().is_empty()
        || parsed.query_pairs().any(|(key, _)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "token" | "access_token" | "auth" | "password" | "api_key" | "secret"
            )
        })
    {
        return Err(LoomError::invalid_request(
            "repository URLs must use HTTPS or SSH and must not embed credentials",
        ));
    }
    let name = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|name| !name.is_empty())
        .unwrap_or(parsed.host_str().unwrap_or("repository"))
        .trim_end_matches(".git");
    Ok(name.to_owned())
}

fn github_copilot_credentials() -> Result<Arc<FileCredentialStore>> {
    let credentials = Arc::new(FileCredentialStore::open(
        FileCredentialStore::default_path(),
    )?);
    if let Ok(token) = std::env::var("LOOM_GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        credentials.insert(CredentialRef::new(GITHUB_COPILOT_CREDENTIAL_REF), token)?;
    }
    Ok(credentials)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct EventJournal {
    next_sequence: EventSequence,
    events: Vec<ServerEventEnvelope>,
    #[serde(skip)]
    pending_events: Vec<ServerEventEnvelope>,
    #[serde(default)]
    workspace_events: Vec<WorkspaceEventEnvelope>,
    #[serde(skip)]
    pending_workspace_events: Vec<WorkspaceEventEnvelope>,
    #[serde(default = "default_event_retention")]
    retention_limit: usize,
}

fn default_event_retention() -> usize {
    DEFAULT_EVENT_RETENTION
}

fn deduplicate_events(events: Vec<ServerEventEnvelope>) -> Vec<ServerEventEnvelope> {
    let mut by_sequence = BTreeMap::new();
    for event in events {
        by_sequence.insert(event.sequence.value(), event);
    }
    by_sequence.into_values().collect()
}

#[derive(Clone, Debug)]
struct PersistedBackendState {
    sessions: loom_session::SessionManagerState,
    workspace_records: loom_session::WorkspaceManagerState,
    journal: EventJournal,
    session_policies: BTreeMap<AgentSessionId, ApprovalPolicy>,
    auto_approve_actions: BTreeMap<AgentSessionId, bool>,
    provider_configs: Vec<ProviderConfig>,
    provider_health: BTreeMap<ProviderId, ProviderHealth>,
    workspace_configs: BTreeMap<WorkspaceId, WorkspaceConfig>,
    provider_usage: UsageLedger,
    idempotency: BTreeMap<loom_core::RequestId, IdempotencyRecord>,
}

#[derive(Clone)]
struct PersistedRunSummary {
    snapshot: AgentRunSnapshot,
    usage: UsageSnapshot,
}

fn durable_run_messages_from_runtime(
    messages: &[ModelMessage],
    timeline_ordinals: &[u64],
) -> Result<Vec<DurableRunMessage>> {
    if messages.len() != timeline_ordinals.len() {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "run transcript and timeline order are out of sync",
            false,
        ));
    }
    Ok(messages
        .iter()
        .zip(timeline_ordinals.iter().copied())
        .map(|(message, timeline_ordinal)| DurableRunMessage {
            timeline_ordinal,
            role: message.role,
            content: message.content.clone(),
            name: message.name.clone(),
            tool_call_id: message.tool_call_id,
            tool_calls: message.tool_calls.clone(),
        })
        .collect())
}

fn persisted_run_messages(messages: Vec<DurableRunMessage>) -> Vec<ModelMessage> {
    messages
        .into_iter()
        .map(|message| ModelMessage {
            role: message.role,
            content: message.content,
            name: message.name,
            tool_call_id: message.tool_call_id,
            tool_calls: message.tool_calls,
        })
        .collect()
}

fn hydrate_run_context_checkpoint(
    persistence: &FilePersistence,
    run_id: loom_core::RunId,
    state: &mut AgentRuntimeState,
) -> Result<()> {
    let Some(checkpoint) = persistence.load_run_context_checkpoint(run_id)? else {
        state.context_checkpoint = None;
        if let Some(inspection) = &mut state.context_inspection {
            inspection.summary = None;
        }
        return Ok(());
    };
    if checkpoint.session_id != state.session_id {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "run context checkpoint belongs to a different session",
            false,
        ));
    }
    state.context_checkpoint = Some(checkpoint.summary.clone());
    if let Some(inspection) = &mut state.context_inspection {
        inspection.summary = Some(checkpoint.summary);
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedSessionFilesystem {
    filesystem: loom_workspace::WorkspaceStateSnapshot,
    #[serde(skip)]
    repositories: BTreeMap<RepositoryId, SessionRepository>,
    #[serde(skip)]
    directories: Vec<SessionDirectory>,
}

fn sync_cached_run_attempt(state: &mut AgentRuntimeState) {
    let Some(attempt) = state
        .attempts
        .iter_mut()
        .rfind(|attempt| attempt.id == state.run.attempt_id)
    else {
        return;
    };
    attempt.state = state.run.state;
    attempt.completed_at = if matches!(
        state.run.state,
        AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
    ) {
        Some(state.run.completed_at.unwrap_or(state.run.updated_at))
    } else {
        None
    };
}

fn deliver_project_agent_messages(
    persistence: &FilePersistence,
    runtime: &mut AgentRuntime,
) -> Result<bool> {
    let session_id = runtime.session_id();
    let project_id = persistence
        .load_delegated_task_for_target(session_id)?
        .map(|task| task.project_id)
        .unwrap_or_else(|| ProjectId::from_uuid(*session_id.as_uuid()));
    let messages = persistence.list_agent_messages(
        project_id,
        session_id,
        runtime.last_project_message_sequence(),
        32,
    )?;
    let mut delivered = false;
    for message in messages {
        if !runtime.append_project_message(&message)? {
            break;
        }
        delivered = true;
    }
    Ok(delivered)
}

fn project_member_branch_messaging_enabled(
    persistence: &FilePersistence,
    root_session_id: AgentSessionId,
    member_session_id: AgentSessionId,
) -> Result<bool> {
    let task_grant = if member_session_id == root_session_id {
        None
    } else {
        persistence
            .load_delegated_task_for_target(member_session_id)?
            .map(|task| task.permissions.branch_messaging)
    };
    if member_session_id != root_session_id && !task_grant.unwrap_or(false) {
        return Ok(false);
    }
    let Some(summary) = persistence.load_latest_run_summary_for_session(member_session_id)? else {
        return Ok(task_grant.unwrap_or(false));
    };
    let Some(config) = persistence.load_run_runtime_config(summary.snapshot.id)? else {
        return Ok(false);
    };
    Ok(config.project_branch_messaging_enabled)
}

fn execution_state_from_runtime(state: &AgentRuntimeState) -> Result<AgentExecutionStateRecord> {
    Ok(AgentExecutionStateRecord {
        run_id: state.run.id,
        session_id: state.session_id,
        attempt_id: state.run.attempt_id,
        control_revision: state.run.control_revision,
        state: state.run.state,
        step_id: state.step_id,
        step_index: state.step_index,
        provider_cursor: u64::try_from(state.provider_cursor).map_err(|_| {
            LoomError::new(
                ErrorCode::Persistence,
                "provider cursor is out of range",
                false,
            )
        })?,
        next_message_id: state.next_message_id,
        active_message_id: state.active_message_id,
        last_project_message_sequence: state.last_project_message_sequence,
        pending_tool_execution: state.pending_tool_execution.clone(),
        pending_project_join: state.pending_project_join.clone(),
        pending_approval: state.pending_approval.clone(),
        pending_input: state.pending_input.clone(),
        last_failed_call: state.last_failed_call.clone(),
    })
}

fn runtime_state_from_durable_config(
    summary: &PersistedRunSummary,
    config: DurableRunRuntimeConfig,
) -> Result<AgentRuntimeState> {
    let options = AgentRuntimeOptions {
        limits: config.limits,
        context: config.context_options,
        checkpoint_id: config.checkpoint_id,
        input_cost_micros_per_1k: config.input_cost_micros_per_1k,
        output_cost_micros_per_1k: config.output_cost_micros_per_1k,
        project_delegation_enabled: config.project_delegation_enabled,
        project_messaging_enabled: config.project_messaging_enabled,
        project_inspection_enabled: config.project_inspection_enabled,
        project_child_control_enabled: config.project_child_control_enabled,
        project_worktree_enabled: config.project_worktree_enabled,
        project_review_enabled: config.project_review_enabled,
        project_integration_enabled: config.project_integration_enabled,
        project_branch_messaging_enabled: config.project_branch_messaging_enabled,
    };
    Ok(AgentRuntimeState {
        session_id: summary.snapshot.session_id,
        task: AgentTask {
            task: summary.snapshot.task.clone(),
            model: summary.snapshot.model.clone(),
            system_instructions: config.system_instructions,
            repository_instructions: config.repository_instructions,
        },
        run: summary.snapshot.clone(),
        plan: loom_agent::AgentPlan { steps: Vec::new() },
        messages: Vec::new(),
        message_timeline_ordinals: Vec::new(),
        last_project_message_sequence: 0,
        attempts: Vec::new(),
        pending_approval: None,
        pending_tool_execution: None,
        pending_project_join: None,
        pending_input: None,
        last_failed_call: None,
        next_message_id: 0,
        active_message_id: None,
        approval_policy: config.approval_policy,
        options,
        usage: summary.usage.clone(),
        context_inspection: config.context_inspection,
        context_checkpoint: None,
        provider_cursor: 0,
        step_id: None,
        step_index: 0,
        activities: Vec::new(),
        interactions: Vec::new(),
    })
}

fn hydrate_runtime_execution_state(
    state: &mut AgentRuntimeState,
    execution: AgentExecutionStateRecord,
) -> Result<()> {
    if execution.run_id != state.run.id
        || execution.session_id != state.session_id
        || execution.attempt_id != state.run.attempt_id
        || execution.control_revision != state.run.control_revision
        || execution.state != state.run.state
    {
        return Err(LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted execution state does not match its run snapshot",
            false,
        ));
    }
    state.step_id = execution.step_id;
    state.step_index = execution.step_index;
    state.provider_cursor = usize::try_from(execution.provider_cursor).map_err(|_| {
        LoomError::new(
            ErrorCode::MalformedPayload,
            "persisted provider cursor is out of range",
            false,
        )
    })?;
    state.next_message_id = execution.next_message_id;
    state.active_message_id = execution.active_message_id;
    state.last_project_message_sequence = execution.last_project_message_sequence;
    state.pending_tool_execution = execution.pending_tool_execution;
    state.pending_project_join = execution.pending_project_join;
    state.pending_approval = execution.pending_approval;
    state.pending_input = execution.pending_input;
    state.last_failed_call = execution.last_failed_call;
    Ok(())
}

fn run_can_be_deferred_during_restore(
    state: AgentRunState,
    has_pending_tool_execution: Option<bool>,
    has_pending_project_join: Option<bool>,
) -> bool {
    matches!(
        state,
        AgentRunState::Planning
            | AgentRunState::Executing
            | AgentRunState::AwaitingApproval
            | AgentRunState::Paused
            | AgentRunState::NeedsInput
            | AgentRunState::Evaluating
    ) && has_pending_tool_execution == Some(false)
        && has_pending_project_join == Some(false)
}

/// One agent run owned by the backend.
///
/// The runtime lock is held only while a step is executing. Reads and control
/// requests use the cached state and the control handle instead, so a running
/// model call never blocks another request.
struct RunHandle {
    run_id: loom_core::RunId,
    session_id: AgentSessionId,
    runtime: Mutex<AgentRuntime>,
    control: RunControl,
    state: Mutex<AgentRuntimeState>,
    message_fragments: Mutex<MessageFragmentState>,
    activity_deltas: Mutex<PendingActivityDeltas>,
    message_checkpoint: Mutex<MessageCheckpointCursor>,
    event_gate: Mutex<()>,
    fragment_wake: Condvar,
    running: Mutex<bool>,
    idle: Condvar,
    failure: Mutex<Option<LoomError>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

struct MessageCheckpointCursor {
    attempt_id: RunAttemptId,
    message_count: usize,
}

#[derive(Default)]
struct MessageFragmentState {
    active_message_ordinal: Option<u64>,
    positions: BTreeMap<u64, MessageFragmentPosition>,
    pending: BTreeMap<u64, PendingMessageFragments>,
    pending_bytes: usize,
    pending_since: Option<Instant>,
}

#[derive(Default)]
struct PendingActivityDeltas {
    by_id: BTreeMap<ActivityId, AgentActivityRecord>,
    appended_order: Vec<ActivityId>,
}

impl PendingActivityDeltas {
    fn ordered_values(&self) -> Vec<AgentActivityRecord> {
        let appended = self.appended_order.iter().copied().collect::<BTreeSet<_>>();
        self.by_id
            .iter()
            .filter(|(id, _)| !appended.contains(id))
            .map(|(_, activity)| activity.clone())
            .chain(
                self.appended_order
                    .iter()
                    .filter_map(|id| self.by_id.get(id).cloned()),
            )
            .collect()
    }
}

#[derive(Default)]
struct PendingMessageFragments {
    content: String,
    committed_bytes: usize,
}

#[derive(Clone, Copy)]
struct MessageFragmentPosition {
    ordinal: u64,
    fragment_ordinal: u64,
    byte_offset: u64,
}

/// Whether a control request pauses a run or ends it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunStop {
    Interrupt,
    Pause,
}

/// How long a control request waits for a running step to honour it.
const CONTROL_SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an operation that needs exclusive runtime access waits for a step
/// that is already finishing. A run that is genuinely busy is reported as a
/// retryable conflict instead.
const ENTRY_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Batch streamed transcript writes until this amount is buffered or the
/// oldest pending bytes have waited this long.
const MESSAGE_FRAGMENT_BATCH_BYTES: usize = 32 * 1024;
const MESSAGE_FRAGMENT_BATCH_INTERVAL: Duration = Duration::from_millis(50);

struct StartRunInput {
    session_id: AgentSessionId,
    project_task_id: Option<loom_core::TaskId>,
    task: String,
    model: ModelId,
    system_instructions: Option<String>,
    repository_instructions: Option<String>,
    options: AgentRuntimeOptions,
}

pub struct InProcessBackend {
    node_id: String,
    node_name: String,
    sessions: Mutex<SessionManager>,
    workspace_records: Mutex<loom_session::WorkspaceManager>,
    runs: Mutex<BTreeMap<loom_core::RunId, Arc<RunHandle>>>,
    persisted_runs: Mutex<BTreeMap<loom_core::RunId, PersistedRunSummary>>,
    journal: Mutex<EventJournal>,
    last_feed_pruned_sequence: AtomicU64,
    feed_bytes_since_prune: AtomicUsize,
    session_filesystems: Mutex<BTreeMap<AgentSessionId, Workspace>>,
    persisted_session_filesystems: Mutex<BTreeSet<AgentSessionId>>,
    session_filesystem_restore: Mutex<()>,
    session_repositories:
        Mutex<BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>,
    session_vcs: Mutex<BTreeMap<(AgentSessionId, RepositoryId), GitService>>,
    session_task_supervisors: Mutex<BTreeMap<AgentSessionId, TaskSupervisor>>,
    session_policies: Mutex<BTreeMap<AgentSessionId, ApprovalPolicy>>,
    auto_approve_actions: Mutex<BTreeMap<AgentSessionId, bool>>,
    workspace_configs: Mutex<BTreeMap<WorkspaceId, WorkspaceConfig>>,
    session_terminals: Mutex<BTreeMap<loom_core::TerminalId, AgentSessionId>>,
    terminals: TerminalManager,
    resource_monitor: Mutex<ResourceMonitor>,
    supported_capabilities: CapabilitySet,
    providers: ProviderRegistry,
    credentials: CredentialService,
    persistence: Option<FilePersistence>,
    session_root_base: PathBuf,
    idempotency_store: IdempotencyStore,
    admissions: AdmissionService,
    self_reference: Mutex<Weak<InProcessBackend>>,
    request_lifecycle: RwLock<u8>,
    persistence_failed: AtomicBool,
    state_persist_gate: Mutex<()>,
    #[cfg(test)]
    fail_next_state_save: AtomicBool,
    #[cfg(test)]
    project_cancellation_failpoint: AtomicUsize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateProjectTaskArguments {
    child_name: String,
    intent: String,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    context_references: Vec<TaskContextReference>,
    #[serde(default)]
    dependencies: Vec<loom_core::TaskId>,
    #[serde(default)]
    permissions: loom_core::ProjectAgentPermissions,
}

fn delegated_child_model_id(requested: Option<String>, current_model: &ModelId) -> String {
    match requested {
        Some(requested) if !requested.trim().eq_ignore_ascii_case("current") => {
            requested.trim().to_owned()
        }
        _ => current_model.as_str().to_owned(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendProjectAgentMessageArguments {
    #[serde(default)]
    target_session_id: Option<AgentSessionId>,
    /// Optional task context for this message; it does not select a recipient.
    #[serde(default)]
    task_id: Option<loom_core::TaskId>,
    kind: loom_core::AgentMessageKind,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListProjectChildrenArguments {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListProjectMessageRecipientsArguments {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitForProjectChildrenArguments {
    task_ids: Vec<loom_core::TaskId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlProjectChildArguments {
    task_id: loom_core::TaskId,
    action: ProjectChildControlAction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewProjectChildArguments {
    task_id: loom_core::TaskId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegrateProjectChildArguments {
    task_id: loom_core::TaskId,
    expected_parent_revision: String,
}

#[derive(Clone, Copy, Default)]
struct ProjectAgentToolGrants {
    delegation: bool,
    messaging: bool,
    branch_messaging: bool,
    inspection: bool,
    child_control: bool,
    worktree: bool,
    review: bool,
    integration: bool,
}

#[derive(Clone, Copy)]
enum ProjectAgentPermission {
    Delegation,
    BranchMessaging,
    ChildControl,
    Inspection,
    WorktreeCreation,
    Review,
    Integration,
}

impl ProjectAgentPermission {
    fn is_granted(self, permissions: loom_core::ProjectAgentPermissions) -> bool {
        match self {
            Self::Delegation => permissions.delegation,
            Self::BranchMessaging => permissions.branch_messaging,
            Self::ChildControl => permissions.child_control,
            Self::Inspection => permissions.inspection,
            Self::WorktreeCreation => permissions.worktree_creation,
            Self::Review => permissions.review,
            Self::Integration => permissions.integration,
        }
    }
}

fn project_permissions_are_subset(
    requested: loom_core::ProjectAgentPermissions,
    granted: loom_core::ProjectAgentPermissions,
) -> bool {
    (!requested.delegation || granted.delegation)
        && (!requested.branch_messaging || granted.branch_messaging)
        && (!requested.child_control || granted.child_control)
        && (!requested.inspection || granted.inspection)
        && (!requested.worktree_creation || granted.worktree_creation)
        && (!requested.review || granted.review)
        && (!requested.integration || granted.integration)
}

struct ProjectAgentTools {
    backend: Weak<InProcessBackend>,
    session_id: AgentSessionId,
    project_id: ProjectId,
    model_id: ModelId,
    can_delegate: bool,
    can_delegate_code: bool,
    can_message: bool,
    can_branch_message: bool,
    can_inspect_children: bool,
    can_wait_children: bool,
    can_control_children: bool,
    can_review_children: bool,
    can_integrate_children: bool,
}

#[derive(Default)]
struct ResourceMonitor {
    system: System,
    has_cpu_baseline: bool,
}

fn cpu_usage_percent(usage: f32) -> Option<u8> {
    usage
        .is_finite()
        .then(|| usage.clamp(0.0, 100.0).round() as u8)
}

fn memory_usage_percent(total: Option<u64>, available: Option<u64>) -> Option<u8> {
    let (Some(total), Some(available)) = (total.filter(|total| *total > 0), available) else {
        return None;
    };
    let usage = total.saturating_sub(available.min(total)) as f64 / total as f64 * 100.0;
    Some(usage.round() as u8)
}

fn worker_node_identity() -> (String, String) {
    let node_id = uuid::Uuid::new_v4().to_string();
    let hostname = ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "Loom backend".to_owned());
    let node_name = format!("{hostname} · {}", &node_id[..8]);
    (node_id, node_name)
}

fn openai_compatible_descriptor(model: ModelId) -> ModelDescriptor {
    ModelDescriptor {
        id: model,
        provider: loom_model::ProviderId::new("openai-compatible"),
        display_name: "OpenAI-compatible model".to_owned(),
        context_window: None,
        max_input_tokens: None,
        max_output_tokens: None,
        capabilities: loom_model::ModelCapabilities {
            streaming: false,
            tool_calling: true,
            vision: false,
            json_mode: true,
        },
    }
}

#[derive(Clone)]
pub struct InProcessConnection {
    backend: Arc<InProcessBackend>,
    negotiated_capabilities: Arc<Mutex<Option<CapabilitySet>>>,
    auth: Option<AuthSession>,
}

fn session_state_for_event(event: &AgentEvent) -> Option<AgentSessionState> {
    let state = match event {
        AgentEvent::RunStarted { snapshot } => snapshot.state,
        AgentEvent::RunStateChanged { state, .. } => *state,
        AgentEvent::RunCompleted { snapshot } => snapshot.state,
        _ => return None,
    };
    Some(session_state_for_run_state(state))
}

fn session_state_for_run_state(state: AgentRunState) -> AgentSessionState {
    match state {
        AgentRunState::Planning => AgentSessionState::Planning,
        AgentRunState::Executing => AgentSessionState::Executing,
        AgentRunState::AwaitingApproval => AgentSessionState::AwaitingApproval,
        AgentRunState::Evaluating => AgentSessionState::Evaluating,
        AgentRunState::Paused => AgentSessionState::Paused,
        AgentRunState::NeedsInput => AgentSessionState::NeedsInput,
        AgentRunState::Completed => AgentSessionState::Completed,
        AgentRunState::Failed => AgentSessionState::Failed,
        AgentRunState::Cancelled => AgentSessionState::Cancelled,
    }
}

fn delegated_task_status_for_session_state(
    state: AgentSessionState,
) -> Option<loom_core::DelegatedTaskStatus> {
    use loom_core::DelegatedTaskStatus as Status;
    Some(match state {
        AgentSessionState::Planning
        | AgentSessionState::Executing
        | AgentSessionState::Evaluating => Status::Running,
        AgentSessionState::AwaitingApproval
        | AgentSessionState::Paused
        | AgentSessionState::NeedsInput => Status::Blocked,
        AgentSessionState::Completed => Status::Completed,
        AgentSessionState::Failed => Status::Failed,
        AgentSessionState::Cancelled => Status::Cancelled,
        AgentSessionState::Idle | AgentSessionState::Queued | AgentSessionState::Archived => {
            return None;
        }
    })
}

fn delegated_task_status_for_run_state(state: AgentRunState) -> loom_core::DelegatedTaskStatus {
    use loom_core::DelegatedTaskStatus as Status;
    match state {
        AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating => {
            Status::Running
        }
        AgentRunState::AwaitingApproval | AgentRunState::Paused | AgentRunState::NeedsInput => {
            Status::Blocked
        }
        AgentRunState::Completed => Status::Completed,
        AgentRunState::Failed => Status::Failed,
        AgentRunState::Cancelled => Status::Cancelled,
    }
}

fn project_agent_capacity_available(running_tasks: usize, limit: u8) -> bool {
    running_tasks < usize::from(limit)
}

fn project_agent_slot_released(state: AgentSessionState) -> bool {
    matches!(
        state,
        AgentSessionState::AwaitingApproval
            | AgentSessionState::Paused
            | AgentSessionState::NeedsInput
            | AgentSessionState::Completed
            | AgentSessionState::Failed
            | AgentSessionState::Cancelled
    )
}

fn project_manager_wait_result_summary(
    persistence: &FilePersistence,
    wait: &loom_core::ProjectManagerWaitRecord,
) -> Result<Option<String>> {
    let mut children = Vec::with_capacity(wait.child_task_ids.len());
    for task_id in &wait.child_task_ids {
        let task = persistence
            .load_delegated_task(*task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", *task_id))?;
        if !matches!(
            task.status,
            loom_core::DelegatedTaskStatus::Blocked
                | loom_core::DelegatedTaskStatus::Completed
                | loom_core::DelegatedTaskStatus::Failed
                | loom_core::DelegatedTaskStatus::Cancelled
        ) {
            return Ok(None);
        }
        let worktree = persistence.load_project_worktree_by_task(task.task_id)?;
        children.push(serde_json::json!({
            "task_id": task.task_id,
            "child_name": task.child_name,
            "status": task.status,
            "code_change": task.code_change,
            "result_revision": worktree.as_ref().and_then(|record| record.result_revision.as_ref()),
            "integrated_revision": worktree.as_ref().and_then(|record| record.integrated_revision.as_ref()),
        }));
    }
    let summary = serde_json::to_string(&serde_json::json!({
        "return_ready": true,
        "children": children,
        "note": "Code child results still require review and integration before the manager reports completion."
    }))
    .map_err(|error| {
        LoomError::new(
            ErrorCode::Persistence,
            format!("could not encode project manager wait result: {error}"),
            false,
        )
    })?;
    if summary.len() > 16 * 1024 {
        return Err(LoomError::new(
            ErrorCode::Persistence,
            "project manager wait result exceeds its durable size limit",
            false,
        ));
    }
    Ok(Some(summary))
}

fn project_child_worktree_status(
    backend: &InProcessBackend,
    worktree: &ProjectWorktreeRecord,
) -> Result<loom_vcs::GitRepositoryStatus> {
    let filesystem = backend.restore_session_filesystem(worktree.child_session_id)?;
    let relative_path = checked_session_relative_path(&worktree.relative_path)?;
    let destination = filesystem.root().join(relative_path);
    let metadata = fs::symlink_metadata(&destination).map_err(|error| {
        LoomError::new(
            ErrorCode::RecoveryRequired,
            format!("project child worktree path is unavailable: {error}"),
            true,
        )
    })?;
    if metadata.file_type().is_symlink() {
        return Err(LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            "refusing to inspect a project child worktree through a symlink",
            false,
        ));
    }
    let root = fs::canonicalize(filesystem.root()).map_err(|error| {
        LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            format!("could not resolve child filesystem root: {error}"),
            false,
        )
    })?;
    let canonical_destination = fs::canonicalize(&destination).map_err(|error| {
        LoomError::new(
            ErrorCode::RecoveryRequired,
            format!("could not resolve project child worktree path: {error}"),
            true,
        )
    })?;
    if !canonical_destination.starts_with(root) {
        return Err(LoomError::new(
            ErrorCode::WorkspaceAccessDenied,
            "project worktree path escapes the child filesystem root",
            false,
        ));
    }
    let status = GitService::open(&destination)?.status()?;
    if status.branch.as_deref() != Some(worktree.branch_name.as_str()) || status.head.is_none() {
        return Err(LoomError::new(
            ErrorCode::RecoveryRequired,
            "linked project worktree does not match its durable branch identity",
            true,
        ));
    }
    Ok(status)
}

fn project_subtree_deepest_first(
    project: &ProjectSnapshot,
    root_session_id: AgentSessionId,
) -> Vec<AgentSessionId> {
    let mut children = BTreeMap::<AgentSessionId, Vec<AgentSessionId>>::new();
    for agent in &project.agents {
        if let Some(parent_session_id) = agent.parent_session_id {
            children
                .entry(parent_session_id)
                .or_default()
                .push(agent.session_id);
        }
    }
    for descendants in children.values_mut() {
        descendants.sort_unstable();
        descendants.dedup();
    }

    let mut visited = BTreeSet::new();
    let mut post_order = Vec::new();
    let mut stack = vec![(root_session_id, false)];
    while let Some((session_id, expanded)) = stack.pop() {
        if session_id != root_session_id && session_id == project.root_session_id {
            continue;
        }
        if expanded {
            post_order.push(session_id);
            continue;
        }
        if !visited.insert(session_id) {
            continue;
        }
        stack.push((session_id, true));
        if let Some(descendants) = children.get(&session_id) {
            stack.extend(
                descendants
                    .iter()
                    .rev()
                    .copied()
                    .map(|descendant| (descendant, false)),
            );
        }
    }
    post_order
}

fn is_terminal_agent_run_state(state: AgentRunState) -> bool {
    matches!(
        state,
        AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
    )
}

fn abandon_project_manager_wait_if_run_terminal(
    persistence: &FilePersistence,
    wait: &loom_core::ProjectManagerWaitRecord,
) -> Result<bool> {
    let Some(summary) = persistence.load_run_summary(wait.run_id)? else {
        return Ok(false);
    };
    if !is_terminal_agent_run_state(summary.snapshot.state) {
        return Ok(false);
    }
    persistence.transition_project_manager_wait(
        wait.wait_id,
        wait.status,
        loom_core::ProjectManagerWaitStatus::Abandoned,
        None,
        Timestamp::now(),
    )?;
    Ok(true)
}

fn bounded_review_text(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let end = value
        .char_indices()
        .map(|(index, character)| (index, index + character.len_utf8()))
        .take_while(|(_, end)| *end <= limit)
        .map(|(_, end)| end)
        .last()
        .unwrap_or_default();
    let mut result = value[..end].to_owned();
    result.push_str("\n...[review output truncated]");
    result
}

fn run_snapshot_projection(state: &AgentRuntimeState) -> AgentRunSnapshotProjection {
    run_snapshot_projection_with_messages(state, true)
}

fn bounded_transcript_content(bytes: &[u8], content_bytes: u64) -> (String, bool) {
    let content_truncated = content_bytes > u64::from(MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES);
    let mut content = String::from_utf8_lossy(bytes).into_owned();
    if content_truncated {
        if content.ends_with('\u{fffd}') {
            content.pop();
        }
        content.push_str("\n...[message truncated]");
    }
    (content, content_truncated)
}

fn run_snapshot_projection_with_messages(
    state: &AgentRuntimeState,
    include_messages: bool,
) -> AgentRunSnapshotProjection {
    let mut messages = if include_messages {
        state.messages.clone()
    } else {
        Vec::new()
    };
    for message in &mut messages {
        message.content = bounded_review_text(&message.content, MAX_RUN_MESSAGE_BYTES);
    }
    let mut run = state.run.clone();
    if let Some(summary) = &mut run.summary {
        *summary = bounded_review_text(summary, MAX_RUN_MESSAGE_BYTES);
    }
    AgentRunSnapshotProjection {
        run,
        plan: state.plan.steps.clone(),
        messages,
        pending_approval: state.pending_approval.clone(),
        pending_input: state.pending_input.clone(),
        usage: state.usage.clone(),
        activities: state.activities.clone(),
        message_timeline_ordinals: if include_messages {
            state.message_timeline_ordinals.clone()
        } else {
            Vec::new()
        },
    }
}

fn add_usage(total: &mut UsageSnapshot, current: &UsageSnapshot) {
    total.add_tokens(
        current.input_tokens,
        current.output_tokens,
        current.cached_input_tokens,
    );
    total.tool_calls = total.tool_calls.saturating_add(current.tool_calls);
    total.cost_micros = total.cost_micros.saturating_add(current.cost_micros);
    total.elapsed_ms = total.elapsed_ms.max(current.elapsed_ms);
}

fn unauthorized_session(session_id: AgentSessionId) -> LoomError {
    LoomError::new(
        ErrorCode::AuthorizationDenied,
        format!("token is not authorized for session {session_id}"),
        false,
    )
}

impl InProcessConnection {
    pub fn disconnected(backend: Arc<InProcessBackend>) -> Self {
        Self {
            backend,
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
        }
    }

    fn authorized_capabilities(&self) -> CapabilitySet {
        self.auth
            .as_ref()
            .and_then(|auth| auth.scope().capabilities.clone())
            .unwrap_or_else(|| self.backend.supported_capabilities.clone())
    }
}

#[cfg(test)]
mod tests;
