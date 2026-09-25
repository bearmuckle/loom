# Architecture

## System overview

```text
             Native client (GPUI)      Browser client (GPUI/WASM)
                       \                       /
                        \  versioned protocol /
                         +-------------------+
                         |  transport adapter |
                         +---------+---------+
                                   |
                         +---------v---------+
                         |  Loom backend     |
                         |  session manager  |
                         |  workspace model |
                         |  agent runtime    |
                         |  provider router  |
                         |  task supervisor  |
                         |  event journal    |
                         +--+---+--+---+----+
                            |   |  |   |
                         files |  | providers
                         processes tools
```

## Frontend

The frontend is a GPUI application using `gpui-kit` for shared native and
browser support. `loom-ui` is organized into:

- `view`: the session navigator, run canvas, composer, review drawer, and the
  client-side projection they render.
- `state`: projection types and pure formatting helpers derived from protocol
  responses and events.
- `connection`: the transport-independent protocol client plus the connection
  worker that executes requests off the UI thread.
- `text_input`: the text buffer and input element.
- `theme`: window chrome, decorations, and colour.
- `platform`: native adapters (process arguments, workspace preparation, local
  credential storage, repository bootstrap) that a browser target cannot use
  unchanged.

Backend requests are submitted to a single connection worker thread and
awaited on a background task, so no UI handler blocks on backend latency. The
startup bootstrap (connect, negotiate, first session load) is deliberately
synchronous because there is nothing to render yet.

The native and wasm/browser targets should share domain state, view models,
commands, and rendering wherever GPUI permits. Platform code must be a small
adapter, not a second application.

Browser limitations must be explicit. Browser clients cannot directly spawn
arbitrary processes or access a local filesystem; those actions belong to the
backend and are exposed through authorized protocol commands.

## Backend

The backend is a Rust service and library that can run:

- Embedded in a native application for local-only use.
- As a local child process reached through an operating-system transport.
- As a standalone service reached over a network.
- In a constrained hosted environment where only permitted capabilities are
  enabled.

Suggested backend boundaries:

- `loom-core`: identifiers, errors, timestamps, permissions, capability
  negotiation, and shared domain types.
- `loom-session`: durable agent sessions, conversation history, reconnect
  state, forks, handoff, and lifecycle.
- `loom-agent`: agent loop, planning, tool-call dispatch, interruption,
  retries, checkpoints, context compaction, limits, pause/resume, and
  completion.
- `loom-model`: provider-independent model requests, streamed responses,
  tool-call normalization, token accounting, and model capabilities.
- `loom-providers`: adapters for hosted APIs, OpenAI-compatible endpoints,
  local model servers, authentication, rate limits, and provider health.
- `loom-context`: context inspection and assembly, repository/system
  instructions, summaries, compaction, and explicit token budgets.
- `loom-persistence`: atomic, sectioned SQLite state storage used by the
  in-process backend. Its JSON section payloads are an implementation detail;
  domain and protocol types remain provider-neutral.
- `loom-tools`: typed tool definitions, permission checks, execution policies,
  result normalization, and tool adapters.
- `loom-workspace`: session-root file trees, file contents, watches, edits,
  snapshots, and repository-instruction discovery.
- `loom-process`: commands, terminals, task supervision, output streaming, and
  cancellation.
- `loom-vcs`: source-control abstraction and read-only repository status,
  diff, branch, and conflict reporting.
- `loom-protocol`: versioned request/response/event schemas, the serializable
  data-transfer types they carry, and codecs. It depends only on `loom-core`
  and `loom-model`, so a client can speak the contract without linking the
  backend implementation; the backend crates depend on the contract and
  produce its types.
- `loom-server`: listeners, authentication, connection management, and
  deployment configuration.
- `loom-cli`: local server startup, diagnostics, and administration.

The backend should expose domain services rather than exposing raw model APIs,
filesystem, and process primitives directly. This keeps provider
normalization, authorization, auditing, cancellation, and future sandboxing
in one place.

`Workspace` is a durable container for sessions and workspace-level settings;
it is not a filesystem root or repository. An `AgentSession` owns an isolated,
backend-managed filesystem root. The root contains zero or more
session-specific repository checkouts at stable relative paths. A checkout
may be implemented as a clone or a Git worktree, but its mutable working tree
belongs exclusively to that session. Shared bare-object caches are an
implementation detail and are never exposed as a shared working directory.
This ownership and path boundary does not by itself promise OS-level process
sandboxing; process isolation is a separate backend security capability.

`loom-workspace` is not a second editor authority; it owns session-root files,
snapshots, edits, checkpoints, repository instruction discovery, and the
mapping from repository-relative paths to paths inside the session root.
Tools, terminal working directories, process permissions, VCS status and
diffs, and file events all resolve against the active session root. A
session's filesystem root is not the workspace's root, and a repository is
not the workspace's identity. The GPUI client projects backend-owned
workspace, session, run, approval, session-filesystem, task, and evidence
state into a workspace/session navigator and active-session canvas.
The canvas contains the chronological agent conversation and tool timeline
plus a composer for new tasks, follow-up direction, and answers to agent
questions.

Changed paths, bounded diffs, task artifacts, and repository status are
read-only review projections opened in a drawer or focused overlay. They are
loaded through the canonical session-filesystem, process, and VCS services
and remain scoped to the active session. The client does not own an editable
buffer, tab/pane layout, language-service state, or orchestration state. A
remote client receives the same projections over the authenticated transport.

## Agent runtime

An agent session is a state machine, not an unbounded loop:

```text
queued -> planning -> awaiting_approval -> executing -> evaluating
                         ^                    |          |
                         |                    +----------+
                         |                         |
                         +---------- paused -------+
                                      |
                         +---- needs_input <-------+
                                      |
                         completed / failed / cancelled
```

Each iteration has a durable record containing the provider/model, input
context, streamed output, requested tool calls, approval decisions, tool
results, token usage, timing, and resulting workspace events. The runtime
must support:

- Streaming model output and tool calls without requiring the frontend to
  remain connected.
- Explicit stop, pause, resume, retry, and continue-after-feedback commands.
  A run executes on a backend-owned worker and holds its runtime lock only for
  the duration of one step, so control requests are serviceable while a model
  call is open.
- Parallel child tasks with bounded concurrency and clear parent ownership.
- Per-session limits for time, model tokens, tool calls, processes, and cost.
- Checkpoints before risky mutations and a way to restore or inspect them.
- Context compaction that preserves the task, decisions, constraints, and
  unresolved work rather than silently dropping history.
- Provider failover only when the policy allows it; never silently switch
  models during a user-visible action.

The UI may render a conversational transcript, but the backend event stream
is authoritative. Internal provider reasoning must not be exposed as a
requirement of the protocol; providers return user-visible messages,
structured tool calls, and status metadata.

## Model provider abstraction

The agent runtime talks to a normalized model interface defined in
`loom-model`:

```text
ModelProvider
  descriptor()
  list_models()
  capabilities()
  stream(request, cancellation_token, sink)
  count_tokens(request)
  health_check()
  reset()
```

`stream` emits each normalized event through the sink as it is decoded, so the
runtime journals an assistant delta before the completion has finished. The
cancellation token is observed between chunks, which is how a pause or
interrupt reaches an open model call. The sink can also ask a provider to stop
early. Adapters request server-sent events; when an endpoint answers with a
complete JSON document instead, declared by its content type, the document is
normalized and emitted in one pass.

Adapters translate provider-specific streaming formats, tool-call schemas,
authentication, errors, and usage metrics into the normalized interface.
Provider configuration belongs to the backend and secrets are referenced by
credential IDs; raw keys must not be sent to or persisted by the frontend.

Supported provider classes are:

- Hosted providers with native APIs and tool calling.
- OpenAI-compatible HTTP endpoints for self-hosted and organization gateways.
- Local inference servers such as Ollama, llama.cpp-compatible servers, or
  other OpenAI-compatible local runtimes.
- Deterministic test providers that replay scripted responses and tool calls.

Local models may have weaker tool calling, smaller context windows, or
different streaming behavior. The capability handshake must let the runtime
adapt prompts and feature availability without making the UI provider-aware.

Provider configuration uses opaque credential references, resolved only while
constructing a backend provider. Usage is recorded as normalized token and
cost records, and transport or status failures map to stable Loom error
codes. Provider configuration summaries never contain credential material or
raw keys.

The persistence layer stores a versioned backend snapshot through an atomic
temporary-file replacement. It includes session state, the authoritative
event journal, serializable agent runtime state, workspace state and
checkpoints, policy decisions, provider health, and usage ledgers. A runtime
that was executing during a process crash is recovered in `paused` state so a
new connection must explicitly resume it.

Remote access uses the same domain services through a transport adapter:

```text
HTTP upgrade + bearer token
          |
  bounded WebSocket connection
          |
 authenticated InProcessConnection
          |
 existing journal/runtime/workspace/provider services
```

`RemoteServerConfig` makes the bind address, path, heartbeat, request
deadline, frame size, and outbound capacity explicit. It defaults to loopback
and `/ws`; a `RunningRemoteServer` owns graceful shutdown. The
`WebSocketTransport` client uses the same JSON codecs as protocol tests, so a
native client and a browser client do not need different domain operations.
Transport tasks may time out or be cancelled, but backend work that already
entered the synchronous runtime is deliberately not tied to the connection.

## Repository layout

The repository should evolve toward a Rust workspace:

```text
crates/
  loom-core/
  loom-session/
  loom-agent/
  loom-model/
  loom-providers/
  loom-context/
  loom-persistence/
  loom-tools/
  loom-workspace/
  loom-process/
  loom-vcs/
  loom-protocol/
  loom-server/
  loom-cli/
  loom-ui/
```

Tests live with the crate they cover: unit tests in each crate, protocol
contract tests in `crates/loom-protocol/tests`, and remote transport tests in
`crates/loom-server/tests`. The important boundary is that shared Rust domain
and protocol crates do not depend on a particular window system or browser
API.
