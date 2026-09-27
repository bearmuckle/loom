# Communication protocol

The protocol is a public boundary between the frontend and backend, even when
both run in one process. It must be deterministic, inspectable, versioned,
and testable without a UI.

## Transport strategy

Use a transport abstraction with these initial implementations:

| Mode | Preferred transport | Fallback |
| --- | --- | --- |
| Native client and local backend | In-process channel or Unix domain socket / named pipe | Loopback TCP |
| Browser client and local/remote backend | WebSocket over HTTP(S) | HTTP request/stream endpoints where required by a host |
| Native client and remote backend | QUIC when deployment supports it | WebSocket or HTTP/2-compatible stream |

The protocol payload is independent of the transport. M4 implements the
initial JSON WebSocket adapter through `loom-server::WebSocketTransport`;
in-process clients continue to use the same typed envelopes. JSON is
intentionally retained as the inspectable contract until payload benchmarks
settle a MessagePack/CBOR migration. A future codec can sit behind the same
transport boundary without changing request IDs or event sequences.

## Protocol requirements

- Request IDs and typed responses.
- Server events with monotonically increasing sequence numbers.
- Session IDs and resumable subscriptions.
- Heartbeats, deadlines, cancellation, and backpressure.
- Capability and protocol-version negotiation.
- Compression for large payloads, without compressing already-compressed data.
- Chunked file, model, terminal, and tool-output streams.
- Idempotency keys for retryable mutations.
- Structured errors with stable codes and user-facing context.
- Authentication and authorization before workspace or provider capabilities
  are exposed.
- Redaction rules for secrets in logs and event history.

## Workspace, session, and repository ownership

The target domain model separates organization from execution:

- A `Workspace` groups sessions and workspace-level settings. It has an ID
  and lifecycle independent of any local directory or Git repository.
- An `AgentSession` belongs to a workspace and owns an isolated filesystem
  root on its backend. The host path is backend-internal and is not returned
  to remote clients. This defines filesystem ownership and path scope; it
  does not by itself guarantee OS-level process sandboxing.
- A session may attach zero or more repository sources. Each attachment
  identifies a source, requested revision, and stable relative checkout path
  within that root. The backend materializes a session-owned clone or
  worktree; mutable working trees are not shared between sessions.
- File, process, terminal, checkpoint, change, and VCS operations are scoped
  to a session. Repository paths are interpreted relative to that session's
  checkout, while root-relative paths can address files elsewhere in the
  session filesystem.

The protocol exposes workspace lifecycle/list operations,
workspace-scoped session listing/creation, and session-scoped repository
attachment and filesystem operations. Workspace IDs are independent of paths
and repository IDs. Protocol version 2 is the clean break for this ownership
model; version 1 clients are rejected rather than adapted.

Protocol version 2 includes `DiscoverCapabilities`,
`ClientFrame`/`ServerFrame` codec types, and `SessionEventsSnapshot`.
Version compatibility is major-version based: a client may negotiate a newer
minor version within the same major, while an incompatible major returns the
existing `unsupported_protocol` error.

Approval policies are scoped to an agent session. Session snapshots
include the effective approval policy and the session's `auto_approve_actions`
preference; the session-scoped `SetApprovalPolicy` form updates both for
subsequent runs.

The WebSocket service authenticates during the HTTP upgrade using a bearer
token, then creates an authenticated view of the existing in-process
connection. Each request re-checks the token so revocation takes effect
without restarting the service. Capability negotiation is intersected with the token grant, and every
workspace/session/run request is checked against the token's explicit scope.

The backend should journal enough event metadata to replay the current
projection after reconnecting, while avoiding unbounded memory growth. A
client that falls behind must be able to request a fresh snapshot and resume
from a known sequence.

Workspace-scoped `GetSessionEvents` requests require the
`SubscribeWorkspaceEvents` capability. The server returns typed workspace
event responses for these requests; it does not fall back to a session-only
response shape.

Workspace-scoped `GetSessionEvents` requests from capable clients return `WorkspaceEvents` when the
cursor is current and `WorkspaceEventsSnapshot` when the client must resync.
Their entries are a union of session event envelopes and workspace event
envelopes. Workspace rename and configuration
revision changes carry a workspace ID directly and never use a synthetic
session ID. The workspace sequence is shared with session events so clients can
resume one ordered workspace feed.

The server retains a bounded global journal (4096 events by default). If a
session-specific cursor is older than the retained range,
`GetSessionEvents` returns `SessionEventsSnapshot` with the current session
projection, retained events, the oldest available sequence, and the latest
global sequence. The client must replace its projection and resume from the
latest sequence in the response. Requests with a stable `RequestId` are
idempotent for retryable mutations; the bounded idempotency cache is persisted
with durable backend state.

The server sends WebSocket ping heartbeats, applies a configured request
deadline, accepts cancellation frames for pending transport tasks, and uses a
bounded outbound queue. A full queue closes the connection rather than
unboundedly buffering output. Disconnecting a client never cancels the
backend's agent, process, terminal, or journal work: a run executes on a
backend-owned worker, not on the connection that started it.

## Example domain operations

These names are illustrative; the schema should use explicit request and
event types rather than a generic "run arbitrary method" envelope.

```text
CreateWorkspace / ListWorkspaces
CreateAgentSessionInWorkspace / ListWorkspaceSessions
AttachSessionRepository / DetachSessionRepository / ListSessionRepositories
StartSessionAgentRun
PauseAgentRun
ResumeAgentRun
InterruptAgentRun
ApproveAgentAction
RejectAgentAction
RetryAgentStep
ForkAgentSession
ListProviders
ListModels
SetSessionModel
GetSessionContext
GetSessionFilesystemSnapshot
ReadSessionFile
ApplySessionEdit
SubscribeSessionFilesystemEvents
OpenTerminal
WriteTerminalInput
ResizeTerminal
StartTask
CancelTask
GetRepositoryStatus
```

M2 adds typed session-filesystem and process requests rather than exposing
filesystem or process primitives directly:

```text
AttachSessionRepository / DetachSessionRepository / ListSessionRepositories
GetSessionFilesystemSnapshot / GetSessionFilesystemEvents
ReadSessionFile / ApplySessionEdit / TakeSessionFilesystemControl
CreateSessionCheckpoint / RevertSessionCheckpoint / UndoSessionEdit
SetSessionApprovalPolicy
OpenSessionTerminal / WriteSessionTerminalInput / ResizeSessionTerminal
GetSessionTerminalEvents / CancelSessionTerminal
StartSessionTask / GetSessionTask / GetSessionTaskEvents / CancelSessionTask
```

Session filesystem edits carry an optional content revision and return a
structured conflict when it no longer matches. Terminal and task output is
delivered as bounded, sequence-numbered records; snapshots remain available
after a client disconnects. Policy evaluations are agent events before a
tool executes, and the existing approval request/decision events remain
authoritative for approval-required actions.

Agent and Edit modes allow read, write, command, and network actions without
prompting by default; the session settings can opt out, which restores explicit
approval for writes, commands, and network actions. **Auto approve** remains an
explicit mode. Destructive actions remain denied in every mode.

M3 adds typed provider and durable-orchestration requests:

```text
StartAgentRunWithOptions
PauseAgentRun / ResumeAgentRun
ForkAgentSession
RetryAgentFromCheckpoint
ListProviders / ListModels
DiscoverProviderModels
GetProviderHealth
ConfigureGitHubCopilot
StartGitHubCopilotLogin / GetGitHubCopilotLoginStatus
GetRunUsage
InspectAgentContext
```

`StartAgentRunWithOptions` carries `SessionLimits` and
`ContextAssemblyOptions`. Limits are reported in `RunLimitReached` and
`RunUsageUpdated` events; a context budget failure is a structured
`context_limit_exceeded` failure rather than silent truncation or provider
failover. `ContextInspected` events identify required, included, omitted, and
compacted context items. Before each model call, the runtime clamps configured
windows to the active model's advertised limit, counts tool schemas once, and
sends the reserved output limit to the provider. Models without a known window
use a visible 8,192-token fallback unless a window is configured explicitly.

At 90% of the input budget, context assembly keeps recent exchanges together
and replaces older history with bounded, explicitly lossy excerpts. System and
repository instructions, the original task, and the latest user direction remain
intact. Large tool outputs may be shortened with omission markers; call IDs and
arguments are preserved. If required context or the latest exchange still cannot
fit, the run reports a context-limit error. Compaction uses the active provider's
token estimator; these counts remain estimates, not server-side token guarantees.

The runtime persists the summary and its history boundary across recovery,
retaining the full transcript separately. The UI shows estimated input usage and
output reserve, and records compaction in the timeline. Excerpts preserve the
beginning and end of older content; they are not model-generated semantic
summaries and may omit intermediate decisions or details.

Provider summaries contain provider/model IDs, capabilities, credential
reference IDs, and health state, never raw credentials. Normalized provider
authentication, rate-limit, invalid-response, and unavailable errors retain
retryability without echoing response bodies or request headers.
`ProviderSummary.api_key_configurable` is optional and defaults to false so
new clients do not offer setup on older workers. `ConfigureApiKeyProvider`
accepts a provider ID and API key for a registered API-key provider and returns
only `ProviderConfigured`; the secret-bearing request is excluded from the
durable idempotency journal. The client requires WSS for remote workers
(loopback WS is allowed) before sending the key.

Provider configuration remains in each backend's SQLite database. Newly
entered API keys are stored in a sibling `<database-stem>.credentials.json`
file, with owner-only permissions on Unix. The provider config stores only a
random credential reference into that backend-specific file, so databases on
the same host do not discover or reuse each other's new keys. Existing
OpenAI-compatible configs that point into the legacy host credential file are
migrated lazily when their backend opens: the key is copied into that
backend's credential file and the SQLite config is updated. The old entry is
retained so another existing backend can migrate independently; existing
provider-specific credentials such as GitHub Copilot remain in their current
store.

`ConfigureGitHubCopilot` accepts a GitHub device-flow access token only over an
authenticated connection, stores it in the worker's credential store, and
returns no credential material. It requires the separate `ConfigureProviders`
capability; tokens are not included in workspace configuration or provider
summaries. Clients must use WSS for remote workers (loopback WS is allowed)
when sending the credential. Browser clients instead use
`StartGitHubCopilotLogin` and `GetGitHubCopilotLoginStatus`: the worker performs
the device-code exchange and stores the resulting credential itself, so no
OAuth access token is sent through or persisted by the browser.

Important event families include `AgentMessageDelta`,
`AgentPlanProposed`, `AgentStepStarted`, `ToolCallRequested`,
`ToolApprovalRequired`, `ToolCallStarted`, `ToolOutputChunk`,
`AgentStepCompleted`, `AgentNeedsInput`, `AgentRunUsage`,
`SessionFilesystemChanged`, and `AgentRunCompleted`. Events must identify the
session, run, step, tool call, and sequence number so a client can render
partial progress and recover a consistent view.

Agent activity history is additionally represented by additive
`ActivityRecorded` events. Each record has a stable activity ID, an
optional parent activity and step, status, timestamps, and elapsed duration.
Model activities identify only the model turn; they do not invent or expose
private model reasoning. Tool activities retain the observable call
arguments and result, with typed file, search, and command details inferred
from the existing tool schemas. Existing lifecycle and tool events remain
authoritative and are preserved for older clients.

M3 also journals `StepStarted`, `StepCompleted`, `ContextInspected`,
`RunUsageUpdated`, `RunLimitReached`, approval/policy events, and forked
session history. `ProviderError`, `ContextError`, and `RecoveryRequired`
events retain normalized failure reasons without secret material. The backend
persists these events before acknowledging the
mutating request when durable mode is enabled. On restart, unfinished
provider calls are not replayed implicitly: recoverable runtime state is
paused and the client explicitly resumes or retries from its workspace
checkpoint. If a referenced credential is unavailable, the run remains
inspectable in `paused` state and receives `RecoveryRequired`; resuming then
returns the normalized provider authentication error rather than switching
models.

M5 adds only the capability-gated projections required for the focused agent
client:

```text
ListWorkspaces / ListWorkspaceSessions / GetAgentSessionSnapshot
GetAgentRunSnapshot / GetSessionEvents
GetSessionFilesystemChanges / ReadSessionFile / GetSessionVcsDiff
ListSessionTasks / GetSessionTask / GetSessionTaskEvidence
```

Worker-node management adds workspace-scoped
`GetWorkspaceConfigForWorkspace` and `SetWorkspaceConfigForWorkspace`
requests. The persisted config contains a revision,
worker WebSocket URLs, and a CPU pulse threshold for session-card node
indicators (default 5%); credentials, workspace files, and session state are
not included. Connected peers receive the updated config when nodes join or
are removed. Sessions remain owned by the node that created them; config
distribution does not migrate sessions.
Provider settings are maintained independently by each worker. The client can
open provider settings for any connected worker, and signing in configures that
worker rather than the client host or other workers.

The UI aggregates sessions from connected nodes and offers a node choice when
creating a session on a multi-node setup, defaulting to the startup backend.
Session and run requests are sent to the recorded owner. If that node becomes
unavailable or is removed, the session remains pinned there and is never
silently moved to another backend.

`GetWorkerNodeStatus` reports CPU utilization and RAM usage as integer
percentages when available. CPU utilization is sampled by the host system
monitor between status requests (rather than inferred from CPU count or load
average); the first sample and unsupported measurements are unavailable.
Memory usage is calculated from total memory minus the operating system's
available memory. Each backend instance uses a UUID node ID that remains
stable for that instance, and its display name includes the host name plus a
short ID suffix so separate backend processes on one host remain distinguishable.
Clients should render unavailable values as `n/a`.

Session-card indicators pulse smoothly only while CPU usage is above the
workspace-configured threshold. The online color changes to red only after
three consecutive 10-second status polls report both CPU and RAM above 90%;
either metric at or below 90%, unavailable metrics, or a failed poll resets
the severe-load streak.

The existing M1-M4 mutations remain the control surface:

```text
CreateAgentSession / RenameAgentSession / ArchiveAgentSession
StartAgentRun / SendAgentMessage
PauseAgentRun / ResumeAgentRun / InterruptAgentRun
ApproveAgentAction / RejectAgentAction / RetryAgentStep
```

`ArchiveAgentSession` interrupts a non-terminal run for the session before
archiving it, so clients do not need to issue a separate interrupt request.

Session and run snapshots must be sufficient to render the active
conversation, plan, step state, tool calls, approvals, bounded output,
questions, errors, and final summary after reconnect. Review responses are
read-only, session-scoped, and bounded; diffs and task artifacts retain stable
references without exposing credentials or shell strings.

The protocol has no editor-buffer, editor-layout, or language-service
requests, and no VCS index or commit mutations. VCS status, diff, branch, and
conflict reads remain available. Those descoped surfaces would be reintroduced
with the milestone that needs them.
All M5 operations work through both `InProcessConnection` and the
authenticated WebSocket adapter after normal version and capability
negotiation.

## Run execution and control

`StartAgentRun` registers the run and returns its snapshot as soon as the run
is observable; the run itself proceeds on a backend worker. Clients follow
progress through the session event stream, which receives assistant deltas
while a model call is still open.

`PauseAgentRun` and `InterruptAgentRun` raise a control flag and cancel the
in-flight model stream rather than waiting for it, so they are answered while
a model call is open. Requests that need exclusive access to a busy run
(`ApproveAgentAction`, `SendAgentMessage`, `RetryAgentStep`, and the
checkpoint retry) return a retryable `conflict` instead of blocking.

Run projections (`GetAgentRunSnapshot`, `GetRunUsage`, `InspectAgentContext`)
are served from the backend's cached run state. Run state, usage, pending
approval, and pending input are updated from events as they are journaled; the
message transcript in a projection is refreshed at each step boundary, so a
transcript may lag the event stream by one in-flight model call.

## Reconnect and consistency

The backend event stream is authoritative. A client connection should:

1. Authenticate and negotiate protocol/capability versions.
2. Subscribe to one or more session streams from a known sequence.
3. Apply a snapshot when its sequence is no longer retained.
4. Resume event delivery without duplicating mutations.
5. Reconcile pending approvals and client commands after reconnect.

Long-running model calls, tool calls, processes, and child agents must not
depend on a frontend connection. A frontend can disconnect, reconnect from a
different device, and continue controlling the same backend session.
