use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock, Weak},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use loom_agent::{
    AgentEvent, AgentEventObserver, AgentRunSnapshot, AgentRunState, AgentRuntime,
    AgentRuntimeOptions, AgentRuntimeState, AgentTask, RunControl, RunProgress,
};
use loom_core::{
    AgentSessionId, AgentSessionSnapshot, AgentSessionState, ApprovalPolicy, Capability,
    CapabilitySet, ErrorCode, EventSequence, LoomError, ProtocolVersion, RepositoryId, RequestId,
    Result, SessionEventRecord, Timestamp, UsageSnapshot, WorkspaceId, WorkspaceRecord,
};
use loom_model::{ModelCapabilities, ModelDescriptor, ModelId, ModelMessage, ProviderId};
use loom_persistence::{
    CURRENT_SCHEMA_VERSION, DurableFeedSessionCursor, DurableFeedState, DurableFeedWorkspaceCursor,
    DurableFilesystemEdit, DurableFilesystemRecord, DurableIdempotencyRecord, DurableProviderState,
    DurableRunContextCheckpoint, DurableRunMessage, DurableRunRuntimeConfig, DurableRunSummary,
    DurableSessionProjectionRead, DurableSessionSettings, DurableStateWrite, FilePersistence,
};
use loom_process::{TaskSupervisor, TerminalManager};
use loom_protocol::{
    AgentExecutionStateRecord, AgentRunMessageHeader, AgentRunSnapshotProjection,
    AgentRunTranscriptMessage, AgentSessionInitialState, AgentSessionSnapshotProjection,
    CURRENT_PROTOCOL_VERSION, ClientRequest, GitHubCopilotLoginStatus, GitHubRepository,
    MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES, MAX_AGENT_RUN_MESSAGE_PAGE_SIZE,
    MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES, MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE, NegotiationResult,
    RequestEnvelope, ResponseEnvelope, ServerEventEnvelope, ServerResponse, SessionDirectory,
    SessionFilesystemChange, SessionFilesystemFile, SessionFilesystemSnapshot, SessionRepository,
    WorkerNodeResources, WorkerNodeStatus, WorkspaceConfig, unsupported_version_error,
};
use loom_providers::{
    CredentialRef, FileCredentialStore, GITHUB_COPILOT_CREDENTIAL_REF, GitHubCopilotAuthenticator,
    ModelProvider, ProviderConfig, ProviderHealth, ProviderRegistry, UnavailableProvider,
    UsageLedger, deterministic_descriptor,
};
use loom_session::SessionManager;
use loom_tools::ToolExecutor;
use loom_vcs::GitService;
use loom_workspace::Workspace;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sysinfo::System;

mod auth;
mod remote;

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

fn validate_retry_horizon(request_id: RequestId, now_ms: u64) -> Result<()> {
    let Some(issued_at_ms) = request_id.issued_at_unix_millis() else {
        // UUIDv4 IDs were used by earlier protocol clients. Keep their bounded
        // count-based cache behavior while new clients use timestamped UUIDv7.
        return Ok(());
    };
    let future_skew_ms = REQUEST_ID_FUTURE_SKEW.as_millis() as u64;
    if issued_at_ms > now_ms.saturating_add(future_skew_ms) {
        return Err(LoomError::invalid_request(
            "request id issue time is too far in the future",
        ));
    }
    if now_ms.saturating_sub(issued_at_ms) > IDEMPOTENCY_RETENTION.as_millis() as u64 {
        return Err(LoomError::new(
            ErrorCode::DeadlineExceeded,
            "retry horizon expired; submit the operation as a new request",
            false,
        ));
    }
    Ok(())
}

fn trim_idempotency_cache(cache: &mut BTreeMap<RequestId, IdempotencyRecord>) {
    let now = current_unix_millis();
    cache.retain(|request_id, record| {
        record
            .expires_at
            .is_none_or(|expires_at| expires_at.as_unix_millis() > now)
            || request_id.issued_at_unix_millis().is_none()
    });

    let mut legacy = cache
        .iter()
        .filter(|(request_id, _)| request_id.issued_at_unix_millis().is_none())
        .map(|(request_id, record)| (*request_id, record.created_at))
        .collect::<Vec<_>>();
    if legacy.len() > LEGACY_IDEMPOTENCY_RETENTION {
        let expired_count = legacy.len() - LEGACY_IDEMPOTENCY_RETENTION;
        legacy.sort_by_key(|(_, created_at)| *created_at);
        for (request_id, _) in legacy.into_iter().take(expired_count) {
            cache.remove(&request_id);
        }
    }
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
const IDEMPOTENCY_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const LEGACY_IDEMPOTENCY_RETENTION: usize = 1024;
const REQUEST_ID_FUTURE_SKEW: Duration = Duration::from_secs(5 * 60);
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
    #[serde(default = "default_event_retention")]
    retention_limit: usize,
}

impl EventJournal {
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
        after_sequence: Option<EventSequence>,
    ) -> Vec<ServerEventEnvelope> {
        self.events
            .iter()
            .filter(|event| {
                session_ids.contains(&event.session_id)
                    && after_sequence.is_none_or(|sequence| event.sequence > sequence)
            })
            .cloned()
            .collect()
    }

    fn workspace_oldest_sequence(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
    ) -> Option<EventSequence> {
        self.events
            .iter()
            .find(|event| session_ids.contains(&event.session_id))
            .map(|event| event.sequence)
    }

    fn workspace_latest_sequence(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
    ) -> Option<EventSequence> {
        self.events
            .iter()
            .rev()
            .find(|event| session_ids.contains(&event.session_id))
            .map(|event| event.sequence)
    }

    fn workspace_cursor_is_stale(
        &self,
        session_ids: &BTreeSet<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> bool {
        let (Some(after), Some(oldest)) =
            (after_sequence, self.workspace_oldest_sequence(session_ids))
        else {
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct IdempotencyRecord {
    created_at: Timestamp,
    expires_at: Option<Timestamp>,
    request: ClientRequest,
    response: ResponseEnvelope,
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

fn durable_run_messages_from_runtime(messages: &[ModelMessage]) -> Vec<DurableRunMessage> {
    messages
        .iter()
        .map(|message| DurableRunMessage {
            role: message.role,
            content: message.content.clone(),
            name: message.name.clone(),
            tool_call_id: message.tool_call_id,
            tool_calls: message.tool_calls.clone(),
        })
        .collect()
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
        pending_tool_execution: state.pending_tool_execution.clone(),
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
        attempts: Vec::new(),
        pending_approval: None,
        pending_tool_execution: None,
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
    state.pending_tool_execution = execution.pending_tool_execution;
    state.pending_approval = execution.pending_approval;
    state.pending_input = execution.pending_input;
    state.last_failed_call = execution.last_failed_call;
    Ok(())
}

fn run_can_be_deferred_during_restore(
    state: AgentRunState,
    has_pending_tool_execution: Option<bool>,
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
    fragment_wake: Condvar,
    running: Mutex<bool>,
    idle: Condvar,
    failure: Mutex<Option<LoomError>>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
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
        Self {
            run_id: runtime.run_id(),
            session_id: runtime.session_id(),
            control: runtime.control(),
            state: Mutex::new(runtime.export_state()),
            message_fragments: Mutex::new(MessageFragmentState::default()),
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
                if let Some(existing) = state
                    .activities
                    .iter_mut()
                    .find(|existing| existing.id == activity.id)
                {
                    *existing = activity.clone();
                } else {
                    state.activities.push(activity.clone());
                }
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
    task: String,
    model: ModelId,
    system_instructions: Option<String>,
    repository_instructions: Option<String>,
    options: AgentRuntimeOptions,
}

struct GitHubCopilotLoginRecord {
    status: GitHubCopilotLoginStatus,
    expires_at: Instant,
}

pub struct InProcessBackend {
    node_id: String,
    node_name: String,
    sessions: Mutex<SessionManager>,
    workspace_records: Mutex<loom_session::WorkspaceManager>,
    runs: Mutex<BTreeMap<loom_core::RunId, Arc<RunHandle>>>,
    persisted_runs: Mutex<BTreeMap<loom_core::RunId, PersistedRunSummary>>,
    journal: Mutex<EventJournal>,
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
    github_copilot_logins: Mutex<BTreeMap<String, GitHubCopilotLoginRecord>>,
    persistence: Option<FilePersistence>,
    session_root_base: PathBuf,
    idempotency: Mutex<BTreeMap<loom_core::RequestId, IdempotencyRecord>>,
    in_flight_requests: Mutex<BTreeMap<loom_core::RequestId, Arc<Mutex<()>>>>,
    session_admissions: Mutex<BTreeMap<AgentSessionId, Arc<Mutex<()>>>>,
    self_reference: Mutex<Weak<InProcessBackend>>,
    request_lifecycle: RwLock<u8>,
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
        credentials.insert(CredentialRef::new("ui-openai-compatible"), api_key.into())?;
        let providers = ProviderRegistry::with_credentials(credentials);
        providers.register(ProviderConfig::openai_compatible(
            "openai-compatible",
            "OpenAI-compatible model",
            endpoint,
            descriptor,
            Some(CredentialRef::new("ui-openai-compatible")),
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
        let backend = Arc::new(Self {
            node_id,
            node_name,
            sessions: Mutex::new(SessionManager::default()),
            workspace_records: Mutex::new(loom_session::WorkspaceManager::default()),
            runs: Mutex::new(BTreeMap::new()),
            persisted_runs: Mutex::new(BTreeMap::new()),
            journal: Mutex::new(EventJournal::default()),
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
            supported_capabilities: CapabilitySet::new([
                Capability::CreateAgentSession,
                Capability::ReadAgentSession,
                Capability::ControlAgentSession,
                Capability::SubscribeSessionEvents,
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
                Capability::ReadSessionFilesystem,
                Capability::WriteSessionFilesystem,
            ]),
            providers,
            github_copilot_logins: Mutex::new(BTreeMap::new()),
            persistence,
            session_root_base,
            idempotency: Mutex::new(BTreeMap::new()),
            in_flight_requests: Mutex::new(BTreeMap::new()),
            session_admissions: Mutex::new(BTreeMap::new()),
            self_reference: Mutex::new(Weak::new()),
            request_lifecycle: RwLock::new(0),
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
        for (repository_id, repository) in &persisted.repositories {
            if *repository_id != repository.id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    format!("repository key does not match repository {}", repository.id),
                    false,
                ));
            }
            let path = filesystem.directory_path(&repository.path)?;
            let service = GitService::open(path)?;
            self.session_vcs()?
                .insert((session_id, *repository_id), service);
        }
        self.session_repositories()?
            .insert(session_id, persisted.repositories);
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
        &self,
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
        {
            return Err(LoomError::invalid_request(
                "workspace worker-node configuration must contain at most 64 WebSocket URLs without access tokens",
            ));
        }
        let current_revision = self
            .workspace_configs()?
            .get(&workspace_id)
            .map(|config| config.revision);
        if current_revision.is_some_and(|revision| revision > config.revision) {
            return Ok(());
        }
        let previous = self.workspace_configs()?.insert(workspace_id, config);
        if let Err(error) = self.persist_state() {
            let mut configs = self.workspace_configs()?;
            if let Some(previous) = previous {
                configs.insert(workspace_id, previous);
            } else {
                configs.remove(&workspace_id);
            }
            return Err(error);
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
                    retention_limit: feed.retention_limit,
                })
                .unwrap_or_default(),
            session_policies: session_settings.approval_policies,
            auto_approve_actions: session_settings.auto_approve_actions,
            provider_configs: persistence.load_provider_configs()?,
            provider_health: persistence.load_provider_health()?,
            workspace_configs: persistence.load_workspace_configs()?,
            provider_usage: persistence.load_provider_usage()?,
            idempotency: {
                let mut cache = persistence
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
                    .collect::<Result<BTreeMap<_, _>>>()?;
                trim_idempotency_cache(&mut cache);
                cache
            },
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
        {
            let mut target = self.idempotency()?;
            *target = state.idempotency;
        }
        {
            let mut target = self.session_policies()?;
            *target = state.session_policies;
        }
        {
            let mut target = self.auto_approve_actions()?;
            *target = state.auto_approve_actions;
        }
        self.providers.restore_configs(state.provider_configs)?;
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
            runtime_state.messages = persisted_run_messages(persistence.load_run_messages(run_id)?);
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
                                capabilities: ModelCapabilities::default(),
                            });
                        recovery_reason = Some(error.message.clone());
                        Box::new(UnavailableProvider::new(descriptor, error))
                    }
                };
            let tools = ToolExecutor::new_with_workspace(workspace)
                .with_github_token(self.providers.github_account_token().ok());
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
            };
            persistence.save_recovery_updates(&recovery_updates, &feed)?;
            journal.pending_events.clear();
        }
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

    fn persist_state(&self) -> Result<()> {
        self.persist_state_with_recovery_updates(&BTreeMap::new())
    }

    fn persist_state_with_recovery_updates(
        &self,
        recovery_updates: &BTreeMap<loom_core::RunId, DurableRunSummary>,
    ) -> Result<()> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
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
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut durable_run_messages = BTreeMap::new();
        let mut durable_run_activities = BTreeMap::new();
        for (run_id, state) in &mut runs {
            durable_run_messages
                .insert(*run_id, durable_run_messages_from_runtime(&state.messages));
            durable_run_activities.insert(*run_id, std::mem::take(&mut state.activities));
        }
        let loaded_repositories = self.session_repositories()?;
        let mut filesystem_records = Vec::new();
        for (session_id, filesystem) in self.session_filesystems()?.iter() {
            let mut filesystem_state = filesystem.export_state()?;
            let checkpoints = std::mem::take(&mut filesystem_state.checkpoints);
            let edits = std::mem::take(&mut filesystem_state.edits)
                .into_iter()
                .map(|edit| DurableFilesystemEdit {
                    path: edit.path,
                    before: edit.before,
                    before_bytes: edit.before_bytes,
                    after_revision: edit.after_revision,
                    source: edit.source,
                })
                .collect();
            let changes = std::mem::take(&mut filesystem_state.changes);
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
            });
        }
        let sessions = self.sessions()?.export_state();
        let mut journal = self.journal()?;
        let feed = DurableFeedState {
            next_sequence: journal.next_sequence,
            retention_limit: journal.retention_limit,
            events: journal.pending_events.clone(),
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
            .idempotency()?
            .iter()
            .map(|(id, record)| {
                Ok((
                    *id,
                    DurableIdempotencyRecord {
                        created_at: record.created_at,
                        expires_at: record.expires_at,
                        request: json_value(&record.request)?,
                        response: json_value(&record.response)?,
                    },
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let result = persistence.save_state(DurableStateWrite {
            schema_version: CURRENT_SCHEMA_VERSION,
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
            records: &[],
            feed: Some(&feed),
            sections: &[],
        });
        if result.is_ok() {
            journal.pending_events.clear();
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
                    if !matches!(
                        state,
                        AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
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
        self.persist_state()?;
        if let Some(persistence) = self.persistence.as_ref() {
            persistence.release_exclusive_writer()?;
        }
        *shutting_down = 2;
        Ok(())
    }

    fn append_recovery_events(
        &self,
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

    fn idempotency(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<loom_core::RequestId, IdempotencyRecord>>> {
        self.idempotency.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "idempotency cache lock was poisoned",
                true,
            )
        })
    }

    /// Serializes retries of one request id without serializing unrelated
    /// mutations, so a long-running request cannot block a control request.
    fn request_slot(&self, request_id: loom_core::RequestId) -> Result<Arc<Mutex<()>>> {
        let mut in_flight = self.in_flight_requests.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "request serialization lock was poisoned",
                true,
            )
        })?;
        Ok(Arc::clone(in_flight.entry(request_id).or_default()))
    }

    fn session_admission(&self, session_id: AgentSessionId) -> Result<Arc<Mutex<()>>> {
        let mut admissions = self.session_admissions.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "session admission lock was poisoned",
                true,
            )
        })?;
        Ok(Arc::clone(admissions.entry(session_id).or_default()))
    }

    fn release_request_slot(&self, request_id: loom_core::RequestId) {
        let mut in_flight = self
            .in_flight_requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if in_flight
            .get(&request_id)
            .is_some_and(|slot| Arc::strong_count(slot) == 1)
        {
            in_flight.remove(&request_id);
        }
    }

    /// Journals one agent event and keeps the session state in step with it.
    fn record_agent_event(&self, session_id: AgentSessionId, event: AgentEvent) -> Result<()> {
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
            if let Some(handle) = handle {
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
                            if let Err(error) = backend.persist_state() {
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

    fn cached_response(
        &self,
        request_id: loom_core::RequestId,
        request: &ClientRequest,
    ) -> Result<Option<ResponseEnvelope>> {
        let cache = self.idempotency()?;
        let Some(record) = cache.get(&request_id) else {
            return Ok(None);
        };
        if &record.request != request {
            return Err(LoomError::conflict(format!(
                "request id {request_id} was already used for a different mutation"
            )));
        }
        Ok(Some(record.response.clone()))
    }

    fn remember_response(
        &self,
        request_id: loom_core::RequestId,
        request: ClientRequest,
        response: ResponseEnvelope,
    ) -> Result<()> {
        let mut cache = self.idempotency()?;
        cache.insert(
            request_id,
            IdempotencyRecord {
                created_at: Timestamp::now(),
                expires_at: request_id.issued_at_unix_millis().map(|issued_at| {
                    Timestamp::from_unix_millis(
                        issued_at.saturating_add(IDEMPOTENCY_RETENTION.as_millis() as u64),
                    )
                }),
                request,
                response,
            },
        );
        trim_idempotency_cache(&mut cache);
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

impl InProcessConnection {
    fn run_snapshot_projection(
        &self,
        run_id: loom_core::RunId,
    ) -> Result<AgentRunSnapshotProjection> {
        if let Some(handle) = self.backend.runs()?.get(&run_id).cloned() {
            return Ok(run_snapshot_projection(&handle.state()));
        }
        let summary = self.run_summary(run_id)?;
        let state = self.load_persisted_run_state(&summary, true)?;
        Ok(run_snapshot_projection(&state))
    }

    fn disk_resources(root: &Path) -> (Option<u64>, Option<u64>) {
        let Some(output) = std::process::Command::new("df")
            .args(["-kP", &root.to_string_lossy()])
            .output()
            .ok()
        else {
            return (None, None);
        };
        let output = String::from_utf8_lossy(&output.stdout).into_owned();
        let Some(line) = output.lines().nth(1) else {
            return (None, None);
        };
        let columns = line.split_whitespace().collect::<Vec<_>>();
        let Some(total) = columns
            .get(1)
            .and_then(|value| value.parse::<u64>().ok())
            .map(|value| value.saturating_mul(1024))
        else {
            return (None, None);
        };
        let Some(available) = columns
            .get(3)
            .and_then(|value| value.parse::<u64>().ok())
            .map(|value| value.saturating_mul(1024))
        else {
            return (None, None);
        };
        (Some(total), Some(available))
    }

    /// Looks a run up without touching its runtime lock.
    fn run_handle(&self, run_id: loom_core::RunId) -> Result<Arc<RunHandle>> {
        if let Some(handle) = self.backend.runs()?.get(&run_id).cloned() {
            if let Some(error) = handle.take_failure() {
                return Err(error);
            }
            return Ok(handle);
        }
        let summary = self.run_summary(run_id)?;
        let state = self.load_persisted_run_state(&summary, true)?;
        if state.run.id != run_id || state.session_id != summary.snapshot.session_id {
            return Err(LoomError::new(
                ErrorCode::MalformedPayload,
                format!("persisted run {run_id} does not match its summary"),
                false,
            ));
        }
        let workspace = self.backend.restore_session_filesystem(state.session_id)?;
        let provider = match self
            .backend
            .provider_at(&state.task.model, state.provider_cursor)
        {
            Ok(provider) => provider,
            Err(error) => {
                let descriptor = self
                    .backend
                    .providers
                    .describe_model(&state.task.model)
                    .unwrap_or_else(|_| ModelDescriptor {
                        id: state.task.model.clone(),
                        provider: ProviderId::new("recovered"),
                        display_name: "Unavailable persisted model".to_owned(),
                        context_window: None,
                        capabilities: ModelCapabilities::default(),
                    });
                Box::new(UnavailableProvider::new(descriptor, error))
            }
        };
        let tools = ToolExecutor::new_with_workspace(workspace)
            .with_github_token(self.backend.providers.github_account_token().ok());
        let runtime = AgentRuntime::from_state(state, provider, tools)?;
        let handle = self.backend.register_runtime(runtime);
        self.backend.runs()?.insert(run_id, Arc::clone(&handle));
        if let Some(error) = handle.take_failure() {
            return Err(error);
        }
        Ok(handle)
    }

    fn run_summary(&self, run_id: loom_core::RunId) -> Result<PersistedRunSummary> {
        if let Some(handle) = self.backend.runs()?.get(&run_id) {
            let state = handle.state();
            return Ok(PersistedRunSummary {
                snapshot: state.run,
                usage: state.usage,
            });
        }
        if let Some(summary) = self.backend.persisted_runs()?.get(&run_id).cloned() {
            return Ok(summary);
        }
        let persistence = self
            .backend
            .persistence
            .as_ref()
            .ok_or_else(|| LoomError::not_found("agent run", run_id))?;
        persistence
            .load_run_summary(run_id)?
            .map(|summary| PersistedRunSummary {
                snapshot: summary.snapshot,
                usage: summary.usage,
            })
            .ok_or_else(|| LoomError::not_found("agent run", run_id))
    }

    fn run_message_page(
        &self,
        run_id: loom_core::RunId,
        before_ordinal: Option<u64>,
        limit: u32,
    ) -> Result<Vec<AgentRunMessageHeader>> {
        if !(1..=MAX_AGENT_RUN_MESSAGE_PAGE_SIZE).contains(&limit) {
            return Err(LoomError::invalid_request(format!(
                "run message page size must be between 1 and {MAX_AGENT_RUN_MESSAGE_PAGE_SIZE}"
            )));
        }
        if let Some(persistence) = &self.backend.persistence {
            return persistence
                .load_run_message_page(run_id, before_ordinal, limit as usize)
                .map(|messages| {
                    messages
                        .into_iter()
                        .map(|message| AgentRunMessageHeader {
                            ordinal: message.ordinal,
                            role: message.role,
                            content_bytes: message.content_bytes,
                            name: message.name,
                            tool_call_id: message.tool_call_id,
                            tool_calls: message.tool_calls,
                        })
                        .collect()
                });
        }

        let state = self.run_handle(run_id)?.state();
        let end = before_ordinal
            .and_then(|ordinal| usize::try_from(ordinal).ok())
            .unwrap_or(state.messages.len())
            .min(state.messages.len());
        let start = end.saturating_sub(limit as usize);
        state.messages[start..end]
            .iter()
            .enumerate()
            .rev()
            .map(|(relative_ordinal, message)| {
                let ordinal = start + relative_ordinal;
                Ok(AgentRunMessageHeader {
                    ordinal: u64::try_from(ordinal).map_err(|_| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            "run message ordinal is out of range",
                            false,
                        )
                    })?,
                    role: message.role,
                    content_bytes: u64::try_from(message.content.len()).map_err(|_| {
                        LoomError::new(
                            ErrorCode::MalformedPayload,
                            "run message content size is out of range",
                            false,
                        )
                    })?,
                    name: message.name.clone(),
                    tool_call_id: message.tool_call_id,
                    tool_calls: message.tool_calls.clone(),
                })
            })
            .collect()
    }

    fn run_transcript_page(
        &self,
        run_id: loom_core::RunId,
        before_ordinal: Option<u64>,
        limit: u32,
    ) -> Result<(Vec<AgentRunTranscriptMessage>, Option<u64>, bool)> {
        if !(1..=MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE).contains(&limit) {
            return Err(LoomError::invalid_request(format!(
                "transcript page size must be between 1 and {MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE}"
            )));
        }
        let headers = self.run_message_page(run_id, before_ordinal, limit)?;
        let next_before = headers.iter().map(|message| message.ordinal).min();
        let has_older = headers.len() == limit as usize;
        let messages = headers
            .into_iter()
            .rev()
            .map(|header| {
                let byte_count = header
                    .content_bytes
                    .min(u64::from(MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES));
                let length = u32::try_from(byte_count).map_err(|_| {
                    LoomError::new(
                        ErrorCode::MalformedPayload,
                        "transcript message length is out of range",
                        false,
                    )
                })?;
                let content = if length == 0 {
                    Vec::new()
                } else {
                    self.run_message_content_range(run_id, header.ordinal, 0, length)?
                };
                let (content, content_truncated) =
                    bounded_transcript_content(&content, header.content_bytes);
                Ok(AgentRunTranscriptMessage {
                    ordinal: header.ordinal,
                    message: ModelMessage {
                        role: header.role,
                        content,
                        name: header.name,
                        tool_call_id: header.tool_call_id,
                        tool_calls: header.tool_calls,
                    },
                    content_truncated,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((messages, next_before, has_older))
    }

    fn run_message_content_range(
        &self,
        run_id: loom_core::RunId,
        message_ordinal: u64,
        byte_offset: u64,
        length: u32,
    ) -> Result<Vec<u8>> {
        if length > MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES {
            return Err(LoomError::invalid_request(format!(
                "message content range exceeds {MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES} bytes"
            )));
        }
        if let Some(persistence) = &self.backend.persistence {
            return persistence.load_run_message_content_range(
                run_id,
                message_ordinal,
                byte_offset,
                length as usize,
            );
        }

        let state = self.run_handle(run_id)?.state();
        let ordinal = usize::try_from(message_ordinal)
            .map_err(|_| LoomError::invalid_request("message ordinal is out of range"))?;
        let message = state
            .messages
            .get(ordinal)
            .ok_or_else(|| LoomError::not_found("run message", message_ordinal))?;
        let start = usize::try_from(byte_offset)
            .map_err(|_| LoomError::invalid_request("message byte offset is out of range"))?;
        if start >= message.content.len() {
            return Ok(Vec::new());
        }
        let end = start
            .saturating_add(length as usize)
            .min(message.content.len());
        Ok(message.content.as_bytes()[start..end].to_vec())
    }

    fn load_persisted_run_state(
        &self,
        summary: &PersistedRunSummary,
        include_messages: bool,
    ) -> Result<AgentRuntimeState> {
        let persistence = self
            .backend
            .persistence
            .as_ref()
            .ok_or_else(|| LoomError::not_found("agent run", summary.snapshot.id))?;
        let runtime_config = persistence
            .load_run_runtime_config(summary.snapshot.id)?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!(
                        "persisted run {} has no runtime configuration",
                        summary.snapshot.id
                    ),
                    true,
                )
            })?;
        let mut state = runtime_state_from_durable_config(summary, runtime_config)?;
        let execution_state = persistence
            .load_run_execution_state(summary.snapshot.id)?
            .ok_or_else(|| {
                LoomError::new(
                    ErrorCode::RecoveryRequired,
                    format!(
                        "persisted run {} has no typed execution state",
                        summary.snapshot.id
                    ),
                    true,
                )
            })?;
        hydrate_runtime_execution_state(&mut state, execution_state)?;
        state.plan = persistence.load_run_plan(summary.snapshot.id)?;
        if include_messages {
            state.messages =
                persisted_run_messages(persistence.load_run_messages(summary.snapshot.id)?);
        } else {
            state.messages.clear();
        }
        hydrate_run_context_checkpoint(persistence, summary.snapshot.id, &mut state)?;
        state.activities = persistence.load_run_activities(summary.snapshot.id)?;
        state.attempts = persistence.load_run_attempts(summary.snapshot.id)?;
        if state.attempts.is_empty() {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                format!(
                    "persisted run {} has no typed attempt history",
                    summary.snapshot.id
                ),
                true,
            ));
        }
        state.interactions = persistence.load_run_interactions(summary.snapshot.id)?;
        Ok(state)
    }

    fn load_persisted_run_state_from_projection(
        &self,
        summary: &PersistedRunSummary,
        persisted: &DurableSessionProjectionRead,
    ) -> Result<AgentRuntimeState> {
        let run_id = summary.snapshot.id;
        let durable_summary = persisted.latest_run.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                "persisted run disappeared",
                true,
            )
        })?;
        if durable_summary.snapshot.id != run_id {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                "persisted projection does not contain the selected run",
                true,
            ));
        }
        let runtime_config = persisted.runtime_config.clone().ok_or_else(|| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("persisted run {run_id} has no runtime configuration"),
                true,
            )
        })?;
        let mut state = runtime_state_from_durable_config(summary, runtime_config)?;
        let execution_state = persisted.execution_state.clone().ok_or_else(|| {
            LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("persisted run {run_id} has no typed execution state"),
                true,
            )
        })?;
        hydrate_runtime_execution_state(&mut state, execution_state)?;
        state.plan = persisted.plan.clone();
        state.messages.clear();
        if let Some(checkpoint) = &persisted.context_checkpoint {
            if checkpoint.session_id != state.session_id {
                return Err(LoomError::new(
                    ErrorCode::MalformedPayload,
                    "run context checkpoint belongs to a different session",
                    false,
                ));
            }
            state.context_checkpoint = Some(checkpoint.summary.clone());
            if let Some(inspection) = &mut state.context_inspection {
                inspection.summary = Some(checkpoint.summary.clone());
            }
        } else {
            state.context_checkpoint = None;
            if let Some(inspection) = &mut state.context_inspection {
                inspection.summary = None;
            }
        }
        state.activities = persisted.activities.clone();
        state.attempts = persisted.attempts.clone();
        if state.attempts.is_empty() {
            return Err(LoomError::new(
                ErrorCode::RecoveryRequired,
                format!("persisted run {run_id} has no typed attempt history"),
                true,
            ));
        }
        state.interactions = persisted.interactions.clone();
        Ok(state)
    }

    fn session_snapshot_projection(
        &self,
        session_id: AgentSessionId,
        include_messages: bool,
    ) -> Result<AgentSessionSnapshotProjection> {
        let session = self.backend.sessions()?.get(session_id)?;
        let (loaded_ids, latest_loaded) = {
            let runs = self.backend.runs()?;
            let loaded_ids = runs.keys().copied().collect::<BTreeSet<_>>();
            let latest = runs
                .values()
                .filter(|handle| handle.session_id == session_id)
                .max_by_key(|handle| handle.snapshot().updated_at)
                .cloned();
            (loaded_ids, latest)
        };
        let persisted_projection = match &self.backend.persistence {
            Some(persistence) if !include_messages => {
                Some(persistence.load_session_projection_read(session_id)?)
            }
            _ => None,
        };
        let latest_persisted = if let Some(projection) = &persisted_projection {
            projection
                .latest_run
                .as_ref()
                .map(|summary| PersistedRunSummary {
                    snapshot: summary.snapshot.clone(),
                    usage: summary.usage.clone(),
                })
                .filter(|summary| !loaded_ids.contains(&summary.snapshot.id))
        } else {
            match &self.backend.persistence {
                Some(persistence) => persistence
                    .load_latest_run_summary_for_session(session_id)?
                    .map(|summary| PersistedRunSummary {
                        snapshot: summary.snapshot,
                        usage: summary.usage,
                    })
                    .filter(|summary| !loaded_ids.contains(&summary.snapshot.id)),
                None => None,
            }
        };
        let load_selected_persisted = |summary: &PersistedRunSummary| {
            if let Some(projection) = &persisted_projection {
                self.load_persisted_run_state_from_projection(summary, projection)
            } else {
                self.load_persisted_run_state(summary, include_messages)
            }
        };
        let active_run = match (latest_loaded, latest_persisted) {
            (Some(handle), Some(summary)) => {
                let projection = handle.snapshot_projection(include_messages);
                if projection.run.updated_at >= summary.snapshot.updated_at {
                    Some(projection)
                } else {
                    Some(run_snapshot_projection(&load_selected_persisted(&summary)?))
                }
            }
            (Some(handle), None) => Some(handle.snapshot_projection(include_messages)),
            (None, Some(summary)) => {
                Some(run_snapshot_projection(&load_selected_persisted(&summary)?))
            }
            (None, None) => None,
        };
        let latest_sequence = persisted_projection
            .as_ref()
            .and_then(|projection| projection.latest_sequence)
            .map(Ok)
            .unwrap_or_else(|| self.latest_session_event_sequence(session_id))?;
        let approval_policy = self.policy(session_id)?;
        let auto_approve_actions = self.auto_approve_actions(session_id)?;
        Ok(AgentSessionSnapshotProjection {
            session,
            active_run,
            latest_sequence,
            approval_policy,
            auto_approve_actions,
        })
    }

    fn session_events_since(
        &self,
        session_id: Option<AgentSessionId>,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<ServerEventEnvelope>> {
        let mut events = match &self.backend.persistence {
            Some(persistence) => persistence.load_feed_events_since(session_id, after_sequence)?,
            None => Vec::new(),
        };
        events.extend(
            self.backend
                .journal()?
                .events_since(session_id, after_sequence),
        );
        Ok(deduplicate_events(events))
    }

    fn workspace_events_since(
        &self,
        workspace_id: WorkspaceId,
        after_sequence: Option<EventSequence>,
    ) -> Result<Vec<ServerEventEnvelope>> {
        let session_ids = self
            .backend
            .sessions()?
            .list_in_workspace(Some(workspace_id), true)
            .into_iter()
            .map(|session| session.id)
            .collect::<BTreeSet<_>>();
        let mut events = match &self.backend.persistence {
            Some(persistence) => {
                persistence.load_feed_workspace_events_since(workspace_id, after_sequence)?
            }
            None => Vec::new(),
        };
        events.extend(
            self.backend
                .journal()?
                .workspace_events_since(&session_ids, after_sequence),
        );
        Ok(deduplicate_events(events))
    }

    fn session_events_with_safe_cursor(
        &self,
        session_id: AgentSessionId,
        after_sequence: Option<EventSequence>,
    ) -> Result<(Vec<ServerEventEnvelope>, EventSequence)> {
        let cursor_before = self.latest_session_event_sequence(session_id)?;
        let events = self.session_events_since(Some(session_id), after_sequence)?;
        let latest_in_batch = events
            .iter()
            .map(|event| event.sequence)
            .max()
            .unwrap_or_default();
        Ok((events, cursor_before.max(latest_in_batch)))
    }

    fn recent_session_events(
        &self,
        session_id: AgentSessionId,
        limit: usize,
    ) -> Result<Vec<ServerEventEnvelope>> {
        let mut events = match &self.backend.persistence {
            Some(persistence) => persistence.load_recent_feed_events(session_id, limit)?,
            None => Vec::new(),
        };
        events.extend(self.backend.journal()?.recent_events(session_id, limit));
        let mut events = deduplicate_events(events);
        let excess = events.len().saturating_sub(limit);
        if excess > 0 {
            events.drain(..excess);
        }
        Ok(events)
    }

    fn feed_session_cursor(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<DurableFeedSessionCursor>> {
        self.backend
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_feed_session_cursor(session_id))
            .transpose()
            .map(Option::flatten)
    }

    fn feed_workspace_cursor(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Option<DurableFeedWorkspaceCursor>> {
        self.backend
            .persistence
            .as_ref()
            .map(|persistence| persistence.load_feed_workspace_cursor(workspace_id))
            .transpose()
            .map(Option::flatten)
    }

    fn latest_workspace_event_sequence(&self, workspace_id: WorkspaceId) -> Result<EventSequence> {
        let session_ids = self
            .backend
            .sessions()?
            .list_in_workspace(Some(workspace_id), true)
            .into_iter()
            .map(|session| session.id)
            .collect::<BTreeSet<_>>();
        let durable = self
            .feed_workspace_cursor(workspace_id)?
            .map(|cursor| cursor.latest_sequence)
            .unwrap_or_default();
        let live = self
            .backend
            .journal()?
            .workspace_latest_sequence(&session_ids)
            .unwrap_or_default();
        Ok(durable.max(live))
    }

    fn latest_session_event_sequence(&self, session_id: AgentSessionId) -> Result<EventSequence> {
        if let Some(cursor) = self.feed_session_cursor(session_id)? {
            return Ok(cursor.latest_sequence);
        }
        Ok(self
            .backend
            .journal()?
            .latest_sequence(Some(session_id))
            .unwrap_or_default())
    }

    fn session_initial_state(
        &self,
        session_id: AgentSessionId,
    ) -> Result<AgentSessionInitialState> {
        for _ in 0..4 {
            let before = self.latest_session_event_sequence(session_id)?;
            let mut projection = self.session_snapshot_projection(session_id, false)?;
            let after = self.latest_session_event_sequence(session_id)?;
            if before == after {
                projection.latest_sequence = after;
                return Ok(AgentSessionInitialState {
                    projection,
                    cursor: after,
                });
            }
        }
        Err(LoomError::new(
            ErrorCode::Conflict,
            "session changed while reading initial state; retry",
            true,
        ))
    }

    fn create_workspace(&self, name: String) -> Result<WorkspaceRecord> {
        self.backend.workspace_records()?.create(name)
    }

    fn register_workspace(&self, workspace: WorkspaceRecord) -> Result<WorkspaceRecord> {
        self.backend.workspace_records()?.register(workspace)
    }

    fn rename_workspace(&self, workspace_id: WorkspaceId, name: String) -> Result<WorkspaceRecord> {
        self.backend.workspace_records()?.rename(workspace_id, name)
    }

    fn create_session_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        name: String,
    ) -> Result<AgentSessionSnapshot> {
        self.backend.workspace_records()?.get(workspace_id)?;
        if name.trim().is_empty() {
            return Err(LoomError::invalid_request(
                "agent session name must not be empty",
            ));
        }
        let session_id = AgentSessionId::new();
        let filesystem = self
            .backend
            .create_session_filesystem(workspace_id, session_id)?;
        let (snapshot, record) =
            self.backend
                .sessions()?
                .create_in_workspace_with_id(workspace_id, session_id, name)?;
        self.backend
            .session_filesystems()?
            .insert(session_id, filesystem);
        self.backend
            .session_repositories()?
            .insert(session_id, BTreeMap::new());
        self.backend.journal()?.append_session(record);
        Ok(snapshot)
    }

    fn session_filesystem(&self, session_id: AgentSessionId) -> Result<Workspace> {
        self.backend.restore_session_filesystem(session_id)
    }

    fn import_session_directory(
        &self,
        session_id: AgentSessionId,
        source: String,
        relative_path: String,
    ) -> Result<ServerResponse> {
        let source_path = Path::new(&source);
        if !source_path.is_absolute() {
            return Err(LoomError::invalid_request(
                "local import source must be an absolute path",
            ));
        }
        if GitService::open(source_path).is_ok() {
            let repository =
                self.attach_session_repository(session_id, source, relative_path.clone(), None)?;
            return Ok(ServerResponse::SessionDirectoryImported {
                path: relative_path,
                repository: Some(repository),
            });
        }
        let session = self.backend.sessions()?.get(session_id)?;
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            return Err(LoomError::invalid_state(
                "directories cannot be imported while a session is active",
            ));
        }
        let relative = checked_session_relative_path(&relative_path)?;
        let filesystem = self.session_filesystem(session_id)?;
        let destination = filesystem.root().join(&relative);
        let parent = destination
            .parent()
            .ok_or_else(|| LoomError::invalid_request("session import path must have a parent"))?;
        fs::create_dir_all(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not create session import parent: {error}"),
                false,
            )
        })?;
        let root = fs::canonicalize(filesystem.root()).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve session filesystem root: {error}"),
                false,
            )
        })?;
        let parent = fs::canonicalize(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve session import parent: {error}"),
                false,
            )
        })?;
        if !parent.starts_with(&root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "session import path escapes its filesystem root",
                false,
            ));
        }
        let destination = parent.join(
            destination
                .file_name()
                .ok_or_else(|| LoomError::invalid_request("invalid session import path"))?,
        );
        copy_directory_contents(source_path, &destination)?;
        Ok(ServerResponse::SessionDirectoryImported {
            path: relative_path,
            repository: None,
        })
    }

    fn attach_session_directory(
        &self,
        session_id: AgentSessionId,
        source: String,
        relative_path: String,
    ) -> Result<ServerResponse> {
        let session = self.backend.sessions()?.get(session_id)?;
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            return Err(LoomError::invalid_state(
                "directories cannot be attached while a session is active",
            ));
        }
        checked_session_relative_path(&relative_path)?;
        let filesystem = self.session_filesystem(session_id)?;
        let source_path = Path::new(&source);
        if !source_path.is_absolute() {
            return Err(LoomError::invalid_request(
                "local directory source must be an absolute path",
            ));
        }
        let source_path = Workspace::canonical_root(source_path)?;
        let mut discovered = Vec::new();
        if source_path.join(".git").exists() {
            discovered.push((relative_path.clone(), source_path.clone()));
        }
        for entry in fs::read_dir(&source_path).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not inspect local directory: {error}"),
                false,
            )
        })? {
            let entry = entry.map_err(|error| {
                LoomError::new(
                    ErrorCode::WorkspaceAccessDenied,
                    format!("could not inspect local directory entry: {error}"),
                    false,
                )
            })?;
            if entry.file_type().is_ok_and(|kind| kind.is_dir())
                && entry.path().join(".git").exists()
            {
                discovered.push((
                    format!("{}/{}", relative_path, entry.file_name().to_string_lossy()),
                    entry.path(),
                ));
            }
        }
        let mut services = Vec::new();
        for (path, source_path) in discovered {
            let service = GitService::open(&source_path)?;
            services.push((path, service));
        }
        let source_path = filesystem.mount_directory(&relative_path, &source_path)?;
        let mut repositories = Vec::new();
        for (path, service) in services {
            let repository = SessionRepository {
                id: RepositoryId::new(),
                source: service.root().display().to_string(),
                path,
                revision: service.status()?.head,
                attached_at: Timestamp::now(),
            };
            self.backend
                .session_vcs()?
                .insert((session_id, repository.id), service);
            self.backend
                .session_repositories()?
                .entry(session_id)
                .or_default()
                .insert(repository.id, repository.clone());
            repositories.push(repository);
        }
        Ok(ServerResponse::SessionDirectoryAttached {
            directory: SessionDirectory {
                source: source_path.display().to_string(),
                path: relative_path,
            },
            repositories,
        })
    }

    fn detach_session_directory(&self, session_id: AgentSessionId, path: String) -> Result<()> {
        let session = self.backend.sessions()?.get(session_id)?;
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            return Err(LoomError::invalid_state(
                "directories cannot be detached while a session is active",
            ));
        }
        let filesystem = self.session_filesystem(session_id)?;
        filesystem.unmount_directory(&path)?;
        let removed = self
            .backend
            .session_repositories()?
            .entry(session_id)
            .or_default()
            .iter()
            .filter(|(_, repository)| {
                repository.path == path || repository.path.starts_with(&format!("{path}/"))
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for repository_id in removed {
            self.backend
                .session_repositories()?
                .entry(session_id)
                .or_default()
                .remove(&repository_id);
            self.backend
                .session_vcs()?
                .remove(&(session_id, repository_id));
        }
        Ok(())
    }

    fn list_github_repositories(&self) -> Result<ServerResponse> {
        let token = self.backend.providers.github_account_token()?;
        let repositories = fetch_github_repositories(&token, "https://api.github.com/user/repos")?;
        Ok(ServerResponse::GitHubRepositories { repositories })
    }

    fn attach_session_repository(
        &self,
        session_id: AgentSessionId,
        source: String,
        relative_path: String,
        revision: Option<String>,
    ) -> Result<SessionRepository> {
        let session = self.backend.sessions()?.get(session_id)?;
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            return Err(LoomError::invalid_state(
                "repositories cannot be attached while a session is active",
            ));
        }
        let source_name = repository_display_name(&source)?;
        let relative = checked_session_relative_path(&relative_path)?;
        let filesystem = self.session_filesystem(session_id)?;
        let root = filesystem.root();
        let destination = root.join(&relative);
        if destination.exists() {
            return Err(LoomError::conflict(format!(
                "session path '{}' already exists",
                relative.display()
            )));
        }
        let parent = destination.parent().ok_or_else(|| {
            LoomError::invalid_request("repository checkout path must have a parent")
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not create repository checkout parent: {error}"),
                false,
            )
        })?;
        let canonical_root = fs::canonicalize(root).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve session filesystem root: {error}"),
                false,
            )
        })?;
        let canonical_parent = fs::canonicalize(parent).map_err(|error| {
            LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                format!("could not resolve repository checkout parent: {error}"),
                false,
            )
        })?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "repository checkout path escapes its session filesystem root",
                false,
            ));
        }
        let repository_id = RepositoryId::new();
        let temporary = parent.join(format!(".loom-clone-{repository_id}"));
        if temporary.exists() {
            return Err(LoomError::conflict(
                "temporary repository checkout path already exists",
            ));
        }
        let github_token = url::Url::parse(&source)
            .ok()
            .filter(|url| url.scheme() == "https" && url.host_str() == Some("github.com"))
            .and_then(|_| self.backend.providers.github_account_token().ok());
        log::info!(
            "[loom-server] cloning repository {source_name} for session {session_id} into {}",
            destination.display()
        );
        let cloned = match GitService::clone_from_authenticated(
            &source,
            &temporary,
            revision.as_deref(),
            github_token.as_deref(),
        ) {
            Ok(cloned) => cloned,
            Err(error) => {
                log::warn!(
                    "[loom-server] clone failed for repository {source_name} in session {session_id}: {}",
                    error.message
                );
                if temporary.exists() {
                    fs::remove_dir_all(&temporary).map_err(|cleanup_error| {
                        LoomError::new(
                            ErrorCode::ToolExecution,
                            format!(
                                "repository clone failed and temporary checkout cleanup failed: {cleanup_error}"
                            ),
                            false,
                        )
                    })?;
                }
                return Err(error);
            }
        };
        drop(cloned);
        log::info!(
            "[loom-server] clone completed for repository {source_name}; installing checkout"
        );
        if let Err(error) = fs::rename(&temporary, &destination) {
            let cleanup = fs::remove_dir_all(&temporary);
            if let Err(cleanup_error) = cleanup {
                return Err(LoomError::new(
                    ErrorCode::ToolExecution,
                    format!(
                        "could not install cloned repository ({error}) or clean its temporary checkout ({cleanup_error})"
                    ),
                    false,
                ));
            }
            return Err(LoomError::new(
                ErrorCode::ToolExecution,
                format!("could not install cloned repository: {error}"),
                false,
            ));
        }
        let service = GitService::open(&destination)?;
        let repository = SessionRepository {
            id: repository_id,
            source: source_name.clone(),
            path: relative_path,
            revision: service.status()?.head,
            attached_at: Timestamp::now(),
        };
        self.backend
            .session_vcs()?
            .insert((session_id, repository_id), service);
        self.backend
            .session_repositories()?
            .entry(session_id)
            .or_default()
            .insert(repository_id, repository.clone());
        log::info!("repository {source_name} attached to session {session_id}");
        Ok(repository)
    }

    fn detach_session_repository(
        &self,
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    ) -> Result<()> {
        let session = self.backend.sessions()?.get(session_id)?;
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            return Err(LoomError::invalid_state(
                "repositories cannot be detached while a session is active",
            ));
        }
        let filesystem = self.session_filesystem(session_id)?;
        let repository = self
            .backend
            .session_repositories()?
            .get(&session_id)
            .and_then(|repositories| repositories.get(&repository_id))
            .cloned()
            .ok_or_else(|| LoomError::not_found("session repository", repository_id))?;
        if filesystem.mounted_source_for(&repository.path)?.is_none() {
            let path = filesystem.directory_path(&repository.path)?;
            fs::remove_dir_all(&path).map_err(|error| {
                LoomError::new(
                    ErrorCode::ToolExecution,
                    format!("could not remove detached repository checkout: {error}"),
                    false,
                )
            })?;
        }
        self.backend
            .session_repositories()?
            .entry(session_id)
            .or_default()
            .remove(&repository_id);
        self.backend
            .session_vcs()?
            .remove(&(session_id, repository_id));
        Ok(())
    }

    fn session_git(
        &self,
        session_id: AgentSessionId,
        repository_id: RepositoryId,
    ) -> Result<GitService> {
        if let Some(service) = self
            .backend
            .session_vcs()?
            .get(&(session_id, repository_id))
            .cloned()
        {
            return Ok(service);
        }
        let _filesystem = self.session_filesystem(session_id)?;
        let repository = self
            .backend
            .session_repositories()?
            .get(&session_id)
            .and_then(|repositories| repositories.get(&repository_id))
            .cloned()
            .ok_or_else(|| LoomError::not_found("session repository", repository_id))?;
        let filesystem = self.session_filesystem(session_id)?;
        let path = filesystem.directory_path(&repository.path)?;
        let service = GitService::open(path)?;
        self.backend
            .session_vcs()?
            .insert((session_id, repository_id), service.clone());
        Ok(service)
    }

    fn session_task_supervisor(&self, session_id: AgentSessionId) -> Result<TaskSupervisor> {
        let filesystem = self.session_filesystem(session_id)?;
        let mut supervisors = self.backend.session_task_supervisors()?;
        let supervisor = if let Some(supervisor) = supervisors.get(&session_id) {
            supervisor.clone()
        } else {
            let supervisor = TaskSupervisor::new(filesystem.root())?;
            supervisors.insert(session_id, supervisor.clone());
            supervisor
        };
        supervisor.set_allowed_roots(
            filesystem
                .mounted_directories()?
                .into_iter()
                .map(|(_, source)| source)
                .collect(),
        )?;
        Ok(supervisor)
    }

    fn check_terminal_session(
        &self,
        session_id: AgentSessionId,
        terminal_id: loom_core::TerminalId,
    ) -> Result<()> {
        let owner = self
            .backend
            .session_terminals()?
            .get(&terminal_id)
            .copied()
            .ok_or_else(|| LoomError::not_found("terminal", terminal_id))?;
        if owner != session_id {
            return Err(LoomError::new(
                ErrorCode::WorkspaceAccessDenied,
                "terminal does not belong to the requested session",
                false,
            ));
        }
        Ok(())
    }

    fn policy(&self, session_id: AgentSessionId) -> Result<ApprovalPolicy> {
        if let Some(policy) = self.backend.session_policies()?.get(&session_id).cloned() {
            return Ok(policy);
        }
        Ok(ApprovalPolicy::auto_approve())
    }

    fn auto_approve_actions(&self, session_id: AgentSessionId) -> Result<bool> {
        let settings = self.backend.auto_approve_actions()?;
        if let Some(auto_approve_actions) = settings.get(&session_id) {
            return Ok(*auto_approve_actions);
        }
        drop(settings);
        Ok(self.policy(session_id)? == ApprovalPolicy::auto_approve())
    }

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
        let retryable = durable_mutation;
        if retryable && let Err(error) = validate_retry_horizon(request_id, current_unix_millis()) {
            return ResponseEnvelope::failure(request_id, error);
        }
        let slot = if retryable {
            match self.backend.request_slot(request_id) {
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
            match self.backend.cached_response(request_id, &request_for_cache) {
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
            request => self.handle_after_negotiation(request),
        };
        let result = match result {
            Ok(response) => {
                if retryable {
                    let response_envelope = ResponseEnvelope::success(request_id, response.clone());
                    if let Err(error) = self.backend.remember_response(
                        request_id,
                        request_for_cache,
                        response_envelope,
                    ) {
                        return ResponseEnvelope::failure(request_id, error);
                    }
                }
                if durable_mutation {
                    self.backend.persist_state().map(|()| response)
                } else {
                    Ok(response)
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
            self.backend.release_request_slot(request_id);
        }
        response
    }

    fn negotiate(
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

    fn discover_capabilities(&self) -> Result<ServerResponse> {
        let capabilities = self
            .backend
            .supported_capabilities
            .intersection(&self.authorized_capabilities());
        Ok(ServerResponse::Capabilities(NegotiationResult {
            protocol_version: CURRENT_PROTOCOL_VERSION,
            capabilities,
        }))
    }

    fn handle_after_negotiation(&self, request: ClientRequest) -> Result<ServerResponse> {
        self.authorize_request(&request)?;
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

        match request {
            ClientRequest::Negotiate { .. } | ClientRequest::DiscoverCapabilities => {
                unreachable!("capability requests are handled above")
            }
            ClientRequest::GetWorkerNodeStatus => {
                Ok(ServerResponse::WorkerNodeStatus(self.worker_node_status()?))
            }
            ClientRequest::CreateWorkspace { name } => Ok(ServerResponse::WorkspaceCreated(
                self.create_workspace(name)?,
            )),
            ClientRequest::RegisterWorkspace { workspace } => Ok(ServerResponse::WorkspaceCreated(
                self.register_workspace(workspace)?,
            )),
            ClientRequest::ListWorkspaces => {
                let workspaces = self
                    .backend
                    .workspace_records()?
                    .list()
                    .into_iter()
                    .filter(|workspace| {
                        self.auth
                            .as_ref()
                            .is_none_or(|auth| auth.scope().allows_workspace(workspace.id))
                    })
                    .collect();
                Ok(ServerResponse::Workspaces { workspaces })
            }
            ClientRequest::RenameWorkspace { workspace_id, name } => Ok(
                ServerResponse::WorkspaceRenamed(self.rename_workspace(workspace_id, name)?),
            ),
            ClientRequest::ListWorkspaceSessions {
                workspace_id,
                include_archived,
            } => Ok(ServerResponse::AgentSessions {
                sessions: self
                    .backend
                    .sessions()?
                    .list_in_workspace(Some(workspace_id), include_archived)
                    .into_iter()
                    .filter(|session| {
                        self.auth
                            .as_ref()
                            .is_none_or(|auth| auth.scope().allows_session(session.id))
                    })
                    .collect(),
            }),
            ClientRequest::CreateAgentSessionInWorkspace { workspace_id, name } => {
                Ok(ServerResponse::AgentSessionCreated(
                    self.create_session_in_workspace(workspace_id, name)?,
                ))
            }
            ClientRequest::GetWorkspaceConfigForWorkspace { workspace_id } => {
                Ok(ServerResponse::WorkspaceConfig(
                    self.backend
                        .workspace_configs()?
                        .get(&workspace_id)
                        .cloned()
                        .unwrap_or_default(),
                ))
            }
            ClientRequest::SetWorkspaceConfigForWorkspace {
                workspace_id,
                config,
            } => {
                self.backend.set_workspace_config(workspace_id, config)?;
                Ok(ServerResponse::WorkspaceConfigUpdated)
            }
            ClientRequest::AttachSessionRepository {
                session_id,
                source,
                path,
                revision,
            } => Ok(ServerResponse::SessionRepositoryAttached(
                self.attach_session_repository(session_id, source, path, revision)?,
            )),
            ClientRequest::ImportSessionDirectory {
                session_id,
                source,
                path,
            } => self.import_session_directory(session_id, source, path),
            ClientRequest::AttachSessionDirectory {
                session_id,
                source,
                path,
            } => self.attach_session_directory(session_id, source, path),
            ClientRequest::ListSessionDirectories { session_id } => {
                self.backend.sessions()?.get(session_id)?;
                Ok(ServerResponse::SessionDirectories {
                    directories: self
                        .session_filesystem(session_id)?
                        .mounted_directories()?
                        .into_iter()
                        .map(|(path, source)| SessionDirectory {
                            path,
                            source: source.display().to_string(),
                        })
                        .collect(),
                })
            }
            ClientRequest::DetachSessionDirectory { session_id, path } => {
                self.detach_session_directory(session_id, path)?;
                Ok(ServerResponse::SessionDirectoryDetached)
            }
            ClientRequest::ListGitHubRepositories => self.list_github_repositories(),
            ClientRequest::ListSessionRepositories { session_id } => {
                self.backend.sessions()?.get(session_id)?;
                let repositories = self
                    .backend
                    .session_repositories()?
                    .get(&session_id)
                    .map(|repositories| repositories.values().cloned().collect());
                let repositories = match repositories {
                    Some(repositories) => repositories,
                    None => self
                        .backend
                        .persisted_filesystem_record(session_id)?
                        .map(|persisted| persisted.repositories.into_values().collect())
                        .unwrap_or_default(),
                };
                Ok(ServerResponse::SessionRepositories { repositories })
            }
            ClientRequest::DetachSessionRepository {
                session_id,
                repository_id,
            } => {
                self.detach_session_repository(session_id, repository_id)?;
                Ok(ServerResponse::SessionRepositoryDetached)
            }
            ClientRequest::GetAgentSession { session_id } => {
                let snapshot = self.backend.sessions()?.get(session_id)?;
                Ok(ServerResponse::AgentSession(snapshot))
            }
            ClientRequest::GetAgentSessionSnapshot { session_id } => {
                Ok(ServerResponse::AgentSessionSnapshot(
                    self.session_snapshot_projection(session_id, true)?,
                ))
            }
            ClientRequest::GetAgentSessionSnapshotMetadata { session_id } => {
                Ok(ServerResponse::AgentSessionSnapshot(
                    self.session_snapshot_projection(session_id, false)?,
                ))
            }
            ClientRequest::GetAgentSessionInitialState { session_id } => Ok(
                ServerResponse::AgentSessionInitialState(self.session_initial_state(session_id)?),
            ),
            ClientRequest::RenameAgentSession { session_id, name } => {
                let (snapshot, record) = self.backend.sessions()?.rename(session_id, name)?;
                self.backend.journal()?.append_session(record);
                Ok(ServerResponse::AgentSessionRenamed(snapshot))
            }
            ClientRequest::ArchiveAgentSession { session_id } => self.archive_session(session_id),
            ClientRequest::GetSessionEvents {
                session_id,
                workspace_id,
                after_sequence,
                stream_epoch,
            } => {
                if session_id.is_some() && workspace_id.is_some() {
                    return Err(LoomError::invalid_request(
                        "session_id and workspace_id cannot both scope an event stream",
                    ));
                }
                let current_stream_epoch = Some(self.backend.node_id.clone());
                let stream_epoch_changed = (session_id.is_some() || workspace_id.is_some())
                    && stream_epoch
                        .as_deref()
                        .is_some_and(|epoch| Some(epoch) != current_stream_epoch.as_deref());
                let after_sequence = if stream_epoch_changed {
                    None
                } else {
                    after_sequence
                };
                if let Some(workspace_id) = workspace_id {
                    self.backend.workspace_records()?.get(workspace_id)?;
                    let events = self.workspace_events_since(workspace_id, after_sequence)?;
                    let durable_cursor = self.feed_workspace_cursor(workspace_id)?;
                    let latest_sequence = self.latest_workspace_event_sequence(workspace_id)?;
                    let journal = self.backend.journal()?;
                    let session_ids = self
                        .backend
                        .sessions()?
                        .list_in_workspace(Some(workspace_id), true)
                        .into_iter()
                        .map(|session| session.id)
                        .collect::<BTreeSet<_>>();
                    // EventJournal.next_sequence stores the last assigned global sequence.
                    let global_head_sequence = journal.next_sequence;
                    let history_missing = after_sequence.is_none()
                        && session_ids.iter().any(|session_id| {
                            !events.iter().any(|event| {
                                event.session_id == *session_id
                                    && matches!(
                                        &event.event,
                                        loom_protocol::ServerEvent::AgentSessionCreated { .. }
                                            | loom_protocol::ServerEvent::AgentSessionForked { .. }
                                    )
                            })
                        });
                    let cursor_stale = stream_epoch_changed
                        || history_missing
                        || match (durable_cursor, after_sequence) {
                            (Some(cursor), Some(after)) => {
                                after < cursor.pruned_through
                                    || after > global_head_sequence
                                    || (after > cursor.latest_sequence
                                        && journal.workspace_cursor_is_stale(
                                            &session_ids,
                                            after_sequence,
                                        ))
                            }
                            (Some(cursor), None) => cursor.pruned_through.value() > 0,
                            (None, Some(after)) => {
                                after > global_head_sequence
                                    || journal
                                        .workspace_cursor_is_stale(&session_ids, after_sequence)
                            }
                            (None, None) => false,
                        };
                    if cursor_stale {
                        let oldest_sequence = durable_cursor
                            .and_then(|cursor| cursor.oldest_retained_sequence)
                            .or_else(|| journal.workspace_oldest_sequence(&session_ids))
                            .or_else(|| {
                                durable_cursor
                                    .filter(|cursor| cursor.pruned_through.value() > 0)
                                    .map(|cursor| cursor.pruned_through.next())
                            })
                            .unwrap_or_else(|| latest_sequence.next());
                        return Ok(ServerResponse::WorkspaceEventsSnapshot {
                            workspace_id,
                            sessions: self
                                .backend
                                .sessions()?
                                .list_in_workspace(Some(workspace_id), true),
                            events,
                            oldest_sequence,
                            latest_sequence,
                            stream_epoch: current_stream_epoch,
                        });
                    }
                    return Ok(ServerResponse::SessionEvents {
                        events,
                        stream_epoch: current_stream_epoch,
                    });
                }
                let (events, session_latest_sequence) = match session_id {
                    Some(session_id) => {
                        let (events, latest) =
                            self.session_events_with_safe_cursor(session_id, after_sequence)?;
                        (events, Some(latest))
                    }
                    None => (self.session_events_since(None, after_sequence)?, None),
                };
                let journal = self.backend.journal()?;
                let history_missing = session_id.is_some()
                    && after_sequence.is_none()
                    && !events.iter().any(|event| {
                        matches!(
                            &event.event,
                            loom_protocol::ServerEvent::AgentSessionCreated { .. }
                                | loom_protocol::ServerEvent::AgentSessionForked { .. }
                        )
                    });
                if let Some(session_id) = session_id {
                    let durable_cursor = self.feed_session_cursor(session_id)?;
                    let cursor_stale = stream_epoch_changed
                        || match (durable_cursor, after_sequence) {
                            (Some(cursor), Some(after)) => {
                                after < cursor.pruned_through
                                    || after > cursor.latest_sequence
                                        && journal.is_cursor_stale(Some(session_id), after_sequence)
                            }
                            (None, _) => journal.is_cursor_stale(Some(session_id), after_sequence),
                            (_, None) => false,
                        };
                    if cursor_stale || history_missing {
                        let oldest_sequence = durable_cursor
                            .and_then(|cursor| cursor.oldest_retained_sequence)
                            .or_else(|| journal.oldest_sequence(Some(session_id)))
                            .or_else(|| {
                                durable_cursor
                                    .filter(|cursor| cursor.pruned_through.value() > 0)
                                    .map(|cursor| cursor.pruned_through.next())
                            })
                            .unwrap_or_else(|| {
                                session_latest_sequence
                                    .unwrap_or(journal.next_sequence)
                                    .next()
                            });
                        return Ok(ServerResponse::SessionEventsSnapshot {
                            session: self.backend.sessions()?.get(session_id)?,
                            events,
                            oldest_sequence,
                            latest_sequence: session_latest_sequence
                                .unwrap_or(journal.next_sequence),
                            stream_epoch: current_stream_epoch,
                        });
                    }
                }
                Ok(ServerResponse::SessionEvents {
                    events,
                    stream_epoch: session_id.map(|_| self.backend.node_id.clone()),
                })
            }
            ClientRequest::GetRecentSessionEvents { session_id, limit } => {
                Ok(ServerResponse::SessionEvents {
                    events: self.recent_session_events(session_id, limit as usize)?,
                    stream_epoch: Some(self.backend.node_id.clone()),
                })
            }
            ClientRequest::StartSessionAgentRun {
                session_id,
                task,
                model,
                system_instructions,
                repository_instructions,
            } => self.start_run_with_options(StartRunInput {
                session_id,
                task,
                model,
                system_instructions,
                repository_instructions,
                options: AgentRuntimeOptions::default(),
            }),
            ClientRequest::StartSessionAgentRunWithOptions {
                session_id,
                task,
                model,
                system_instructions,
                repository_instructions,
                limits,
                context,
            } => self.start_run_with_options(StartRunInput {
                session_id,
                task,
                model,
                system_instructions,
                repository_instructions,
                options: AgentRuntimeOptions {
                    limits,
                    context,
                    checkpoint_id: None,
                    ..Default::default()
                },
            }),
            ClientRequest::GetAgentRun { run_id } => {
                Ok(ServerResponse::AgentRun(self.run_summary(run_id)?.snapshot))
            }
            ClientRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal,
                limit,
            } => Ok(ServerResponse::AgentRunMessagePage {
                run_id,
                messages: self.run_message_page(run_id, before_ordinal, limit)?,
            }),
            ClientRequest::GetAgentRunTranscriptPage {
                run_id,
                before_ordinal,
                limit,
            } => {
                let (messages, next_before, has_older) =
                    self.run_transcript_page(run_id, before_ordinal, limit)?;
                Ok(ServerResponse::AgentRunTranscriptPage {
                    run_id,
                    messages,
                    next_before,
                    has_older,
                })
            }
            ClientRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal,
                byte_offset,
                length,
            } => Ok(ServerResponse::AgentRunMessageContentRange {
                run_id,
                message_ordinal,
                byte_offset,
                content: self.run_message_content_range(
                    run_id,
                    message_ordinal,
                    byte_offset,
                    length,
                )?,
            }),
            ClientRequest::GetAgentRunSnapshot { run_id } => Ok(ServerResponse::AgentRunSnapshot(
                self.run_snapshot_projection(run_id)?,
            )),
            ClientRequest::GetRunCheckpoint { run_id } => {
                let (session_id, checkpoint_id) = {
                    let handle = self.run_handle(run_id)?;
                    let session = self.backend.sessions()?.get(handle.session_id)?;
                    (
                        session.id,
                        handle.state().options.checkpoint_id.ok_or_else(|| {
                            LoomError::new(
                                ErrorCode::NotFound,
                                format!("agent run {run_id} has no checkpoint"),
                                false,
                            )
                        })?,
                    )
                };
                Ok(ServerResponse::RunCheckpoint(
                    self.session_filesystem(session_id)?
                        .checkpoint(checkpoint_id)?,
                ))
            }
            ClientRequest::ApproveAgentAction {
                run_id,
                attempt_id,
                expected_control_revision,
                tool_call_id,
            } => self.continue_run(run_id, |run| {
                run.approve_entry(tool_call_id, attempt_id, expected_control_revision)
            }),
            ClientRequest::RejectAgentAction {
                run_id,
                attempt_id,
                expected_control_revision,
                tool_call_id,
                reason,
            } => self.continue_run(run_id, |run| {
                run.reject_entry(tool_call_id, reason, attempt_id, expected_control_revision)
            }),
            ClientRequest::SendAgentMessage {
                run_id,
                attempt_id,
                expected_control_revision,
                message,
            } => self.continue_run(run_id, |run| {
                run.message_entry_at_revision(message, attempt_id, expected_control_revision)
            }),
            ClientRequest::InterruptAgentRun { run_id } => {
                self.stop_run(run_id, RunStop::Interrupt)
            }
            ClientRequest::RetryAgentStep { run_id } => {
                self.continue_run(run_id, AgentRuntime::retry_entry)
            }
            ClientRequest::PauseAgentRun { run_id } => self.stop_run(run_id, RunStop::Pause),
            ClientRequest::ResumeAgentRun { run_id } => {
                self.continue_run(run_id, AgentRuntime::resume_entry)
            }
            ClientRequest::RetryAgentFromCheckpoint {
                run_id,
                checkpoint_id,
            } => self.retry_from_checkpoint(run_id, checkpoint_id),
            ClientRequest::ForkAgentSession { session_id, name } => {
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
                Ok(ServerResponse::AgentSessionForked(snapshot))
            }
            ClientRequest::ListModels => Ok(ServerResponse::Models {
                models: self.backend.providers.list_models()?,
            }),
            ClientRequest::ListProviders => Ok(ServerResponse::Providers {
                providers: self.backend.providers.list_providers()?,
            }),
            ClientRequest::ConfigureGitHubCopilot { access_token } => {
                self.backend
                    .providers
                    .configure_github_copilot(access_token)?;
                Ok(ServerResponse::ProviderConfigured)
            }
            ClientRequest::StartGitHubCopilotLogin => self.start_github_copilot_login(),
            ClientRequest::GetGitHubCopilotLoginStatus { login_id } => {
                self.github_copilot_login_status(&login_id)
            }
            ClientRequest::DiscoverProviderModels { provider_id } => Ok(ServerResponse::Models {
                models: self.backend.providers.discover_models(&provider_id)?,
            }),
            ClientRequest::GetProviderHealth { provider_id } => Ok(ServerResponse::ProviderHealth(
                self.backend.providers.check_health(&provider_id)?,
            )),
            ClientRequest::GetRunUsage { run_id } => {
                let summary = self.run_summary(run_id)?;
                let provider = self
                    .backend
                    .providers
                    .usage()?
                    .summary(None, Some(&summary.snapshot.model));
                Ok(ServerResponse::RunUsage {
                    usage: summary.usage,
                    provider,
                })
            }
            ClientRequest::GetSessionUsage { session_id } => {
                self.backend.sessions()?.get(session_id)?;
                let loaded_ids = {
                    let runs = self.backend.runs()?;
                    runs.iter()
                        .filter(|(_, handle)| handle.session_id == session_id)
                        .map(|(run_id, _)| *run_id)
                        .collect::<BTreeSet<_>>()
                };
                let mut usage = self
                    .backend
                    .persistence
                    .as_ref()
                    .map(|persistence| persistence.load_session_usage(session_id, &loaded_ids))
                    .transpose()?
                    .unwrap_or_default();
                {
                    let runs = self.backend.runs()?;
                    for handle in runs
                        .values()
                        .filter(|handle| handle.session_id == session_id)
                    {
                        add_usage(&mut usage, &handle.state().usage);
                    }
                }
                Ok(ServerResponse::SessionUsage {
                    usage,
                    provider: self.backend.providers.usage()?.summary(None, None),
                })
            }
            ClientRequest::InspectAgentContext { run_id } => {
                let inspection = self
                    .run_handle(run_id)?
                    .state()
                    .context_inspection
                    .ok_or_else(|| {
                        LoomError::new(
                            ErrorCode::InvalidState,
                            "agent run has not assembled context yet",
                            false,
                        )
                    })?;
                Ok(ServerResponse::ContextInspection(inspection))
            }
            ClientRequest::GetSessionFilesystemSnapshot { session_id } => {
                let mut snapshot = self.session_filesystem(session_id)?.snapshot()?;
                snapshot.root = ".".to_owned();
                Ok(ServerResponse::SessionFilesystemSnapshot(
                    SessionFilesystemSnapshot {
                        session_id,
                        root: snapshot.root,
                        captured_at: snapshot.captured_at,
                        entries: snapshot.entries,
                    },
                ))
            }
            ClientRequest::GetSessionFilesystemChanges {
                session_id,
                after_sequence,
            } => {
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
                    return Ok(ServerResponse::SessionFilesystemChanges {
                        changes: page.changes,
                        truncated: page.truncated,
                    });
                }
                let mut changes = filesystem.changes_since(after_sequence)?;
                let history_pruned = filesystem_history_pruned(after_sequence, &changes);
                let truncated = history_pruned || changes.len() > MAX_REVIEW_CHANGES;
                if changes.len() > MAX_REVIEW_CHANGES {
                    changes = changes.split_off(changes.len() - MAX_REVIEW_CHANGES);
                }
                Ok(ServerResponse::SessionFilesystemChanges {
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
                })
            }
            ClientRequest::ReadSessionFile { session_id, path } => {
                let mut file = self.session_filesystem(session_id)?.read_file(&path)?;
                file.content = bounded_review_text(&file.content, MAX_REVIEW_FILE_BYTES);
                Ok(ServerResponse::SessionFilesystemFile(
                    SessionFilesystemFile {
                        session_id,
                        path: file.path,
                        content: file.content,
                        revision: file.revision,
                    },
                ))
            }
            ClientRequest::ApplySessionFilesystemEdit { session_id, edit } => {
                Ok(ServerResponse::WorkspaceEditApplied(
                    self.session_filesystem(session_id)?.apply_user_edit(edit)?,
                ))
            }
            ClientRequest::TakeSessionFilesystemControl {
                session_id,
                control,
            } => {
                self.session_filesystem(session_id)?.take_control(control)?;
                Ok(ServerResponse::WorkspaceControl(control))
            }
            ClientRequest::CreateSessionCheckpoint { session_id, label } => {
                Ok(ServerResponse::CheckpointCreated(
                    self.session_filesystem(session_id)?
                        .create_checkpoint(label)?,
                ))
            }
            ClientRequest::RevertSessionCheckpoint {
                session_id,
                checkpoint_id,
            } => Ok(ServerResponse::CheckpointReverted(
                self.session_filesystem(session_id)?
                    .revert_checkpoint(checkpoint_id)?,
            )),
            ClientRequest::UndoSessionEdit { session_id } => Ok(ServerResponse::WorkspaceUndo(
                self.session_filesystem(session_id)?
                    .undo_last_agent_edit()?,
            )),
            ClientRequest::GetSessionContextFiles { session_id } => {
                Ok(ServerResponse::ContextFiles {
                    files: self.session_filesystem(session_id)?.context_files()?,
                })
            }
            ClientRequest::GetSessionVcsStatus {
                session_id,
                repository_id,
            } => Ok(ServerResponse::VcsStatus(
                self.session_git(session_id, repository_id)?.status()?,
            )),
            ClientRequest::GetSessionVcsDiff {
                session_id,
                repository_id,
                path,
                staged,
            } => {
                let mut diff = self
                    .session_git(session_id, repository_id)?
                    .diff(path.as_deref(), staged)?;
                let mut remaining = MAX_REVIEW_DIFF_BYTES;
                let mut truncated = false;
                for hunk in &mut diff.hunks {
                    if truncated {
                        hunk.lines.clear();
                        continue;
                    }
                    let keep = hunk
                        .lines
                        .iter()
                        .take_while(|line| {
                            let size = line.content.len() + 32;
                            if size > remaining {
                                truncated = true;
                                false
                            } else {
                                remaining -= size;
                                true
                            }
                        })
                        .count();
                    if keep < hunk.lines.len() {
                        truncated = true;
                        hunk.lines.truncate(keep);
                    }
                }
                diff.hunks.retain(|hunk| !hunk.lines.is_empty());
                diff.truncated = truncated;
                diff.patch = bounded_review_text(&diff.patch, MAX_REVIEW_DIFF_BYTES);
                Ok(ServerResponse::VcsDiff(diff))
            }
            ClientRequest::GetSessionVcsBranches {
                session_id,
                repository_id,
            } => Ok(ServerResponse::VcsBranches {
                branches: self.session_git(session_id, repository_id)?.branches()?,
            }),
            ClientRequest::GetSessionVcsConflicts {
                session_id,
                repository_id,
            } => Ok(ServerResponse::VcsConflicts {
                paths: self.session_git(session_id, repository_id)?.conflicts()?,
            }),
            ClientRequest::SetSessionApprovalPolicy {
                session_id,
                policy,
                auto_approve_actions,
            } => {
                self.backend.sessions()?.get(session_id)?;
                self.backend
                    .session_policies()?
                    .insert(session_id, policy.clone());
                if let Some(auto_approve_actions) = auto_approve_actions {
                    self.backend
                        .auto_approve_actions()?
                        .insert(session_id, auto_approve_actions);
                }
                Ok(ServerResponse::ApprovalPolicy(policy))
            }
            ClientRequest::OpenSessionTerminal {
                session_id,
                command,
                args,
                cwd,
            } => {
                let filesystem = self.session_filesystem(session_id)?;
                let cwd = filesystem.directory_path(cwd.as_deref().unwrap_or("."))?;
                let snapshot = self.backend.terminals.open(command, args, cwd)?;
                self.backend
                    .session_terminals()?
                    .insert(snapshot.id, session_id);
                Ok(ServerResponse::TerminalOpened(snapshot))
            }
            ClientRequest::WriteSessionTerminalInput {
                session_id,
                terminal_id,
                input,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                self.backend.terminals.write_input(terminal_id, &input)?;
                Ok(ServerResponse::Terminal(
                    self.backend.terminals.get(terminal_id)?,
                ))
            }
            ClientRequest::ResizeSessionTerminal {
                session_id,
                terminal_id,
                rows,
                columns,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::Terminal(self.backend.terminals.resize(
                    terminal_id,
                    rows,
                    columns,
                )?))
            }
            ClientRequest::GetSessionTerminalEvents {
                session_id,
                terminal_id,
                after_sequence,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::TerminalEvents {
                    events: self
                        .backend
                        .terminals
                        .events_since(terminal_id, after_sequence)?,
                })
            }
            ClientRequest::CancelSessionTerminal {
                session_id,
                terminal_id,
            } => {
                self.check_terminal_session(session_id, terminal_id)?;
                Ok(ServerResponse::Terminal(
                    self.backend.terminals.cancel(terminal_id)?,
                ))
            }
            ClientRequest::StartSessionTask { session_id, spec } => Ok(
                ServerResponse::TaskStarted(self.session_task_supervisor(session_id)?.start(spec)?),
            ),
            ClientRequest::ListSessionTasks { session_id } => Ok(ServerResponse::Tasks {
                tasks: self.session_task_supervisor(session_id)?.list()?,
            }),
            ClientRequest::GetSessionTask {
                session_id,
                task_id,
            } => Ok(ServerResponse::Task(
                self.session_task_supervisor(session_id)?.get(task_id)?,
            )),
            ClientRequest::GetSessionTaskEvents {
                session_id,
                task_id,
                after_sequence,
            } => Ok(ServerResponse::TaskEvents {
                events: self
                    .session_task_supervisor(session_id)?
                    .events_since(task_id, after_sequence)?,
            }),
            ClientRequest::CancelSessionTask {
                session_id,
                task_id,
            } => Ok(ServerResponse::Task(
                self.session_task_supervisor(session_id)?.cancel(task_id)?,
            )),
            ClientRequest::GetSessionTaskEvidence {
                session_id,
                task_id,
            } => Ok(ServerResponse::TaskEvidence {
                evidence: self
                    .session_task_supervisor(session_id)?
                    .get(task_id)?
                    .evidence,
            }),
            ClientRequest::AttachRunEvidence { run_id, evidence } => {
                let handle = self.run_handle(run_id)?;
                let mut runtime = handle.runtime_for_entry()?;
                runtime.add_evidence(evidence);
                handle.refresh(&runtime);
                Ok(ServerResponse::AgentRun(handle.snapshot()))
            }
        }
    }

    fn worker_node_status(&self) -> Result<WorkerNodeStatus> {
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

    fn start_github_copilot_login(&self) -> Result<ServerResponse> {
        const MAX_PENDING_LOGINS: usize = 8;
        const COMPLETED_LOGIN_RETENTION: Duration = Duration::from_secs(300);

        let now = Instant::now();
        let login_id = uuid::Uuid::new_v4().to_string();
        {
            let mut logins = self
                .backend
                .github_copilot_logins
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            logins.retain(|_, login| login.expires_at + COMPLETED_LOGIN_RETENTION > now);
            if logins
                .values()
                .filter(|login| matches!(login.status, GitHubCopilotLoginStatus::Pending))
                .count()
                >= MAX_PENDING_LOGINS
            {
                return Err(LoomError::new(
                    ErrorCode::Conflict,
                    "too many GitHub Copilot sign-ins are already pending on this worker",
                    true,
                ));
            }
            logins.insert(
                login_id.clone(),
                GitHubCopilotLoginRecord {
                    status: GitHubCopilotLoginStatus::Pending,
                    expires_at: now + Duration::from_secs(3600),
                },
            );
        }
        let device = match GitHubCopilotAuthenticator::default().begin() {
            Ok(device) => device,
            Err(error) => {
                self.backend
                    .github_copilot_logins
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&login_id);
                return Err(error);
            }
        };
        if let Some(login) = self
            .backend
            .github_copilot_logins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&login_id)
        {
            login.expires_at = now + Duration::from_secs(device.expires_in.min(3600));
        }

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
                let mut logins = backend
                    .github_copilot_logins
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(login) = logins.get_mut(&worker_login_id)
                    && matches!(login.status, GitHubCopilotLoginStatus::Pending)
                {
                    login.status = status;
                }
            });
        if let Err(error) = spawn_result {
            self.backend
                .github_copilot_logins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&login_id);
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

    fn github_copilot_login_status(&self, login_id: &str) -> Result<ServerResponse> {
        let mut logins = self
            .backend
            .github_copilot_logins
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let login = logins
            .get_mut(login_id)
            .ok_or_else(|| LoomError::not_found("GitHub Copilot sign-in", login_id))?;
        if matches!(login.status, GitHubCopilotLoginStatus::Pending)
            && Instant::now() >= login.expires_at
        {
            login.status = GitHubCopilotLoginStatus::Failed {
                message: "GitHub device authorization expired".to_owned(),
            };
        }
        Ok(ServerResponse::GitHubCopilotLoginStatus {
            status: login.status.clone(),
        })
    }

    fn authorize_request(&self, request: &ClientRequest) -> Result<()> {
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
            ClientRequest::ConfigureGitHubCopilot { .. } => {}
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

    fn start_run_with_options(&self, mut input: StartRunInput) -> Result<ServerResponse> {
        let admission = self.backend.session_admission(input.session_id)?;
        let _admission_guard = admission.try_lock().map_err(|_| {
            LoomError::conflict("another agent run is already being started for this session")
        })?;
        let session = self.backend.sessions()?.get(input.session_id)?;
        if session.state != AgentSessionState::Idle {
            return Err(LoomError::new(
                ErrorCode::InvalidState,
                "agent session is already running or completed",
                false,
            ));
        }
        let provider = self.backend.provider(&input.model)?;
        let (input_cost_micros_per_1k, output_cost_micros_per_1k) =
            self.backend.providers.pricing(&input.model)?;
        input.options.input_cost_micros_per_1k = input_cost_micros_per_1k;
        input.options.output_cost_micros_per_1k = output_cost_micros_per_1k;
        let workspace = self.session_filesystem(session.id)?;
        if input.repository_instructions.is_none() {
            let instructions = workspace.instruction_text()?;
            if !instructions.trim().is_empty() {
                input.repository_instructions = Some(instructions);
            }
        }
        let checkpoint = workspace.create_checkpoint("before agent run")?;
        input.options.checkpoint_id = Some(checkpoint.id);
        let tools = ToolExecutor::new_with_workspace(workspace)
            .with_github_token(self.backend.providers.github_account_token().ok());
        let policy = self.policy(session.id)?;
        let mut agent_task = AgentTask::new(input.task, input.model)?;
        agent_task.system_instructions = input.system_instructions;
        agent_task.repository_instructions = input.repository_instructions;
        let runtime = AgentRuntime::new_with_policy_and_options(
            input.session_id,
            agent_task,
            provider,
            tools,
            policy,
            input.options,
        );
        let run_id = runtime.run_id();
        // The run is registered, and its events observable, before any model
        // work starts, so a second client can control it immediately.
        let handle = self.backend.register_runtime(runtime);
        self.backend.runs()?.insert(run_id, Arc::clone(&handle));
        let progress = {
            let mut runtime = handle.try_runtime()?;
            let progress = runtime.begin();
            handle.refresh(&runtime);
            progress?
        };
        self.backend.persist_state()?;
        if progress.continues {
            self.backend.spawn_run_worker(Arc::clone(&handle))?;
        }
        Ok(ServerResponse::AgentRunStarted(handle.snapshot()))
    }

    /// Applies an operation that may leave the run with more work, then hands
    /// the remaining work to the run worker instead of the request handler.
    fn continue_run(
        &self,
        run_id: loom_core::RunId,
        operation: impl FnOnce(&mut AgentRuntime) -> Result<RunProgress>,
    ) -> Result<ServerResponse> {
        let handle = self.run_handle(run_id)?;
        let progress = {
            let mut runtime = handle.runtime_for_entry()?;
            let progress = operation(&mut runtime);
            handle.refresh(&runtime);
            progress?
        };
        if let Err(error) = self.backend.persist_state() {
            handle.record_failure(error.clone());
            return Err(error);
        }
        if progress.continues {
            self.backend.spawn_run_worker(Arc::clone(&handle))?;
        }
        Ok(ServerResponse::AgentRun(handle.snapshot()))
    }

    /// Pauses or interrupts a run. The request only raises the control flag, so
    /// it is never queued behind the model call it is stopping.
    fn stop_run(&self, run_id: loom_core::RunId, stop: RunStop) -> Result<ServerResponse> {
        let handle = self.run_handle(run_id)?;
        if handle.is_running() {
            match stop {
                RunStop::Interrupt => handle.control.request_interrupt(),
                RunStop::Pause => handle.control.request_pause(),
            }
            handle.wait_until_idle()?;
            if let Some(error) = handle.take_failure() {
                return Err(error);
            }
            if handle.control.is_stopping() {
                let state = handle.state().run.state;
                if !matches!(
                    state,
                    AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                ) {
                    let mut runtime = handle.try_runtime()?;
                    let result = match stop {
                        RunStop::Interrupt => runtime.interrupt(),
                        RunStop::Pause => runtime.pause(),
                    };
                    handle.refresh(&runtime);
                    result?;
                    self.backend.persist_state()?;
                }
                handle.control.clear_request();
            }
            return Ok(ServerResponse::AgentRun(handle.snapshot()));
        }
        let mut runtime = handle.runtime_for_entry()?;
        let result = match stop {
            RunStop::Interrupt => runtime.interrupt(),
            RunStop::Pause => runtime.pause(),
        };
        handle.refresh(&runtime);
        result?;
        Ok(ServerResponse::AgentRun(handle.snapshot()))
    }

    fn archive_session(&self, session_id: AgentSessionId) -> Result<ServerResponse> {
        let session = self.backend.sessions()?.get(session_id)?;
        if matches!(
            session.state,
            AgentSessionState::Queued
                | AgentSessionState::Planning
                | AgentSessionState::AwaitingApproval
                | AgentSessionState::Paused
                | AgentSessionState::Executing
                | AgentSessionState::Evaluating
                | AgentSessionState::NeedsInput
        ) {
            let run_id = self
                .backend
                .runs()?
                .iter()
                .filter(|(_, handle)| handle.session_id == session_id)
                .map(|(run_id, handle)| (*run_id, handle.snapshot()))
                .filter(|(_, snapshot)| {
                    !matches!(
                        snapshot.state,
                        AgentRunState::Completed | AgentRunState::Failed | AgentRunState::Cancelled
                    )
                })
                .max_by_key(|(_, snapshot)| snapshot.updated_at)
                .map(|(run_id, _)| run_id)
                .ok_or_else(|| {
                    LoomError::invalid_state(
                        "running agent sessions must be stopped before archiving",
                    )
                })?;
            self.stop_run(run_id, RunStop::Interrupt)?;
        }

        let (snapshot, record) = self.backend.sessions()?.archive(session_id)?;
        self.backend.journal()?.append_session(record);
        Ok(ServerResponse::AgentSessionArchived(snapshot))
    }

    fn retry_from_checkpoint(
        &self,
        run_id: loom_core::RunId,
        checkpoint_id: loom_core::CheckpointId,
    ) -> Result<ServerResponse> {
        let handle = self.run_handle(run_id)?;
        let session_id = self.backend.sessions()?.get(handle.session_id)?.id;
        if handle.state().options.checkpoint_id != Some(checkpoint_id) {
            return Err(LoomError::conflict(format!(
                "checkpoint {checkpoint_id} is not the checkpoint associated with run {run_id}"
            )));
        }
        self.session_filesystem(session_id)?
            .revert_checkpoint(checkpoint_id)?;
        self.continue_run(run_id, AgentRuntime::checkpoint_retry_entry)
    }

    fn negotiated_capabilities(&self) -> Result<MutexGuard<'_, Option<CapabilitySet>>> {
        self.negotiated_capabilities.lock().map_err(|_| {
            LoomError::new(
                ErrorCode::Internal,
                "connection state lock was poisoned",
                true,
            )
        })
    }
}

fn session_state_for_event(event: &AgentEvent) -> Option<AgentSessionState> {
    let state = match event {
        AgentEvent::RunStarted { snapshot } => snapshot.state,
        AgentEvent::RunStateChanged { state, .. } => *state,
        AgentEvent::RunCompleted { snapshot } => snapshot.state,
        _ => return None,
    };
    Some(match state {
        AgentRunState::Planning => AgentSessionState::Planning,
        AgentRunState::Executing => AgentSessionState::Executing,
        AgentRunState::AwaitingApproval => AgentSessionState::AwaitingApproval,
        AgentRunState::Evaluating => AgentSessionState::Evaluating,
        AgentRunState::Paused => AgentSessionState::Paused,
        AgentRunState::NeedsInput => AgentSessionState::NeedsInput,
        AgentRunState::Completed => AgentSessionState::Completed,
        AgentRunState::Failed => AgentSessionState::Failed,
        AgentRunState::Cancelled => AgentSessionState::Cancelled,
    })
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
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        path::PathBuf,
        process::Command,
        thread,
        time::Duration,
    };

    use loom_context::ContextAssemblyOptions;
    use loom_core::{
        AgentSessionId, CapabilitySet, EventSequence, PolicyDecision, ToolCallId, WorkspaceId,
    };
    use loom_process::{TaskEvent, TaskKind, TaskSpec, TaskStatus, TerminalEvent};
    use loom_protocol::{
        AgentActivityStatus, AgentInteractionStatus, ApprovalDecision, ClientRequest,
        RequestEnvelope, ServerEvent, ServerResponse, WorkerNodeConfig, WorkspaceConfig,
    };
    use loom_workspace::{WorkspaceControl, WorkspaceEdit};

    use super::*;

    fn request_id_with_issued_at(issued_at_ms: u64) -> loom_core::RequestId {
        let mut bytes = *loom_core::RequestId::new().as_uuid().as_bytes();
        bytes[..6].copy_from_slice(&issued_at_ms.to_be_bytes()[2..]);
        loom_core::RequestId::from_uuid(uuid::Uuid::from_bytes(bytes))
    }

    #[test]
    fn idempotency_cache_keeps_uuidv7_horizon_and_bounds_uuidv4_compatibility() {
        let now = Timestamp::now();
        let mut current = BTreeMap::new();
        for _ in 0..LEGACY_IDEMPOTENCY_RETENTION + 1 {
            let request_id = RequestId::new();
            current.insert(
                request_id,
                IdempotencyRecord {
                    created_at: now,
                    expires_at: request_id.issued_at_unix_millis().map(|issued_at| {
                        Timestamp::from_unix_millis(
                            issued_at + IDEMPOTENCY_RETENTION.as_millis() as u64,
                        )
                    }),
                    request: ClientRequest::ListWorkspaces,
                    response: ResponseEnvelope::success(
                        request_id,
                        ServerResponse::WorkspaceConfigUpdated,
                    ),
                },
            );
        }
        let expired_id = request_id_with_issued_at(
            now.as_unix_millis()
                .saturating_sub(IDEMPOTENCY_RETENTION.as_millis() as u64)
                .saturating_sub(1),
        );
        current.insert(
            expired_id,
            IdempotencyRecord {
                created_at: now,
                expires_at: Some(now),
                request: ClientRequest::ListWorkspaces,
                response: ResponseEnvelope::success(
                    expired_id,
                    ServerResponse::WorkspaceConfigUpdated,
                ),
            },
        );
        trim_idempotency_cache(&mut current);
        assert_eq!(current.len(), LEGACY_IDEMPOTENCY_RETENTION + 1);
        assert!(!current.contains_key(&expired_id));

        let mut legacy = BTreeMap::new();
        for _ in 0..LEGACY_IDEMPOTENCY_RETENTION + 1 {
            let request_id = RequestId::from_uuid(uuid::Uuid::new_v4());
            legacy.insert(
                request_id,
                IdempotencyRecord {
                    created_at: now,
                    expires_at: None,
                    request: ClientRequest::ListWorkspaces,
                    response: ResponseEnvelope::success(
                        request_id,
                        ServerResponse::WorkspaceConfigUpdated,
                    ),
                },
            );
        }
        trim_idempotency_cache(&mut legacy);
        assert_eq!(legacy.len(), LEGACY_IDEMPOTENCY_RETENTION);
    }

    #[test]
    fn resumable_runs_without_pending_tool_intent_are_deferred_on_restore() {
        for state in [
            AgentRunState::Planning,
            AgentRunState::Executing,
            AgentRunState::Evaluating,
            AgentRunState::AwaitingApproval,
            AgentRunState::NeedsInput,
            AgentRunState::Paused,
        ] {
            assert!(run_can_be_deferred_during_restore(state, Some(false)));
            assert!(!run_can_be_deferred_during_restore(state, Some(true)));
            assert!(!run_can_be_deferred_during_restore(state, None));
        }
        for state in [
            AgentRunState::Completed,
            AgentRunState::Failed,
            AgentRunState::Cancelled,
        ] {
            assert!(!run_can_be_deferred_during_restore(state, Some(false)));
        }
    }

    #[test]
    fn filesystem_change_response_detects_pruned_client_cursors() {
        let session_id = AgentSessionId::new();
        let changes = vec![SessionFilesystemChange {
            sequence: EventSequence::new(5),
            session_id,
            path: "src/main.rs".to_owned(),
            kind: loom_protocol::WorkspaceChangeKind::Modified,
            revision: Some("revision".to_owned()),
        }];
        assert!(filesystem_history_pruned(
            Some(EventSequence::new(1)),
            &changes
        ));
        assert!(!filesystem_history_pruned(
            Some(EventSequence::new(4)),
            &changes
        ));
        assert!(!filesystem_history_pruned(None, &changes));
    }

    #[test]
    fn event_journal_retention_is_independent_per_session() {
        let first_session = AgentSessionId::new();
        let second_session = AgentSessionId::new();
        let mut journal = EventJournal::default();
        journal.set_retention(2);
        for (session_id, name) in [
            (first_session, "first-1"),
            (second_session, "second-1"),
            (first_session, "first-2"),
            (first_session, "first-3"),
        ] {
            journal.append_session(SessionEventRecord {
                sequence: EventSequence::default(),
                session_id,
                occurred_at: Timestamp::from_unix_millis(1),
                event: loom_core::SessionEvent::AgentSessionRenamed {
                    session_id,
                    name: name.to_owned(),
                },
            });
        }

        assert_eq!(journal.latest_sequence(None), Some(EventSequence::new(4)));
        assert_eq!(
            journal
                .events_since(Some(first_session), None)
                .iter()
                .map(|event| event.sequence.value())
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(
            journal
                .events_since(Some(second_session), None)
                .iter()
                .map(|event| event.sequence.value())
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(journal.pending_events.len(), 3);
        assert_eq!(
            journal
                .pending_events
                .iter()
                .filter(|event| event.session_id == first_session)
                .count(),
            2
        );
    }

    fn negotiate(connection: &InProcessConnection) {
        let response = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities: connection.backend.supported_capabilities.clone(),
        }));
        assert!(matches!(response.result, Ok(ServerResponse::Negotiated(_))));
    }

    fn negotiate_m2(connection: &InProcessConnection) {
        negotiate(connection);
    }

    fn negotiate_m3(connection: &InProcessConnection) {
        negotiate(connection);
    }

    fn negotiate_m5(connection: &InProcessConnection) {
        negotiate(connection);
    }

    fn respond_http(mut stream: TcpStream, status: &str, body: &str) {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        let request = String::from_utf8_lossy(&request);
        let headers = request.to_ascii_lowercase();
        assert!(headers.contains("authorization: bearer fixture-token"));
        assert!(headers.contains("x-github-api-version: 2022-11-28"));
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    }

    fn github_repository_json(name: &str) -> serde_json::Value {
        serde_json::json!({
            "full_name": name,
            "description": null,
            "clone_url": format!("https://github.com/{name}.git"),
            "private": false,
            "default_branch": "main"
        })
    }

    #[test]
    fn github_repository_fetch_paginates_sorts_and_maps_api_records() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (first, _) = listener.accept().unwrap();
            let page_one = (0..100)
                .map(|index| github_repository_json(&format!("owner/repo-{index:03}")))
                .collect::<Vec<_>>();
            respond_http(first, "200 OK", &serde_json::to_string(&page_one).unwrap());
            let (second, _) = listener.accept().unwrap();
            respond_http(
                second,
                "200 OK",
                &serde_json::to_string(&vec![github_repository_json("owner/aaa")]).unwrap(),
            );
        });

        let repositories =
            fetch_github_repositories("fixture-token", &format!("http://{address}/user/repos"))
                .unwrap();
        server.join().unwrap();

        assert_eq!(repositories.len(), 101);
        assert_eq!(repositories.first().unwrap().full_name, "owner/aaa");
        assert_eq!(repositories.last().unwrap().full_name, "owner/repo-099");
        assert_eq!(
            repositories[1].clone_url,
            "https://github.com/owner/repo-000.git"
        );
        assert_eq!(repositories[1].default_branch, "main");
    }

    #[test]
    fn github_repository_fetch_normalizes_transport_and_payload_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (invalid_json, _) = listener.accept().unwrap();
            respond_http(invalid_json, "200 OK", "not-json");
            let (unauthorized, _) = listener.accept().unwrap();
            respond_http(unauthorized, "401 Unauthorized", "{}");
        });

        let endpoint = format!("http://{address}/user/repos");
        let malformed = fetch_github_repositories("fixture-token", &endpoint).unwrap_err();
        assert_eq!(malformed.code, ErrorCode::ProviderInvalidResponse);
        let unauthorized = fetch_github_repositories("fixture-token", &endpoint).unwrap_err();
        assert_eq!(unauthorized.code, ErrorCode::ProviderAuthentication);
        assert!(unauthorized.retryable);
        server.join().unwrap();
    }

    #[test]
    fn filesystem_and_repository_helpers_reject_unsafe_inputs_and_copy_trees() {
        assert_eq!(
            checked_session_relative_path("nested/file.txt").unwrap(),
            PathBuf::from("nested/file.txt")
        );
        for invalid in [
            "",
            "  ",
            ".",
            "..",
            "../secret",
            "/absolute",
            "nested\\file",
        ] {
            assert!(
                checked_session_relative_path(invalid).is_err(),
                "{invalid:?}"
            );
        }

        for (url, safe) in [
            ("wss://worker.example/ws", true),
            ("ws://localhost:9000/", true),
            ("https://worker.example/ws", false),
            ("wss://", false),
            ("wss://user@worker.example/ws", false),
            ("wss://user:secret@worker.example/ws", false),
            ("wss://worker.example/ws#fragment", false),
            ("wss://worker.example/ws?access_TOKEN=secret", false),
        ] {
            assert_eq!(worker_node_url_is_safe(url), safe, "{url}");
        }

        assert_eq!(
            repository_display_name("https://github.com/owner/project.git").unwrap(),
            "project"
        );
        assert_eq!(
            repository_display_name("ssh://git@github.com/owner/project.git").unwrap(),
            "project"
        );
        for unsafe_source in [
            "relative/path",
            "http://github.com/owner/project",
            "https://user:secret@github.com/owner/project",
            "https://github.com/owner/project?access_token=secret",
        ] {
            assert!(
                repository_display_name(unsafe_source).is_err(),
                "{unsafe_source}"
            );
        }

        let source = workspace();
        let destination = workspace();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "copy me").unwrap();
        copy_filesystem_tree(&source, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "copy me"
        );
        assert_eq!(
            checked_session_path(&destination, "nested/file.txt").unwrap(),
            fs::canonicalize(destination.join("nested/file.txt")).unwrap()
        );
        assert!(checked_session_path(&destination, "../outside").is_err());
        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(destination).unwrap();

        let repository = git_repository();
        assert_eq!(
            repository_display_name(repository.to_str().unwrap()).unwrap(),
            repository.file_name().unwrap().to_string_lossy()
        );
        fs::remove_dir_all(repository).unwrap();
    }

    #[test]
    fn local_directory_import_copies_tree_and_rejects_unsafe_sources() {
        let source = workspace();
        let target_root = workspace();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), "copied content").unwrap();
        let destination = target_root.join("imported");
        copy_directory_contents(&source, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("nested/file.txt")).unwrap(),
            "copied content"
        );
        assert_eq!(
            copy_directory_contents(&source, &destination)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );

        let inside_source = source.join("session/imported");
        assert_eq!(
            copy_directory_contents(&source, &inside_source)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        let file_source = source.join("nested/file.txt");
        assert_eq!(
            copy_directory_contents(&file_source, &target_root.join("file"))
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            copy_directory_contents(&source.join("missing"), &target_root.join("missing"))
                .unwrap_err()
                .code,
            ErrorCode::WorkspaceAccessDenied
        );
        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(target_root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn local_directory_import_rejects_symlinks_that_escape_source() {
        use std::os::unix::fs::symlink;

        let source = workspace();
        let outside = workspace();
        let destination_root = workspace();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        symlink(outside.join("secret.txt"), source.join("escape")).unwrap();
        assert_eq!(
            copy_directory_contents(&source, &destination_root.join("import"))
                .unwrap_err()
                .code,
            ErrorCode::WorkspaceAccessDenied
        );
        assert_eq!(fs::read_dir(&destination_root).unwrap().count(), 0);
        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(outside).unwrap();
        fs::remove_dir_all(destination_root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_copy_preserves_symlinks_and_checked_paths_reject_escape() {
        use std::os::unix::fs::symlink;

        let source = workspace();
        let destination = workspace();
        let outside = workspace();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        symlink(outside.join("secret.txt"), source.join("outside-link")).unwrap();
        copy_filesystem_tree(&source, &destination).unwrap();
        assert!(
            fs::symlink_metadata(destination.join("outside-link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(checked_session_path(&destination, "outside-link").is_err());

        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(destination).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn bounded_review_text_is_unicode_safe_and_session_auth_errors_are_structured() {
        assert_eq!(bounded_review_text("short", 5), "short");
        assert_eq!(
            bounded_review_text("éclair", 2),
            "é\n...[review output truncated]"
        );
        assert_eq!(
            bounded_review_text("éclair", 1),
            "\n...[review output truncated]"
        );
        let error = unauthorized_session(AgentSessionId::new());
        assert_eq!(error.code, ErrorCode::AuthorizationDenied);
        assert!(!error.retryable);
        assert!(error.message.contains("not authorized for session"));
    }

    #[test]
    fn worker_node_status_reports_capabilities_and_resources() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let response = connection.request(RequestEnvelope::new(ClientRequest::GetWorkerNodeStatus));
        let response =
            loom_protocol::decode_response(&loom_protocol::encode_response(&response).unwrap())
                .unwrap();
        let ServerResponse::WorkerNodeStatus(status) = response.result.unwrap() else {
            panic!("expected worker node status");
        };
        assert!(status.online);
        assert!(status.resources.cpu_count > 0);
        assert_eq!(status.resources.cpu_usage_percent, None);
        assert!(
            status
                .resources
                .memory_total_bytes
                .is_some_and(|bytes| bytes > 0)
        );
        assert!(status.resources.memory_available_bytes.is_some());
        assert!(
            status
                .resources
                .memory_usage_percent
                .is_some_and(|value| value <= 100)
        );

        std::thread::sleep(Duration::from_millis(250));
        let refreshed =
            connection.request(RequestEnvelope::new(ClientRequest::GetWorkerNodeStatus));
        let refreshed =
            loom_protocol::decode_response(&loom_protocol::encode_response(&refreshed).unwrap())
                .unwrap();
        let ServerResponse::WorkerNodeStatus(refreshed) = refreshed.result.unwrap() else {
            panic!("expected refreshed worker node status");
        };
        assert!(
            refreshed
                .resources
                .cpu_usage_percent
                .is_some_and(|value| value <= 100)
        );
        assert!(
            refreshed
                .resources
                .memory_total_bytes
                .is_some_and(|bytes| bytes > 0)
        );
        assert!(
            refreshed
                .resources
                .memory_usage_percent
                .is_some_and(|value| value <= 100)
        );
        assert_eq!(refreshed.resources.cpu_count, status.resources.cpu_count);
        assert_eq!(refreshed.node_id, status.node_id);
        assert_eq!(refreshed.name, status.name);
        assert_eq!(
            refreshed.resources.memory_total_bytes,
            status.resources.memory_total_bytes
        );
        assert!(status.capabilities.contains(Capability::ReadAgentSession));
    }

    #[test]
    fn worker_resource_percentages_handle_unavailable_and_out_of_range_samples() {
        assert_eq!(cpu_usage_percent(f32::NAN), None);
        assert_eq!(cpu_usage_percent(-1.0), Some(0));
        assert_eq!(cpu_usage_percent(47.6), Some(48));
        assert_eq!(cpu_usage_percent(120.0), Some(100));
        assert_eq!(memory_usage_percent(None, Some(5)), None);
        assert_eq!(memory_usage_percent(Some(0), Some(0)), None);
        assert_eq!(memory_usage_percent(Some(100), Some(25)), Some(75));
        assert_eq!(memory_usage_percent(Some(100), Some(150)), Some(0));
    }

    #[test]
    fn worker_resource_monitor_measures_cpu_utilization_after_a_baseline_sample() {
        let mut monitor = ResourceMonitor::default();

        assert_eq!(monitor.sample(None, None).cpu_usage_percent, None);
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            monitor
                .sample(None, None)
                .cpu_usage_percent
                .is_some_and(|value| value <= 100)
        );
    }

    #[test]
    fn workspace_config_is_persisted_and_excludes_access_tokens() {
        let path =
            std::env::temp_dir().join(format!("loom-workspace-config-{}.db", WorkspaceId::new()));
        let workspace_id;
        let config = WorkspaceConfig {
            revision: 1,
            cpu_pulse_threshold_percent: 37,
            worker_nodes: vec![WorkerNodeConfig {
                url: "wss://worker.example/ws".to_owned(),
            }],
        };
        {
            let backend = InProcessBackend::new_persistent(&path).unwrap();
            let connection = backend.connect();
            negotiate_m5(&connection);
            let created =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Config test".to_owned(),
                }));
            let Ok(ServerResponse::WorkspaceCreated(workspace)) = created.result else {
                panic!("expected workspace creation");
            };
            workspace_id = workspace.id;
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id: workspace.id,
                    config: config.clone(),
                },
            ));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfigUpdated)
            ));
            backend.shutdown().unwrap();
        }

        {
            let backend = InProcessBackend::new_persistent(&path).unwrap();
            let connection = backend.connect();
            negotiate_m5(&connection);
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::GetWorkspaceConfigForWorkspace { workspace_id },
            ));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfig(saved)) if saved == config
            ));
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::SetWorkspaceConfigForWorkspace {
                    workspace_id,
                    config: WorkspaceConfig {
                        revision: 0,
                        cpu_pulse_threshold_percent: 5,
                        worker_nodes: vec![WorkerNodeConfig {
                            url: "wss://stale.example/ws".to_owned(),
                        }],
                    },
                },
            ));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfigUpdated)
            ));
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::GetWorkspaceConfigForWorkspace { workspace_id },
            ));
            assert!(matches!(
                response.result,
                Ok(ServerResponse::WorkspaceConfig(saved)) if saved == config
            ));
            for url in [
                "wss://worker.example/ws?%61ccess_token=secret",
                "wss://user:secret@worker.example/ws",
            ] {
                let response = connection.request(RequestEnvelope::new(
                    ClientRequest::SetWorkspaceConfigForWorkspace {
                        workspace_id,
                        config: WorkspaceConfig {
                            revision: 2,
                            cpu_pulse_threshold_percent: 5,
                            worker_nodes: vec![WorkerNodeConfig {
                                url: url.to_owned(),
                            }],
                        },
                    },
                ));
                assert!(response.result.is_err());
            }
            backend.shutdown().unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }

    /// Waits until a run stops needing the model, because a run is now driven by
    /// its own worker rather than by the request that started it.
    fn await_settled_run(
        connection: &InProcessConnection,
        run_id: loom_core::RunId,
    ) -> loom_agent::AgentRunSnapshot {
        let mut last_snapshot = None;
        for _ in 0..1_000 {
            let response =
                connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
            let Ok(ServerResponse::AgentRun(snapshot)) = response.result else {
                panic!("unexpected run response");
            };
            last_snapshot = Some(snapshot.clone());
            if !matches!(
                snapshot.state,
                AgentRunState::Planning | AgentRunState::Executing | AgentRunState::Evaluating
            ) {
                return snapshot;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let failure = connection
            .backend
            .runs()
            .ok()
            .and_then(|runs| runs.get(&run_id).cloned())
            .and_then(|handle| handle.failure());
        panic!("agent run did not settle: {last_snapshot:?}; failure: {failure:?}");
    }

    fn workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("loom-server-{}", AgentSessionId::new()));
        fs::create_dir(&root).unwrap();
        root
    }

    fn git_repository() -> PathBuf {
        let root = workspace();
        let run = |arguments: &[&str]| {
            assert!(
                Command::new("git")
                    .args(["-C", root.to_str().unwrap()])
                    .args(arguments)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.name", "Loom Test"]);
        run(&["config", "user.email", "loom@example.test"]);
        fs::write(root.join("README.md"), "source\n").unwrap();
        run(&["add", "--", "README.md"]);
        run(&["commit", "-qm", "initial"]);
        root
    }

    #[cfg(unix)]
    #[test]
    fn attaching_local_directory_uses_original_and_discovers_immediate_repositories() {
        let source = workspace();
        fs::write(source.join("note.txt"), "original").unwrap();
        fs::rename(git_repository(), source.join("child-repo")).unwrap();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Local source".to_owned(),
        }));
        let Ok(ServerResponse::WorkspaceCreated(workspace)) = created.result else {
            panic!("expected workspace creation");
        };
        let created = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Local source".to_owned(),
            },
        ));
        let Ok(ServerResponse::AgentSessionCreated(session)) = created.result else {
            panic!("expected session creation");
        };
        let attached = connection.request(RequestEnvelope::new(
            ClientRequest::AttachSessionDirectory {
                session_id: session.id,
                source: source.display().to_string(),
                path: "sources/local".to_owned(),
            },
        ));
        let Ok(ServerResponse::SessionDirectoryAttached {
            directory,
            repositories,
        }) = attached.result
        else {
            panic!("expected directory attachment: {:?}", attached.result);
        };
        assert_eq!(directory.source, source.display().to_string());
        assert_eq!(repositories.len(), 1);
        assert_eq!(repositories[0].path, "sources/local/child-repo");
        let edit = connection.request(RequestEnvelope::new(
            ClientRequest::ApplySessionFilesystemEdit {
                session_id: session.id,
                edit: WorkspaceEdit {
                    path: "sources/local/note.txt".to_owned(),
                    old_text: "original".to_owned(),
                    new_text: "changed".to_owned(),
                    expected_revision: None,
                },
            },
        ));
        assert!(matches!(
            edit.result,
            Ok(ServerResponse::WorkspaceEditApplied(_))
        ));
        assert_eq!(
            fs::read_to_string(source.join("note.txt")).unwrap(),
            "changed"
        );
        let detached = connection.request(RequestEnvelope::new(
            ClientRequest::DetachSessionDirectory {
                session_id: session.id,
                path: directory.path,
            },
        ));
        assert!(matches!(
            detached.result,
            Ok(ServerResponse::SessionDirectoryDetached)
        ));
        assert!(source.join("child-repo/.git").exists());
        let repository_root = git_repository();
        let attached = connection.request(RequestEnvelope::new(
            ClientRequest::AttachSessionDirectory {
                session_id: session.id,
                source: repository_root.display().to_string(),
                path: "sources/repo-root".to_owned(),
            },
        ));
        let Ok(ServerResponse::SessionDirectoryAttached {
            directory,
            repositories,
        }) = attached.result
        else {
            panic!("expected repository root attachment");
        };
        assert_eq!(repositories.len(), 1);
        assert_eq!(repositories[0].path, "sources/repo-root");
        let detached = connection.request(RequestEnvelope::new(
            ClientRequest::DetachSessionDirectory {
                session_id: session.id,
                path: directory.path,
            },
        ));
        assert!(matches!(
            detached.result,
            Ok(ServerResponse::SessionDirectoryDetached)
        ));
        fs::remove_dir_all(repository_root).unwrap();
        fs::remove_dir_all(source).unwrap();
    }

    #[test]
    fn workspace_sessions_get_independent_filesystems_and_repository_clones() {
        let source = git_repository();

        let backend = InProcessBackend::new();
        let connection = backend.connect();
        let capabilities = CapabilitySet::new([
            Capability::ManageWorkspaces,
            Capability::ReadAgentSession,
            Capability::CreateAgentSession,
            Capability::ReadSessionFilesystem,
            Capability::WriteSessionFilesystem,
            Capability::ManageSessionRepositories,
            Capability::ForkAgentSession,
            Capability::ReadVcsStatus,
        ]);
        let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
            client_version: CURRENT_PROTOCOL_VERSION,
            capabilities,
        }));
        assert!(matches!(
            negotiated.result,
            Ok(ServerResponse::Negotiated(_))
        ));

        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Isolation test".to_owned(),
        }));
        let Ok(ServerResponse::WorkspaceCreated(workspace)) = workspace.result else {
            panic!("expected workspace creation");
        };
        let create_session = |name: &str| {
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: name.to_owned(),
                },
            ));
            let Ok(ServerResponse::AgentSessionCreated(session)) = response.result else {
                panic!("expected session creation");
            };
            session
        };
        let first = create_session("First");
        let second = create_session("Second");
        let attach_repository = |session_id| {
            let response = connection.request(RequestEnvelope::new(
                ClientRequest::AttachSessionRepository {
                    session_id,
                    source: source.display().to_string(),
                    path: "repo".to_owned(),
                    revision: None,
                },
            ));
            let Ok(ServerResponse::SessionRepositoryAttached(repository)) = response.result else {
                panic!("expected repository attachment");
            };
            repository
        };
        let first_repository = attach_repository(first.id);
        let second_repository = attach_repository(second.id);
        assert_ne!(first_repository.id, second_repository.id);

        let edit = connection.request(RequestEnvelope::new(
            ClientRequest::ApplySessionFilesystemEdit {
                session_id: first.id,
                edit: WorkspaceEdit {
                    path: "repo/README.md".to_owned(),
                    old_text: "source".to_owned(),
                    new_text: "first session".to_owned(),
                    expected_revision: None,
                },
            },
        ));
        assert!(matches!(
            edit.result,
            Ok(ServerResponse::WorkspaceEditApplied(_))
        ));

        let fork = connection.request(RequestEnvelope::new(ClientRequest::ForkAgentSession {
            session_id: first.id,
            name: "Forked first".to_owned(),
        }));
        let Ok(ServerResponse::AgentSessionForked(fork)) = fork.result else {
            panic!("expected forked session");
        };
        let repositories = connection.request(RequestEnvelope::new(
            ClientRequest::ListSessionRepositories {
                session_id: fork.id,
            },
        ));
        let Ok(ServerResponse::SessionRepositories { repositories }) = repositories.result else {
            panic!("expected forked repositories");
        };
        let fork_repository = repositories.first().expect("repository was copied");
        assert_ne!(fork_repository.id, first_repository.id);
        let fork_edit = connection.request(RequestEnvelope::new(
            ClientRequest::ApplySessionFilesystemEdit {
                session_id: fork.id,
                edit: WorkspaceEdit {
                    path: "repo/README.md".to_owned(),
                    old_text: "first session".to_owned(),
                    new_text: "forked session".to_owned(),
                    expected_revision: None,
                },
            },
        ));
        assert!(matches!(
            fork_edit.result,
            Ok(ServerResponse::WorkspaceEditApplied(_))
        ));
        for (session_id, expected_content) in
            [(first.id, "first session\n"), (fork.id, "forked session\n")]
        {
            let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            }));
            let Ok(ServerResponse::SessionFilesystemFile(file)) = file.result else {
                panic!("expected session file");
            };
            assert_eq!(file.content, expected_content);
        }

        for (session_id, expected_content, repository_id, expected_clean) in [
            (first.id, "first session\n", first_repository.id, false),
            (second.id, "source\n", second_repository.id, true),
        ] {
            let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            }));
            let Ok(ServerResponse::SessionFilesystemFile(file)) = file.result else {
                panic!("expected session file");
            };
            assert_eq!(file.content, expected_content);

            let status =
                connection.request(RequestEnvelope::new(ClientRequest::GetSessionVcsStatus {
                    session_id,
                    repository_id,
                }));
            let Ok(ServerResponse::VcsStatus(status)) = status.result else {
                panic!("expected repository status");
            };
            assert_eq!(status.clean, expected_clean);
        }
        assert_eq!(
            fs::read_to_string(source.join("README.md")).unwrap(),
            "source\n"
        );
        fs::remove_dir_all(&backend.session_root_base).unwrap();
        fs::remove_dir_all(source).unwrap();
    }

    #[test]
    fn session_filesystem_and_repository_metadata_survive_restart() {
        let source = git_repository();
        let state_dir = workspace();
        let persistence = state_dir.join("backend.sqlite");
        let (workspace_id, session_id, checkpoint_id) = {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            let connection = backend.connect();
            let capabilities = CapabilitySet::new([
                Capability::ManageWorkspaces,
                Capability::ReadAgentSession,
                Capability::CreateAgentSession,
                Capability::ReadSessionFilesystem,
                Capability::WriteSessionFilesystem,
                Capability::ManageCheckpoints,
                Capability::ManageSessionRepositories,
                Capability::ReadVcsStatus,
            ]);
            let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities,
            }));
            assert!(matches!(
                negotiated.result,
                Ok(ServerResponse::Negotiated(_))
            ));
            let created =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Persistent workspace".to_owned(),
                }));
            let Ok(ServerResponse::WorkspaceCreated(workspace)) = created.result else {
                panic!("expected workspace creation");
            };
            let created = connection.request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Persistent session".to_owned(),
                },
            ));
            let Ok(ServerResponse::AgentSessionCreated(session)) = created.result else {
                panic!("expected session creation");
            };
            let attached = connection.request(RequestEnvelope::new(
                ClientRequest::AttachSessionRepository {
                    session_id: session.id,
                    source: source.display().to_string(),
                    path: "repo".to_owned(),
                    revision: None,
                },
            ));
            assert!(
                matches!(
                    attached.result,
                    Ok(ServerResponse::SessionRepositoryAttached(_))
                ),
                "{:?}",
                attached.result
            );
            let checkpoint = connection.request(RequestEnvelope::new(
                ClientRequest::CreateSessionCheckpoint {
                    session_id: session.id,
                    label: "before persistent edit".to_owned(),
                },
            ));
            let Ok(ServerResponse::CheckpointCreated(checkpoint)) = checkpoint.result else {
                panic!("expected persisted checkpoint, got {:?}", checkpoint.result);
            };
            backend
                .session_filesystems()
                .unwrap()
                .get(&session.id)
                .unwrap()
                .apply_edit(WorkspaceEdit {
                    path: "repo/README.md".to_owned(),
                    old_text: "source".to_owned(),
                    new_text: "persisted session edit".to_owned(),
                    expected_revision: None,
                })
                .unwrap();
            backend
                .session_filesystems()
                .unwrap()
                .get(&session.id)
                .unwrap()
                .poll_changes()
                .unwrap();
            backend.flush().unwrap();
            backend.shutdown().unwrap();
            (workspace.id, session.id, checkpoint.id)
        };

        {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            let connection = backend.connect();
            let capabilities = CapabilitySet::new([
                Capability::ReadAgentSession,
                Capability::ReadSessionFilesystem,
                Capability::WriteSessionFilesystem,
                Capability::ManageCheckpoints,
                Capability::ReadVcsStatus,
            ]);
            let negotiated = connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities,
            }));
            assert!(matches!(
                negotiated.result,
                Ok(ServerResponse::Negotiated(_))
            ));
            let sessions =
                connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
                    workspace_id,
                    include_archived: false,
                }));
            assert!(matches!(
                sessions.result,
                Ok(ServerResponse::AgentSessions { sessions })
                    if sessions.iter().any(|session| session.id == session_id)
            ));
            let repositories = connection.request(RequestEnvelope::new(
                ClientRequest::ListSessionRepositories { session_id },
            ));
            let Ok(ServerResponse::SessionRepositories { repositories }) = repositories.result
            else {
                panic!("expected restored repository metadata");
            };
            let repository = repositories.first().expect("repository was restored");
            let persisted_filesystem = backend
                .persistence
                .as_ref()
                .unwrap()
                .load_filesystem_record(session_id)
                .unwrap()
                .expect("filesystem record remains inspectable");
            assert!(persisted_filesystem.edits.iter().any(|edit| {
                edit.path == "repo/README.md" && edit.before.as_deref() == Some("source\n")
            }));
            assert!(
                backend
                    .persistence
                    .as_ref()
                    .unwrap()
                    .load_filesystem_changes_page(session_id, None, 512)
                    .unwrap()
                    .changes
                    .iter()
                    .any(|change| {
                        change.path == "repo/README.md"
                            && change.session_id == session_id
                            && change.sequence.value() > 0
                    })
            );
            let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
                session_id,
                path: "repo/README.md".to_owned(),
            }));
            let Ok(ServerResponse::SessionFilesystemFile(file)) = file.result else {
                panic!("expected restored session file");
            };
            assert_eq!(file.content, "persisted session edit\n");
            assert_eq!(
                persisted_filesystem.checkpoints[0].files["repo/README.md"].expected_revision,
                file.revision
            );
            let reverted = connection.request(RequestEnvelope::new(
                ClientRequest::RevertSessionCheckpoint {
                    session_id,
                    checkpoint_id,
                },
            ));
            assert!(
                matches!(reverted.result, Ok(ServerResponse::CheckpointReverted(_))),
                "checkpoint revert failed: {:?}",
                reverted.result
            );
            let restored =
                connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
                    session_id,
                    path: "repo/README.md".to_owned(),
                }));
            let Ok(ServerResponse::SessionFilesystemFile(restored)) = restored.result else {
                panic!("expected checkpoint file contents after revert");
            };
            assert_eq!(restored.content, "source\n");
            let status =
                connection.request(RequestEnvelope::new(ClientRequest::GetSessionVcsStatus {
                    session_id,
                    repository_id: repository.id,
                }));
            let Ok(ServerResponse::VcsStatus(status)) = status.result else {
                panic!("expected restored repository status");
            };
            assert!(status.clean);
            backend.shutdown().unwrap();
        }

        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(persistence.with_extension("session-roots")).unwrap();
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn forked_session_filesystem_and_policy_survive_restart_and_checkpoint_revert() {
        let source = git_repository();
        let state_dir = workspace();
        let persistence = state_dir.join("backend.sqlite");
        let (workspace_id, source_session_id, fork_session_id, checkpoint_id) = {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            let connection = backend.connect();
            negotiate_m5(&connection);
            let created =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Persistent fork workspace".to_owned(),
                }));
            let Ok(ServerResponse::WorkspaceCreated(workspace)) = created.result else {
                panic!("expected workspace creation");
            };
            let created = connection.request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Persistent source".to_owned(),
                },
            ));
            let Ok(ServerResponse::AgentSessionCreated(session)) = created.result else {
                panic!("expected source session creation");
            };
            let policy = ApprovalPolicy::auto_approve();
            let configured = connection.request(RequestEnvelope::new(
                ClientRequest::SetSessionApprovalPolicy {
                    session_id: session.id,
                    policy: policy.clone(),
                    auto_approve_actions: Some(true),
                },
            ));
            assert!(matches!(
                configured.result,
                Ok(ServerResponse::ApprovalPolicy(configured)) if configured == policy
            ));
            let attached = connection.request(RequestEnvelope::new(
                ClientRequest::AttachSessionRepository {
                    session_id: session.id,
                    source: source.display().to_string(),
                    path: "repo".to_owned(),
                    revision: None,
                },
            ));
            assert!(matches!(
                attached.result,
                Ok(ServerResponse::SessionRepositoryAttached(_))
            ));
            let edited = connection.request(RequestEnvelope::new(
                ClientRequest::ApplySessionFilesystemEdit {
                    session_id: session.id,
                    edit: WorkspaceEdit {
                        path: "repo/README.md".to_owned(),
                        old_text: "source".to_owned(),
                        new_text: "source branch".to_owned(),
                        expected_revision: None,
                    },
                },
            ));
            assert!(matches!(
                edited.result,
                Ok(ServerResponse::WorkspaceEditApplied(_))
            ));
            let forked =
                connection.request(RequestEnvelope::new(ClientRequest::ForkAgentSession {
                    session_id: session.id,
                    name: "Persistent fork".to_owned(),
                }));
            let Ok(ServerResponse::AgentSessionForked(forked)) = forked.result else {
                panic!("expected fork creation: {:?}", forked.result);
            };
            let checkpoint = connection.request(RequestEnvelope::new(
                ClientRequest::CreateSessionCheckpoint {
                    session_id: forked.id,
                    label: "fork baseline".to_owned(),
                },
            ));
            let Ok(ServerResponse::CheckpointCreated(checkpoint)) = checkpoint.result else {
                panic!("expected fork checkpoint: {:?}", checkpoint.result);
            };
            backend
                .session_filesystems()
                .unwrap()
                .get(&forked.id)
                .unwrap()
                .apply_edit(WorkspaceEdit {
                    path: "repo/README.md".to_owned(),
                    old_text: "source branch".to_owned(),
                    new_text: "fork-only change".to_owned(),
                    expected_revision: None,
                })
                .unwrap();
            backend
                .session_filesystems()
                .unwrap()
                .get(&forked.id)
                .unwrap()
                .poll_changes()
                .unwrap();
            backend.flush().unwrap();
            backend.shutdown().unwrap();
            (workspace.id, session.id, forked.id, checkpoint.id)
        };

        {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            let connection = backend.connect();
            negotiate_m5(&connection);
            let sessions =
                connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
                    workspace_id,
                    include_archived: false,
                }));
            let Ok(ServerResponse::AgentSessions { sessions }) = sessions.result else {
                panic!("expected restored sessions");
            };
            assert!(
                sessions
                    .iter()
                    .any(|session| session.id == source_session_id)
            );
            assert!(sessions.iter().any(|session| session.id == fork_session_id));

            let snapshot = connection.request(RequestEnvelope::new(
                ClientRequest::GetAgentSessionSnapshot {
                    session_id: fork_session_id,
                },
            ));
            let Ok(ServerResponse::AgentSessionSnapshot(snapshot)) = snapshot.result else {
                panic!("expected restored fork snapshot");
            };
            assert!(snapshot.auto_approve_actions);
            assert_eq!(snapshot.approval_policy, ApprovalPolicy::auto_approve());

            let source_repositories = connection.request(RequestEnvelope::new(
                ClientRequest::ListSessionRepositories {
                    session_id: source_session_id,
                },
            ));
            let Ok(ServerResponse::SessionRepositories {
                repositories: source_repositories,
            }) = source_repositories.result
            else {
                panic!("expected restored source repository metadata");
            };
            let fork_repositories = connection.request(RequestEnvelope::new(
                ClientRequest::ListSessionRepositories {
                    session_id: fork_session_id,
                },
            ));
            let Ok(ServerResponse::SessionRepositories {
                repositories: fork_repositories,
            }) = fork_repositories.result
            else {
                panic!("expected restored fork repository metadata");
            };
            assert_ne!(
                source_repositories.first().unwrap().id,
                fork_repositories.first().unwrap().id
            );

            let read_file = |session_id| {
                let response =
                    connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
                        session_id,
                        path: "repo/README.md".to_owned(),
                    }));
                let Ok(ServerResponse::SessionFilesystemFile(file)) = response.result else {
                    panic!("expected restored session file: {:?}", response.result);
                };
                file.content
            };
            assert_eq!(read_file(source_session_id), "source branch\n");
            assert_eq!(read_file(fork_session_id), "fork-only change\n");

            let events =
                connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(fork_session_id),
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                }));
            let Ok(ServerResponse::SessionEvents { events, .. }) = events.result else {
                panic!("expected restored fork event stream");
            };
            assert!(events.iter().any(|event| {
                matches!(
                    &event.event,
                    loom_protocol::ServerEvent::AgentSessionForked {
                        source_session_id: source_id,
                        snapshot,
                    } if *source_id == source_session_id && snapshot.id == fork_session_id
                )
            }));

            let reverted = connection.request(RequestEnvelope::new(
                ClientRequest::RevertSessionCheckpoint {
                    session_id: fork_session_id,
                    checkpoint_id,
                },
            ));
            assert!(
                matches!(reverted.result, Ok(ServerResponse::CheckpointReverted(_))),
                "fork checkpoint revert failed: {:?}",
                reverted.result
            );
            assert_eq!(read_file(fork_session_id), "source branch\n");
            assert_eq!(read_file(source_session_id), "source branch\n");
            backend.shutdown().unwrap();
        }

        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(persistence.with_extension("session-roots")).unwrap();
        fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn persisted_session_filesystems_restore_lazily_and_survive_unrelated_writes() {
        let persistence =
            std::env::temp_dir().join(format!("loom-server-state-{}.db", WorkspaceId::new()));
        let session_root_base;
        let (workspace_id, session_id) = {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            session_root_base = backend.session_root_base.clone();
            let connection = backend.connect();
            negotiate_m3(&connection);
            let workspace =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Lazy restore workspace".to_owned(),
                }));
            let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
                panic!("unexpected workspace response");
            };
            let session = connection.request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Archived history".to_owned(),
                },
            ));
            let ServerResponse::AgentSessionCreated(session) = session.result.unwrap() else {
                panic!("unexpected session response");
            };
            let root = session_root_base
                .join(workspace.id.to_string())
                .join(session.id.to_string())
                .join("fs");
            fs::write(root.join("retained.txt"), "retained content\n").unwrap();
            backend.flush().unwrap();
            backend.shutdown().unwrap();
            (workspace.id, session.id)
        };

        let filesystem_root = session_root_base
            .join(workspace_id.to_string())
            .join(session_id.to_string())
            .join("fs");
        let parked_root = filesystem_root.with_extension("parked");
        fs::rename(&filesystem_root, &parked_root).unwrap();
        {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            let connection = backend.connect();
            negotiate_m3(&connection);
            let renamed =
                connection.request(RequestEnvelope::new(ClientRequest::RenameAgentSession {
                    session_id,
                    name: "Still lazy".to_owned(),
                }));
            assert!(matches!(
                renamed.result,
                Ok(ServerResponse::AgentSessionRenamed(_))
            ));
            backend.shutdown().unwrap();
        }
        fs::rename(&parked_root, &filesystem_root).unwrap();
        {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            let connection = backend.connect();
            negotiate_m3(&connection);
            let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
                session_id,
                path: "retained.txt".to_owned(),
            }));
            let ServerResponse::SessionFilesystemFile(file) = file.result.unwrap() else {
                panic!("unexpected filesystem response");
            };
            assert_eq!(file.content, "retained content\n");
            backend.shutdown().unwrap();
            fs::remove_dir_all(&backend.session_root_base).unwrap();
        }
        let _ = fs::remove_file(&persistence);
    }

    #[test]
    fn m5_session_projections_reconnect_and_archive_authoritatively() {
        let root = git_repository();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let created = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Navigator".to_owned(),
        }));
        let Ok(ServerResponse::WorkspaceCreated(workspace)) = created.result else {
            panic!("expected workspace creation");
        };
        let created = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Navigator session".to_owned(),
            },
        ));
        let session = match created.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot,
            response => panic!("unexpected response: {response:?}"),
        };

        let attached = connection.request(RequestEnvelope::new(
            ClientRequest::AttachSessionRepository {
                session_id: session.id,
                source: root.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        ));
        assert!(matches!(
            attached.result,
            Ok(ServerResponse::SessionRepositoryAttached(_))
        ));
        let workspaces = connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaces));
        let ServerResponse::Workspaces { workspaces } = workspaces.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        assert_eq!(workspaces, vec![workspace.clone()]);

        let renamed = connection.request(RequestEnvelope::new(ClientRequest::RenameAgentSession {
            session_id: session.id,
            name: "Renamed session".to_owned(),
        }));
        let session = match renamed.result.unwrap() {
            ServerResponse::AgentSessionRenamed(snapshot) => snapshot,
            response => panic!("unexpected rename response: {response:?}"),
        };
        assert_eq!(session.name, "Renamed session");

        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id: session.id,
                task: "inspect the workspace".to_owned(),
                model: loom_model::ModelId::new("deterministic/demo"),
                system_instructions: None,
                repository_instructions: None,
            }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected run response: {response:?}"),
        };

        let snapshot = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot {
                session_id: session.id,
            },
        ));
        let ServerResponse::AgentSessionSnapshot(snapshot) = snapshot.result.unwrap() else {
            panic!("unexpected session snapshot response");
        };
        assert_eq!(snapshot.session.id, session.id);
        assert_eq!(
            snapshot.active_run.as_ref().map(|run| run.run.id),
            Some(run_id)
        );
        assert!(snapshot.active_run.unwrap().plan.is_empty());

        let metadata = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshotMetadata {
                session_id: session.id,
            },
        ));
        let ServerResponse::AgentSessionSnapshot(metadata) = metadata.result.unwrap() else {
            panic!("unexpected metadata snapshot response");
        };
        assert_eq!(
            metadata.active_run.as_ref().map(|run| run.run.id),
            Some(run_id)
        );
        assert!(
            metadata
                .active_run
                .as_ref()
                .is_some_and(|run| run.messages.is_empty())
        );
        let run = connection.request(RequestEnvelope::new(ClientRequest::GetAgentRunSnapshot {
            run_id,
        }));
        let ServerResponse::AgentRunSnapshot(run) = run.result.unwrap() else {
            panic!("unexpected run snapshot response");
        };
        assert_eq!(run.run.id, run_id);
        assert!(!run.messages.is_empty());

        let changes = connection.request(RequestEnvelope::new(
            ClientRequest::GetSessionFilesystemChanges {
                session_id: session.id,
                after_sequence: None,
            },
        ));
        assert!(matches!(
            changes.result,
            Ok(ServerResponse::SessionFilesystemChanges { .. })
        ));

        let archived =
            connection.request(RequestEnvelope::new(ClientRequest::ArchiveAgentSession {
                session_id: session.id,
            }));
        assert!(matches!(
            archived.result,
            Ok(ServerResponse::AgentSessionArchived(_))
        ));
        let sessions =
            connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
                workspace_id: workspace.id,
                include_archived: false,
            }));
        let ServerResponse::AgentSessions { sessions } = sessions.result.unwrap() else {
            panic!("unexpected session list response");
        };
        assert!(sessions.is_empty());
        fs::remove_dir_all(&backend.session_root_base).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creates_session_and_reads_event_stream() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection);

        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "In-process workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let create = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "In-process demo".to_owned(),
            },
        ));
        let session_id = match create.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };

        let events = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        }));
        let ServerResponse::SessionEvents { events, .. } = events.result.unwrap() else {
            panic!("unexpected response");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, session_id);

        let initial = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionInitialState { session_id },
        ));
        let ServerResponse::AgentSessionInitialState(initial) = initial.result.unwrap() else {
            panic!("unexpected initial state response");
        };
        assert_eq!(initial.cursor, events[0].sequence);
        let renamed = connection.request(RequestEnvelope::new(ClientRequest::RenameAgentSession {
            session_id,
            name: "Renamed after snapshot".to_owned(),
        }));
        assert!(matches!(
            renamed.result,
            Ok(ServerResponse::AgentSessionRenamed(_))
        ));
        let resumed = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: Some(initial.cursor),
            stream_epoch: None,
        }));
        let ServerResponse::SessionEvents { events, .. } = resumed.result.unwrap() else {
            panic!("unexpected incremental event response");
        };
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].event,
            ServerEvent::AgentSessionRenamed { .. }
        ));
    }

    #[test]
    fn runs_deterministic_agent_through_approvals() {
        let root = git_repository();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "M1 workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "M1 run".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        connection.request(RequestEnvelope::new(
            ClientRequest::SetSessionApprovalPolicy {
                session_id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        ));
        let attached = connection.request(RequestEnvelope::new(
            ClientRequest::AttachSessionRepository {
                session_id,
                source: root.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        ));
        assert!(
            matches!(
                attached.result,
                Ok(ServerResponse::SessionRepositoryAttached(_))
            ),
            "{:?}",
            attached.result
        );
        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "create a demo file".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: Some("Be concise.".to_owned()),
                repository_instructions: Some("Keep changes focused.".to_owned()),
            }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };

        let mut after = None;
        loop {
            let response =
                connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    workspace_id: None,
                    after_sequence: after,
                    stream_epoch: None,
                }));
            let ServerResponse::SessionEvents { events, .. } = response.result.unwrap() else {
                panic!("unexpected response");
            };
            let mut completed = false;
            for event in &events {
                after = Some(event.sequence);
                if let ServerEvent::Agent {
                    event:
                        AgentEvent::ToolApprovalRequired {
                            run_id: event_run,
                            attempt_id,
                            control_revision,
                            call,
                            ..
                        },
                } = &event.event
                {
                    assert_eq!(*event_run, run_id);
                    let response = connection.request(RequestEnvelope::new(
                        ClientRequest::ApproveAgentAction {
                            run_id,
                            attempt_id: *attempt_id,
                            expected_control_revision: *control_revision,
                            tool_call_id: call.id,
                        },
                    ));
                    assert!(response.result.is_ok());
                }
                if matches!(
                    &event.event,
                    ServerEvent::Agent {
                        event: AgentEvent::RunCompleted { .. }
                    }
                ) {
                    completed = true;
                }
            }
            if completed {
                break;
            }
        }
        let final_run =
            connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        let ServerResponse::AgentRun(snapshot) = final_run.result.unwrap() else {
            panic!("unexpected response");
        };
        assert_eq!(snapshot.state, AgentRunState::Completed);
        let page = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal: None,
                limit: 2,
            },
        ));
        let ServerResponse::AgentRunMessagePage { messages, .. } = page.result.unwrap() else {
            panic!("unexpected run message page response");
        };
        let oldest_ordinal = messages.last().unwrap().ordinal;
        let previous_page = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal: Some(oldest_ordinal),
                limit: 1,
            },
        ));
        let ServerResponse::AgentRunMessagePage {
            messages: previous_messages,
            ..
        } = previous_page.result.unwrap()
        else {
            panic!("unexpected previous run message page response");
        };
        assert!(
            previous_messages
                .iter()
                .all(|message| message.ordinal < oldest_ordinal)
        );
        let header = messages
            .iter()
            .find(|message| message.content_bytes > 0)
            .unwrap();
        let length = header.content_bytes.min(32) as u32;
        let content = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal: header.ordinal,
                byte_offset: 0,
                length,
            },
        ));
        let ServerResponse::AgentRunMessageContentRange { content, .. } = content.result.unwrap()
        else {
            panic!("unexpected run message content response");
        };
        assert_eq!(content.len(), length as usize);
        let beyond_content = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal: header.ordinal,
                byte_offset: u64::MAX,
                length: 8,
            },
        ));
        assert!(matches!(
            beyond_content.result,
            Ok(ServerResponse::AgentRunMessageContentRange { content, .. }) if content.is_empty()
        ));
        let missing_message = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal: oldest_ordinal + 1000,
                byte_offset: 0,
                length: 8,
            },
        ));
        assert_eq!(
            missing_message.result.unwrap_err().code,
            ErrorCode::NotFound
        );
        let empty_page = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal: Some(0),
                limit: 1,
            },
        ));
        assert!(matches!(
            empty_page.result,
            Ok(ServerResponse::AgentRunMessagePage { messages, .. }) if messages.is_empty()
        ));
        let history = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        }));
        let ServerResponse::SessionEvents { events, .. } = history.result.unwrap() else {
            panic!("unexpected history response");
        };
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::ActivityRecorded { activity, .. }
                } if activity.run_id == run_id && activity.completed_at.is_some()
            )
        }));
        assert!(
            backend
                .session_root_base
                .join(workspace.id.to_string())
                .join(session_id.to_string())
                .join("fs/loom-m1-demo.txt")
                .is_file()
        );
        fs::remove_dir_all(&backend.session_root_base).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn requires_negotiation_before_session_requests() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        let response = connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaces));

        assert_eq!(response.result.unwrap_err().code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn unknown_run_is_structured_not_found() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate(&connection);

        let response = connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun {
            run_id: loom_core::RunId::new(),
        }));

        assert_eq!(response.result.unwrap_err().code, ErrorCode::NotFound);
    }

    #[test]
    fn exposes_workspace_terminal_task_and_checkpoint_controls() {
        let root = git_repository();
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m2(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Filesystem controls".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let created = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Filesystem controls".to_owned(),
            },
        ));
        let ServerResponse::AgentSessionCreated(session) = created.result.unwrap() else {
            panic!("unexpected session response");
        };
        let session_id = session.id;
        let attached = connection.request(RequestEnvelope::new(
            ClientRequest::AttachSessionRepository {
                session_id,
                source: root.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        ));
        assert!(matches!(
            attached.result,
            Ok(ServerResponse::SessionRepositoryAttached(_))
        ));
        let snapshot = connection.request(RequestEnvelope::new(
            ClientRequest::GetSessionFilesystemSnapshot { session_id },
        ));
        let ServerResponse::SessionFilesystemSnapshot(snapshot) = snapshot.result.unwrap() else {
            panic!("unexpected filesystem snapshot");
        };
        assert!(
            snapshot
                .entries
                .iter()
                .any(|entry| entry.path == "repo/README.md")
        );
        let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
            session_id,
            path: "repo/README.md".to_owned(),
        }));
        let revision = match file.result.unwrap() {
            ServerResponse::SessionFilesystemFile(file) => file.revision,
            response => panic!("unexpected response: {response:?}"),
        };
        let checkpoint = connection.request(RequestEnvelope::new(
            ClientRequest::CreateSessionCheckpoint {
                session_id,
                label: "before user edit".to_owned(),
            },
        ));
        let checkpoint_id = match checkpoint.result.unwrap() {
            ServerResponse::CheckpointCreated(checkpoint) => checkpoint.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let edit = connection.request(RequestEnvelope::new(
            ClientRequest::ApplySessionFilesystemEdit {
                session_id,
                edit: WorkspaceEdit {
                    path: "repo/README.md".to_owned(),
                    old_text: "source".to_owned(),
                    new_text: "user".to_owned(),
                    expected_revision: Some(revision),
                },
            },
        ));
        assert!(matches!(
            edit.result,
            Ok(ServerResponse::WorkspaceEditApplied(_))
        ));
        let changes = connection.request(RequestEnvelope::new(
            ClientRequest::GetSessionFilesystemChanges {
                session_id,
                after_sequence: None,
            },
        ));
        let ServerResponse::SessionFilesystemChanges { changes, .. } = changes.result.unwrap()
        else {
            panic!("unexpected filesystem changes response");
        };
        assert!(changes.iter().any(|event| event.path == "repo/README.md"));
        let revert = connection.request(RequestEnvelope::new(
            ClientRequest::RevertSessionCheckpoint {
                session_id,
                checkpoint_id,
            },
        ));
        assert_eq!(
            revert.result.unwrap_err().code,
            ErrorCode::Conflict,
            "checkpoint revert must preserve the intervening user edit"
        );
        let file = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
            session_id,
            path: "repo/README.md".to_owned(),
        }));
        assert!(matches!(
            file.result,
            Ok(ServerResponse::SessionFilesystemFile(file)) if file.content == "user\n"
        ));

        let terminal_command = if cfg!(windows) {
            (
                "cmd".to_owned(),
                vec!["/C".to_owned(), "echo terminal".to_owned()],
            )
        } else {
            ("printf".to_owned(), vec!["terminal".to_owned()])
        };
        let terminal =
            connection.request(RequestEnvelope::new(ClientRequest::OpenSessionTerminal {
                session_id,
                command: terminal_command.0,
                args: terminal_command.1,
                cwd: None,
            }));
        let terminal_id = match terminal.result.unwrap() {
            ServerResponse::TerminalOpened(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let mut terminal_done = false;
        for _ in 0..100 {
            let events = connection.request(RequestEnvelope::new(
                ClientRequest::GetSessionTerminalEvents {
                    session_id,
                    terminal_id,
                    after_sequence: None,
                },
            ));
            let ServerResponse::TerminalEvents { events } = events.result.unwrap() else {
                panic!("unexpected terminal event response");
            };
            if events.iter().any(|event| {
                matches!(
                    event.event,
                    TerminalEvent::Exited {
                        status: loom_process::TerminalStatus::Exited,
                        ..
                    }
                )
            }) {
                terminal_done = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(terminal_done);

        let task_command = if cfg!(windows) {
            (
                "cmd".to_owned(),
                vec!["/C".to_owned(), "echo artifact>artifact.txt".to_owned()],
            )
        } else {
            (
                "sh".to_owned(),
                vec!["-c".to_owned(), "printf artifact > artifact.txt".to_owned()],
            )
        };
        let task = connection.request(RequestEnvelope::new(ClientRequest::StartSessionTask {
            session_id,
            spec: TaskSpec {
                kind: TaskKind::Test,
                label: "M2 task".to_owned(),
                command: task_command.0,
                args: task_command.1,
                cwd: Some("repo".to_owned()),
                output_limit_bytes: Some(4096),
                artifact_paths: vec!["repo/artifact.txt".to_owned()],
            },
        }));
        let task_id = match task.result.unwrap() {
            ServerResponse::TaskStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let mut task_done = false;
        for _ in 0..100 {
            let current = connection.request(RequestEnvelope::new(ClientRequest::GetSessionTask {
                session_id,
                task_id,
            }));
            let ServerResponse::Task(snapshot) = current.result.unwrap() else {
                panic!("unexpected task response");
            };
            if matches!(
                snapshot.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
            ) {
                assert!(snapshot.artifacts[0].exists);
                task_done = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(task_done);
        let listed = connection.request(RequestEnvelope::new(ClientRequest::ListSessionTasks {
            session_id,
        }));
        let ServerResponse::Tasks { tasks } = listed.result.unwrap() else {
            panic!("unexpected task list response");
        };
        assert!(tasks.iter().any(|task| task.id == task_id));
        let task_events =
            connection.request(RequestEnvelope::new(ClientRequest::GetSessionTaskEvents {
                session_id,
                task_id,
                after_sequence: None,
            }));
        let ServerResponse::TaskEvents { events } = task_events.result.unwrap() else {
            panic!("unexpected task event response");
        };
        assert!(
            events
                .iter()
                .any(|event| matches!(event.event, TaskEvent::Completed { .. }))
        );

        let control = connection.request(RequestEnvelope::new(
            ClientRequest::TakeSessionFilesystemControl {
                session_id,
                control: WorkspaceControl::User,
            },
        ));
        assert!(matches!(
            control.result,
            Ok(ServerResponse::WorkspaceControl(WorkspaceControl::User))
        ));
        fs::remove_dir_all(&backend.session_root_base).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn policy_decisions_are_visible_and_can_stop_agent_writes() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m2(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Policy workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "M2 policy".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let default_settings = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot { session_id },
        ));
        let ServerResponse::AgentSessionSnapshot(default_settings) =
            default_settings.result.unwrap()
        else {
            panic!("unexpected session snapshot response");
        };
        assert!(default_settings.auto_approve_actions);
        assert_eq!(
            default_settings.approval_policy,
            loom_core::ApprovalPolicy::auto_approve()
        );
        let other_session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Other session".to_owned(),
            },
        ));
        let other_session_id = match other_session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let policy = loom_core::ApprovalPolicy {
            write: PolicyDecision::Deny,
            ..Default::default()
        };
        let policy_response = connection.request(RequestEnvelope::new(
            ClientRequest::SetSessionApprovalPolicy {
                session_id,
                policy,
                auto_approve_actions: Some(false),
            },
        ));
        assert!(matches!(
            policy_response.result,
            Ok(ServerResponse::ApprovalPolicy(_))
        ));
        let other_settings = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot {
                session_id: other_session_id,
            },
        ));
        let ServerResponse::AgentSessionSnapshot(other_settings) = other_settings.result.unwrap()
        else {
            panic!("unexpected session snapshot response");
        };
        assert!(other_settings.auto_approve_actions);
        assert_eq!(
            other_settings.approval_policy,
            loom_core::ApprovalPolicy::auto_approve()
        );
        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "attempt a write".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: None,
                repository_instructions: None,
            }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        await_settled_run(&connection, run_id);
        let events = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        }));
        let ServerResponse::SessionEvents { events, .. } = events.result.unwrap() else {
            panic!("unexpected session event response");
        };
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                ServerEvent::Agent {
                    event: loom_agent::AgentEvent::ToolPolicyEvaluated {
                        run_id: event_run,
                        evaluation,
                        ..
                    }
                } if *event_run == run_id && evaluation.decision == PolicyDecision::Deny
            )
        }));
        let run = connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        let ServerResponse::AgentRun(snapshot) = run.result.unwrap() else {
            panic!("unexpected run response");
        };
        assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn persistent_backend_recovers_transcript_workspace_and_pending_approval() {
        let persistence =
            std::env::temp_dir().join(format!("loom-server-state-{}.db", WorkspaceId::new()));
        let session_root_base;
        let (session_id, run_id, approval) = {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            session_root_base = backend.session_root_base.clone();
            let connection = backend.connect();
            negotiate_m3(&connection);
            let created =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Durable workspace".to_owned(),
                }));
            let ServerResponse::WorkspaceCreated(workspace) = created.result.unwrap() else {
                panic!("unexpected workspace response");
            };
            let session = connection.request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "durable run".to_owned(),
                },
            ));
            let session_id = match session.result.unwrap() {
                ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
                response => panic!("unexpected response: {response:?}"),
            };
            connection.request(RequestEnvelope::new(
                ClientRequest::SetSessionApprovalPolicy {
                    session_id,
                    policy: ApprovalPolicy::default(),
                    auto_approve_actions: Some(false),
                },
            ));
            let started = connection.request(RequestEnvelope::new(
                ClientRequest::StartSessionAgentRunWithOptions {
                    session_id,
                    task: "create a demo file".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    system_instructions: Some("Be concise.".to_owned()),
                    repository_instructions: Some("Keep changes focused.".to_owned()),
                    limits: loom_core::SessionLimits {
                        max_tool_calls: Some(20),
                        ..Default::default()
                    },
                    context: ContextAssemblyOptions {
                        context_window: Some(8_192),
                        max_input_tokens: Some(4_096),
                        reserved_output_tokens: Some(1_024),
                    },
                },
            ));
            let run_id = match started.result.unwrap() {
                ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
                response => panic!("unexpected response: {response:?}"),
            };
            await_settled_run(&connection, run_id);
            backend.flush().unwrap();
            let persisted = FilePersistence::open(&persistence).unwrap();
            assert!(
                persisted
                    .load_section::<serde_json::Value>(
                        &format!("run:{run_id}"),
                        CURRENT_SCHEMA_VERSION
                    )
                    .unwrap()
                    .is_none(),
                "run runtime snapshots must not be stored in generic JSON sections"
            );
            let runtime_config = persisted.load_run_runtime_config(run_id).unwrap().unwrap();
            assert_eq!(
                runtime_config.system_instructions.as_deref(),
                Some("Be concise.")
            );
            assert_eq!(
                runtime_config.repository_instructions.as_deref(),
                Some("Keep changes focused.")
            );
            assert_eq!(runtime_config.context_options.context_window, Some(8_192));
            assert_eq!(runtime_config.limits.max_tool_calls, Some(20));
            let events = match connection
                .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(session_id),
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                }))
                .result
                .unwrap()
            {
                ServerResponse::SessionEvents { events, .. } => events,
                response => panic!("unexpected response: {response:?}"),
            };
            let approval = events
                .iter()
                .find_map(|event| match &event.event {
                    ServerEvent::Agent {
                        event:
                            AgentEvent::ToolApprovalRequired {
                                call,
                                attempt_id,
                                control_revision,
                                ..
                            },
                    } => Some((call.id, *attempt_id, *control_revision)),
                    _ => None,
                })
                .unwrap();
            (session_id, run_id, approval)
        };
        assert!(persistence.is_file());

        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        assert!(backend.journal().unwrap().events.is_empty());
        assert!(backend.runs().unwrap().is_empty());
        assert!(backend.persisted_runs().unwrap().contains_key(&run_id));
        let connection = backend.connect();
        negotiate_m3(&connection);
        let recovered_session = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot { session_id },
        ));
        let ServerResponse::AgentSessionSnapshot(recovered_session) =
            recovered_session.result.unwrap()
        else {
            panic!("unexpected recovered session snapshot response");
        };
        assert!(!recovered_session.auto_approve_actions);
        assert_eq!(recovered_session.approval_policy, ApprovalPolicy::default());
        let detail = connection.request(RequestEnvelope::new(ClientRequest::GetAgentRunSnapshot {
            run_id,
        }));
        let ServerResponse::AgentRunSnapshot(detail) = detail.result.unwrap() else {
            panic!("unexpected run snapshot response");
        };
        assert_eq!(detail.run.id, run_id);
        assert!(detail.messages.iter().any(|message| {
            message.role == loom_model::MessageRole::User
                && message.content.contains("create a demo file")
        }));
        assert!(detail.messages.iter().any(|message| {
            message.role == loom_model::MessageRole::Assistant && !message.tool_calls.is_empty()
        }));
        assert!(
            detail
                .activities
                .iter()
                .any(|activity| { activity.status == AgentActivityStatus::AwaitingApproval })
        );
        assert!(backend.runs().unwrap().is_empty());
        let recovered =
            connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        let ServerResponse::AgentRun(snapshot) = recovered.result.unwrap() else {
            panic!("unexpected run response");
        };
        assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);
        assert_eq!(snapshot.attempt_id, approval.1);
        assert_eq!(snapshot.control_revision, approval.2);
        let reconstructed_state = connection.run_handle(run_id).unwrap().state();
        assert_eq!(
            reconstructed_state.task.system_instructions.as_deref(),
            Some("Be concise.")
        );
        assert_eq!(
            reconstructed_state.options.context.context_window,
            Some(8_192)
        );
        assert_eq!(reconstructed_state.options.limits.max_tool_calls, Some(20));
        let recovered_interactions = backend
            .persistence
            .as_ref()
            .unwrap()
            .load_run_interactions(run_id)
            .unwrap();
        assert!(recovered_interactions.iter().any(|interaction| {
            interaction.attempt_id == approval.1
                && interaction.control_revision == approval.2
                && interaction.tool_call_id == Some(approval.0)
                && interaction.status == AgentInteractionStatus::Pending
        }));
        let recovered_snapshot =
            connection.request(RequestEnvelope::new(ClientRequest::GetAgentRunSnapshot {
                run_id,
            }));
        let ServerResponse::AgentRunSnapshot(projection) = recovered_snapshot.result.unwrap()
        else {
            panic!("unexpected run snapshot response");
        };
        assert!(!projection.activities.is_empty());
        assert!(
            projection
                .activities
                .iter()
                .any(|activity| activity.status == AgentActivityStatus::AwaitingApproval)
        );
        let page = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal: None,
                limit: MAX_AGENT_RUN_MESSAGE_PAGE_SIZE,
            },
        ));
        let ServerResponse::AgentRunMessagePage {
            run_id: page_run_id,
            messages,
        } = page.result.unwrap()
        else {
            panic!("unexpected run message page response");
        };
        assert_eq!(page_run_id, run_id);
        assert!(!messages.is_empty());
        assert!(
            messages
                .windows(2)
                .all(|pair| pair[0].ordinal > pair[1].ordinal),
            "message page must be in descending keyset order"
        );
        let oversized_page = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal: None,
                limit: MAX_AGENT_RUN_MESSAGE_PAGE_SIZE + 1,
            },
        ));
        assert_eq!(
            oversized_page.result.unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let message_header = messages
            .iter()
            .find(|message| message.content_bytes > 0)
            .unwrap();
        let range = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal: message_header.ordinal,
                byte_offset: 0,
                length: message_header.content_bytes.min(32) as u32,
            },
        ));
        let ServerResponse::AgentRunMessageContentRange {
            run_id: range_run_id,
            message_ordinal,
            byte_offset,
            content,
        } = range.result.unwrap()
        else {
            panic!("unexpected run message content response");
        };
        assert_eq!(range_run_id, run_id);
        assert_eq!(message_ordinal, message_header.ordinal);
        assert_eq!(byte_offset, 0);
        assert!(!content.is_empty());
        let expected_content = projection.messages[message_ordinal as usize]
            .content
            .as_bytes();
        assert_eq!(content, expected_content[..content.len()]);
        let oversized_range = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal,
                byte_offset: 0,
                length: MAX_AGENT_RUN_MESSAGE_CONTENT_RANGE_BYTES + 1,
            },
        ));
        assert_eq!(
            oversized_range.result.unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let legacy_connection = backend.connect();
        let legacy_negotiation =
            legacy_connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
                client_version: ProtocolVersion::new(2, 0),
                capabilities: backend.supported_capabilities.clone(),
            }));
        assert_eq!(
            legacy_negotiation.result.unwrap_err().code,
            ErrorCode::UnsupportedProtocol
        );
        let protocol_3_connection = backend.connect();
        let protocol_3_negotiation =
            protocol_3_connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
                client_version: ProtocolVersion::new(3, 0),
                capabilities: backend.supported_capabilities.clone(),
            }));
        assert_eq!(
            protocol_3_negotiation.result.unwrap_err().code,
            ErrorCode::UnsupportedProtocol
        );

        let capability_limited_connection = backend.connect();
        let capability_limited = CapabilitySet::new(
            backend
                .supported_capabilities
                .iter()
                .copied()
                .filter(|capability| *capability != Capability::ReadAgentRunMessages),
        );
        let current_negotiation =
            capability_limited_connection.request(RequestEnvelope::new(ClientRequest::Negotiate {
                client_version: CURRENT_PROTOCOL_VERSION,
                capabilities: capability_limited,
            }));
        assert!(matches!(
            current_negotiation.result,
            Ok(ServerResponse::Negotiated(_))
        ));
        let unsupported_page = capability_limited_connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal: None,
                limit: MAX_AGENT_RUN_MESSAGE_PAGE_SIZE,
            },
        ));
        assert_eq!(
            unsupported_page.result.unwrap_err().code,
            ErrorCode::CapabilityDenied
        );
        let events = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: None,
            stream_epoch: None,
        }));
        let ServerResponse::SessionEvents { events, .. } = events.result.unwrap() else {
            panic!("unexpected events response");
        };
        assert!(events.len() >= 5);
        let checkpoint =
            connection.request(RequestEnvelope::new(ClientRequest::GetRunCheckpoint {
                run_id,
            }));
        let ServerResponse::RunCheckpoint(checkpoint) = checkpoint.result.unwrap() else {
            panic!("unexpected checkpoint response");
        };
        assert_eq!(checkpoint.session_id, session_id);

        let wrong_attempt =
            connection.request(RequestEnvelope::new(ClientRequest::ApproveAgentAction {
                run_id,
                attempt_id: loom_core::RunAttemptId::new(),
                expected_control_revision: approval.2,
                tool_call_id: approval.0,
            }));
        assert_eq!(wrong_attempt.result.unwrap_err().code, ErrorCode::Conflict);
        let stale_revision =
            connection.request(RequestEnvelope::new(ClientRequest::ApproveAgentAction {
                run_id,
                attempt_id: approval.1,
                expected_control_revision: approval.2.saturating_sub(1),
                tool_call_id: approval.0,
            }));
        assert_eq!(stale_revision.result.unwrap_err().code, ErrorCode::Conflict);

        let response =
            connection.request(RequestEnvelope::new(ClientRequest::ApproveAgentAction {
                run_id,
                attempt_id: approval.1,
                expected_control_revision: approval.2,
                tool_call_id: approval.0,
            }));
        assert!(response.result.is_ok());
        await_settled_run(&connection, run_id);
        let resolved_interactions = backend
            .persistence
            .as_ref()
            .unwrap()
            .load_run_interactions(run_id)
            .unwrap();
        assert!(resolved_interactions.iter().any(|interaction| {
            interaction.tool_call_id == Some(approval.0)
                && interaction.status == AgentInteractionStatus::Approved
                && interaction.decision == Some(ApprovalDecision::Approved)
        }));
        let command_approval = (0..1_000)
            .find_map(|_| {
                let events = match connection
                    .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                        session_id: Some(session_id),
                        workspace_id: None,
                        after_sequence: None,
                        stream_epoch: None,
                    }))
                    .result
                    .unwrap()
                {
                    ServerResponse::SessionEvents { events, .. } => events,
                    response => panic!("unexpected events response: {response:?}"),
                };
                let approval = events.iter().find_map(|event| match &event.event {
                    ServerEvent::Agent {
                        event:
                            AgentEvent::ToolApprovalRequired {
                                call,
                                attempt_id,
                                control_revision,
                                ..
                            },
                    } if call.name == "run_command" => {
                        Some((call.id, *attempt_id, *control_revision))
                    }
                    _ => None,
                });
                approval.or_else(|| {
                    thread::sleep(Duration::from_millis(5));
                    None
                })
            })
            .expect("run_command approval did not arrive");
        let response =
            connection.request(RequestEnvelope::new(ClientRequest::ApproveAgentAction {
                run_id,
                attempt_id: command_approval.1,
                expected_control_revision: command_approval.2,
                tool_call_id: command_approval.0,
            }));
        assert!(response.result.is_ok());
        let mut usage = match connection
            .request(RequestEnvelope::new(ClientRequest::GetRunUsage { run_id }))
            .result
            .unwrap()
        {
            ServerResponse::RunUsage { usage, .. } => usage,
            response => panic!("unexpected usage response: {response:?}"),
        };
        for _ in 0..1_000 {
            if usage.input_tokens > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(5));
            usage = match connection
                .request(RequestEnvelope::new(ClientRequest::GetRunUsage { run_id }))
                .result
                .unwrap()
            {
                ServerResponse::RunUsage { usage, .. } => usage,
                response => panic!("unexpected usage response: {response:?}"),
            };
        }
        assert_eq!(usage.input_tokens, 240);
        assert_eq!(usage.output_tokens, 52);
        assert_eq!(usage.tool_calls, 3);
        let filesystem = connection.request(RequestEnvelope::new(
            ClientRequest::GetSessionFilesystemSnapshot { session_id },
        ));
        assert!(matches!(
            filesystem.result,
            Ok(ServerResponse::SessionFilesystemSnapshot(_))
        ));
        backend.shutdown().unwrap();
        backend.shutdown().unwrap();
        assert_eq!(
            connection
                .request(RequestEnvelope::new(ClientRequest::GetRunUsage { run_id }))
                .result
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        drop(connection);
        drop(backend);
        let reopened = InProcessBackend::new_persistent(&persistence).unwrap();
        let reopened_connection = reopened.connect();
        negotiate_m3(&reopened_connection);
        let recovered_usage = reopened_connection
            .request(RequestEnvelope::new(ClientRequest::GetRunUsage { run_id }));
        let ServerResponse::RunUsage { usage, .. } = recovered_usage.result.unwrap() else {
            panic!("unexpected recovered usage response");
        };
        assert_eq!(usage.input_tokens, 240);
        assert_eq!(usage.output_tokens, 52);
        let before_retry = reopened_connection
            .request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        let ServerResponse::AgentRun(before_retry) = before_retry.result.unwrap() else {
            panic!("unexpected run response before checkpoint retry");
        };
        let prior_attempts = reopened
            .persistence
            .as_ref()
            .unwrap()
            .load_run_attempts(run_id)
            .unwrap();
        assert_eq!(prior_attempts.len(), 1);
        assert_eq!(prior_attempts[0].id, before_retry.attempt_id);
        let retried = reopened_connection.request(RequestEnvelope::new(
            ClientRequest::RetryAgentFromCheckpoint {
                run_id,
                checkpoint_id: checkpoint.id,
            },
        ));
        let ServerResponse::AgentRun(retried) = retried.result.unwrap() else {
            panic!("unexpected checkpoint retry response");
        };
        assert_ne!(retried.attempt_id, before_retry.attempt_id);
        assert_eq!(
            await_settled_run(&reopened_connection, run_id).state,
            AgentRunState::AwaitingApproval
        );
        let after_retry = reopened_connection
            .request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        let ServerResponse::AgentRun(after_retry) = after_retry.result.unwrap() else {
            panic!("unexpected run response after checkpoint retry");
        };
        assert_eq!(after_retry.attempt_id, retried.attempt_id);
        let mut attempts = Vec::new();
        for _ in 0..1_000 {
            attempts = reopened
                .persistence
                .as_ref()
                .unwrap()
                .load_run_attempts(run_id)
                .unwrap();
            if attempts
                .last()
                .is_some_and(|attempt| attempt.state == AgentRunState::AwaitingApproval)
            {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].id, before_retry.attempt_id);
        assert_eq!(attempts[0].number, 1);
        assert_eq!(attempts[1].id, retried.attempt_id);
        assert_eq!(attempts[1].number, 2);
        assert_eq!(attempts[1].state, AgentRunState::AwaitingApproval);
        reopened.shutdown().unwrap();
        drop(reopened_connection);
        drop(reopened);

        // Model a crash after execution started but before the runtime could
        // persist its paused recovery state.
        let persistence_store = FilePersistence::open(&persistence).unwrap();
        let mut summary = persistence_store.load_run_summary(run_id).unwrap().unwrap();
        summary.snapshot.state = AgentRunState::Executing;
        summary.snapshot.completed_at = None;
        let mut execution = persistence_store
            .load_run_execution_state(run_id)
            .unwrap()
            .unwrap();
        execution.state = AgentRunState::Executing;
        execution.pending_approval = None;
        execution.pending_input = None;
        summary.execution_state = Some(execution);
        let mut attempts = persistence_store.load_run_attempts(run_id).unwrap();
        let current_attempt = attempts.last_mut().unwrap();
        current_attempt.state = AgentRunState::Executing;
        current_attempt.completed_at = None;
        summary.attempts = Some(attempts);
        let summaries = BTreeMap::from([(run_id, summary)]);
        let sessions = persistence_store.load_sessions().unwrap().unwrap();
        persistence_store
            .save_state(DurableStateWrite {
                schema_version: CURRENT_SCHEMA_VERSION,
                sessions: &sessions,
                workspaces: None,
                settings: None,
                workspace_configs: None,
                providers: None,
                usage: None,
                idempotency: None,
                run_summaries: Some(&summaries),
                run_runtime_configs: None,
                run_context_checkpoints: None,
                run_plans: None,
                run_messages: None,
                run_activities: None,
                filesystem_records: None,
                records: &[],
                feed: None,
                sections: &[],
            })
            .unwrap();
        drop(persistence_store);

        let restored = InProcessBackend::new_persistent(&persistence).unwrap();
        assert!(restored.runs().unwrap().is_empty());
        assert_eq!(
            restored
                .persisted_runs()
                .unwrap()
                .get(&run_id)
                .unwrap()
                .snapshot
                .state,
            AgentRunState::Paused
        );
        let recovery_session_id = restored
            .persisted_runs()
            .unwrap()
            .get(&run_id)
            .unwrap()
            .snapshot
            .session_id;
        let restored_connection = restored.connect();
        negotiate_m3(&restored_connection);
        let metadata = restored_connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshotMetadata {
                session_id: recovery_session_id,
            },
        ));
        let ServerResponse::AgentSessionSnapshot(metadata) = metadata.result.unwrap() else {
            panic!("unexpected metadata session snapshot response");
        };
        assert!(
            metadata
                .active_run
                .as_ref()
                .is_some_and(|projection| projection.messages.is_empty())
        );
        let initial = restored_connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionInitialState {
                session_id: recovery_session_id,
            },
        ));
        let ServerResponse::AgentSessionInitialState(initial) = initial.result.unwrap() else {
            panic!("unexpected initial session state response");
        };
        assert_eq!(initial.cursor, initial.projection.latest_sequence);
        assert!(
            initial
                .projection
                .active_run
                .as_ref()
                .is_some_and(|projection| projection.messages.is_empty())
        );
        assert!(restored.runs().unwrap().is_empty());
        let page = restored_connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessagePage {
                run_id,
                before_ordinal: None,
                limit: 10,
            },
        ));
        let ServerResponse::AgentRunMessagePage { messages, .. } = page.result.unwrap() else {
            panic!("unexpected transcript page response");
        };
        assert!(!messages.is_empty());
        let first = messages.first().unwrap();
        assert!(first.content_bytes > 0);
        let content = restored_connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunMessageContentRange {
                run_id,
                message_ordinal: first.ordinal,
                byte_offset: 0,
                length: u32::try_from(first.content_bytes.min(128)).unwrap(),
            },
        ));
        let ServerResponse::AgentRunMessageContentRange { content, .. } = content.result.unwrap()
        else {
            panic!("unexpected transcript content response");
        };
        assert!(!content.is_empty());
        let invalid_page = restored_connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunTranscriptPage {
                run_id,
                before_ordinal: None,
                limit: 0,
            },
        ));
        assert_eq!(
            invalid_page.result.unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let transcript_page = restored_connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentRunTranscriptPage {
                run_id,
                before_ordinal: None,
                limit: MAX_AGENT_RUN_TRANSCRIPT_PAGE_SIZE,
            },
        ));
        let ServerResponse::AgentRunTranscriptPage {
            messages,
            next_before,
            has_older,
            ..
        } = transcript_page.result.unwrap()
        else {
            panic!("unexpected bounded transcript page response");
        };
        assert!(!messages.is_empty());
        assert!(
            messages
                .windows(2)
                .all(|pair| pair[0].ordinal < pair[1].ordinal)
        );
        assert_eq!(next_before, messages.first().map(|message| message.ordinal));
        assert!(!has_older);

        let projection =
            restored_connection.request(RequestEnvelope::new(ClientRequest::GetAgentRunSnapshot {
                run_id,
            }));
        let ServerResponse::AgentRunSnapshot(projection) = projection.result.unwrap() else {
            panic!("unexpected lazily restored run snapshot response");
        };
        assert_eq!(projection.run.state, AgentRunState::Paused);
        assert!(!projection.messages.is_empty());
        assert!(restored.runs().unwrap().is_empty());
        let execution = restored
            .persistence
            .as_ref()
            .unwrap()
            .load_run_execution_state(run_id)
            .unwrap()
            .unwrap();
        assert_eq!(execution.state, AgentRunState::Paused);
        assert!(execution.pending_tool_execution.is_none());
        let events =
            restored_connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                session_id: Some(recovery_session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            }));
        let ServerResponse::SessionEvents { events, .. } = events.result.unwrap() else {
            panic!("unexpected recovery event response");
        };
        assert!(events.iter().any(|event| matches!(
            &event.event,
            ServerEvent::Agent {
                event: AgentEvent::RunStateChanged {
                    run_id: event_run_id,
                    state: AgentRunState::Paused,
                }
            } if *event_run_id == run_id
        )));
        drop(restored);
        fs::remove_file(persistence).unwrap();
        fs::remove_dir_all(session_root_base).unwrap();
    }

    #[test]
    fn completed_runs_keep_indexed_summaries_without_restoring_runtime_objects() {
        let persistence =
            std::env::temp_dir().join(format!("loom-server-state-{}.db", WorkspaceId::new()));
        let (run_id, session_root_base) = {
            let backend = InProcessBackend::new_persistent(&persistence).unwrap();
            let session_root_base = backend.session_root_base.clone();
            let connection = backend.connect();
            negotiate_m3(&connection);
            let workspace =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Run summary workspace".to_owned(),
                }));
            let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
                panic!("unexpected workspace response");
            };
            let session = connection.request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Completed history".to_owned(),
                },
            ));
            let ServerResponse::AgentSessionCreated(session) = session.result.unwrap() else {
                panic!("unexpected session response");
            };
            let started =
                connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                    session_id: session.id,
                    task: "answer briefly".to_owned(),
                    model: ModelId::new("deterministic/demo"),
                    system_instructions: None,
                    repository_instructions: None,
                }));
            let run_id = match started.result.unwrap() {
                ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
                response => panic!("unexpected response: {response:?}"),
            };
            let settled = await_settled_run(&connection, run_id);
            assert_eq!(settled.state, AgentRunState::Completed);
            backend.flush().unwrap();
            backend.shutdown().unwrap();
            (run_id, session_root_base)
        };

        let backend = InProcessBackend::new_persistent(&persistence).unwrap();
        assert!(backend.runs().unwrap().is_empty());
        assert!(backend.persisted_runs().unwrap().is_empty());
        let connection = backend.connect();
        negotiate_m3(&connection);
        let response =
            connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        assert!(matches!(
            response.result,
            Ok(ServerResponse::AgentRun(snapshot)) if snapshot.state == AgentRunState::Completed
        ));
        let projection =
            connection.request(RequestEnvelope::new(ClientRequest::GetAgentRunSnapshot {
                run_id,
            }));
        assert!(matches!(
            projection.result,
            Ok(ServerResponse::AgentRunSnapshot(snapshot))
                if snapshot.run.state == AgentRunState::Completed && !snapshot.messages.is_empty()
        ));
        assert!(backend.runs().unwrap().is_empty());
        fs::remove_dir_all(session_root_base).unwrap();
        let _ = fs::remove_file(persistence);
    }

    #[test]
    fn pause_resume_fork_and_provider_discovery_are_protocol_operations() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Control workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "control run".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        connection.request(RequestEnvelope::new(
            ClientRequest::SetSessionApprovalPolicy {
                session_id,
                policy: ApprovalPolicy::default(),
                auto_approve_actions: Some(false),
            },
        ));
        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "control".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: None,
                repository_instructions: None,
            }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        await_settled_run(&connection, run_id);
        let paused = connection.request(RequestEnvelope::new(ClientRequest::PauseAgentRun {
            run_id,
        }));
        let ServerResponse::AgentRun(snapshot) = paused.result.unwrap() else {
            panic!("unexpected pause response");
        };
        assert_eq!(snapshot.state, AgentRunState::Paused);
        let resumed = connection.request(RequestEnvelope::new(ClientRequest::ResumeAgentRun {
            run_id,
        }));
        let ServerResponse::AgentRun(snapshot) = resumed.result.unwrap() else {
            panic!("unexpected resume response");
        };
        assert_eq!(snapshot.state, AgentRunState::AwaitingApproval);

        let forked = connection.request(RequestEnvelope::new(ClientRequest::ForkAgentSession {
            session_id,
            name: "control fork".to_owned(),
        }));
        let forked_id = match forked.result.unwrap() {
            ServerResponse::AgentSessionForked(snapshot) => snapshot.id,
            response => panic!("unexpected fork response: {response:?}"),
        };
        assert_ne!(forked_id, session_id);
        let forked_settings = connection.request(RequestEnvelope::new(
            ClientRequest::GetAgentSessionSnapshot {
                session_id: forked_id,
            },
        ));
        let ServerResponse::AgentSessionSnapshot(forked_settings) = forked_settings.result.unwrap()
        else {
            panic!("unexpected forked session snapshot response");
        };
        assert!(!forked_settings.auto_approve_actions);
        assert_eq!(forked_settings.approval_policy, ApprovalPolicy::default());

        let providers = connection.request(RequestEnvelope::new(ClientRequest::ListProviders));
        let ServerResponse::Providers { providers } = providers.result.unwrap() else {
            panic!("unexpected provider response");
        };
        assert!(
            providers
                .iter()
                .any(|provider| provider.kind == loom_providers::ProviderKind::Ollama)
        );
        assert!(
            providers
                .iter()
                .any(|provider| provider.kind == loom_providers::ProviderKind::Deterministic)
        );
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn explicit_limits_and_context_inspection_are_durable_protocol_state() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Limited workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "limited run".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started = connection.request(RequestEnvelope::new(
            ClientRequest::StartSessionAgentRunWithOptions {
                session_id,
                task: "limited".to_owned(),
                model: ModelId::new("deterministic/demo"),
                system_instructions: Some("system".to_owned()),
                repository_instructions: Some("repository".to_owned()),
                limits: loom_core::SessionLimits {
                    max_tool_calls: Some(0),
                    ..Default::default()
                },
                context: ContextAssemblyOptions {
                    context_window: Some(1_024),
                    max_input_tokens: Some(512),
                    reserved_output_tokens: Some(128),
                },
            },
        ));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        assert_eq!(
            await_settled_run(&connection, run_id).state,
            AgentRunState::Failed
        );
        let events = match connection
            .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            }))
            .result
            .unwrap()
        {
            ServerResponse::SessionEvents { events, .. } => events,
            response => panic!("unexpected response: {response:?}"),
        };
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                ServerEvent::Agent {
                    event: AgentEvent::RunLimitReached { status, .. }
                } if status.exceeded.contains(&loom_core::LimitKind::ToolCalls)
            )
        }));
        let usage = connection.request(RequestEnvelope::new(ClientRequest::GetSessionUsage {
            session_id,
        }));
        let ServerResponse::SessionUsage { usage, .. } = usage.result.unwrap() else {
            panic!("unexpected session usage response");
        };
        assert_eq!(usage.tool_calls, 0);
        let context =
            connection.request(RequestEnvelope::new(ClientRequest::InspectAgentContext {
                run_id,
            }));
        assert_eq!(context.result.unwrap_err().code, ErrorCode::InvalidState);
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn non_sqlite_persistence_file_is_rejected_without_fallback() {
        let path =
            std::env::temp_dir().join(format!("loom-server-malformed-{}.db", WorkspaceId::new()));
        fs::write(&path, br#"{"schema_version":1,"state":{"broken":true}}"#).unwrap();
        let error = match InProcessBackend::new_persistent(&path) {
            Ok(_) => panic!("non-SQLite persistence unexpectedly loaded"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::Persistence);
        assert!(!path.with_extension("json.legacy").exists());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn m5_workspace_context_vcs_and_task_evidence_are_authoritative() {
        let root = workspace();
        fs::write(root.join("README.md"), "fn answer() {\n TODO\n}\n").unwrap();
        let git = |arguments: &[&str]| {
            assert!(
                Command::new("git")
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_WORK_TREE")
                    .env_remove("GIT_INDEX_FILE")
                    .env_remove("GIT_COMMON_DIR")
                    .args(arguments)
                    .current_dir(&root)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "loom@example.test"]);
        git(&["config", "user.name", "Loom Test"]);
        git(&["add", "--", "README.md"]);
        git(&["commit", "-qm", "initial"]);

        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Context workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let created = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "Context session".to_owned(),
            },
        ));
        let ServerResponse::AgentSessionCreated(session) = created.result.unwrap() else {
            panic!("unexpected session response");
        };
        let session_id = session.id;
        let attached = connection.request(RequestEnvelope::new(
            ClientRequest::AttachSessionRepository {
                session_id,
                source: root.display().to_string(),
                path: "repo".to_owned(),
                revision: None,
            },
        ));
        assert!(matches!(
            attached.result,
            Ok(ServerResponse::SessionRepositoryAttached(_))
        ));
        let context = connection.request(RequestEnvelope::new(
            ClientRequest::GetSessionContextFiles { session_id },
        ));
        assert!(matches!(
            context.result,
            Ok(ServerResponse::ContextFiles { .. })
        ));
        let repositories = connection.request(RequestEnvelope::new(
            ClientRequest::ListSessionRepositories { session_id },
        ));
        let ServerResponse::SessionRepositories { repositories } = repositories.result.unwrap()
        else {
            panic!("unexpected session repositories");
        };
        let vcs = connection.request(RequestEnvelope::new(ClientRequest::GetSessionVcsStatus {
            session_id,
            repository_id: repositories[0].id,
        }));
        assert!(matches!(vcs.result, Ok(ServerResponse::VcsStatus(_))));

        let task = connection.request(RequestEnvelope::new(ClientRequest::StartSessionTask {
            session_id,
            spec: TaskSpec {
                kind: TaskKind::Test,
                label: "evidence fixture".to_owned(),
                command: if cfg!(windows) {
                    "cmd".to_owned()
                } else {
                    "printf".to_owned()
                },
                args: if cfg!(windows) {
                    vec!["/C".to_owned(), "ok".to_owned()]
                } else {
                    vec!["ok".to_owned()]
                },
                cwd: None,
                output_limit_bytes: Some(128),
                artifact_paths: Vec::new(),
            },
        }));
        let task_id = match task.result.unwrap() {
            ServerResponse::TaskStarted(task) => task.id,
            response => panic!("unexpected task response: {response:?}"),
        };
        for _ in 0..100 {
            let current = connection.request(RequestEnvelope::new(ClientRequest::GetSessionTask {
                session_id,
                task_id,
            }));
            if let Ok(ServerResponse::Task(snapshot)) = current.result
                && matches!(
                    snapshot.status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                )
            {
                let evidence = connection.request(RequestEnvelope::new(
                    ClientRequest::GetSessionTaskEvidence {
                        session_id,
                        task_id,
                    },
                ));
                assert!(matches!(
                    evidence.result,
                    Ok(ServerResponse::TaskEvidence { .. })
                ));
                fs::remove_dir_all(&backend.session_root_base).unwrap();
                fs::remove_dir_all(root).unwrap();
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("task evidence fixture did not finish");
    }

    #[test]
    fn protocol_filesystem_requests_cover_snapshots_edits_checkpoints_and_undo() {
        let backend = InProcessBackend::new();
        let connection = backend.connect();
        negotiate_m5(&connection);
        let workspace = match connection
            .request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                name: "Filesystem protocol".to_owned(),
            }))
            .result
            .unwrap()
        {
            ServerResponse::WorkspaceCreated(workspace) => workspace,
            response => panic!("unexpected workspace response: {response:?}"),
        };
        let session_id = match connection
            .request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Filesystem session".to_owned(),
                },
            ))
            .result
            .unwrap()
        {
            ServerResponse::AgentSessionCreated(session) => session.id,
            response => panic!("unexpected session response: {response:?}"),
        };

        let created = connection.request(RequestEnvelope::new(
            ClientRequest::ApplySessionFilesystemEdit {
                session_id,
                edit: WorkspaceEdit {
                    path: "notes/plan.md".to_owned(),
                    old_text: String::new(),
                    new_text: "first version".to_owned(),
                    expected_revision: None,
                },
            },
        ));
        assert!(matches!(
            created.result,
            Ok(ServerResponse::WorkspaceEditApplied(_))
        ));
        let checkpoint_id = match connection
            .request(RequestEnvelope::new(
                ClientRequest::CreateSessionCheckpoint {
                    session_id,
                    label: "before update".to_owned(),
                },
            ))
            .result
            .unwrap()
        {
            ServerResponse::CheckpointCreated(checkpoint) => checkpoint.id,
            response => panic!("unexpected checkpoint response: {response:?}"),
        };
        let read = connection.request(RequestEnvelope::new(ClientRequest::ReadSessionFile {
            session_id,
            path: "notes/plan.md".to_owned(),
        }));
        let revision = match read.result.unwrap() {
            ServerResponse::SessionFilesystemFile(file) => {
                assert_eq!(file.content, "first version");
                file.revision
            }
            response => panic!("unexpected file response: {response:?}"),
        };
        assert!(matches!(
            connection
                .request(RequestEnvelope::new(
                    ClientRequest::RevertSessionCheckpoint {
                        session_id,
                        checkpoint_id,
                    },
                ))
                .result,
            Ok(ServerResponse::CheckpointReverted(_))
        ));
        let updated = connection.request(RequestEnvelope::new(
            ClientRequest::ApplySessionFilesystemEdit {
                session_id,
                edit: WorkspaceEdit {
                    path: "notes/plan.md".to_owned(),
                    old_text: "first".to_owned(),
                    new_text: "second".to_owned(),
                    expected_revision: Some(revision),
                },
            },
        ));
        assert!(matches!(
            updated.result,
            Ok(ServerResponse::WorkspaceEditApplied(_))
        ));
        let undo = connection.request(RequestEnvelope::new(ClientRequest::UndoSessionEdit {
            session_id,
        }));
        assert_eq!(undo.result.unwrap_err().code, ErrorCode::InvalidState);
        assert!(matches!(
            connection
                .request(RequestEnvelope::new(
                    ClientRequest::GetSessionFilesystemSnapshot { session_id },
                ))
                .result,
            Ok(ServerResponse::SessionFilesystemSnapshot(_))
        ));
        assert!(matches!(
            connection
                .request(RequestEnvelope::new(
                    ClientRequest::GetSessionFilesystemChanges {
                        session_id,
                        after_sequence: None,
                    },
                ))
                .result,
            Ok(ServerResponse::SessionFilesystemChanges { .. })
        ));
        assert!(matches!(
            connection
                .request(RequestEnvelope::new(
                    ClientRequest::GetSessionContextFiles { session_id }
                ))
                .result,
            Ok(ServerResponse::ContextFiles { .. })
        ));
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn session_scoped_tokens_are_limited_to_their_sessions_and_source_roots() {
        let backend = InProcessBackend::new();
        let unrestricted = backend.connect();
        negotiate(&unrestricted);
        let workspace = match unrestricted
            .request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                name: "Scoped access".to_owned(),
            }))
            .result
            .unwrap()
        {
            ServerResponse::WorkspaceCreated(workspace) => workspace,
            response => panic!("unexpected workspace response: {response:?}"),
        };
        let session_id = match unrestricted
            .request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Authorized session".to_owned(),
                },
            ))
            .result
            .unwrap()
        {
            ServerResponse::AgentSessionCreated(session) => session.id,
            response => panic!("unexpected session response: {response:?}"),
        };

        assert!(matches!(
            unrestricted
                .request(RequestEnvelope::new(ClientRequest::ListWorkspaces))
                .result,
            Ok(ServerResponse::Workspaces { .. })
        ));
        assert!(matches!(
            unrestricted
                .request(RequestEnvelope::new(
                    ClientRequest::GetWorkspaceConfigForWorkspace {
                        workspace_id: workspace.id,
                    },
                ))
                .result,
            Ok(ServerResponse::WorkspaceConfig(_))
        ));
        assert!(matches!(
            unrestricted
                .request(RequestEnvelope::new(
                    ClientRequest::SetWorkspaceConfigForWorkspace {
                        workspace_id: workspace.id,
                        config: WorkspaceConfig::default(),
                    },
                ))
                .result,
            Ok(ServerResponse::WorkspaceConfigUpdated)
        ));
        assert!(matches!(
            unrestricted
                .request(RequestEnvelope::new(ClientRequest::RenameWorkspace {
                    workspace_id: workspace.id,
                    name: "Renamed workspace".to_owned(),
                }))
                .result,
            Ok(ServerResponse::WorkspaceRenamed(_))
        ));
        assert!(matches!(
            unrestricted
                .request(RequestEnvelope::new(ClientRequest::ListWorkspaceSessions {
                    workspace_id: workspace.id,
                    include_archived: false,
                }))
                .result,
            Ok(ServerResponse::AgentSessions { .. })
        ));

        let tokens = AuthTokenStore::new();
        let issued = tokens
            .issue(AuthorizationScope::for_sessions(
                [session_id],
                backend.supported_capabilities.clone(),
            ))
            .unwrap();
        let scoped = backend.connect_authenticated(tokens.authenticate(&issued.token).unwrap());
        negotiate(&scoped);
        assert!(matches!(
            scoped
                .request(RequestEnvelope::new(ClientRequest::GetAgentSession {
                    session_id
                }))
                .result,
            Ok(ServerResponse::AgentSession(_))
        ));
        assert_eq!(
            scoped
                .request(RequestEnvelope::new(ClientRequest::GetAgentSession {
                    session_id: AgentSessionId::new(),
                }))
                .result
                .unwrap_err()
                .code,
            ErrorCode::AuthorizationDenied
        );
        assert_eq!(
            scoped
                .request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Denied workspace".to_owned(),
                }))
                .result
                .unwrap_err()
                .code,
            ErrorCode::AuthorizationDenied
        );
        assert_eq!(
            scoped
                .request(RequestEnvelope::new(
                    ClientRequest::CreateAgentSessionInWorkspace {
                        workspace_id: workspace.id,
                        name: "Denied session".to_owned(),
                    },
                ))
                .result
                .unwrap_err()
                .code,
            ErrorCode::AuthorizationDenied
        );
        assert_eq!(
            scoped
                .request(RequestEnvelope::new(
                    ClientRequest::AttachSessionDirectory {
                        session_id,
                        source: std::env::temp_dir().display().to_string(),
                        path: "sources/local".to_owned(),
                    }
                ))
                .result
                .unwrap_err()
                .code,
            ErrorCode::WorkspaceAccessDenied
        );
        assert_eq!(
            scoped
                .request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: None,
                    workspace_id: None,
                    after_sequence: None,
                    stream_epoch: None,
                }))
                .result
                .unwrap_err()
                .code,
            ErrorCode::AuthorizationDenied
        );
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    /// Serves an event stream that keeps a completion open until the client
    /// gives up, so a run can be controlled while the model is still working.
    fn slow_model_endpoint() -> (String, std::sync::mpsc::Receiver<()>) {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            use std::io::{Read, Write};

            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            let _ = stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"thinking\"}}]}\n\n");
            let _ = stream.flush();
            let _ = sender.send(());
            // Keep the completion open; the run must be stoppable anyway.
            for _ in 0..600 {
                if stream
                    .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\".\"}}]}\n\n")
                    .is_err()
                {
                    return;
                }
                let _ = stream.flush();
                thread::sleep(Duration::from_millis(20));
            }
        });
        (format!("http://{address}/v1/chat/completions"), receiver)
    }

    fn gated_model_endpoint(
        first_content: &str,
    ) -> (
        String,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (first_sender, first_receiver) = std::sync::mpsc::channel();
        let (second_sender, second_receiver) = std::sync::mpsc::channel();
        let (second_gate_sender, second_gate_receiver) = std::sync::mpsc::channel();
        let (finish_gate_sender, finish_gate_receiver) = std::sync::mpsc::channel();
        let first_event = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
            serde_json::to_string(first_content).unwrap()
        );
        thread::spawn(move || {
            use std::io::{Read, Write};

            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request);
            if stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .is_err()
                || stream.write_all(first_event.as_bytes()).is_err()
                || stream.flush().is_err()
            {
                return;
            }
            let _ = first_sender.send(());
            if second_gate_receiver.recv().is_err() {
                return;
            }
            if stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"second\"}}]}\n\n")
                .is_err()
                || stream.flush().is_err()
            {
                return;
            }
            let _ = second_sender.send(());
            if finish_gate_receiver.recv().is_err() {
                return;
            }
            let _ = stream.write_all(b"data: [DONE]\n\n");
            let _ = stream.flush();
        });
        (
            format!("http://{address}/v1/chat/completions"),
            first_receiver,
            second_gate_sender,
            second_receiver,
            finish_gate_sender,
        )
    }

    #[test]
    fn streamed_message_fragments_batch_until_the_time_threshold() {
        let (endpoint, first_delta, release_second, second_delta, finish) =
            gated_model_endpoint("first");
        let persistence =
            std::env::temp_dir().join(format!("loom-server-batched-{}.db", WorkspaceId::new()));
        let backend = InProcessBackend::with_openai_compatible_persistent(
            endpoint,
            "key",
            ModelId::new("slow/model"),
            &persistence,
        )
        .unwrap();
        let session_root_base = backend.session_root_base.clone();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Batched transcript workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "batched transcript".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "stream a response".to_owned(),
                model: ModelId::new("slow/model"),
                system_instructions: None,
                repository_instructions: None,
            }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        first_delta
            .recv_timeout(Duration::from_secs(10))
            .expect("first model delta");

        let handle = backend.runs().unwrap().get(&run_id).cloned().unwrap();
        for _ in 0..200 {
            if handle.message_fragments.lock().unwrap().pending_bytes == "first".len() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            handle.message_fragments.lock().unwrap().pending_bytes,
            "first".len()
        );
        assert!(
            backend
                .persistence
                .as_ref()
                .unwrap()
                .load_run_messages(run_id)
                .unwrap()
                .iter()
                .all(|message| message.content != "first")
        );

        thread::sleep(MESSAGE_FRAGMENT_BATCH_INTERVAL + Duration::from_millis(10));
        let mut flushed_prefix = None;
        for _ in 0..200 {
            flushed_prefix = backend
                .persistence
                .as_ref()
                .unwrap()
                .load_run_messages(run_id)
                .unwrap()
                .into_iter()
                .find(|message| message.role == loom_model::MessageRole::Assistant)
                .map(|message| message.content);
            if flushed_prefix.as_deref() == Some("first") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(flushed_prefix.as_deref(), Some("first"));

        release_second.send(()).unwrap();
        second_delta
            .recv_timeout(Duration::from_secs(10))
            .expect("second model delta");
        let transcript = backend.persistence.as_ref().unwrap();
        let mut persisted_content = None;
        for _ in 0..200 {
            persisted_content = transcript
                .load_run_messages(run_id)
                .unwrap()
                .into_iter()
                .find(|message| message.role == loom_model::MessageRole::Assistant)
                .map(|message| message.content);
            if persisted_content.as_deref() == Some("firstsecond") {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(persisted_content.as_deref(), Some("firstsecond"));
        assert_eq!(handle.message_fragments.lock().unwrap().pending_bytes, 0);

        finish.send(()).unwrap();
        await_settled_run(&connection, run_id);
        drop(connection);
        drop(backend);
        fs::remove_dir_all(session_root_base).unwrap();
        let _ = fs::remove_file(&persistence);
        let _ = fs::remove_file(persistence.with_extension("db-shm"));
        let _ = fs::remove_file(persistence.with_extension("db-wal"));
    }

    #[test]
    fn streamed_message_fragments_flush_at_the_byte_threshold_without_splitting_utf8() {
        let content = format!(
            "{}é",
            "a".repeat(MESSAGE_FRAGMENT_BATCH_BYTES.saturating_sub(1))
        );
        let (endpoint, first_delta, release_second, second_delta, finish) =
            gated_model_endpoint(&content);
        let persistence =
            std::env::temp_dir().join(format!("loom-server-large-delta-{}.db", WorkspaceId::new()));
        let backend = InProcessBackend::with_openai_compatible_persistent(
            endpoint,
            "key",
            ModelId::new("slow/model"),
            &persistence,
        )
        .unwrap();
        let session_root_base = backend.session_root_base.clone();
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Large transcript workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "large transcript".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "stream a large response".to_owned(),
                model: ModelId::new("slow/model"),
                system_instructions: None,
                repository_instructions: None,
            }));
        let run_id = match started.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        first_delta
            .recv_timeout(Duration::from_secs(10))
            .expect("large model delta");

        let transcript = backend.persistence.as_ref().unwrap();
        let mut persisted_content = None;
        for _ in 0..200 {
            persisted_content = transcript
                .load_run_messages(run_id)
                .unwrap()
                .into_iter()
                .find(|message| message.role == loom_model::MessageRole::Assistant)
                .map(|message| message.content);
            if persisted_content.as_deref() == Some(content.as_str()) {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(persisted_content.as_deref(), Some(content.as_str()));

        release_second.send(()).unwrap();
        second_delta
            .recv_timeout(Duration::from_secs(10))
            .expect("second model delta");
        finish.send(()).unwrap();
        await_settled_run(&connection, run_id);
        drop(connection);
        drop(backend);
        fs::remove_dir_all(session_root_base).unwrap();
        let _ = fs::remove_file(&persistence);
        let _ = fs::remove_file(persistence.with_extension("db-shm"));
        let _ = fs::remove_file(persistence.with_extension("db-wal"));
    }

    #[test]
    fn a_running_model_call_can_be_interrupted_without_blocking_the_request() {
        let (endpoint, started) = slow_model_endpoint();
        let backend =
            InProcessBackend::with_openai_compatible(endpoint, "key", ModelId::new("slow/model"));
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Interruptible workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "interruptible run".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started_run =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "stream for a long time".to_owned(),
                model: ModelId::new("slow/model"),
                system_instructions: None,
                repository_instructions: None,
            }));
        let run_id = match started_run.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        started
            .recv_timeout(Duration::from_secs(10))
            .expect("model stream started");

        // A second connection controls the run while the first one's model call
        // is still open.
        let observer = backend.connect();
        negotiate_m3(&observer);
        // The delta is journaled while the completion is still open, so a second
        // client sees it before the run ends.
        let mut streamed = false;
        for _ in 0..1_000 {
            let events = observer.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: None,
                stream_epoch: None,
            }));
            let ServerResponse::SessionEvents { events, .. } = events.result.unwrap() else {
                panic!("unexpected events response");
            };
            if events.iter().any(|event| {
                matches!(
                    &event.event,
                    ServerEvent::Agent {
                        event: AgentEvent::AssistantMessageDelta { text, .. }
                    } if text == "thinking"
                )
            }) {
                streamed = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            streamed,
            "an assistant delta was not journaled mid-completion"
        );

        let before = Instant::now();
        let interrupted =
            observer.request(RequestEnvelope::new(ClientRequest::InterruptAgentRun {
                run_id,
            }));
        let elapsed = before.elapsed();
        let ServerResponse::AgentRun(snapshot) = interrupted.result.unwrap() else {
            panic!("unexpected interrupt response");
        };
        assert_eq!(snapshot.state, AgentRunState::Cancelled);
        assert!(
            elapsed < Duration::from_secs(5),
            "interrupt waited {elapsed:?} for the model call"
        );
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn a_running_model_call_can_be_paused_and_resumed() {
        let (endpoint, started) = slow_model_endpoint();
        let backend =
            InProcessBackend::with_openai_compatible(endpoint, "key", ModelId::new("slow/model"));
        let connection = backend.connect();
        negotiate_m3(&connection);
        let workspace = connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
            name: "Pausable workspace".to_owned(),
        }));
        let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
            panic!("unexpected workspace response");
        };
        let session = connection.request(RequestEnvelope::new(
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id: workspace.id,
                name: "pausable run".to_owned(),
            },
        ));
        let session_id = match session.result.unwrap() {
            ServerResponse::AgentSessionCreated(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        let started_run =
            connection.request(RequestEnvelope::new(ClientRequest::StartSessionAgentRun {
                session_id,
                task: "stream for a long time".to_owned(),
                model: ModelId::new("slow/model"),
                system_instructions: None,
                repository_instructions: None,
            }));
        let run_id = match started_run.result.unwrap() {
            ServerResponse::AgentRunStarted(snapshot) => snapshot.id,
            response => panic!("unexpected response: {response:?}"),
        };
        started
            .recv_timeout(Duration::from_secs(10))
            .expect("model stream started");
        let before = Instant::now();
        let paused = connection.request(RequestEnvelope::new(ClientRequest::PauseAgentRun {
            run_id,
        }));
        let elapsed = before.elapsed();
        let ServerResponse::AgentRun(snapshot) = paused.result.unwrap() else {
            panic!("unexpected pause response");
        };
        assert_eq!(snapshot.state, AgentRunState::Paused);
        assert!(
            elapsed < Duration::from_secs(5),
            "pause waited {elapsed:?} for the model call"
        );
        let current =
            connection.request(RequestEnvelope::new(ClientRequest::GetAgentRun { run_id }));
        let ServerResponse::AgentRun(snapshot) = current.result.unwrap() else {
            panic!("unexpected run response");
        };
        assert_eq!(snapshot.state, AgentRunState::Paused);
        fs::remove_dir_all(&backend.session_root_base).unwrap();
    }

    #[test]
    fn retryable_mutation_idempotency_survives_backend_restart() {
        let path =
            std::env::temp_dir().join(format!("loom-server-idempotency-{}.db", WorkspaceId::new()));
        let request_id = loom_core::RequestId::new();
        let (workspace_id, first) = {
            let backend = InProcessBackend::new_persistent(&path).unwrap();
            let connection = backend.connect();
            negotiate(&connection);
            let created =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Idempotency workspace".to_owned(),
                }));
            let ServerResponse::WorkspaceCreated(workspace) = created.result.unwrap() else {
                panic!("unexpected workspace response");
            };
            let request = RequestEnvelope::with_request_id(
                request_id,
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "durable idempotency".to_owned(),
                },
            );
            (workspace.id, connection.request(request))
        };
        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let request = RequestEnvelope::with_request_id(
            request_id,
            ClientRequest::CreateAgentSessionInWorkspace {
                workspace_id,
                name: "durable idempotency".to_owned(),
            },
        );
        let second = connection.request(request);
        assert_eq!(first, second);
        assert!(matches!(
            first.result,
            Ok(ServerResponse::AgentSessionCreated(_))
        ));
        let expired_request_id = request_id_with_issued_at(
            current_unix_millis()
                .saturating_sub(IDEMPOTENCY_RETENTION.as_millis() as u64)
                .saturating_sub(1),
        );
        let expired = connection.request(RequestEnvelope::with_request_id(
            expired_request_id,
            ClientRequest::CreateWorkspace {
                name: "must not be replayed".to_owned(),
            },
        ));
        assert_eq!(
            expired.result.unwrap_err().code,
            ErrorCode::DeadlineExceeded
        );
        let workspaces = connection.request(RequestEnvelope::new(ClientRequest::ListWorkspaces));
        assert!(matches!(
            workspaces.result,
            Ok(ServerResponse::Workspaces { workspaces }) if workspaces.len() == 1
        ));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reconnect_feed_payloads_load_lazily_and_keep_pruned_cursor_after_restart() {
        let path = std::env::temp_dir().join(format!("loom-server-feed-{}.db", WorkspaceId::new()));
        let (session_id, session_root_base, previous_stream_epoch) = {
            let backend = InProcessBackend::new_persistent(&path).unwrap();
            backend.set_event_retention(1).unwrap();
            let connection = backend.connect();
            negotiate(&connection);
            let workspace =
                connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                    name: "Lazy feed workspace".to_owned(),
                }));
            let ServerResponse::WorkspaceCreated(workspace) = workspace.result.unwrap() else {
                panic!("unexpected workspace response");
            };
            let created = connection.request(RequestEnvelope::new(
                ClientRequest::CreateAgentSessionInWorkspace {
                    workspace_id: workspace.id,
                    name: "Lazy feed session".to_owned(),
                },
            ));
            let ServerResponse::AgentSessionCreated(session) = created.result.unwrap() else {
                panic!("unexpected session response");
            };
            for name in ["renamed once", "renamed twice"] {
                connection
                    .request(RequestEnvelope::new(ClientRequest::RenameAgentSession {
                        session_id: session.id,
                        name: name.to_owned(),
                    }))
                    .result
                    .unwrap();
            }
            backend.flush().unwrap();
            (
                session.id,
                backend.session_root_base.clone(),
                backend.node_id.clone(),
            )
        };

        let backend = InProcessBackend::new_persistent(&path).unwrap();
        assert!(backend.journal().unwrap().events.is_empty());
        let connection = backend.connect();
        negotiate(&connection);
        let stale = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: Some(EventSequence::new(1)),
            stream_epoch: None,
        }));
        let ServerResponse::SessionEventsSnapshot {
            events,
            oldest_sequence,
            latest_sequence,
            ..
        } = stale.result.unwrap()
        else {
            panic!("expected a stale-cursor snapshot");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, EventSequence::new(3));
        assert_eq!(oldest_sequence, EventSequence::new(3));
        assert_eq!(latest_sequence, EventSequence::new(3));

        let changed_epoch =
            connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                session_id: Some(session_id),
                workspace_id: None,
                after_sequence: Some(EventSequence::new(3)),
                stream_epoch: Some(previous_stream_epoch.clone()),
            }));
        let ServerResponse::SessionEventsSnapshot {
            events,
            latest_sequence,
            stream_epoch: Some(current_epoch),
            ..
        } = changed_epoch.result.unwrap()
        else {
            panic!("expected a snapshot after the feed epoch changed");
        };
        assert_ne!(current_epoch, previous_stream_epoch);
        assert_eq!(latest_sequence, EventSequence::new(3));
        assert_eq!(events.len(), 1);

        let current = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: Some(session_id),
            workspace_id: None,
            after_sequence: Some(EventSequence::new(2)),
            stream_epoch: None,
        }));
        let ServerResponse::SessionEvents { events, .. } = current.result.unwrap() else {
            panic!("expected retained session events");
        };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, EventSequence::new(3));

        fs::remove_dir_all(session_root_base).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn workspace_reconnect_feed_isolated_and_pruned_cursors_resync_to_workspace_snapshot() {
        let path = std::env::temp_dir().join(format!(
            "loom-server-workspace-feed-{}.db",
            WorkspaceId::new()
        ));
        let (workspace_a, workspace_b, session_a, session_b, previous_epoch) = {
            let backend = InProcessBackend::new_persistent(&path).unwrap();
            backend.set_event_retention(1).unwrap();
            let connection = backend.connect();
            negotiate(&connection);
            let create_workspace = |name: &str| {
                let response =
                    connection.request(RequestEnvelope::new(ClientRequest::CreateWorkspace {
                        name: name.to_owned(),
                    }));
                let ServerResponse::WorkspaceCreated(workspace) = response.result.unwrap() else {
                    panic!("unexpected workspace response");
                };
                workspace.id
            };
            let workspace_a = create_workspace("Workspace feed A");
            let workspace_b = create_workspace("Workspace feed B");
            let create_session = |workspace_id, name: &str| {
                let response = connection.request(RequestEnvelope::new(
                    ClientRequest::CreateAgentSessionInWorkspace {
                        workspace_id,
                        name: name.to_owned(),
                    },
                ));
                let ServerResponse::AgentSessionCreated(session) = response.result.unwrap() else {
                    panic!("unexpected session response");
                };
                session.id
            };
            let session_a = create_session(workspace_a, "A");
            let session_b = create_session(workspace_b, "B");
            for (session_id, label) in [(session_a, "A"), (session_b, "B")] {
                for revision in 1..=2 {
                    connection
                        .request(RequestEnvelope::new(ClientRequest::RenameAgentSession {
                            session_id,
                            name: format!("{label} {revision}"),
                        }))
                        .result
                        .unwrap();
                }
            }

            for workspace_id in [workspace_a, workspace_b] {
                let response =
                    connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                        session_id: None,
                        workspace_id: Some(workspace_id),
                        after_sequence: None,
                        stream_epoch: None,
                    }));
                let ServerResponse::WorkspaceEventsSnapshot {
                    workspace_id: returned_workspace,
                    sessions,
                    events,
                    ..
                } = response.result.unwrap()
                else {
                    panic!("expected a snapshot after in-memory feed pruning");
                };
                assert_eq!(returned_workspace, workspace_id);
                assert_eq!(sessions.len(), 1);
                assert!(
                    events
                        .iter()
                        .all(|event| event.session_id == sessions[0].id)
                );
            }

            let ambiguous =
                connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: Some(session_a),
                    workspace_id: Some(workspace_a),
                    after_sequence: None,
                    stream_epoch: None,
                }));
            assert!(ambiguous.result.is_err());
            let unknown =
                connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                    session_id: None,
                    workspace_id: Some(WorkspaceId::new()),
                    after_sequence: None,
                    stream_epoch: None,
                }));
            assert!(unknown.result.is_err());

            backend.flush().unwrap();
            (
                workspace_a,
                workspace_b,
                session_a,
                session_b,
                backend.node_id.clone(),
            )
        };

        let backend = InProcessBackend::new_persistent(&path).unwrap();
        let connection = backend.connect();
        negotiate(&connection);
        let stale = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace_a),
            after_sequence: Some(EventSequence::new(1)),
            stream_epoch: None,
        }));
        let ServerResponse::WorkspaceEventsSnapshot {
            workspace_id,
            sessions,
            events,
            oldest_sequence,
            latest_sequence,
            stream_epoch: Some(current_epoch),
        } = stale.result.unwrap()
        else {
            panic!("expected a workspace snapshot after persisted pruning");
        };
        assert_eq!(workspace_id, workspace_a);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_a);
        assert!(events.iter().all(|event| event.session_id == session_a));
        assert!(!events.iter().any(|event| event.session_id == session_b));
        assert!(oldest_sequence <= latest_sequence);
        assert_ne!(current_epoch, previous_epoch);

        let events_b = connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
            session_id: None,
            workspace_id: Some(workspace_b),
            after_sequence: None,
            stream_epoch: None,
        }));
        let ServerResponse::WorkspaceEventsSnapshot {
            sessions,
            events,
            latest_sequence: global_cursor,
            stream_epoch: Some(current_epoch),
            ..
        } = events_b.result.unwrap()
        else {
            panic!("expected a workspace snapshot after persisted pruning");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_b);
        assert!(events.iter().all(|event| event.session_id == session_b));

        let advanced_cursor =
            connection.request(RequestEnvelope::new(ClientRequest::GetSessionEvents {
                session_id: None,
                workspace_id: Some(workspace_a),
                after_sequence: Some(global_cursor),
                stream_epoch: Some(current_epoch),
            }));
        assert!(matches!(
            advanced_cursor.result.unwrap(),
            ServerResponse::SessionEvents { events, .. } if events.is_empty()
        ));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn transcript_page_content_is_bounded_and_marks_truncation() {
        let large = vec![b'x'; MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as usize + 1];
        let (content, truncated) = bounded_transcript_content(
            &large[..MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as usize],
            MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as u64 + 1,
        );
        assert!(truncated);
        assert!(content.starts_with(&"x".repeat(MAX_AGENT_RUN_TRANSCRIPT_MESSAGE_BYTES as usize)));
        assert!(content.ends_with("\n...[message truncated]"));

        let (content, truncated) = bounded_transcript_content(b"short", 5);
        assert!(!truncated);
        assert_eq!(content, "short");
        assert_eq!(bounded_transcript_content(&[], 0), (String::new(), false));
    }

    #[allow(dead_code)]
    fn _keep_tool_id_in_scope(_: ToolCallId) {}
}
