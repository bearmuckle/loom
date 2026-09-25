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

M4 adds `DiscoverCapabilities`, additive `ClientFrame`/`ServerFrame` codec
types, and `SessionEventsSnapshot`. Existing `RequestEnvelope`,
`ResponseEnvelope`, and `ServerEventEnvelope` JSON forms remain valid.
Version compatibility is major-version based: a client may negotiate a newer
minor version within the same major, while an incompatible major returns the
existing `unsupported_protocol` error.

Protocol 1.1 scopes approval policies to an agent session. Session snapshots
include the effective approval policy and the session's `auto_approve_actions`
preference; the session-scoped `SetApprovalPolicy` form updates both for
subsequent runs. The project-wide request form remains accepted for older
clients, and existing project policies remain the fallback until a session has
its own override.

The WebSocket service authenticates during the HTTP upgrade using a bearer
token, then creates an authenticated view of the existing in-process
connection. Each request re-checks the token so revocation takes effect
without restarting the service. Capability negotiation is intersected with
the token grant, and every project/session/run request is checked against the
token's explicit scope.

The backend should journal enough event metadata to replay the current
projection after reconnecting, while avoiding unbounded memory growth. A
client that falls behind must be able to request a fresh snapshot and resume
from a known sequence.

M4 retains a bounded global journal (4096 events by default). If a
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
OpenProject
CreateAgentSession
StartAgentRun
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
GetWorkspaceSnapshot
ReadFile
ApplyTextEdit
SubscribeWorkspaceEvents
OpenTerminal
WriteTerminalInput
ResizeTerminal
StartTask
CancelTask
GetRepositoryStatus
```

M2 adds typed workspace and process requests rather than exposing filesystem or
process primitives directly:

```text
OpenWorkspace / GetWorkspaceSnapshot / GetWorkspaceEvents
ReadWorkspaceFile / ApplyWorkspaceEdit / TakeWorkspaceControl
CreateCheckpoint / RevertCheckpoint / UndoWorkspaceEdit
SetApprovalPolicy
OpenTerminal / WriteTerminalInput / ResizeTerminal
GetTerminalEvents / CancelTerminal
StartTask / GetTask / GetTaskEvents / CancelTask
```

Workspace edits carry an optional content revision and return a structured
conflict when it no longer matches. Terminal and task output is delivered as
bounded, sequence-numbered records; snapshots remain available after a client
disconnects. Policy evaluations are agent events before a tool executes, and
the existing approval request/decision events remain authoritative for
approval-required actions.

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
compacted context items.

Provider summaries contain provider/model IDs, capabilities, credential
reference IDs, and health state, never raw credentials. Normalized provider
authentication, rate-limit, invalid-response, and unavailable errors retain
retryability without echoing response bodies or request headers.
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
`WorkspaceChanged`, and `AgentRunCompleted`. Events must identify the
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
ListProjects / ListAgentSessions / GetAgentSessionSnapshot
GetAgentRunSnapshot / GetSessionEvents
GetWorkspaceChanges / ReadWorkspaceFile / GetVcsDiff
ListTasks / GetTask / GetTaskEvidence
```

Worker-node management adds project-scoped `GetWorkspaceConfig` and
`SetWorkspaceConfig` requests. The persisted config contains a revision,
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
read-only, project-scoped, and bounded; diffs and task artifacts retain stable
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
