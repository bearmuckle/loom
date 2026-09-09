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

The protocol payload should be independent of the transport. Start with a
compact, schema-driven binary representation such as MessagePack or CBOR, and
provide JSON only as a debugging and integration format. The choice should be
settled with benchmarks that include large file responses and high-frequency
terminal output.

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

The backend should journal enough event metadata to replay the current
projection after reconnecting, while avoiding unbounded memory growth. A
client that falls behind must be able to request a fresh snapshot and resume
from a known sequence.

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

Important event families include `AgentMessageDelta`,
`AgentPlanProposed`, `AgentStepStarted`, `ToolCallRequested`,
`ToolApprovalRequired`, `ToolCallStarted`, `ToolOutputChunk`,
`AgentStepCompleted`, `AgentNeedsInput`, `AgentRunUsage`,
`WorkspaceChanged`, and `AgentRunCompleted`. Events must identify the
session, run, step, tool call, and sequence number so a client can render
partial progress and recover a consistent view.

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
