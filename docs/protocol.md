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
backend's synchronous agent, process, terminal, or journal work.

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
GetDiagnostics
GetRepositoryStatus
CreateCommit
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

M3 adds typed provider and durable-orchestration requests:

```text
StartAgentRunWithOptions
PauseAgentRun / ResumeAgentRun
ForkAgentSession
RetryAgentFromCheckpoint
ListProviders / ListModels
DiscoverProviderModels
GetProviderHealth
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

Important event families include `AgentMessageDelta`,
`AgentPlanProposed`, `AgentStepStarted`, `ToolCallRequested`,
`ToolApprovalRequired`, `ToolCallStarted`, `ToolOutputChunk`,
`AgentStepCompleted`, `AgentNeedsInput`, `AgentRunUsage`,
`WorkspaceChanged`, and `AgentRunCompleted`. Events must identify the
session, run, step, tool call, and sequence number so a client can render
partial progress and recover a consistent view.

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

The existing M1-M4 mutations remain the control surface:

```text
CreateAgentSession / RenameAgentSession / ArchiveAgentSession
StartAgentRun / SendAgentMessage
PauseAgentRun / ResumeAgentRun / InterruptAgentRun
ApproveAgentAction / RejectAgentAction / RetryAgentStep
```

Session and run snapshots must be sufficient to render the active
conversation, plan, step state, tool calls, approvals, bounded output,
questions, errors, and final summary after reconnect. Review responses are
read-only, project-scoped, and bounded; diffs and task artifacts retain stable
references without exposing credentials or shell strings.

M5 does not add editor-buffer mutations, language-server lifecycle requests,
interactive terminal panes, VCS staging, branch management, or commit
operations. Those remain backend/tool capabilities or later client surfaces.
All M5 operations work through both `InProcessConnection` and the
authenticated WebSocket adapter after normal version and capability
negotiation.

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
