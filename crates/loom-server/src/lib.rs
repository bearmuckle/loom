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
mod connection;
mod dispatch;
mod remote;
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

impl EventJournal {
    /// Capture only the pending session rows owned by one run checkpoint. The
    /// workspace feed is committed by full-state saves, never worker saves.
    fn capture_session_feed(
        &self,
        session_id: AgentSessionId,
    ) -> (DurableFeedState, BTreeSet<EventSequence>) {
        let events = self
            .pending_events
            .iter()
            .filter(|event| event.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        let sequences = events.iter().map(|event| event.sequence).collect();
        (
            DurableFeedState {
                next_sequence: self.next_sequence,
                retention_limit: self.retention_limit,
                events,
                workspace_events: Vec::new(),
            },
            sequences,
        )
    }

    /// Acknowledge precisely the rows included in a successfully committed
    /// checkpoint; unrelated sessions and workspace events remain pending.
    fn acknowledge_session_feed(&mut self, sequences: &BTreeSet<EventSequence>) {
        self.pending_events
            .retain(|event| !sequences.contains(&event.sequence));
    }

    fn append_session(&mut self, record: SessionEventRecord) {
        let sequence = self.next();
        let event =
            ServerEventEnvelope::from_session_event(sequence, record.session_id, record.event);
        self.append_event(event);
    }

    fn append_agent(&mut self, session_id: AgentSessionId, event: AgentEvent) {
        let sequence = self.next();
        let event = ServerEventEnvelope::from_agent_event(sequence, session_id, event);
        self.append_event(event);
    }

    fn next(&mut self) -> EventSequence {
        self.next_sequence = self.next_sequence.next();
        if self.retention_limit == 0 {
            self.retention_limit = DEFAULT_EVENT_RETENTION;
        }
        self.next_sequence
    }

    fn append_event(&mut self, event: ServerEventEnvelope) {
        self.events.push(event.clone());
        self.pending_events.push(event);
        Self::prune_events(&mut self.events, self.retention_limit);
        Self::prune_events(&mut self.pending_events, self.retention_limit);
    }

    fn append_workspace(
        &mut self,
        workspace_id: WorkspaceId,
        event: WorkspaceEvent,
    ) -> EventSequence {
        let envelope = WorkspaceEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence: self.next(),
            workspace_id,
            event,
        };
        let sequence = envelope.sequence;
        self.workspace_events.push(envelope.clone());
        self.pending_workspace_events.push(envelope);
        Self::prune_workspace_events(&mut self.workspace_events, self.retention_limit);
        Self::prune_workspace_events(&mut self.pending_workspace_events, self.retention_limit);
        sequence
    }

    fn discard_pending_workspace(&mut self, sequence: EventSequence) {
        self.pending_workspace_events
            .retain(|event| event.sequence != sequence);
        self.workspace_events
            .retain(|event| event.sequence != sequence);
    }

    fn prune_workspace_events(events: &mut Vec<WorkspaceEventEnvelope>, limit: usize) {
        let mut counts = BTreeMap::<WorkspaceId, usize>::new();
        for event in events.iter() {
            *counts.entry(event.workspace_id).or_default() += 1;
        }
        for (workspace_id, count) in counts {
            let mut excess = count.saturating_sub(limit);
            if excess > 0 {
                events.retain(|event| {
                    if excess > 0 && event.workspace_id == workspace_id {
                        excess -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }

    fn prune_events(events: &mut Vec<ServerEventEnvelope>, limit: usize) {
        let mut counts = BTreeMap::<AgentSessionId, usize>::new();
        for event in events.iter() {
            *counts.entry(event.session_id).or_default() += 1;
        }
        for (session_id, count) in counts {
            let mut excess = count.saturating_sub(limit);
            if excess > 0 {
                events.retain(|event| {
                    if excess > 0 && event.session_id == session_id {
                        excess -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }

    fn oldest_sequence(&self, session_id: Option<AgentSessionId>) -> Option<EventSequence> {
        self.oldest_event(session_id).map(|event| event.sequence)
    }

    fn oldest_event(&self, session_id: Option<AgentSessionId>) -> Option<&ServerEventEnvelope> {
        self.events
            .iter()
            .find(|event| session_id.is_none_or(|id| event.session_id == id))
    }

    fn latest_sequence(&self, session_id: Option<AgentSessionId>) -> Option<EventSequence> {
        self.events
            .iter()
            .rev()
            .find(|event| session_id.is_none_or(|id| event.session_id == id))
            .map(|event| event.sequence)
    }

    fn is_cursor_stale(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> bool {
        let Some(after_sequence) = after_sequence else {
            return false;
        };
        let Some(oldest) = self.oldest_event(session_id) else {
            return false;
        };
        if after_sequence.next() >= oldest.sequence {
            return false;
        }
        let Some(session_id) = session_id else {
            return true;
        };
        !matches!(
            &oldest.event,
            loom_protocol::ServerEvent::AgentSessionCreated { snapshot }
                | loom_protocol::ServerEvent::AgentSessionForked { snapshot, .. }
                if snapshot.id == session_id
        )
    }

    fn set_retention(&mut self, limit: usize) {
        self.retention_limit = limit;
        Self::prune_events(&mut self.events, limit);
        Self::prune_events(&mut self.pending_events, limit);
        Self::prune_workspace_events(&mut self.workspace_events, limit);
        Self::prune_workspace_events(&mut self.pending_workspace_events, limit);
    }

    fn events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Vec<ServerEventEnvelope> {
        self.events
            .iter()
            .filter(|event| {
                session_id.is_none_or(|id| event.session_id == id)
                    && after_sequence.is_none_or(|sequence| event.sequence > sequence)
            })
            .cloned()
            .collect()
    }

    fn workspace_events_since(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Vec<WorkspaceFeedEvent> {
        let mut events = self
            .events
            .iter()
            .filter(|event| {
                session_ids.contains(&event.session_id)
                    && after_sequence.is_none_or(|sequence| event.sequence > sequence)
            })
            .cloned()
            .map(WorkspaceFeedEvent::Session)
            .collect::<Vec<_>>();
        events.extend(
            self.workspace_events
                .iter()
                .filter(|event| {
                    event.workspace_id == workspace_id
                        && after_sequence.is_none_or(|sequence| event.sequence > sequence)
                })
                .cloned()
                .map(WorkspaceFeedEvent::Workspace),
        );
        events.sort_by_key(|event| match event {
            WorkspaceFeedEvent::Session(event) => event.sequence,
            WorkspaceFeedEvent::Workspace(event) => event.sequence,
        });
        events
    }

    fn workspace_oldest_sequence(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
        workspace_id: WorkspaceId,
    ) -> Option<EventSequence> {
        self.events
            .iter()
            .find(|event| session_ids.contains(&event.session_id))
            .map(|event| event.sequence)
            .into_iter()
            .chain(
                self.workspace_events
                    .iter()
                    .find(|event| event.workspace_id == workspace_id)
                    .map(|event| event.sequence),
            )
            .min()
    }

    fn workspace_latest_sequence(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
        workspace_id: WorkspaceId,
    ) -> Option<EventSequence> {
        self.events
            .iter()
            .rev()
            .find(|event| session_ids.contains(&event.session_id))
            .map(|event| event.sequence)
            .into_iter()
            .chain(
                self.workspace_events
                    .iter()
                    .rev()
                    .find(|event| event.workspace_id == workspace_id)
                    .map(|event| event.sequence),
            )
            .max()
    }

    fn workspace_cursor_is_stale(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> bool {
        let (Some(after), Some(oldest)) = (
            after_sequence,
            self.workspace_oldest_sequence(session_ids, workspace_id),
        ) else {
            return false;
        };
        after.next() < oldest
    }

    fn recent_events(&self, session_id: AgentSessionId, limit: usize) -> Vec<ServerEventEnvelope> {
        self.events
            .iter()
            .filter(|event| event.session_id == session_id)
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
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

impl RunHandle {
    fn new(runtime: AgentRuntime) -> Self {
        let initial_state = runtime.export_state();
        let message_checkpoint = MessageCheckpointCursor {
            attempt_id: initial_state.run.attempt_id,
            // The handle cannot distinguish a newly-created run from a restored
            // one without touching SQLite. Start at zero so its first checkpoint
            // writes the complete current transcript; later checkpoints are tails.
            message_count: 0,
        };
        Self {
            run_id: runtime.run_id(),
            session_id: runtime.session_id(),
            control: runtime.control(),
            state: Mutex::new(initial_state),
            message_fragments: Mutex::new(MessageFragmentState::default()),
            activity_deltas: Mutex::new(PendingActivityDeltas::default()),
            message_checkpoint: Mutex::new(message_checkpoint),
            event_gate: Mutex::new(()),
            fragment_wake: Condvar::new(),
            runtime: Mutex::new(runtime),
            running: Mutex::new(false),
            idle: Condvar::new(),
            failure: Mutex::new(None),
            worker: Mutex::new(None),
        }
    }

    fn state(&self) -> AgentRuntimeState {
        self.locked_state().clone()
    }

    fn snapshot(&self) -> AgentRunSnapshot {
        self.locked_state().run.clone()
    }

    fn snapshot_projection(&self, include_messages: bool) -> AgentRunSnapshotProjection {
        run_snapshot_projection_with_messages(&self.locked_state(), include_messages)
    }

    fn locked_state(&self) -> MutexGuard<'_, AgentRuntimeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn refresh(&self, runtime: &AgentRuntime) {
        let state = runtime.export_state();
        let mut fragments = self
            .message_fragments
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *self.locked_state() = state;
        fragments.active_message_ordinal = None;
        self.fragment_wake.notify_all();
    }

    fn append_message_delta(&self, persistence: &FilePersistence, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let mut fragments = self.message_fragments.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "run message fragment lock was poisoned",
                true,
            )
        })?;
        let ordinal = match fragments.active_message_ordinal {
            Some(ordinal) => ordinal,
            None => {
                let state = self.locked_state();
                let message_count = u64::try_from(state.messages.len()).map_err(|_| {
                    LoomError::new(
                        ErrorCode::Persistence,
                        "run transcript has too many messages",
                        false,
                    )
                })?;
                let ordinal = if state
                    .messages
                    .last()
                    .is_some_and(|message| message.role == loom_model::MessageRole::Assistant)
                {
                    message_count.saturating_sub(1)
                } else {
                    message_count
                };
                fragments.active_message_ordinal = Some(ordinal);
                ordinal
            }
        };
        if let std::collections::btree_map::Entry::Vacant(entry) =
            fragments.positions.entry(ordinal)
        {
            let (fragment_ordinal, byte_offset) =
                persistence.next_run_message_fragment_position(self.run_id, ordinal)?;
            entry.insert(MessageFragmentPosition {
                ordinal,
                fragment_ordinal,
                byte_offset,
            });
        }
        let pending_bytes = fragments
            .pending_bytes
            .checked_add(text.len())
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::Persistence,
                    "pending run message content is too large",
                    false,
                )
            })?;
        let now = Instant::now();
        if fragments.pending_bytes == 0 {
            fragments.pending_since = Some(now);
        }
        fragments
            .pending
            .entry(ordinal)
            .or_default()
            .content
            .push_str(text);
        fragments.pending_bytes = pending_bytes;
        let interval_elapsed = fragments.pending_since.is_some_and(|pending_since| {
            now.saturating_duration_since(pending_since) >= MESSAGE_FRAGMENT_BATCH_INTERVAL
        });
        if fragments.pending_bytes >= MESSAGE_FRAGMENT_BATCH_BYTES || interval_elapsed {
            self.flush_message_fragments_locked(persistence, &mut fragments)?;
        }
        self.fragment_wake.notify_one();
        Ok(())
    }

    fn flush_message_fragments(&self, persistence: &FilePersistence) -> Result<()> {
        let mut fragments = self.message_fragments.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "run message fragment lock was poisoned",
                true,
            )
        })?;
        self.flush_message_fragments_locked(persistence, &mut fragments)
    }

    fn flush_message_fragments_locked(
        &self,
        persistence: &FilePersistence,
        fragments: &mut MessageFragmentState,
    ) -> Result<()> {
        let ordinals = fragments.pending.keys().copied().collect::<Vec<_>>();
        for ordinal in ordinals {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                fragments.positions.entry(ordinal)
            {
                let (fragment_ordinal, byte_offset) =
                    persistence.next_run_message_fragment_position(self.run_id, ordinal)?;
                entry.insert(MessageFragmentPosition {
                    ordinal,
                    fragment_ordinal,
                    byte_offset,
                });
            }
            loop {
                let (start, end, content) = {
                    let pending = fragments.pending.get(&ordinal).ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::Internal,
                            "pending run message fragment buffer is missing",
                            false,
                        )
                    })?;
                    let start = pending.committed_bytes;
                    if start >= pending.content.len() {
                        break;
                    }
                    let mut end = (start + MESSAGE_FRAGMENT_BATCH_BYTES).min(pending.content.len());
                    while !pending.content.is_char_boundary(end) {
                        end -= 1;
                    }
                    (start, end, pending.content.as_bytes()[start..end].to_vec())
                };
                let position = fragments.positions.get(&ordinal).copied().ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Internal,
                        "streamed message fragment cursor is missing",
                        false,
                    )
                })?;
                let next_fragment_ordinal =
                    position.fragment_ordinal.checked_add(1).ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::Persistence,
                            "run message has too many persisted fragments",
                            false,
                        )
                    })?;
                let next_byte_offset = position
                    .byte_offset
                    .checked_add(u64::try_from(content.len()).map_err(|_| {
                        LoomError::new(
                            ErrorCode::Persistence,
                            "run message fragment length is out of range",
                            false,
                        )
                    })?)
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::Persistence,
                            "run message content is too large",
                            false,
                        )
                    })?;
                persistence.append_run_message_fragment(
                    self.run_id,
                    self.session_id,
                    position.ordinal,
                    position.fragment_ordinal,
                    position.byte_offset,
                    &content,
                )?;
                let position = fragments.positions.get_mut(&ordinal).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Internal,
                        "streamed message fragment cursor is missing",
                        false,
                    )
                })?;
                position.fragment_ordinal = next_fragment_ordinal;
                position.byte_offset = next_byte_offset;
                let pending = fragments.pending.get_mut(&ordinal).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::Internal,
                        "pending run message fragment buffer is missing",
                        false,
                    )
                })?;
                pending.committed_bytes = end;
                fragments.pending_bytes = fragments
                    .pending_bytes
                    .checked_sub(end - start)
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::Internal,
                            "pending run message byte count is inconsistent",
                            false,
                        )
                    })?;
            }
            if fragments
                .pending
                .get(&ordinal)
                .is_some_and(|pending| pending.committed_bytes == pending.content.len())
            {
                fragments.pending.remove(&ordinal);
            }
        }
        if fragments.pending_bytes == 0 {
            fragments.pending_since = None;
        }
        Ok(())
    }

    fn flush_message_fragments_until_stopped(handle: Weak<Self>, persistence: FilePersistence) {
        loop {
            let Some(handle) = handle.upgrade() else {
                return;
            };
            let mut fragments = handle
                .message_fragments
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if fragments.pending_bytes == 0 {
                if !handle.is_running() {
                    return;
                }
                let (guard, _) = handle
                    .fragment_wake
                    .wait_timeout(fragments, Duration::from_secs(1))
                    .unwrap_or_else(PoisonError::into_inner);
                fragments = guard;
                if fragments.pending_bytes == 0 && !handle.is_running() {
                    return;
                }
                continue;
            }
            let deadline = fragments
                .pending_since
                .map(|pending_since| pending_since + MESSAGE_FRAGMENT_BATCH_INTERVAL)
                .unwrap_or_else(Instant::now);
            let Some(wait) = deadline.checked_duration_since(Instant::now()) else {
                if let Err(error) =
                    handle.flush_message_fragments_locked(&persistence, &mut fragments)
                {
                    drop(fragments);
                    handle.record_failure(error);
                    handle.control.request_interrupt();
                    return;
                }
                continue;
            };
            let (guard, _) = handle
                .fragment_wake
                .wait_timeout(fragments, wait)
                .unwrap_or_else(PoisonError::into_inner);
            drop(guard);
        }
    }

    /// Keeps the cached run state current while a step is still executing.
    ///
    /// The transcript in the cached state is only replaced when the step ends;
    /// live message deltas are observable through the event journal.
    fn apply_event(&self, event: &AgentEvent) {
        let mut state = self.locked_state();
        match event {
            AgentEvent::RunStarted { snapshot } | AgentEvent::RunCompleted { snapshot } => {
                state.run = snapshot.clone();
                sync_cached_run_attempt(&mut state);
            }
            AgentEvent::RunStateChanged {
                state: run_state, ..
            } => {
                state.run.state = *run_state;
                state.run.updated_at = Timestamp::now();
                if !matches!(
                    run_state,
                    AgentRunState::AwaitingApproval | AgentRunState::Paused
                ) {
                    state.pending_approval = None;
                }
                if !matches!(run_state, AgentRunState::NeedsInput | AgentRunState::Paused) {
                    state.pending_input = None;
                }
                let checkpoint_retry_transition = *run_state == AgentRunState::Planning
                    && state.attempts.iter().any(|attempt| {
                        attempt.id == state.run.attempt_id && attempt.completed_at.is_some()
                    });
                if !checkpoint_retry_transition {
                    sync_cached_run_attempt(&mut state);
                }
            }
            AgentEvent::RunUsageUpdated { usage, .. } => state.usage = usage.clone(),
            AgentEvent::ToolApprovalRequired {
                attempt_id,
                control_revision,
                call,
                ..
            } => {
                state.pending_approval = Some(call.clone());
                state.run.attempt_id = *attempt_id;
                state.run.control_revision = *control_revision;
            }
            AgentEvent::ToolApprovalDecided {
                attempt_id,
                control_revision,
                ..
            } => {
                state.pending_approval = None;
                state.run.attempt_id = *attempt_id;
                state.run.control_revision = *control_revision;
            }
            AgentEvent::NeedsInput {
                attempt_id,
                control_revision,
                prompt,
                ..
            } => {
                state.pending_input = Some(prompt.clone());
                state.run.attempt_id = *attempt_id;
                state.run.control_revision = *control_revision;
            }
            AgentEvent::UserMessage {
                attempt_id,
                control_revision,
                ..
            } => {
                state.pending_input = None;
                state.run.attempt_id = *attempt_id;
                state.run.control_revision = *control_revision;
            }
            AgentEvent::ToolCallStarted { call, .. } => {
                if state
                    .pending_tool_execution
                    .as_ref()
                    .is_some_and(|pending| pending.id == call.id)
                {
                    state.pending_tool_execution = None;
                }
            }
            AgentEvent::ActivityRecorded { activity, .. } => {
                let updated = if let Some(existing) = state
                    .activities
                    .iter_mut()
                    .find(|existing| existing.id == activity.id)
                {
                    *existing = activity.clone();
                    true
                } else {
                    state.activities.push(activity.clone());
                    false
                };
                let mut deltas = self
                    .activity_deltas
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if !updated && !deltas.by_id.contains_key(&activity.id) {
                    deltas.appended_order.push(activity.id);
                }
                deltas.by_id.insert(activity.id, activity.clone());
            }
            _ => {}
        }
    }

    fn is_running(&self) -> bool {
        *self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_running(&self, running: bool) {
        *self.running.lock().unwrap_or_else(PoisonError::into_inner) = running;
        self.idle.notify_all();
        self.fragment_wake.notify_all();
    }

    fn join_worker(&self) -> Result<()> {
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if worker.is_some_and(|worker| worker.join().is_err()) {
            return Err(LoomError::new(
                ErrorCode::Internal,
                format!("agent run {} worker panicked", self.run_id),
                false,
            ));
        }
        Ok(())
    }

    fn failure(&self) -> Option<LoomError> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Locks the runtime for an operation that requires exclusive access.
    ///
    /// Returns a retryable conflict instead of blocking when a step is in
    /// flight, so a caller is never parked behind a model call.
    fn try_runtime(&self) -> Result<MutexGuard<'_, AgentRuntime>> {
        match self.runtime.try_lock() {
            Ok(runtime) => Ok(runtime),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => Err(LoomError::new(
                ErrorCode::Conflict,
                format!("agent run {} is executing a step", self.run_id),
                true,
            )),
        }
    }

    /// Locks the runtime for an operation that responds to a state the run has
    /// already reached, allowing the worker a moment to finish its last step.
    fn runtime_for_entry(&self) -> Result<MutexGuard<'_, AgentRuntime>> {
        // Approval events are journaled before the worker releases the runtime
        // lock. Give that transition the same settle time as other control
        // operations so a client can approve as soon as the prompt appears.
        let settle_timeout = if self.state().run.state == AgentRunState::AwaitingApproval {
            CONTROL_SETTLE_TIMEOUT
        } else {
            ENTRY_SETTLE_TIMEOUT
        };
        if self.is_running() && self.wait_until_idle_for(settle_timeout).is_err() {
            return Err(LoomError::new(
                ErrorCode::Conflict,
                format!("agent run {} is executing a step", self.run_id),
                true,
            ));
        }
        self.try_runtime()
    }

    /// Waits for an in-flight step to observe a pause or interrupt request.
    fn wait_until_idle(&self) -> Result<()> {
        self.wait_until_idle_for(CONTROL_SETTLE_TIMEOUT)
    }

    fn wait_until_idle_for(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        while *running {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(LoomError::new(
                    ErrorCode::DeadlineExceeded,
                    format!("agent run {} did not stop in time", self.run_id),
                    true,
                ));
            };
            let (guard, timeout) = self
                .idle
                .wait_timeout(running, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            running = guard;
            if timeout.timed_out() && *running {
                return Err(LoomError::new(
                    ErrorCode::DeadlineExceeded,
                    format!("agent run {} did not stop in time", self.run_id),
                    true,
                ));
            }
        }
        Ok(())
    }

    fn take_failure(&self) -> Option<LoomError> {
        self.failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn record_failure(&self, error: LoomError) {
        let mut failure = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        if failure.is_none() {
            *failure = Some(error);
        }
    }
}

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

impl ToolExtension for ProjectAgentTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = Vec::new();
        if self.can_delegate {
            definitions.push(ToolDefinition {
                name: "delegate_project_task".to_owned(),
                description: "Create a bounded non-code child agent task in this project. Omit model_id or set it to `current` to reuse this agent's model; use a provider/model ID to choose another model.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "child_name": {"type": "string", "minLength": 1, "maxLength": 128},
                        "intent": {"type": "string", "minLength": 1, "maxLength": 16384},
                        "model_id": {"type": "string", "minLength": 1, "maxLength": 512, "description": "Optional provider/model ID. Omit this field or use `current` to reuse this agent's model."},
                        "context_references": {
                            "type": "array",
                            "maxItems": 128,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string"},
                                    "uri": {"type": "string"}
                                },
                                "required": ["label", "uri"],
                                "additionalProperties": false
                            }
                        },
                        "dependencies": {
                            "type": "array",
                            "maxItems": 128,
                            "items": {"type": "string", "format": "uuid"}
                        },
                        "permissions": {
                            "type": "object",
                            "properties": {
                                "delegation": {"type": "boolean"},
                                "branch_messaging": {"type": "boolean"},
                                "child_control": {"type": "boolean"},
                                "inspection": {"type": "boolean"},
                                "worktree_creation": {"type": "boolean"},
                                "review": {"type": "boolean"},
                                "integration": {"type": "boolean"}
                            },
                            "additionalProperties": false
                        }
                    },
                    "required": ["child_name", "intent"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_delegate_code {
            definitions.push(ToolDefinition {
                name: "delegate_project_code_task".to_owned(),
                description: "Create a bounded code-changing child task in an isolated Git worktree based on the project's clean current revision. The child must commit its result and report the commit. Only use this for source changes; the child cannot access the parent checkout.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "child_name": {"type": "string", "minLength": 1, "maxLength": 128},
                        "intent": {"type": "string", "minLength": 1, "maxLength": 16384},
                        "model_id": {"type": "string", "minLength": 1, "maxLength": 512, "description": "Optional provider/model ID. Omit this field or use `current` to reuse this agent's model."},
                        "context_references": {
                            "type": "array",
                            "maxItems": 128,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string"},
                                    "uri": {"type": "string"}
                                },
                                "required": ["label", "uri"],
                                "additionalProperties": false
                            }
                        },
                        "dependencies": {
                            "type": "array",
                            "maxItems": 128,
                            "items": {"type": "string", "format": "uuid"}
                        },
                        "permissions": {
                            "type": "object",
                            "properties": {
                                "delegation": {"type": "boolean"},
                                "branch_messaging": {"type": "boolean"},
                                "child_control": {"type": "boolean"},
                                "inspection": {"type": "boolean"},
                                "worktree_creation": {"type": "boolean"},
                                "review": {"type": "boolean"},
                                "integration": {"type": "boolean"}
                            },
                            "additionalProperties": false
                        }
                    },
                    "required": ["child_name", "intent"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_message || self.can_branch_message {
            definitions.push(ToolDefinition {
                name: "send_project_agent_message".to_owned(),
                description: "Send a durable message to an explicitly named project member. Direct parent-child routes use the direct messaging grant; non-adjacent routes require the sender and recipient to have independent branch-messaging grants. task_id supplies optional context and never selects the recipient. The sender is bound to this agent run.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target_session_id": {"type": "string", "format": "uuid"},
                        "task_id": {"type": "string", "format": "uuid"},
                        "kind": {"type": "string", "enum": ["progress", "result", "question", "blocker", "direction", "answer"]},
                        "body": {"type": "string", "minLength": 1, "maxLength": 16384}
                    },
                    "required": ["target_session_id", "kind", "body"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_branch_message {
            definitions.push(ToolDefinition {
                name: "list_project_message_recipients".to_owned(),
                description: "List only project members who have an explicit branch-messaging grant, so you can address an authorized non-adjacent recipient by session ID.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            });
        }
        if self.can_inspect_children {
            definitions.push(ToolDefinition {
                name: "list_project_children".to_owned(),
                description: "Inspect the current status of your direct child agents and their delegated tasks in this project.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
            });
        }
        if self.can_wait_children {
            definitions.push(ToolDefinition {
                name: "wait_for_project_children".to_owned(),
                description: "Wait until the listed direct child tasks are return-ready, releasing this manager's agent slot while they run. The result includes terminal child states; code results still need review and integration before the overall task is complete.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_ids": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 50,
                            "items": {"type": "string", "format": "uuid"}
                        }
                    },
                    "required": ["task_ids"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_control_children {
            definitions.push(ToolDefinition {
                name: "control_project_child".to_owned(),
                description: "Continue a paused child, retry its most recent failed tool step, or cancel it. Address children only by delegated task_id. Retry does not restart an entire task.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid"},
                        "action": {"type": "string", "enum": ["continue", "retry_failed_step", "cancel"]}
                    },
                    "required": ["task_id", "action"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_review_children {
            definitions.push(ToolDefinition {
                name: "review_project_child".to_owned(),
                description: "Review a code child by task_id. Returns its checkout status and a bounded diff from the revision where its worktree was created.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid"}
                    },
                    "required": ["task_id"],
                    "additionalProperties": false
                }),
            });
        }
        if self.can_integrate_children {
            definitions.push(ToolDefinition {
                name: "integrate_project_child".to_owned(),
                description: "Fast-forward the clean parent checkout to a completed code child's committed revision. Supply the exact base revision reported by review; integration fails and preserves the child worktree if the parent has changed or the child does not descend from that base.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "format": "uuid"},
                        "expected_parent_revision": {"type": "string", "minLength": 1, "maxLength": 64}
                    },
                    "required": ["task_id", "expected_parent_revision"],
                    "additionalProperties": false
                }),
            });
        }
        definitions
    }

    fn action_kind(&self, call: &ToolCall) -> Option<loom_core::ActionKind> {
        match call.name.as_str() {
            "delegate_project_task" if self.can_delegate => Some(loom_core::ActionKind::Write),
            "delegate_project_code_task" if self.can_delegate_code => {
                Some(loom_core::ActionKind::Write)
            }
            // Project messaging is a bounded internal coordination action. It
            // uses the low-risk policy tier so routine reports do not require
            // per-message approval; routing is checked against live project
            // membership and direct parent-child relationships.
            "send_project_agent_message" if self.can_message || self.can_branch_message => {
                Some(loom_core::ActionKind::Read)
            }
            "list_project_message_recipients" if self.can_branch_message => {
                Some(loom_core::ActionKind::Read)
            }
            "list_project_children" if self.can_inspect_children => {
                Some(loom_core::ActionKind::Read)
            }
            "wait_for_project_children" if self.can_wait_children => {
                Some(loom_core::ActionKind::Read)
            }
            "control_project_child" if self.can_control_children => {
                Some(loom_core::ActionKind::Write)
            }
            "review_project_child" if self.can_review_children => Some(loom_core::ActionKind::Read),
            "integrate_project_child" if self.can_integrate_children => {
                Some(loom_core::ActionKind::Write)
            }
            _ => None,
        }
    }

    fn execute(&self, call: &ToolCall) -> ToolResult {
        match call.name.as_str() {
            "delegate_project_task" if self.can_delegate => self.execute_delegation(call),
            "delegate_project_code_task" if self.can_delegate_code => {
                self.execute_code_delegation(call)
            }
            "send_project_agent_message" if self.can_message || self.can_branch_message => {
                self.execute_message(call)
            }
            "list_project_message_recipients" if self.can_branch_message => {
                self.execute_list_message_recipients(call)
            }
            "list_project_children" if self.can_inspect_children => {
                self.execute_list_children(call)
            }
            "wait_for_project_children" if self.can_wait_children => {
                self.execute_wait_for_children(call)
            }
            "control_project_child" if self.can_control_children => {
                self.execute_control_child(call)
            }
            "review_project_child" if self.can_review_children => self.execute_review_child(call),
            "integrate_project_child" if self.can_integrate_children => {
                self.execute_integrate_child(call)
            }
            _ => ToolResult::failure(call, format!("unknown project agent tool '{}'", call.name)),
        }
    }

    fn prepare_deferred(&self, call: &ToolCall) -> Option<String> {
        if call.name != "wait_for_project_children" || !self.can_wait_children {
            return None;
        }
        let arguments =
            serde_json::from_value::<WaitForProjectChildrenArguments>(call.arguments.clone())
                .ok()?;
        let tasks = self.load_wait_child_tasks(&arguments.task_ids).ok()?;
        if tasks.iter().any(|task| {
            !matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            )
        }) {
            Some(call.id.to_string())
        } else {
            None
        }
    }

    fn completion_blocker(&self) -> Option<String> {
        let Some(backend) = self.backend.upgrade() else {
            return Some(
                "project state is unavailable; verify child work before reporting completion"
                    .to_owned(),
            );
        };
        let persistence = backend.persistence.as_ref()?;
        let tasks = match persistence.list_project_tasks(self.project_id) {
            Ok(tasks) => tasks,
            Err(_) => {
                return Some(
                    "project child state could not be confirmed; inspect child tasks before reporting completion"
                        .to_owned(),
                );
            }
        };
        for task in tasks
            .iter()
            .filter(|task| task.requester_session_id == self.session_id)
        {
            if !matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            ) {
                return Some(format!(
                    "direct child task {} ({}) is still {:?}; wait for all active children before reporting completion",
                    task.task_id, task.child_name, task.status
                ));
            }
            if task.code_change && task.status == loom_core::DelegatedTaskStatus::Completed {
                match persistence.load_project_worktree_by_task(task.task_id) {
                    Ok(Some(worktree)) => {
                        let Some(result_revision) = worktree.result_revision.as_deref() else {
                            return Some(format!(
                                "completed code child task {} ({}) has not had its result reviewed",
                                task.task_id, task.child_name
                            ));
                        };
                        if worktree.status != ProjectWorktreeStatus::Removed {
                            match project_child_worktree_status(&backend, &worktree) {
                                Ok(status)
                                    if status.clean
                                        && status.head.as_deref() == Some(result_revision) => {}
                                Ok(_) => {
                                    return Some(format!(
                                        "completed code child task {} ({}) changed after review or has uncommitted work; review its current checkout before reporting completion",
                                        task.task_id, task.child_name
                                    ));
                                }
                                Err(_) => {
                                    return Some(format!(
                                        "the live checkout for code child task {} ({}) could not be confirmed",
                                        task.task_id, task.child_name
                                    ));
                                }
                            }
                        }
                        if result_revision != worktree.base_revision
                            && worktree.integrated_revision.as_deref() != Some(result_revision)
                        {
                            return Some(format!(
                                "completed code child task {} ({}) has a reviewed result that still needs integration",
                                task.task_id, task.child_name
                            ));
                        }
                    }
                    Ok(None) => {
                        return Some(format!(
                            "completed code child task {} ({}) has no durable worktree record to verify",
                            task.task_id, task.child_name
                        ));
                    }
                    Err(_) => {
                        return Some(format!(
                            "integration state for code child task {} ({}) could not be confirmed",
                            task.task_id, task.child_name
                        ));
                    }
                }
            }
        }
        None
    }
}

impl ProjectAgentTools {
    fn load_wait_child_tasks(
        &self,
        task_ids: &[loom_core::TaskId],
    ) -> Result<Vec<loom_core::DelegatedTaskRecord>> {
        if task_ids.is_empty() || task_ids.len() > 50 {
            return Err(LoomError::invalid_request(
                "wait_for_project_children requires between one and fifty task IDs",
            ));
        }
        let unique = task_ids.iter().copied().collect::<BTreeSet<_>>();
        if unique.len() != task_ids.len() {
            return Err(LoomError::invalid_request(
                "wait_for_project_children task IDs must be unique",
            ));
        }
        let backend = self.backend.upgrade().ok_or_else(|| {
            LoomError::new(
                ErrorCode::Internal,
                "project backend is no longer available",
                true,
            )
        })?;
        let persistence = backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project child waits require durable storage",
                false,
            )
        })?;
        task_ids
            .iter()
            .map(|task_id| {
                let task = persistence
                    .load_delegated_task(*task_id)?
                    .ok_or_else(|| LoomError::not_found("delegated task", *task_id))?;
                if task.project_id != self.project_id
                    || task.requester_session_id != self.session_id
                {
                    return Err(LoomError::new(
                        ErrorCode::AuthorizationDenied,
                        "wait_for_project_children accepts only direct child task IDs",
                        false,
                    ));
                }
                Ok(task)
            })
            .collect()
    }

    fn execute_wait_for_children(&self, call: &ToolCall) -> ToolResult {
        let arguments =
            match serde_json::from_value::<WaitForProjectChildrenArguments>(call.arguments.clone())
            {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolResult::failure(
                        call,
                        format!("invalid project child wait arguments: {error}"),
                    );
                }
            };
        match self.load_wait_child_tasks(&arguments.task_ids) {
            Ok(tasks)
                if tasks.iter().all(|task| {
                    matches!(
                        task.status,
                        loom_core::DelegatedTaskStatus::Completed
                            | loom_core::DelegatedTaskStatus::Failed
                            | loom_core::DelegatedTaskStatus::Cancelled
                    )
                }) => ToolResult::success(
                call,
                serde_json::to_string(&serde_json::json!({
                    "return_ready": true,
                    "children": tasks.iter().map(|task| serde_json::json!({
                        "task_id": task.task_id,
                        "child_name": task.child_name,
                        "status": task.status,
                        "code_change": task.code_change,
                    })).collect::<Vec<_>>(),
                    "note": "Code child results still require review and integration before the manager reports completion."
                }))
                .unwrap_or_else(|error| format!("could not encode child wait result: {error}")),
            ),
            Ok(_) => ToolResult::failure(
                call,
                "child tasks are still active; retry through the durable wait continuation",
            ),
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    fn connection(&self) -> Option<InProcessConnection> {
        self.backend.upgrade().map(|backend| InProcessConnection {
            backend,
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
        })
    }

    fn execute_delegation(&self, call: &ToolCall) -> ToolResult {
        self.execute_delegation_with_kind(call, false)
    }

    fn execute_code_delegation(&self, call: &ToolCall) -> ToolResult {
        self.execute_delegation_with_kind(call, true)
    }

    fn execute_delegation_with_kind(&self, call: &ToolCall, code_change: bool) -> ToolResult {
        let arguments =
            match serde_json::from_value::<DelegateProjectTaskArguments>(call.arguments.clone()) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolResult::failure(
                        call,
                        format!("invalid project delegation arguments: {error}"),
                    );
                }
            };
        let run_grants = loom_core::ProjectAgentPermissions {
            delegation: self.can_delegate,
            branch_messaging: self.can_branch_message,
            child_control: self.can_control_children,
            inspection: self.can_inspect_children,
            worktree_creation: self.can_delegate_code,
            review: self.can_review_children,
            integration: self.can_integrate_children,
        };
        if !project_permissions_are_subset(arguments.permissions, run_grants) {
            return ToolResult::failure(
                call,
                "this agent run cannot grant one or more requested project permissions to a child",
            );
        }
        let Some(backend) = self.backend.upgrade() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        let connection = InProcessConnection {
            backend,
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
        };
        match connection.load_project_snapshot(self.project_id) {
            Ok(project)
                if project.agents.iter().any(|agent| {
                    agent.session_id == self.session_id && agent.project_id == self.project_id
                }) => {}
            Ok(_) => {
                return ToolResult::failure(call, "project delegation grant is no longer valid");
            }
            Err(error) => return ToolResult::failure(call, error.message),
        }
        let request_id = RequestId::from_uuid(*call.id.as_uuid());
        let spec = DelegatedTaskSpec {
            intent: arguments.intent,
            model_id: delegated_child_model_id(arguments.model_id, &self.model_id),
            context_references: arguments.context_references,
            dependencies: arguments.dependencies,
            code_change,
            permissions: arguments.permissions,
        };
        match connection.create_project_child(
            request_id,
            self.session_id,
            arguments.child_name,
            spec,
        ) {
            Ok(ServerResponse::ProjectChildCreated { task, child }) => ToolResult {
                tool_call_id: call.id,
                name: call.name.clone(),
                success: true,
                output: serde_json::to_string(&serde_json::json!({
                    "task_id": task.task_id,
                    "child_session_id": child.session_id,
                    "status": task.status,
                    "child_name": task.child_name,
                    "intent": task.intent,
                }))
                .unwrap_or_else(|error| format!("could not encode delegation result: {error}")),
            },
            Ok(_) => ToolResult::failure(call, "project delegation returned an unexpected result"),
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    fn execute_message(&self, call: &ToolCall) -> ToolResult {
        let arguments = match serde_json::from_value::<SendProjectAgentMessageArguments>(
            call.arguments.clone(),
        ) {
            Ok(arguments) => arguments,
            Err(error) => {
                return ToolResult::failure(
                    call,
                    format!("invalid project agent message arguments: {error}"),
                );
            }
        };
        if arguments.body.trim().is_empty() || arguments.body.len() > 16 * 1024 {
            return ToolResult::failure(call, "agent message body must contain 1 to 16384 bytes");
        }
        let Some(target_session_id) = arguments.target_session_id else {
            return ToolResult::failure(call, "target_session_id is required");
        };
        let Some(backend) = self.backend.upgrade() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        let draft = loom_core::AgentMessageDraft {
            project_id: self.project_id,
            task_id: arguments.task_id,
            sender_session_id: self.session_id,
            target_session_id,
            kind: arguments.kind,
            body: arguments.body,
        };
        match backend.accept_project_agent_message(
            RequestId::from_uuid(*call.id.as_uuid()),
            self.session_id,
            self.can_message,
            self.can_branch_message,
            draft,
        ) {
            Ok(ServerResponse::ProjectAgentMessageAccepted(message)) => ToolResult {
                tool_call_id: call.id,
                name: call.name.clone(),
                success: true,
                output: serde_json::to_string(&serde_json::json!({
                    "message_id": message.message_id,
                    "project_sequence": message.project_sequence,
                    "accepted_at": message.accepted_at,
                    "target_session_id": message.target_session_id,
                    "kind": message.kind,
                }))
                .unwrap_or_else(|error| {
                    format!("could not encode project message result: {error}")
                }),
            },
            Ok(_) => ToolResult::failure(call, "project messaging returned an unexpected result"),
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    fn execute_list_message_recipients(&self, call: &ToolCall) -> ToolResult {
        if let Err(error) =
            serde_json::from_value::<ListProjectMessageRecipientsArguments>(call.arguments.clone())
        {
            return ToolResult::failure(
                call,
                format!("invalid project message-recipient query: {error}"),
            );
        }
        let Some(backend) = self.backend.upgrade() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        let Some(persistence) = backend.persistence.as_ref() else {
            return ToolResult::failure(call, "project messaging requires durable storage");
        };
        let project = match persistence.load_project_snapshot(self.project_id) {
            Ok(Some(project)) => project,
            Ok(None) => return ToolResult::failure(call, "project no longer exists"),
            Err(error) => return ToolResult::failure(call, error.message),
        };
        let recipients = project
            .agents
            .iter()
            .filter(|agent| agent.session_id != self.session_id)
            .filter_map(|agent| {
                let name = if agent.session_id == project.root_session_id {
                    "project root".to_owned()
                } else {
                    project
                        .tasks
                        .iter()
                        .find(|task| task.target_session_id == agent.session_id)
                        .map(|task| task.child_name.clone())
                        .or_else(|| agent.task_summary.clone())
                        .unwrap_or_else(|| "project agent".to_owned())
                };
                match project_member_branch_messaging_enabled(
                    persistence,
                    project.root_session_id,
                    agent.session_id,
                ) {
                    Ok(true) => Some(Ok(serde_json::json!({
                        "session_id": agent.session_id,
                        "name": name,
                        "depth": agent.depth,
                        "parent_session_id": agent.parent_session_id,
                    }))),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .collect::<Result<Vec<_>>>();
        match recipients {
            Ok(recipients) => ToolResult {
                tool_call_id: call.id,
                name: call.name.clone(),
                success: true,
                output: serde_json::to_string(&recipients)
                    .unwrap_or_else(|error| format!("could not encode recipients: {error}")),
            },
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    fn execute_list_children(&self, call: &ToolCall) -> ToolResult {
        if let Err(error) =
            serde_json::from_value::<ListProjectChildrenArguments>(call.arguments.clone())
        {
            return ToolResult::failure(call, format!("invalid project child query: {error}"));
        }
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        let project = match connection.load_project_snapshot(self.project_id) {
            Ok(project) => project,
            Err(error) => return ToolResult::failure(call, error.message),
        };
        if !project
            .agents
            .iter()
            .any(|agent| agent.session_id == self.session_id)
        {
            return ToolResult::failure(call, "project agent inspection grant is no longer valid");
        }
        let Some(persistence) = &connection.backend.persistence else {
            return ToolResult::failure(call, "project agent inspection requires durable storage");
        };
        let tasks = match persistence.list_project_tasks(self.project_id) {
            Ok(tasks) => tasks,
            Err(error) => return ToolResult::failure(call, error.message),
        };
        let sessions = match connection.backend.sessions() {
            Ok(sessions) => sessions,
            Err(error) => return ToolResult::failure(call, error.message),
        };
        let mut children = project
            .agents
            .iter()
            .filter(|agent| agent.parent_session_id == Some(self.session_id))
            .map(|agent| {
                let task = tasks
                    .iter()
                    .find(|task| task.target_session_id == agent.session_id);
                let name = sessions
                    .get(agent.session_id)
                    .map(|session| session.name.clone())
                    .unwrap_or_default();
                serde_json::json!({
                    "session_id": agent.session_id,
                    "name": name,
                    "state": agent.state,
                    "task_summary": agent.task_summary.as_ref().map(|summary| summary.chars().take(256).collect::<String>()),
                    "task_id": task.map(|task| task.task_id),
                    "task_status": task.map(|task| task.status),
                    "updated_at": agent.updated_at,
                })
            })
            .collect::<Vec<_>>();
        children.sort_by(|left, right| {
            right["updated_at"]
                .as_u64()
                .cmp(&left["updated_at"].as_u64())
        });
        let truncated = children.len() > 50;
        children.truncate(50);
        ToolResult {
            tool_call_id: call.id,
            name: call.name.clone(),
            success: true,
            output: serde_json::to_string(&serde_json::json!({
                "children": children,
                "truncated": truncated,
            }))
            .unwrap_or_else(|error| format!("could not encode project child status: {error}")),
        }
    }

    fn execute_control_child(&self, call: &ToolCall) -> ToolResult {
        let arguments =
            match serde_json::from_value::<ControlProjectChildArguments>(call.arguments.clone()) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolResult::failure(
                        call,
                        format!("invalid project child control arguments: {error}"),
                    );
                }
            };
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        match connection.control_project_child(
            self.session_id,
            self.project_id,
            arguments.task_id,
            arguments.action,
        ) {
            Ok((task, run)) => ToolResult {
                tool_call_id: call.id,
                name: call.name.clone(),
                success: true,
                output: serde_json::to_string(&serde_json::json!({
                    "task_id": task.task_id,
                    "status": task.status,
                    "child_session_id": task.target_session_id,
                    "run_state": run.map(|snapshot| snapshot.state),
                }))
                .unwrap_or_else(|error| {
                    format!("could not encode project child control result: {error}")
                }),
            },
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    fn execute_review_child(&self, call: &ToolCall) -> ToolResult {
        let arguments =
            match serde_json::from_value::<ReviewProjectChildArguments>(call.arguments.clone()) {
                Ok(arguments) => arguments,
                Err(error) => {
                    return ToolResult::failure(
                        call,
                        format!("invalid project child review arguments: {error}"),
                    );
                }
            };
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        match connection.get_project_child_review(
            self.project_id,
            self.session_id,
            arguments.task_id,
        ) {
            Ok(response @ ServerResponse::ProjectChildReview { .. }) => ToolResult {
                tool_call_id: call.id,
                name: call.name.clone(),
                success: true,
                output: serde_json::to_string(&response)
                    .unwrap_or_else(|error| format!("could not encode child review: {error}")),
            },
            Ok(_) => {
                ToolResult::failure(call, "project child review returned an unexpected result")
            }
            Err(error) => ToolResult::failure(call, error.message),
        }
    }

    fn execute_integrate_child(&self, call: &ToolCall) -> ToolResult {
        let arguments = match serde_json::from_value::<IntegrateProjectChildArguments>(
            call.arguments.clone(),
        ) {
            Ok(arguments) => arguments,
            Err(error) => {
                return ToolResult::failure(
                    call,
                    format!("invalid project child integration arguments: {error}"),
                );
            }
        };
        if arguments.expected_parent_revision.trim().is_empty()
            || arguments.expected_parent_revision.len() > 64
        {
            return ToolResult::failure(
                call,
                "expected_parent_revision must contain 1 to 64 bytes",
            );
        }
        let Some(connection) = self.connection() else {
            return ToolResult::failure(call, "project backend is no longer available");
        };
        match connection.integrate_project_child(
            RequestId::from_uuid(*call.id.as_uuid()),
            self.project_id,
            self.session_id,
            arguments.task_id,
            arguments.expected_parent_revision,
        ) {
            Ok(ServerResponse::ProjectChildWorktreeUpdated(worktree)) => ToolResult {
                tool_call_id: call.id,
                name: call.name.clone(),
                success: true,
                output: serde_json::to_string(&serde_json::json!({
                    "task_id": worktree.task_id,
                    "status": worktree.status,
                    "integrated_revision": worktree.integrated_revision,
                }))
                .unwrap_or_else(|error| format!("could not encode child integration: {error}")),
            },
            Ok(_) => ToolResult::failure(
                call,
                "project child integration returned an unexpected result",
            ),
            Err(error) => ToolResult::failure(call, error.message),
        }
    }
}

#[derive(Default)]
struct ResourceMonitor {
    system: System,
    has_cpu_baseline: bool,
}

impl ResourceMonitor {
    fn sample(
        &mut self,
        disk_total_bytes: Option<u64>,
        disk_available_bytes: Option<u64>,
    ) -> WorkerNodeResources {
        self.system.refresh_cpu_all();
        self.system.refresh_memory();

        let cpu_usage_percent = if self.has_cpu_baseline {
            cpu_usage_percent(self.system.global_cpu_usage())
        } else {
            None
        };
        self.has_cpu_baseline = true;

        let memory_total = self.system.total_memory();
        let memory_available = self.system.available_memory();
        let memory_total_bytes = (memory_total > 0).then_some(memory_total);
        let memory_available_bytes = memory_total_bytes.map(|_| memory_available.min(memory_total));

        WorkerNodeResources {
            cpu_count: std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1),
            cpu_usage_percent,
            memory_usage_percent: memory_usage_percent(memory_total_bytes, memory_available_bytes),
            memory_total_bytes,
            memory_available_bytes,
            disk_total_bytes,
            disk_available_bytes,
        }
    }
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

impl InProcessBackend {
    pub fn new() -> Arc<Self> {
        Self::with_provider_registry(ProviderRegistry::demo())
    }

    pub fn new_with_github_copilot() -> Result<Arc<Self>> {
        let credentials = github_copilot_credentials()?;
        Self::with_provider_registry_persistent_credentials(
            ProviderRegistry::configured(credentials)?,
            None,
        )
    }

    pub fn demo_with_github_copilot() -> Result<Arc<Self>> {
        let credentials = github_copilot_credentials()?;
        Self::with_provider_registry_persistent_credentials(
            ProviderRegistry::demo_with_credentials(credentials),
            None,
        )
    }

    pub fn with_models(models: Vec<ModelDescriptor>) -> Arc<Self> {
        let credentials = Arc::new(loom_providers::InMemoryCredentialStore::default());
        let providers = ProviderRegistry::with_credentials(credentials);
        for model in &models {
            let existing_provider = providers
                .list_models()
                .unwrap_or_else(|error| panic!("could not inspect provider models: {error}"))
                .iter()
                .any(|existing| existing.provider == model.provider);
            if existing_provider {
                if model.provider.as_str() == "deterministic" {
                    panic!(
                        "deterministic provider only supports model '{}'",
                        deterministic_descriptor().id.as_str()
                    );
                }
                providers
                    .add_model(&model.provider, model.clone())
                    .unwrap_or_else(|error| panic!("could not add provider model: {error}"));
                continue;
            }
            let result = if model.provider.as_str() == "deterministic" {
                providers.register(ProviderConfig::deterministic())
            } else if model.provider.as_str() == "ollama" {
                providers.register(ProviderConfig::ollama(
                    "http://127.0.0.1:11434",
                    model.id.clone(),
                ))
            } else {
                providers.register(ProviderConfig::openai_compatible(
                    model.provider.clone(),
                    "OpenAI-compatible model",
                    "http://127.0.0.1:8000/v1/chat/completions",
                    model.clone(),
                    None,
                ))
            };
            result.unwrap_or_else(|error| panic!("could not register provider model: {error}"));
        }
        Self::with_provider_registry_and_persistence(providers, None)
            .unwrap_or_else(|error| panic!("could not configure providers: {error}"))
    }

    pub fn with_openai_compatible(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
    ) -> Arc<Self> {
        let descriptor = openai_compatible_descriptor(model.into());
        let credentials = Arc::new(loom_providers::InMemoryCredentialStore::default());
        credentials.insert(CredentialRef::new("legacy-openai"), api_key.into());
        let providers = ProviderRegistry::with_credentials(credentials);
        let config = ProviderConfig::openai_compatible(
            "openai-compatible",
            "OpenAI-compatible model",
            endpoint,
            descriptor,
            Some(CredentialRef::new("legacy-openai")),
        );
        providers
            .register(config)
            .expect("legacy OpenAI provider configuration is valid");
        Self::with_provider_registry(providers)
    }

    pub fn with_openai_compatible_persistent(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
        path: impl Into<PathBuf>,
    ) -> Result<Arc<Self>> {
        let descriptor = openai_compatible_descriptor(model.into());
        let credentials = Arc::new(loom_providers::InMemoryCredentialStore::default());
        let api_key = api_key.into();
        let credential = if api_key.is_empty() {
            None
        } else {
            let reference = CredentialRef::new("ui-openai-compatible");
            credentials.insert(reference.clone(), api_key);
            Some(reference)
        };
        let providers = ProviderRegistry::with_credentials(credentials);
        providers.register(ProviderConfig::openai_compatible(
            "openai-compatible",
            "OpenAI-compatible model",
            endpoint,
            descriptor,
            credential,
        ))?;
        Self::with_provider_registry_persistent(providers, path)
    }

    pub fn with_openai_compatible_persistent_with_github_copilot(
        endpoint: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<ModelId>,
        path: impl Into<PathBuf>,
    ) -> Result<Arc<Self>> {
        let descriptor = openai_compatible_descriptor(model.into());
        let credentials = github_copilot_credentials()?;
        let api_key = api_key.into();
        let credential = if api_key.trim().is_empty() {
            None
        } else {
            let reference = CredentialRef::new("ui-openai-compatible");
            credentials.insert(reference.clone(), api_key)?;
            Some(reference)
        };
        let providers = ProviderRegistry::with_credentials(credentials);
        providers.register(ProviderConfig::openai_compatible(
            "openai-compatible",
            "OpenAI-compatible model",
            endpoint,
            descriptor,
            credential,
        ))?;
        providers.register(ProviderConfig::github_copilot(CredentialRef::new(
            GITHUB_COPILOT_CREDENTIAL_REF,
        )))?;
        Self::with_provider_registry_persistent(providers, path)
    }

    pub fn with_provider_registry(providers: ProviderRegistry) -> Arc<Self> {
        Self::with_provider_registry_and_persistence(providers, None)
            .unwrap_or_else(|error| panic!("could not configure providers: {error}"))
    }

    pub fn new_persistent(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let providers = ProviderRegistry::demo();
        Self::with_provider_registry_and_persistence(
            providers,
            Some(FilePersistence::open_exclusive_writer(path.into())?),
        )
    }

    pub fn new_persistent_with_github_copilot(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let credentials = github_copilot_credentials()?;
        Self::with_provider_registry_persistent_credentials(
            ProviderRegistry::configured(credentials)?,
            Some(path.into()),
        )
    }

    pub fn open_persistent(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        Self::new_persistent(path)
    }

    pub fn with_persistence(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        Self::new_persistent(path)
    }

    pub fn with_provider_registry_persistent(
        providers: ProviderRegistry,
        path: impl Into<PathBuf>,
    ) -> Result<Arc<Self>> {
        Self::with_provider_registry_and_persistence(
            providers,
            Some(FilePersistence::open_exclusive_writer(path.into())?),
        )
    }

    fn with_provider_registry_persistent_credentials(
        providers: ProviderRegistry,
        path: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        Self::with_provider_registry_and_persistence(
            providers,
            path.map(FilePersistence::open_exclusive_writer)
                .transpose()?,
        )
    }

    fn with_provider_registry_and_persistence(
        providers: ProviderRegistry,
        persistence: Option<FilePersistence>,
    ) -> Result<Arc<Self>> {
        if let Some(persistence) = &persistence {
            let credential_path = persistence.path().with_extension("credentials.json");
            providers
                .scope_api_key_credentials(Arc::new(FileCredentialStore::open(credential_path)?))?;
        }
        let (node_id, node_name) = worker_node_identity();
        let session_root_base = persistence.as_ref().map_or_else(
            || {
                std::env::temp_dir().join(format!(
                    "loom-session-roots-{node_id}-{}",
                    WorkspaceId::new()
                ))
            },
            |persistence| persistence.path().with_extension("session-roots"),
        );
        let mut supported_capabilities = vec![
            Capability::CreateAgentSession,
            Capability::ReadAgentSession,
            Capability::ControlAgentSession,
            Capability::SubscribeSessionEvents,
            Capability::SubscribeWorkspaceEvents,
            Capability::StartAgentRun,
            Capability::ReadAgentRun,
            Capability::ReadAgentRunMessages,
            Capability::ControlAgentRun,
            Capability::PauseAgentRun,
            Capability::ResumeAgentRun,
            Capability::ForkAgentSession,
            Capability::RetryFromCheckpoint,
            Capability::ApproveAgentAction,
            Capability::ListProviders,
            Capability::ConfigureProviders,
            Capability::ReadProviderHealth,
            Capability::ReadUsage,
            Capability::InspectContext,
            Capability::ReadWorkspaceConfig,
            Capability::OpenSessionTerminal,
            Capability::ControlSessionTerminal,
            Capability::ReadSessionTask,
            Capability::StartSessionTask,
            Capability::ControlSessionTask,
            Capability::ConfigureApprovalPolicy,
            Capability::ManageCheckpoints,
            Capability::ReadVcsStatus,
            Capability::ReadVcsDiff,
            Capability::ReadSessionTaskEvidence,
            Capability::ReadWorkerNodeStatus,
            Capability::JsonProtocol,
            Capability::ManageWorkspaces,
            Capability::ManageSessionRepositories,
            Capability::BrowseGitHubRepositories,
            Capability::ReadProject,
            Capability::ReadSessionFilesystem,
            Capability::WriteSessionFilesystem,
        ];
        if persistence.is_some() {
            supported_capabilities.extend([
                Capability::CreateProjectChild,
                Capability::ControlProjectChild,
                Capability::SendProjectAgentMessage,
                Capability::SendProjectBranchMessage,
                Capability::ReadProjectAgentMessages,
                Capability::CreateProjectWorktree,
                Capability::ReadProjectChildReview,
                Capability::IntegrateProjectChild,
                Capability::CleanupProjectChildWorktree,
                Capability::CreateNestedProjectChild,
            ]);
        }
        let backend = Arc::new(Self {
            node_id,
            node_name,
            sessions: Mutex::new(SessionManager::default()),
            workspace_records: Mutex::new(loom_session::WorkspaceManager::default()),
            runs: Mutex::new(BTreeMap::new()),
            persisted_runs: Mutex::new(BTreeMap::new()),
            journal: Mutex::new(EventJournal::default()),
            last_feed_pruned_sequence: AtomicU64::new(0),
            feed_bytes_since_prune: AtomicUsize::new(0),
            session_filesystems: Mutex::new(BTreeMap::new()),
            persisted_session_filesystems: Mutex::new(BTreeSet::new()),
            session_filesystem_restore: Mutex::new(()),
            session_repositories: Mutex::new(BTreeMap::new()),
            session_vcs: Mutex::new(BTreeMap::new()),
            session_task_supervisors: Mutex::new(BTreeMap::new()),
            session_policies: Mutex::new(BTreeMap::new()),
            auto_approve_actions: Mutex::new(BTreeMap::new()),
            workspace_configs: Mutex::new(BTreeMap::new()),
            session_terminals: Mutex::new(BTreeMap::new()),
            terminals: TerminalManager::new(),
            resource_monitor: Mutex::new(ResourceMonitor::default()),
            supported_capabilities: CapabilitySet::new(supported_capabilities),
            providers,
            credentials: CredentialService::new(),
            persistence,
            session_root_base,
            idempotency_store: IdempotencyStore::new(),
            admissions: AdmissionService::new(),
            self_reference: Mutex::new(Weak::new()),
            request_lifecycle: RwLock::new(0),
            persistence_failed: AtomicBool::new(false),
            state_persist_gate: Mutex::new(()),
            #[cfg(test)]
            fail_next_state_save: AtomicBool::new(false),
            #[cfg(test)]
            project_cancellation_failpoint: AtomicUsize::new(0),
        });
        *backend
            .self_reference
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Arc::downgrade(&backend);
        backend.restore_persisted()?;
        Ok(backend)
    }

    pub fn connect(self: &Arc<Self>) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: None,
        }
    }

    pub fn connect_authenticated(self: &Arc<Self>, auth: AuthSession) -> InProcessConnection {
        InProcessConnection {
            backend: Arc::clone(self),
            negotiated_capabilities: Arc::new(Mutex::new(None)),
            auth: Some(auth),
        }
    }

    pub fn provider_registry(&self) -> ProviderRegistry {
        self.providers.clone()
    }

    fn provider(&self, model: &ModelId) -> Result<Box<dyn ModelProvider>> {
        self.providers.create_provider(model)
    }

    fn provider_at(&self, model: &ModelId, cursor: usize) -> Result<Box<dyn ModelProvider>> {
        self.providers.create_provider_at(model, cursor)
    }

    fn sessions(&self) -> Result<MutexGuard<'_, SessionManager>> {
        self.sessions.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session manager lock was poisoned",
                true,
            )
        })
    }

    fn workspace_records(&self) -> Result<MutexGuard<'_, loom_session::WorkspaceManager>> {
        self.workspace_records.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace record manager lock was poisoned",
                true,
            )
        })
    }

    fn session_filesystems(&self) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, Workspace>>> {
        self.session_filesystems.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session filesystem manager lock was poisoned",
                true,
            )
        })
    }

    fn persisted_session_filesystems(&self) -> Result<MutexGuard<'_, BTreeSet<AgentSessionId>>> {
        self.persisted_session_filesystems.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "persisted session filesystem manager lock was poisoned",
                true,
            )
        })
    }

    fn restore_session_filesystem(&self, session_id: AgentSessionId) -> Result<Workspace> {
        if let Some(filesystem) = self.session_filesystems()?.get(&session_id).cloned() {
            return Ok(filesystem);
        }
        let _restore_guard = self.session_filesystem_restore.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session filesystem restore lock was poisoned",
                true,
            )
        })?;
        if let Some(filesystem) = self.session_filesystems()?.get(&session_id).cloned() {
            return Ok(filesystem);
        }
        if !self.persisted_session_filesystems()?.contains(&session_id) {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("filesystem for session {session_id} is unavailable"),
                true,
            ));
        }
        let durable = self
            .persistence
            .as_ref()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("filesystem for session {session_id} has no persistence store"),
                    true,
                )
            })?
            .load_filesystem_record(session_id)?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted filesystem record for session {session_id} is missing"),
                    false,
                )
            })?;
        let mut payload = durable.payload;
        let repositories = durable.repositories;
        let directories = durable.directories;
        payload["filesystem"]["checkpoints"] = json_value(durable.checkpoints)?;
        let edits = durable.edits;
        let changes = durable.changes;
        let mut persisted: PersistedSessionFilesystem = from_json(payload)?;
        persisted.filesystem.edits = edits
            .into_iter()
            .map(|edit| loom_workspace::WorkspaceEditHistory {
                id: edit.id,
                path: edit.path,
                before: edit.before,
                before_bytes: edit.before_bytes,
                after_revision: edit.after_revision,
                source: edit.source,
            })
            .collect();
        persisted.filesystem.changes = changes;
        persisted.repositories = repositories;
        persisted.directories = directories;
        if durable.session_id != session_id
            || persisted.filesystem.session_id != session_id
            || persisted.filesystem.root != durable.root
            || persisted.filesystem.control != durable.control
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem identity for session {session_id} is invalid"),
                false,
            ));
        }
        let session = self.sessions()?.get(session_id)?;
        let expected_root = self
            .session_root_base
            .join(session.workspace_id.to_string())
            .join(session_id.to_string())
            .join("fs");
        let canonical_expected_root = fs::canonicalize(&expected_root).map_err(|error| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("session filesystem root for {session_id} is unavailable: {error}"),
                true,
            )
        })?;
        if Path::new(&persisted.filesystem.root) != canonical_expected_root {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem root for session {session_id} is invalid"),
                false,
            ));
        }
        let filesystem = Workspace::open_for_restore(session_id, &canonical_expected_root)?;
        for directory in &persisted.directories {
            filesystem.mount_directory(&directory.path, &directory.source)?;
        }
        filesystem.restore_state(persisted.filesystem)?;
        let owned_worktree_repository_ids = self
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_project_snapshot_for_session(session_id))
            .transpose()?
            .flatten()
            .map(|project| {
                project
                    .worktrees
                    .into_iter()
                    .filter(|worktree| worktree.child_session_id == session_id)
                    .map(|worktree| worktree.child_repository_id)
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let mut missing_worktree_repositories = Vec::new();
        for (repository_id, repository) in &persisted.repositories {
            if *repository_id != repository.id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("repository key does not match repository {}", repository.id),
                    false,
                ));
            }
            let path = filesystem.resolve_path(&repository.path, true)?;
            if !path.exists() && owned_worktree_repository_ids.contains(repository_id) {
                // The worktree record is authoritative for recovery. A missing
                // checkout must not prevent the child session from opening;
                // the scheduler or cleanup request will reconcile its durable
                // worktree state before using the repository.
                missing_worktree_repositories.push(*repository_id);
                continue;
            }
            let service = GitService::open(path)?;
            self.session_vcs()?
                .insert((session_id, *repository_id), service);
        }
        for repository_id in &missing_worktree_repositories {
            persisted.repositories.remove(repository_id);
        }
        self.session_repositories()?
            .insert(session_id, persisted.repositories);
        if !missing_worktree_repositories.is_empty() {
            filesystem.mark_state_dirty()?;
        }
        self.session_filesystems()?
            .insert(session_id, filesystem.clone());
        self.persisted_session_filesystems()?.remove(&session_id);
        Ok(filesystem)
    }

    fn persisted_filesystem_record(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<PersistedSessionFilesystem>> {
        if !self.persisted_session_filesystems()?.contains(&session_id) {
            return Ok(None);
        }
        let Some(record) = self
            .persistence
            .as_ref()
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("filesystem for session {session_id} has no persistence store"),
                    true,
                )
            })?
            .load_filesystem_record(session_id)?
        else {
            return Ok(None);
        };
        let mut payload = record.payload;
        let repositories = record.repositories;
        let directories = record.directories;
        payload["filesystem"]["checkpoints"] = json_value(record.checkpoints)?;
        let edits = record.edits;
        let changes = record.changes;
        let mut persisted: PersistedSessionFilesystem = from_json(payload)?;
        persisted.filesystem.edits = edits
            .into_iter()
            .map(|edit| loom_workspace::WorkspaceEditHistory {
                id: edit.id,
                path: edit.path,
                before: edit.before,
                before_bytes: edit.before_bytes,
                after_revision: edit.after_revision,
                source: edit.source,
            })
            .collect();
        persisted.filesystem.changes = changes;
        persisted.repositories = repositories;
        persisted.directories = directories;
        if record.session_id != session_id
            || persisted.filesystem.session_id != session_id
            || persisted.filesystem.root != record.root
            || persisted.filesystem.control != record.control
        {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted filesystem identity for session {session_id} is invalid"),
                false,
            ));
        }
        Ok(Some(persisted))
    }

    fn session_repositories(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, BTreeMap<RepositoryId, SessionRepository>>>>
    {
        self.session_repositories.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session repository manager lock was poisoned",
                true,
            )
        })
    }

    fn session_vcs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<(AgentSessionId, RepositoryId), GitService>>> {
        self.session_vcs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session Git service manager lock was poisoned",
                true,
            )
        })
    }

    fn session_task_supervisors(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, TaskSupervisor>>> {
        self.session_task_supervisors.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session task supervisor lock was poisoned",
                true,
            )
        })
    }

    fn session_terminals(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::TerminalId, AgentSessionId>>> {
        self.session_terminals.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session terminal lock was poisoned",
                true,
            )
        })
    }

    fn runs(&self) -> Result<MutexGuard<'_, BTreeMap<loom_core::RunId, Arc<RunHandle>>>> {
        self.runs
            .lock()
            .map_err(|_| LoomError::new(ErrorCode::Internal, "agent run lock was poisoned", true))
    }

    fn persisted_runs(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::RunId, PersistedRunSummary>>> {
        self.persisted_runs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "persisted run summary lock was poisoned",
                true,
            )
        })
    }

    fn journal(&self) -> Result<MutexGuard<'_, EventJournal>> {
        self.journal.lock().map_err(|_| {
            LoomError::new(ErrorCode::Internal, "event journal lock was poisoned", true)
        })
    }

    fn session_policies(&self) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, ApprovalPolicy>>> {
        self.session_policies.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session approval policy lock was poisoned",
                true,
            )
        })
    }

    fn auto_approve_actions(&self) -> Result<MutexGuard<'_, BTreeMap<AgentSessionId, bool>>> {
        self.auto_approve_actions.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session approval settings lock was poisoned",
                true,
            )
        })
    }

    fn workspace_configs(&self) -> Result<MutexGuard<'_, BTreeMap<WorkspaceId, WorkspaceConfig>>> {
        self.workspace_configs.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "workspace config lock was poisoned",
                true,
            )
        })
    }

    fn resource_monitor(&self) -> Result<MutexGuard<'_, ResourceMonitor>> {
        self.resource_monitor.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "worker resource monitor lock was poisoned",
                true,
            )
        })
    }

    fn set_workspace_config(
        self: &Arc<Self>,
        workspace_id: WorkspaceId,
        config: WorkspaceConfig,
    ) -> Result<()> {
        let unique_urls = config
            .worker_nodes
            .iter()
            .map(|node| node.url.as_str())
            .collect::<BTreeSet<_>>();
        if config.worker_nodes.len() > 64
            || unique_urls.len() != config.worker_nodes.len()
            || config
                .worker_nodes
                .iter()
                .any(|node| node.url.trim() != node.url || !worker_node_url_is_safe(&node.url))
            || !(loom_protocol::MIN_PROJECT_AGENT_CONCURRENCY
                ..=loom_protocol::MAX_PROJECT_AGENT_CONCURRENCY)
                .contains(&config.project_agent_concurrency)
        {
            return Err(LoomError::invalid_request(
                "workspace configuration must contain at most 64 safe worker-node URLs and project agent concurrency between 1 and 16",
            ));
        }
        let current_revision = self
            .workspace_configs()?
            .get(&workspace_id)
            .map(|config| config.revision);
        if current_revision.is_some_and(|revision| revision > config.revision) {
            return Ok(());
        }
        let previous_concurrency = self
            .workspace_configs()?
            .get(&workspace_id)
            .map(|config| config.project_agent_concurrency)
            .unwrap_or_else(|| WorkspaceConfig::default().project_agent_concurrency);
        let concurrency_changed = previous_concurrency != config.project_agent_concurrency;
        let revision = config.revision;
        let previous = self.workspace_configs()?.insert(workspace_id, config);
        let sequence = self
            .journal()?
            .append_workspace(workspace_id, WorkspaceEvent::ConfigChanged { revision });
        if let Err(error) = self.persist_state() {
            let mut configs = self.workspace_configs()?;
            if let Some(previous) = previous {
                configs.insert(workspace_id, previous);
            } else {
                configs.remove(&workspace_id);
            }
            self.journal()?.discard_pending_workspace(sequence);
            return Err(error);
        }
        if concurrency_changed {
            self.reconcile_project_tasks_and_resume_queued(false)?;
        }
        Ok(())
    }

    fn create_session_filesystem(
        &self,
        workspace_id: WorkspaceId,
        session_id: AgentSessionId,
    ) -> Result<Workspace> {
        let root = self
            .session_root_base
            .join(workspace_id.to_string())
            .join(session_id.to_string())
            .join("fs");
        fs::create_dir_all(&root).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not create session filesystem root: {error}"),
                false,
            )
        })?;
        Workspace::open(session_id, root)
    }

    fn restore_persisted(self: &Arc<Self>) -> Result<()> {
        let Some(persistence) = self.persistence.clone() else {
            return Ok(());
        };
        persistence.prune_expired_idempotency_records(Timestamp::from_unix_millis(
            current_unix_millis(),
        ))?;
        let startup_started = Instant::now();
        let mut needs_persist = false;
        let Some(sessions) = persistence.load_sessions()? else {
            let mut provider_configs = persistence.load_provider_configs()?;
            let migrated = self
                .providers
                .migrate_api_key_credentials(&mut provider_configs)?;
            self.providers.restore_configs(provider_configs)?;
            self.providers
                .restore_health(persistence.load_provider_health()?)?;
            self.providers
                .restore_usage(persistence.load_provider_usage()?)?;
            *self.workspace_configs()? = persistence.load_workspace_configs()?;
            if migrated {
                self.persist_state()?;
            }
            return Ok(());
        };
        let session_settings = persistence.load_session_settings()?;
        let state = PersistedBackendState {
            sessions,
            workspace_records: persistence.load_workspaces()?.unwrap_or_default(),
            journal: persistence
                .load_feed_header()?
                .map(|feed| EventJournal {
                    next_sequence: feed.next_sequence,
                    events: Vec::new(),
                    pending_events: Vec::new(),
                    workspace_events: Vec::new(),
                    pending_workspace_events: Vec::new(),
                    retention_limit: feed.retention_limit,
                })
                .unwrap_or_default(),
            session_policies: session_settings.approval_policies,
            auto_approve_actions: session_settings.auto_approve_actions,
            provider_configs: persistence.load_provider_configs()?,
            provider_health: persistence.load_provider_health()?,
            workspace_configs: persistence.load_workspace_configs()?,
            provider_usage: persistence.load_provider_usage()?,
            idempotency: persistence
                .load_idempotency_records()?
                .into_iter()
                .map(|(id, record)| {
                    Ok((
                        id,
                        IdempotencyRecord {
                            created_at: record.created_at,
                            expires_at: record.expires_at,
                            request: from_json(record.request)?,
                            response: from_json(record.response)?,
                        },
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?,
        };
        log::info!(
            "loaded persisted catalogs and feed cursors in {} ms",
            startup_started.elapsed().as_millis()
        );
        let sessions = SessionManager::from_state(state.sessions)?;
        {
            let mut target = self.sessions()?;
            *target = sessions;
        }
        {
            let mut target = self.journal()?;
            if state
                .journal
                .events
                .windows(2)
                .any(|events| events[0].sequence >= events[1].sequence)
                || state
                    .journal
                    .events
                    .last()
                    .is_some_and(|event| event.sequence != state.journal.next_sequence)
            {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "persisted event journal sequences are invalid",
                    false,
                ));
            }
            *target = state.journal;
        }
        self.idempotency_store.replace_records(state.idempotency)?;
        {
            let mut target = self.session_policies()?;
            *target = state.session_policies;
        }
        {
            let mut target = self.auto_approve_actions()?;
            *target = state.auto_approve_actions;
        }
        let mut provider_configs = state.provider_configs;
        if self
            .providers
            .migrate_api_key_credentials(&mut provider_configs)?
        {
            needs_persist = true;
        }
        self.providers.restore_configs(provider_configs)?;
        self.providers.restore_health(state.provider_health)?;
        self.providers.restore_usage(state.provider_usage)?;
        *self.workspace_configs()? = state.workspace_configs;

        *self.workspace_records()? =
            loom_session::WorkspaceManager::from_state(state.workspace_records)?;

        for session_id in persistence.list_filesystem_sessions()? {
            self.sessions()?.get(session_id)?;
            self.persisted_session_filesystems()?.insert(session_id);
        }

        let active_run_summaries = persistence.load_active_run_summaries()?;
        let active_run_count = active_run_summaries.len();
        let mut lazy_run_count = 0;
        let mut recovery_updates = BTreeMap::new();
        let mut run_summaries = BTreeMap::new();
        let mut restored_runs = BTreeMap::new();
        for (run_id, mut summary) in active_run_summaries {
            let mut snapshot = summary.snapshot.clone();
            let run_state = snapshot.state;
            let usage = summary.usage.clone();
            self.sessions()?.get(snapshot.session_id)?;
            run_summaries.insert(
                run_id,
                PersistedRunSummary {
                    snapshot: snapshot.clone(),
                    usage: usage.clone(),
                },
            );
            let mut execution_state = persistence.load_run_execution_state(run_id)?;
            let safely_deferred = run_can_be_deferred_during_restore(
                run_state,
                execution_state
                    .as_ref()
                    .map(|state| state.pending_tool_execution.is_some()),
                execution_state
                    .as_ref()
                    .map(|state| state.pending_project_join.is_some()),
            );
            if safely_deferred {
                if matches!(
                    run_state,
                    AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
                ) {
                    let updated_at = Timestamp::now();
                    snapshot.state = AgentRunState::Paused;
                    snapshot.updated_at = updated_at;
                    snapshot.completed_at = None;
                    let execution = execution_state.as_mut().ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::RecoveryRequired,
                            format!("persisted run {run_id} has no typed execution state"),
                            true,
                        )
                    })?;
                    execution.state = AgentRunState::Paused;
                    let mut attempts = persistence.load_run_attempts(run_id)?;
                    let attempt = attempts
                        .iter_mut()
                        .find(|attempt| attempt.id == snapshot.attempt_id)
                        .ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::RecoveryRequired,
                                format!("persisted run {run_id} has no current attempt"),
                                true,
                            )
                        })?;
                    attempt.state = AgentRunState::Paused;
                    attempt.completed_at = None;
                    summary.snapshot = snapshot.clone();
                    summary.attempts = Some(attempts);
                    summary.execution_state = execution_state;
                    summary.interactions = None;
                    recovery_updates.insert(run_id, summary);
                    run_summaries.insert(
                        run_id,
                        PersistedRunSummary {
                            snapshot: snapshot.clone(),
                            usage: usage.clone(),
                        },
                    );
                    self.append_recovery_events(
                        snapshot.session_id,
                        vec![AgentEvent::RunStateChanged {
                            run_id,
                            state: AgentRunState::Paused,
                        }],
                    )?;
                }
                lazy_run_count += 1;
                continue;
            }
            let runtime_config = persistence
                .load_run_runtime_config(run_id)?
                .ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::RecoveryRequired,
                        format!("persisted run {run_id} has no runtime configuration"),
                        true,
                    )
                })?;
            let mut runtime_state = runtime_state_from_durable_config(
                run_summaries.get(&run_id).ok_or_else(|| {
                    LoomError::new(
                        ErrorCode::RecoveryRequired,
                        format!("persisted run {run_id} summary is unavailable"),
                        true,
                    )
                })?,
                runtime_config,
            )?;
            let execution_state = execution_state.take().ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("persisted run {run_id} has no typed execution state"),
                    true,
                )
            })?;
            hydrate_runtime_execution_state(&mut runtime_state, execution_state)?;
            runtime_state.plan = persistence.load_run_plan(run_id)?;
            let durable_messages = persistence.load_run_messages(run_id)?;
            runtime_state.message_timeline_ordinals = durable_messages
                .iter()
                .map(|message| message.timeline_ordinal)
                .collect();
            runtime_state.messages = persisted_run_messages(durable_messages);
            hydrate_run_context_checkpoint(&persistence, run_id, &mut runtime_state)?;
            runtime_state.activities = persistence.load_run_activities(run_id)?;
            runtime_state.attempts = persistence.load_run_attempts(run_id)?;
            if runtime_state.attempts.is_empty() {
                return Err(LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!("persisted run {run_id} has no typed attempt history"),
                    true,
                ));
            }
            runtime_state.interactions = persistence.load_run_interactions(run_id)?;
            if runtime_state.run.id != run_id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted run key does not match runtime state {run_id}"),
                    false,
                ));
            }
            let session = self.sessions()?.get(runtime_state.session_id)?;
            let workspace = self.restore_session_filesystem(session.id)?;
            let mut recovery_reason = None;
            let provider =
                match self.provider_at(&runtime_state.task.model, runtime_state.provider_cursor) {
                    Ok(provider) => provider,
                    Err(error) => {
                        let descriptor = self
                            .providers
                            .describe_model(&runtime_state.task.model)
                            .unwrap_or_else(|_| ModelDescriptor {
                                id: runtime_state.task.model.clone(),
                                provider: ProviderId::new("recovered"),
                                display_name: "Unavailable persisted model".to_owned(),
                                context_window: None,
                                max_input_tokens: None,
                                max_output_tokens: None,
                                capabilities: ModelCapabilities::default(),
                            });
                        recovery_reason = Some(error.message.clone());
                        Box::new(UnavailableProvider::new(descriptor, error))
                    }
                };
            let tools = ToolExecutor::new_with_workspace(workspace)
                .with_github_token(self.providers.github_account_token().ok());
            let tools = self.with_project_agent_tools(
                tools,
                runtime_state.session_id,
                runtime_state.task.model.clone(),
                ProjectAgentToolGrants {
                    delegation: runtime_state.options.project_delegation_enabled,
                    messaging: runtime_state.options.project_messaging_enabled,
                    branch_messaging: runtime_state.options.project_branch_messaging_enabled,
                    inspection: runtime_state.options.project_inspection_enabled,
                    child_control: runtime_state.options.project_child_control_enabled,
                    worktree: runtime_state.options.project_worktree_enabled,
                    review: runtime_state.options.project_review_enabled,
                    integration: runtime_state.options.project_integration_enabled,
                },
            )?;
            let mut runtime = AgentRuntime::from_state(runtime_state, provider, tools)?;
            if runtime.session_id() != session.id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("persisted run {run_id} references a different session"),
                    false,
                ));
            }
            let mut recovery_events = runtime.recover_after_restart()?;
            if let Some(reason) = recovery_reason {
                recovery_events.push(AgentEvent::RecoveryRequired { run_id, reason });
            }
            let handle = self.register_runtime(runtime);
            if !recovery_events.is_empty()
                && let Some(summary) = run_summaries.get_mut(&run_id)
            {
                summary.snapshot = handle.state().run;
            }
            restored_runs.insert(run_id, handle);
            if !recovery_events.is_empty() {
                self.append_recovery_events(session.id, recovery_events)?;
                needs_persist = true;
            }
        }
        let restored_run_count = restored_runs.len();
        *self.persisted_runs()? = run_summaries;
        *self.runs()? = restored_runs;
        if needs_persist {
            self.persist_state_with_recovery_updates(&recovery_updates)?;
        } else if !recovery_updates.is_empty() {
            let mut journal = self.journal()?;
            let feed = DurableFeedState {
                next_sequence: journal.next_sequence,
                retention_limit: journal.retention_limit,
                events: journal.pending_events.clone(),
                workspace_events: journal.pending_workspace_events.clone(),
            };
            persistence.save_recovery_updates(&recovery_updates, &feed)?;
            journal.pending_events.clear();
        }
        // Finish durable cascade intents before the scheduler can reconcile or
        // admit any queued project work after restart.
        self.connect()
            .recover_pending_project_cancellation_cascades()?;
        self.reconcile_project_tasks_and_resume_queued(true)?;
        let lazy_filesystem_count = self.persisted_session_filesystems()?.len();
        log::info!(
            "indexed {} resumable runs, restored {} runtimes, deferred {} runtimes, and left {} filesystem services lazy in {} ms total",
            active_run_count,
            restored_run_count,
            lazy_run_count,
            lazy_filesystem_count,
            startup_started.elapsed().as_millis()
        );
        Ok(())
    }

    fn reconcile_project_tasks_and_resume_queued(
        self: &Arc<Self>,
        reconcile_persisted_runs: bool,
    ) -> Result<()> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        let sessions = self.sessions()?.list_in_workspace(None, true);
        let session_ids = sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        let workspace_ids = sessions
            .iter()
            .map(|session| session.workspace_id)
            .collect::<BTreeSet<_>>();
        let mut project_ids = BTreeSet::new();
        for session_id in session_ids {
            let project_id = ProjectId::from_uuid(*session_id.as_uuid());
            if persistence.load_project_snapshot(project_id)?.is_some() {
                project_ids.insert(project_id);
            }
        }
        let connection = self.connect();
        for project_id in project_ids {
            let mut project_tasks = persistence.list_project_tasks(project_id)?;
            for task in &mut project_tasks {
                if reconcile_persisted_runs {
                    let latest_run =
                        persistence.load_latest_run_summary_for_session(task.target_session_id)?;
                    let recovered_status = latest_run
                        .as_ref()
                        .map(|summary| delegated_task_status_for_run_state(summary.snapshot.state))
                        .or_else(|| {
                            (task.status == loom_core::DelegatedTaskStatus::Running)
                                .then_some(loom_core::DelegatedTaskStatus::Queued)
                        });
                    if let Some(status) = recovered_status
                        && status != task.status
                        && persistence.update_delegated_task_status(
                            task.task_id,
                            status,
                            Timestamp::now(),
                        )?
                    {
                        let Some(updated_task) = persistence.load_delegated_task(task.task_id)?
                        else {
                            return Err(LoomError::not_found("delegated task", task.task_id));
                        };
                        *task = updated_task;
                        let sequence = self.journal()?.next();
                        self.journal()?.append_event(ServerEventEnvelope {
                            protocol_version: CURRENT_PROTOCOL_VERSION,
                            sequence,
                            session_id: task.requester_session_id,
                            event: ServerEvent::ProjectTaskUpdated { task: task.clone() },
                        });
                    }
                }
            }

            let task_statuses = project_tasks
                .iter()
                .map(|task| (task.task_id, task.status))
                .collect::<Vec<_>>();
            for task in &mut project_tasks {
                let failed_dependency = task.dependencies.iter().any(|dependency| {
                    task_statuses.iter().any(|(task_id, status)| {
                        task_id == dependency
                            && matches!(
                                status,
                                loom_core::DelegatedTaskStatus::Failed
                                    | loom_core::DelegatedTaskStatus::Cancelled
                            )
                    })
                });
                if task.status == loom_core::DelegatedTaskStatus::Queued
                    && failed_dependency
                    && persistence.update_delegated_task_status_if_queued(
                        task.task_id,
                        loom_core::DelegatedTaskStatus::Blocked,
                        Timestamp::now(),
                    )?
                {
                    *task = persistence
                        .load_delegated_task(task.task_id)?
                        .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
                    let sequence = self.journal()?.next();
                    self.journal()?.append_event(ServerEventEnvelope {
                        protocol_version: CURRENT_PROTOCOL_VERSION,
                        sequence,
                        session_id: task.requester_session_id,
                        event: ServerEvent::ProjectTaskUpdated { task: task.clone() },
                    });
                }
            }
        }
        for workspace_id in workspace_ids {
            connection
                .drain_workspace_project_admissions(workspace_id, reconcile_persisted_runs)?;
        }
        Ok(())
    }

    fn persist_state(&self) -> Result<()> {
        self.persist_state_with_recovery_updates(&BTreeMap::new())
    }

    fn persist_state_with_idempotency_candidate(
        &self,
        candidate: (loom_core::RequestId, IdempotencyRecord),
    ) -> Result<()> {
        self.latch_on_persistence_error(self.persist_state_inner(&BTreeMap::new(), Some(candidate)))
    }

    /// Persists the current worker checkpoint without enumerating unrelated runs,
    /// catalogs, or session filesystems. The journal lock is retained through the
    /// transaction so only the captured event prefix can be acknowledged.
    fn persist_run_checkpoint(&self, handle: &RunHandle) -> Result<()> {
        self.ensure_persistence_healthy()?;
        let result = self.persist_run_checkpoint_inner(handle);
        self.latch_on_persistence_error(result)
    }

    fn persist_worker_state(&self) -> Result<()> {
        self.ensure_persistence_healthy()?;
        self.persist_state()
    }

    fn ensure_persistence_healthy(&self) -> Result<()> {
        if self.persistence_failed.load(Ordering::SeqCst) {
            Err(LoomError::new(
                ErrorCode::Persistence,
                "backend is unavailable after a durable state save failure; reopen it to recover",
                true,
            ))
        } else {
            Ok(())
        }
    }

    fn persist_run_checkpoint_inner(&self, handle: &RunHandle) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let _event_guard = handle
            .event_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        handle.flush_message_fragments(persistence)?;
        // Project only the checkpoint fields while holding the state lock.
        // This intentionally excludes the potentially large activities vector;
        // changed rows come from the ID-keyed queue below.
        let (
            session_id,
            summary,
            runtime_config,
            context_checkpoint,
            plan,
            message_delta,
            activities,
            project_manager_wait,
        ) = {
            let state = handle.locked_state();
            let summary = DurableRunSummary {
                snapshot: state.run.clone(),
                usage: state.usage.clone(),
                attempts: Some(state.attempts.clone()),
                execution_state: Some(execution_state_from_runtime(&state)?),
                interactions: Some(state.interactions.clone()),
            };
            let mut context_inspection = state.context_inspection.clone();
            if let Some(inspection) = &mut context_inspection {
                inspection.summary = None;
            }
            let runtime_config = DurableRunRuntimeConfig {
                system_instructions: state.task.system_instructions.clone(),
                repository_instructions: state.task.repository_instructions.clone(),
                approval_policy: state.approval_policy.clone(),
                limits: state.options.limits.clone(),
                context_options: state.options.context.clone(),
                checkpoint_id: state.options.checkpoint_id,
                input_cost_micros_per_1k: state.options.input_cost_micros_per_1k,
                output_cost_micros_per_1k: state.options.output_cost_micros_per_1k,
                context_inspection,
                project_delegation_enabled: state.options.project_delegation_enabled,
                project_messaging_enabled: state.options.project_messaging_enabled,
                project_inspection_enabled: state.options.project_inspection_enabled,
                project_child_control_enabled: state.options.project_child_control_enabled,
                project_worktree_enabled: state.options.project_worktree_enabled,
                project_review_enabled: state.options.project_review_enabled,
                project_integration_enabled: state.options.project_integration_enabled,
                project_branch_messaging_enabled: state.options.project_branch_messaging_enabled,
            };
            let context_checkpoint =
                state
                    .context_checkpoint
                    .clone()
                    .map(|summary| DurableRunContextCheckpoint {
                        session_id: state.session_id,
                        summary,
                    });
            let activities = handle
                .activity_deltas
                .lock()
                .map_err(|_| {
                    LoomError::new(
                        ErrorCode::Internal,
                        "activity delta lock was poisoned",
                        true,
                    )
                })?
                .ordered_values();
            let cursor = handle.message_checkpoint.lock().map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "message checkpoint cursor lock was poisoned",
                    true,
                )
            })?;
            let reset_messages = cursor.attempt_id != state.run.attempt_id
                || state.messages.len() < cursor.message_count;
            let start_ordinal = if reset_messages {
                0
            } else {
                cursor.message_count.saturating_sub(1)
            };
            let start_index = start_ordinal.min(state.messages.len());
            let message_delta = DurableRunMessageDelta {
                start_ordinal: start_ordinal as u64,
                reset: reset_messages,
                messages: durable_run_messages_from_runtime(
                    &state.messages[start_index..],
                    &state.message_timeline_ordinals[start_index..],
                )?,
            };
            let project_manager_wait = state
                .pending_project_join
                .as_ref()
                .map(|continuation| {
                    let arguments = serde_json::from_value::<WaitForProjectChildrenArguments>(
                        continuation.call.arguments.clone(),
                    )
                    .map_err(|error| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            format!("parked project join has invalid child task IDs: {error}"),
                            false,
                        )
                    })?;
                    let timestamp = Timestamp::now();
                    Ok(loom_core::ProjectManagerWaitRecord {
                        wait_id: loom_core::ProjectManagerWaitId::from_uuid(
                            *continuation.call.id.as_uuid(),
                        ),
                        run_id: state.run.id,
                        attempt_id: state.run.attempt_id,
                        tool_call_id: continuation.call.id,
                        manager_session_id: state.session_id,
                        child_task_ids: arguments.task_ids,
                        status: loom_core::ProjectManagerWaitStatus::Waiting,
                        result_summary: None,
                        created_at: timestamp,
                        updated_at: timestamp,
                    })
                })
                .transpose()?;
            (
                state.session_id,
                summary,
                runtime_config,
                context_checkpoint,
                state.plan.clone(),
                message_delta,
                activities,
                project_manager_wait,
            )
        };

        let (filesystem_record, filesystem_ack) = {
            let filesystem = self.session_filesystems()?.get(&session_id).cloned();
            if let Some(filesystem) = filesystem {
                if let Some(versioned) = filesystem.export_delta_if_dirty()? {
                    let workspace_delta = versioned.delta;
                    let filesystem_state = versioned.state;
                    let checkpoints = workspace_delta.checkpoints;
                    let edits = workspace_delta
                        .edits
                        .into_iter()
                        .map(|edit| DurableFilesystemEdit {
                            id: edit.id,
                            path: edit.path,
                            before: edit.before,
                            before_bytes: edit.before_bytes,
                            after_revision: edit.after_revision,
                            source: edit.source,
                        })
                        .collect();
                    let changes = workspace_delta.changes;
                    let deleted_checkpoints = workspace_delta.deleted_checkpoints;
                    let deleted_edits = workspace_delta.deleted_edits;
                    let deleted_changes = workspace_delta.deleted_changes;
                    let directories = filesystem
                        .mounted_directories()?
                        .into_iter()
                        .map(|(path, source)| SessionDirectory {
                            path,
                            source: source.display().to_string(),
                        })
                        .collect::<Vec<_>>();
                    let repositories = self
                        .session_repositories()?
                        .get(&session_id)
                        .cloned()
                        .unwrap_or_default();
                    let persisted = PersistedSessionFilesystem {
                        filesystem: filesystem_state,
                        repositories: repositories.clone(),
                        directories: directories.clone(),
                    };
                    (
                        Some(DurableFilesystemRecord {
                            session_id,
                            root: persisted.filesystem.root.clone(),
                            control: persisted.filesystem.control,
                            checkpoints,
                            edits,
                            changes,
                            repositories,
                            directories,
                            payload: json_value(persisted)?,
                            delta: Some(DurableFilesystemDelta {
                                deleted_checkpoints,
                                deleted_edits,
                                deleted_changes,
                            }),
                        }),
                        Some((filesystem, versioned.generation)),
                    )
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        };

        let mut journal = self.journal()?;
        let (feed, captured_event_sequences) = journal.capture_session_feed(session_id);
        // Full retention ranking scans the retained feed, so amortize it until
        // at least 64 new global event sequences or 4 MiB of pending payloads
        // have arrived. The byte threshold prevents large event bodies from
        // overshooting the total feed retention budget between pruning passes.
        let sequence = feed.next_sequence.value();
        let last_pruned = self.last_feed_pruned_sequence.load(Ordering::Relaxed);
        let pending_feed_bytes = serde_json::to_vec(&(&feed.events, &feed.workspace_events))
            .map_err(|error| {
                LoomError::new(
                    ErrorCode::Persistence,
                    format!("could not size pending event feed: {error}"),
                    false,
                )
            })?
            .len();
        let prior_feed_bytes = self.feed_bytes_since_prune.load(Ordering::Relaxed);
        let accumulated_feed_bytes = prior_feed_bytes.saturating_add(pending_feed_bytes);
        let prune_feed =
            should_prune_worker_feed(sequence, last_pruned, prior_feed_bytes, pending_feed_bytes);
        let session = self.sessions()?.get(session_id)?;
        let session_next_sequence = self.sessions()?.next_sequence();
        let checkpoint = DurableRunCheckpointWrite {
            session: &session,
            session_next_sequence,
            prune_feed,
            summary: &summary,
            runtime_config: &runtime_config,
            context_checkpoint: context_checkpoint.as_ref(),
            plan: &plan,
            messages: &[],
            message_delta: Some(&message_delta),
            activities: &[],
            activity_deltas: Some(&activities),
            filesystem: filesystem_record.as_ref(),
            feed: &feed,
        };
        let checkpoint_result = match project_manager_wait.as_ref() {
            Some(wait) => {
                persistence.save_run_checkpoint_with_project_manager_wait(checkpoint, wait)
            }
            None => persistence.save_run_checkpoint(checkpoint),
        };
        if let Err(error) = checkpoint_result {
            self.persistence_failed.store(true, Ordering::SeqCst);
            return Err(error);
        }
        if let Some((filesystem, generation)) = filesystem_ack {
            filesystem.acknowledge_persisted_generation(generation)?;
        }
        if prune_feed {
            self.last_feed_pruned_sequence
                .store(sequence, Ordering::Relaxed);
            self.feed_bytes_since_prune.store(0, Ordering::Relaxed);
        } else {
            self.feed_bytes_since_prune
                .store(accumulated_feed_bytes, Ordering::Relaxed);
        }
        {
            let mut cursor = handle
                .message_checkpoint
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            cursor.attempt_id = summary.snapshot.attempt_id;
            cursor.message_count = usize::try_from(message_delta.start_ordinal)
                .unwrap_or(usize::MAX)
                .saturating_add(message_delta.messages.len());
        }
        if !activities.is_empty() {
            let mut pending = handle
                .activity_deltas
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for activity in &activities {
                if pending.by_id.get(&activity.id) == Some(activity) {
                    pending.by_id.remove(&activity.id);
                }
            }
            let still_pending = pending.by_id.keys().copied().collect::<BTreeSet<_>>();
            pending
                .appended_order
                .retain(|activity_id| still_pending.contains(activity_id));
        }
        journal.acknowledge_session_feed(&captured_event_sequences);
        Ok(())
    }

    fn persist_state_with_recovery_updates(
        &self,
        recovery_updates: &BTreeMap<loom_core::RunId, DurableRunSummary>,
    ) -> Result<()> {
        self.latch_on_persistence_error(self.persist_state_inner(recovery_updates, None))
    }

    fn latch_on_persistence_error<T>(&self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.persistence_failed.store(true, Ordering::SeqCst);
        }
        result
    }

    fn persist_state_inner(
        &self,
        recovery_updates: &BTreeMap<loom_core::RunId, DurableRunSummary>,
        idempotency_candidate: Option<(loom_core::RequestId, IdempotencyRecord)>,
    ) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let _state_persist_guard = self.state_persist_gate.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "state persistence lock was poisoned",
                true,
            )
        })?;
        let handles = self.runs()?.values().cloned().collect::<Vec<_>>();
        for handle in handles {
            handle.flush_message_fragments(persistence)?;
        }
        let mut runs: BTreeMap<loom_core::RunId, AgentRuntimeState> = self
            .runs()?
            .iter()
            .map(|(run_id, handle)| (*run_id, handle.state()))
            .collect();
        let durable_run_plans = runs
            .iter()
            .map(|(run_id, state)| (*run_id, state.plan.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut durable_run_summaries: BTreeMap<loom_core::RunId, DurableRunSummary> = self
            .persisted_runs()?
            .iter()
            .map(|(run_id, summary)| {
                (
                    *run_id,
                    DurableRunSummary {
                        snapshot: summary.snapshot.clone(),
                        usage: summary.usage.clone(),
                        attempts: None,
                        execution_state: None,
                        interactions: None,
                    },
                )
            })
            .collect();
        for (run_id, state) in &runs {
            durable_run_summaries.insert(
                *run_id,
                DurableRunSummary {
                    snapshot: state.run.clone(),
                    usage: state.usage.clone(),
                    attempts: Some(state.attempts.clone()),
                    execution_state: Some(execution_state_from_runtime(state)?),
                    interactions: Some(state.interactions.clone()),
                },
            );
        }
        durable_run_summaries.extend(
            recovery_updates
                .iter()
                .map(|(run_id, summary)| (*run_id, summary.clone())),
        );
        let durable_run_context_checkpoints =
            runs.iter()
                .map(|(run_id, state)| {
                    (
                        *run_id,
                        state.context_checkpoint.clone().map(|summary| {
                            DurableRunContextCheckpoint {
                                session_id: state.session_id,
                                summary,
                            }
                        }),
                    )
                })
                .collect::<BTreeMap<_, _>>();
        let durable_run_runtime_configs = runs
            .iter()
            .map(|(run_id, state)| {
                let mut context_inspection = state.context_inspection.clone();
                if let Some(inspection) = &mut context_inspection {
                    inspection.summary = None;
                }
                Ok((
                    *run_id,
                    DurableRunRuntimeConfig {
                        system_instructions: state.task.system_instructions.clone(),
                        repository_instructions: state.task.repository_instructions.clone(),
                        approval_policy: state.approval_policy.clone(),
                        limits: state.options.limits.clone(),
                        context_options: state.options.context.clone(),
                        checkpoint_id: state.options.checkpoint_id,
                        input_cost_micros_per_1k: state.options.input_cost_micros_per_1k,
                        output_cost_micros_per_1k: state.options.output_cost_micros_per_1k,
                        context_inspection,
                        project_delegation_enabled: state.options.project_delegation_enabled,
                        project_messaging_enabled: state.options.project_messaging_enabled,
                        project_inspection_enabled: state.options.project_inspection_enabled,
                        project_child_control_enabled: state.options.project_child_control_enabled,
                        project_worktree_enabled: state.options.project_worktree_enabled,
                        project_review_enabled: state.options.project_review_enabled,
                        project_integration_enabled: state.options.project_integration_enabled,
                        project_branch_messaging_enabled: state
                            .options
                            .project_branch_messaging_enabled,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut durable_run_messages = BTreeMap::new();
        let mut durable_run_activities = BTreeMap::new();
        for (run_id, state) in &mut runs {
            durable_run_messages.insert(
                *run_id,
                durable_run_messages_from_runtime(
                    &state.messages,
                    &state.message_timeline_ordinals,
                )?,
            );
            durable_run_activities.insert(*run_id, std::mem::take(&mut state.activities));
        }
        let loaded_repositories = self.session_repositories()?.clone();
        let mut filesystem_records = Vec::new();
        let mut filesystem_generations = Vec::new();
        for (session_id, filesystem) in self.session_filesystems()?.iter() {
            let Some(versioned) = filesystem.export_delta_if_dirty()? else {
                continue;
            };
            let workspace_delta = versioned.delta;
            let filesystem_state = versioned.state;
            let checkpoints = workspace_delta.checkpoints;
            let edits = workspace_delta
                .edits
                .into_iter()
                .map(|edit| DurableFilesystemEdit {
                    id: edit.id,
                    path: edit.path,
                    before: edit.before,
                    before_bytes: edit.before_bytes,
                    after_revision: edit.after_revision,
                    source: edit.source,
                })
                .collect();
            let changes = workspace_delta.changes;
            let deleted_checkpoints = workspace_delta.deleted_checkpoints;
            let deleted_edits = workspace_delta.deleted_edits;
            let deleted_changes = workspace_delta.deleted_changes;
            let directories = filesystem
                .mounted_directories()?
                .into_iter()
                .map(|(path, source)| SessionDirectory {
                    path,
                    source: source.display().to_string(),
                })
                .collect::<Vec<_>>();
            let repositories = loaded_repositories
                .get(session_id)
                .cloned()
                .unwrap_or_default();
            let persisted = PersistedSessionFilesystem {
                filesystem: filesystem_state,
                repositories: repositories.clone(),
                directories: directories.clone(),
            };
            filesystem_records.push(DurableFilesystemRecord {
                session_id: *session_id,
                root: persisted.filesystem.root.clone(),
                control: persisted.filesystem.control,
                checkpoints,
                edits,
                changes,
                repositories,
                directories,
                payload: json_value(persisted)?,
                delta: Some(DurableFilesystemDelta {
                    deleted_checkpoints,
                    deleted_edits,
                    deleted_changes,
                }),
            });
            filesystem_generations.push((filesystem.clone(), versioned.generation));
        }
        let sessions = self.sessions()?.export_state();
        let mut journal = self.journal()?;
        let feed = DurableFeedState {
            next_sequence: journal.next_sequence,
            retention_limit: journal.retention_limit,
            events: journal.pending_events.clone(),
            workspace_events: journal.pending_workspace_events.clone(),
        };
        let session_settings = DurableSessionSettings {
            approval_policies: self.session_policies()?.clone(),
            auto_approve_actions: self.auto_approve_actions()?.clone(),
        };
        let workspace_configs = self.workspace_configs()?.clone();
        let provider_state = DurableProviderState {
            configs: self
                .providers
                .export_configs()?
                .into_iter()
                .map(|config| (config.id.clone(), config))
                .collect(),
            health: self.providers.export_health()?,
        };
        let workspace_records = self.workspace_records()?.export_state();
        let provider_usage = self.providers.usage()?;
        let idempotency = self
            .idempotency_store
            .durable_records(idempotency_candidate.as_ref())?;
        #[cfg(test)]
        if self.fail_next_state_save.swap(false, Ordering::SeqCst) {
            self.persistence_failed.store(true, Ordering::SeqCst);
            return Err(LoomError::new(
                ErrorCode::Internal,
                "injected durable state save failure",
                true,
            ));
        }
        let result = persistence.save_state(DurableStateWrite {
            sessions: &sessions,
            workspaces: Some(&workspace_records),
            settings: Some(&session_settings),
            workspace_configs: Some(&workspace_configs),
            providers: Some(&provider_state),
            usage: Some(&provider_usage),
            idempotency: Some(&idempotency),
            run_summaries: Some(&durable_run_summaries),
            run_runtime_configs: Some(&durable_run_runtime_configs),
            run_context_checkpoints: Some(&durable_run_context_checkpoints),
            run_plans: Some(&durable_run_plans),
            run_messages: Some(&durable_run_messages),
            run_activities: Some(&durable_run_activities),
            filesystem_records: Some(&filesystem_records),
            feed: Some(&feed),
        });
        if result.is_err() {
            self.persistence_failed.store(true, Ordering::SeqCst);
        }
        if result.is_ok() {
            for (filesystem, generation) in filesystem_generations {
                filesystem.acknowledge_persisted_generation(generation)?;
            }
            journal.pending_events.clear();
            journal.pending_workspace_events.clear();
            self.last_feed_pruned_sequence
                .store(feed.next_sequence.value(), Ordering::Relaxed);
            self.feed_bytes_since_prune.store(0, Ordering::Relaxed);
        }
        result
    }

    pub fn flush(&self) -> Result<()> {
        if *self.request_lifecycle.read().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "backend request lifecycle lock was poisoned",
                true,
            )
        })? != 0
        {
            return Err(LoomError::conflict("backend is shutting down"));
        }
        self.persist_state()
    }

    /// Stops active run workers, persists their paused continuation state,
    /// joins all worker threads, and releases exclusive database ownership.
    /// Requests through existing connections are rejected after shutdown.
    pub fn shutdown(&self) -> Result<()> {
        let mut shutting_down = self.request_lifecycle.write().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "backend request lifecycle lock was poisoned",
                true,
            )
        })?;
        if *shutting_down == 2 {
            return Ok(());
        }
        *shutting_down = 1;
        let handles = self
            .runs()?
            .values()
            .cloned()
            .collect::<Vec<Arc<RunHandle>>>();
        for handle in handles {
            if handle.is_running() {
                handle.control.request_pause();
                handle.wait_until_idle()?;
                if let Some(error) = handle.take_failure() {
                    return Err(error);
                }
                if handle.control.is_stopping() {
                    let state = handle.state().run.state;
                    // Only active runs need to be parked. Awaiting approval and
                    // waiting for input are already durable, resumable stops and
                    // must not be downgraded to Paused by shutdown.
                    if matches!(
                        state,
                        AgentRunState::Planning
                            | AgentRunState::Executing
                            | AgentRunState::Evaluating
                    ) {
                        let mut runtime = handle.try_runtime()?;
                        runtime.pause()?;
                        handle.refresh(&runtime);
                    }
                    handle.control.clear_request();
                }
            }
            handle.join_worker()?;
        }
        // A fail-stopped backend must not write more state, but shutdown still
        // has to release database ownership so a restart can reopen it.
        let persist_result = if self.persistence_failed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            self.persist_state()
        };
        if let Some(persistence) = self.persistence.as_ref() {
            persistence.release_exclusive_writer()?;
        }
        *shutting_down = 2;
        persist_result
    }
    fn append_recovery_events(
        self: &Arc<Self>,
        session_id: AgentSessionId,
        events: Vec<AgentEvent>,
    ) -> Result<()> {
        for event in events {
            let state = session_state_for_event(&event);
            self.journal()?.append_agent(session_id, event);
            if let Some(state) = state {
                let current = self.sessions()?.get(session_id)?.state;
                if current != state {
                    let (_, record) = self.sessions()?.transition(session_id, state)?;
                    self.journal()?.append_session(record);
                }
            }
        }
        Ok(())
    }

    /// Journals one agent event and keeps the session state in step with it.
    fn record_agent_event(
        self: &Arc<Self>,
        session_id: AgentSessionId,
        event: AgentEvent,
    ) -> Result<()> {
        let state = session_state_for_event(&event);
        self.journal()?.append_agent(session_id, event);
        if let Some(state) = state {
            let current = self.sessions()?.get(session_id)?.state;
            if current != state {
                let (_, record) = self.sessions()?.transition(session_id, state)?;
                self.journal()?.append_session(record);
            }
        }
        Ok(())
    }

    fn update_project_task_for_session_state(
        &self,
        session_id: AgentSessionId,
        state: AgentSessionState,
    ) -> Result<()> {
        let Some(next_status) = delegated_task_status_for_session_state(state) else {
            return Ok(());
        };
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        let Some(task) = persistence.load_delegated_task_for_target(session_id)? else {
            return Ok(());
        };
        if !persistence.update_delegated_task_status(task.task_id, next_status, Timestamp::now())? {
            return Ok(());
        }
        let task = persistence
            .load_delegated_task(task.task_id)?
            .ok_or_else(|| LoomError::not_found("delegated task", task.task_id))?;
        let sequence = self.journal()?.next();
        self.journal()?.append_event(ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id: task.requester_session_id,
            event: ServerEvent::ProjectTaskUpdated { task },
        });
        Ok(())
    }

    fn after_run_checkpoint(self: &Arc<Self>, handle: &RunHandle) -> Result<()> {
        let state = handle.state().run.state;
        let session_id = handle.session_id;
        let session_state = session_state_for_run_state(state);
        if !project_agent_slot_released(session_state) {
            return Ok(());
        }
        self.update_project_task_for_session_state(session_id, session_state)?;
        self.reconcile_project_tasks_and_resume_queued(false)
    }

    /// Observer installed on every runtime so events are journaled as they are
    /// produced rather than after the run finishes.
    fn run_observer(
        self: &Arc<Self>,
        handle: Weak<RunHandle>,
        session_id: AgentSessionId,
    ) -> AgentEventObserver {
        let backend = Arc::downgrade(self);
        Arc::new(move |event: &AgentEvent| {
            let Some(backend) = backend.upgrade() else {
                return;
            };
            let handle = handle.upgrade();
            // Keep journal append and cached-state/dirty-activity application
            // indivisible relative to a durable worker checkpoint.
            let _event_guard = handle.as_ref().map(|handle| {
                handle
                    .event_gate
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            });
            let fragment_result = match (event, backend.persistence.as_ref(), handle.as_ref()) {
                (
                    AgentEvent::AssistantMessageDelta { text, .. },
                    Some(persistence),
                    Some(handle),
                ) => handle.append_message_delta(persistence, text),
                (AgentEvent::RunCompleted { .. }, Some(persistence), Some(handle)) => {
                    handle.flush_message_fragments(persistence)
                }
                _ => Ok(()),
            };
            let recorded = fragment_result
                .and_then(|()| backend.record_agent_event(session_id, event.clone()));
            if let Some(handle) = handle.as_ref() {
                handle.apply_event(event);
                if let Err(error) = recorded {
                    handle.record_failure(error);
                }
            }
        })
    }

    /// Wraps a runtime in a handle and attaches the journaling observer.
    fn register_runtime(self: &Arc<Self>, mut runtime: AgentRuntime) -> Arc<RunHandle> {
        let session_id = runtime.session_id();
        Arc::new_cyclic(|weak: &Weak<RunHandle>| {
            runtime.set_event_observer(self.run_observer(weak.clone(), session_id));
            RunHandle::new(runtime)
        })
    }

    /// Drives a registered run on its own worker so the request handler returns
    /// as soon as the run is registered.
    fn spawn_run_worker(self: &Arc<Self>, handle: Arc<RunHandle>) -> Result<()> {
        handle.join_worker()?;
        handle.set_running(true);
        let backend = Arc::clone(self);
        let worker_handle = Arc::clone(&handle);
        let run_id = handle.run_id;
        let worker = thread::Builder::new()
            .name(format!("loom-run-{run_id}"))
            .spawn(move || {
                let fragment_flusher = backend.persistence.clone().map(|persistence| {
                    let handle = Arc::downgrade(&handle);
                    thread::spawn(move || {
                        RunHandle::flush_message_fragments_until_stopped(handle, persistence)
                    })
                });
                loop {
                    let delivered = if let Some(persistence) = backend.persistence.as_ref() {
                        let mut runtime = handle
                            .runtime
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        match deliver_project_agent_messages(persistence, &mut runtime) {
                            Ok(delivered) => {
                                if delivered {
                                    handle.refresh(&runtime);
                                }
                                delivered
                            }
                            Err(error) => {
                                handle.record_failure(error);
                                false
                            }
                        }
                    } else {
                        false
                    };
                    if handle.failure().is_some() {
                        break;
                    }
                    if delivered && let Err(error) = backend.persist_run_checkpoint(&handle) {
                        handle.record_failure(error);
                        break;
                    }
                    let progress = {
                        let mut runtime = handle
                            .runtime
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let progress = runtime.run_step();
                        handle.refresh(&runtime);
                        progress
                    };
                    if handle.failure().is_some() {
                        break;
                    }
                    match progress {
                        Ok(progress) => {
                            if let Some(persistence) = backend.persistence.as_ref()
                                && let Err(error) = handle.flush_message_fragments(persistence)
                            {
                                handle.record_failure(error);
                                break;
                            }
                            if let Err(error) = backend.persist_run_checkpoint(&handle) {
                                handle.record_failure(error);
                                break;
                            }
                            if let Err(error) = backend.after_run_checkpoint(&handle) {
                                handle.record_failure(error);
                                break;
                            }
                            if !progress.continues {
                                break;
                            }
                        }
                        Err(error) => {
                            let flush_error =
                                backend.persistence.as_ref().and_then(|persistence| {
                                    handle.flush_message_fragments(persistence).err()
                                });
                            handle.record_failure(flush_error.unwrap_or(error));
                            break;
                        }
                    }
                }
                if let Err(error) = backend.persist_worker_state() {
                    handle.record_failure(error);
                }
                handle.set_running(false);
                if fragment_flusher.is_some_and(|flusher| flusher.join().is_err()) {
                    log::error!("run message fragment flusher thread panicked");
                }
            })
            .map_err(|error| {
                worker_handle.set_running(false);
                LoomError::new(
                    ErrorCode::Internal,
                    format!("could not start worker for run {run_id}: {error}"),
                    true,
                )
            })?;
        *worker_handle
            .worker
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(worker);
        Ok(())
    }

    pub fn set_event_retention(&self, limit: usize) -> Result<()> {
        if limit == 0 {
            return Err(LoomError::invalid_request(
                "event retention limit must be greater than zero",
            ));
        }
        self.journal()?.set_retention(limit);
        Ok(())
    }

    fn project_agent_tools(
        &self,
        session_id: AgentSessionId,
        model_id: ModelId,
        grants: ProjectAgentToolGrants,
    ) -> Result<Option<Arc<dyn ToolExtension>>> {
        let Some(persistence) = &self.persistence else {
            return Ok(None);
        };
        let Some(project) = persistence.load_project_snapshot_for_session(session_id)? else {
            return Ok(None);
        };
        if !project
            .agents
            .iter()
            .any(|agent| agent.session_id == session_id)
        {
            return Ok(None);
        }
        let can_delegate = grants.delegation
            && self
                .supported_capabilities
                .contains(Capability::CreateProjectChild);
        let can_delegate_code = can_delegate
            && grants.worktree
            && self
                .supported_capabilities
                .contains(Capability::CreateProjectWorktree);
        let can_message = grants.messaging
            && self
                .supported_capabilities
                .contains(Capability::SendProjectAgentMessage);
        let can_branch_message = grants.branch_messaging
            && self
                .supported_capabilities
                .contains(Capability::SendProjectBranchMessage);
        let can_inspect_children = grants.inspection
            && self
                .supported_capabilities
                .contains(Capability::ReadProject);
        let can_wait_children = grants.delegation
            && can_inspect_children
            && self
                .supported_capabilities
                .contains(Capability::CreateProjectChild);
        let can_control_children = grants.child_control
            && self
                .supported_capabilities
                .contains(Capability::ControlProjectChild);
        let can_review_children = grants.review
            && self
                .supported_capabilities
                .contains(Capability::ReadProjectChildReview);
        let can_integrate_children = grants.integration
            && self
                .supported_capabilities
                .contains(Capability::IntegrateProjectChild);
        let backend = self
            .self_reference
            .lock()
            .map_err(|_| {
                LoomError::new(
                    ErrorCode::Internal,
                    "backend self reference lock was poisoned",
                    true,
                )
            })?
            .clone();
        if backend.strong_count() == 0 {
            return Err(LoomError::new(
                ErrorCode::Internal,
                "project agent tools require a registered backend",
                true,
            ));
        }
        Ok(Some(Arc::new(ProjectAgentTools {
            backend,
            session_id,
            project_id: project.project_id,
            model_id,
            can_delegate,
            can_delegate_code,
            can_message,
            can_branch_message,
            can_inspect_children,
            can_wait_children,
            can_control_children,
            can_review_children,
            can_integrate_children,
        })))
    }

    fn with_project_agent_tools(
        &self,
        tools: ToolExecutor,
        session_id: AgentSessionId,
        model_id: ModelId,
        grants: ProjectAgentToolGrants,
    ) -> Result<ToolExecutor> {
        Ok(
            match self.project_agent_tools(session_id, model_id, grants)? {
                Some(extension) => tools.with_extension(extension),
                None => tools,
            },
        )
    }

    fn accept_project_agent_message(
        &self,
        request_id: RequestId,
        trusted_sender_session_id: AgentSessionId,
        sender_can_message: bool,
        sender_can_branch_message: bool,
        mut draft: loom_core::AgentMessageDraft,
    ) -> Result<ServerResponse> {
        if draft.body.trim().is_empty() || draft.body.len() > 16 * 1024 {
            return Err(LoomError::invalid_request(
                "agent message body must contain 1 to 16384 bytes",
            ));
        }
        let persistence = self.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "agent messaging requires durable storage",
                false,
            )
        })?;
        let project = persistence
            .load_project_snapshot(draft.project_id)?
            .ok_or_else(|| LoomError::not_found("project", draft.project_id))?;
        draft.sender_session_id = trusted_sender_session_id;
        let sender = project
            .agents
            .iter()
            .find(|agent| agent.session_id == trusted_sender_session_id)
            .ok_or_else(|| LoomError::invalid_request("message sender is not a project member"))?;
        let target = project
            .agents
            .iter()
            .find(|agent| agent.session_id == draft.target_session_id)
            .ok_or_else(|| LoomError::invalid_request("message target is not a project member"))?;
        let is_direct_route = sender.parent_session_id == Some(target.session_id)
            || target.parent_session_id == Some(sender.session_id);
        if let Some(task_id) = draft.task_id {
            let context_task = persistence
                .load_delegated_task(task_id)?
                .ok_or_else(|| LoomError::not_found("delegated task", task_id))?;
            if context_task.project_id != draft.project_id {
                return Err(LoomError::invalid_request(
                    "message task context must belong to the sender's project",
                ));
            }
        }
        let target_task = persistence.load_delegated_task_for_target(draft.target_session_id)?;
        let branch_route = !is_direct_route;
        if is_direct_route && !sender_can_message {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "direct project messaging is not granted to this run",
                false,
            ));
        }
        if branch_route
            && (!sender_can_branch_message
                || !project_member_branch_messaging_enabled(
                    persistence,
                    project.root_session_id,
                    target.session_id,
                )?)
        {
            return Err(LoomError::new(
                ErrorCode::AuthorizationDenied,
                "branch messages require explicit sender and recipient grants",
                false,
            ));
        }
        if persistence
            .load_agent_message_by_request(request_id)?
            .is_some()
        {
            let message = persistence.accept_agent_message(request_id, &draft)?;
            return Ok(ServerResponse::ProjectAgentMessageAccepted(message));
        }
        if matches!(
            target.state,
            AgentSessionState::Completed
                | AgentSessionState::Failed
                | AgentSessionState::Cancelled
                | AgentSessionState::Archived
        ) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "cannot message a terminal project agent that has no resume path",
                false,
            ));
        }
        if target_task.as_ref().is_some_and(|task| {
            matches!(
                task.status,
                loom_core::DelegatedTaskStatus::Completed
                    | loom_core::DelegatedTaskStatus::Failed
                    | loom_core::DelegatedTaskStatus::Cancelled
            )
        }) {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "cannot message a terminal delegated task that has no resume path",
                false,
            ));
        }
        let message = persistence.accept_agent_message(request_id, &draft)?;
        let sequence = self.journal()?.next();
        self.journal()?.append_event(ServerEventEnvelope {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            sequence,
            session_id: draft.target_session_id,
            event: ServerEvent::ProjectAgentMessageAccepted {
                message: message.clone(),
            },
        });
        if !branch_route && draft.target_session_id != project.root_session_id {
            let sequence = self.journal()?.next();
            self.journal()?.append_event(ServerEventEnvelope {
                protocol_version: CURRENT_PROTOCOL_VERSION,
                sequence,
                session_id: project.root_session_id,
                event: ServerEvent::ProjectAgentMessageAccepted {
                    message: message.clone(),
                },
            });
        }
        Ok(ServerResponse::ProjectAgentMessageAccepted(message))
    }

    pub fn event_retention(&self) -> Result<usize> {
        Ok(self.journal()?.retention_limit.max(1))
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
