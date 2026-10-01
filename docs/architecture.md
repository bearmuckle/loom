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

- `view`: screen modules (navigator, canvas, composer, review drawer, settings)
  plus the client-side projection they render. The `LoomView` implementation is
  split by concern under `view/{lifecycle,sessions,project,runs,composer,
  providers,workers,source,review,render,helpers}`.
- `state`: projection types and pure formatting helpers derived from protocol
  responses and events.
- `connection`: the transport-independent protocol client plus the connection
  worker that executes requests off the UI thread.
- `theme`: the semantic colour palette resolved against the active
  `gpui-kit` theme. Window chrome and decorations are provided by
  `gpui-kit`'s window root, not by Loom.

Native adapters (process arguments, workspace preparation, local credential
storage, repository bootstrap, backend state files, the in-process/remote
transport, and the GitHub Copilot device-login flow) live in `loom-local`, not
in the UI. `loom-ui` therefore depends only on `loom-protocol` (plus
`loom-core`/`loom-model` for protocol types and `loom-local` on native
targets), so the native and browser paths share the same protocol client
surface and neither links the backend implementation.

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
  local model servers, authentication, rate limits, and provider health. HTTP
  and streaming use an async `reqwest`/`tokio` client driven from synchronous
  workers with a runtime.
- `loom-context`: context inspection and assembly, repository/system
  instructions, summaries, compaction, and explicit token budgets.
- `loom-persistence`: typed, indexed SQLite state storage used by the
  in-process backend, split into per-aggregate repository traits (catalog,
  session, run, filesystem, feed, project) behind a `Persistence` supertrait.
  Durable state is written in per-mutation transactions, and large immutable
  payloads live in a content-addressed store inside SQLite. It uses a single
  typed schema (currently version 4, with an in-place ladder from version 2);
  a database written by an unsupported revision is rejected unchanged and must
  be wiped. `FilePersistence` and `FilePersistence::in_memory()` share the same
  schema, repository, and serialization code path. The crate depends only on
  neutral domain crates (`loom-core`/`loom-model`), never on the protocol
  contract or `loom-session`/`loom-providers`.
- `loom-tools`: typed tool definitions, permission checks, execution policies,
  result normalization, and tool adapters. Workspace exploration is bounded:
  search supports literal or regex matching with context and a result cap,
  listing supports depth/glob/entry limits, edits can batch exact replacements,
  commands accept a timeout, and oversized output keeps both ends with an
  explicit omitted-byte report.
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
  deployment configuration. The composition root owns the `InProcessBackend`,
  whose request dispatch is split into per-domain modules under `dispatch/`,
  per-domain connection helpers under `connection/`, and owned services
  (`IdempotencyStore`, `CredentialService`, `AdmissionService`) under
  `services/`.
- `loom-local`: the native launcher and local backend host. It owns process
  arguments, persistence-path preparation, schema status/reset, local
  credential storage, repository bootstrap, and native backend embedding, so
  `loom-ui` does not link backend implementation crates directly.
- `loom-cli`: local server startup, diagnostics, and administration.

The backend should expose domain services rather than exposing raw model APIs,
filesystem, and process primitives directly. This keeps provider
normalization, authorization, auditing, cancellation, and future sandboxing
in one place.

In this document, a **workspace** is a durable logical grouping for agent
sessions and settings shared by those sessions. It has an ID and display
metadata, but it has no filesystem root and is not a repository. A workspace
is a grouping and configuration boundary, not a security boundary. Sessions
reference their workspace; their files and repository checkouts remain
session-scoped.

Each `AgentSession` owns a backend-managed **session filesystem** with its own
root. It contains session data and zero or more attached sources at normalized
paths relative to that root. GitHub repositories receive independent checkouts.
A directory selected in native local mode is attached in place: file and Git
operations use its original path, and the session discovers Git repositories
in that directory and its immediate children. The worker persists both the
source path and the session-relative attachment path. A fork copies attached
contents into its own filesystem so the two sessions do not share a mutable
working tree after forking.

Use independent clones for remote repositories. A shared bare-object cache may
reduce clone cost, but it is an internal optimization. Native local directories
are an explicit exception: a session edits the original files, and another
session can attach the same path. Sharing a checkout with hierarchical
sub-sessions may be considered later; it would need explicit ownership and
coordination rules for concurrent file edits and Git operations. The
filesystem boundary does not by itself promise OS-level process sandboxing;
process isolation is a separate backend security capability.

`loom-workspace` is the session-filesystem service, despite the crate's
historical name. It owns file trees, contents, watches, edits, snapshots,
checkpoints, and repository-instruction discovery rooted at one session
filesystem. It validates and resolves session-relative paths, but does not
own the durable workspace record or its settings. The session owns its
repository membership. The worker node coordinates attach, detach, and
fork-copy operations, using the filesystem service for paths and files and
`loom-vcs` for Git operations against a session's checkout. The network
server authenticates and transports these requests to the worker node; it does
not own repository lifecycle.

Tools, terminal working directories, process permissions, VCS status and
diffs, and file events all resolve against the active session filesystem.
Repository-relative paths are resolved through that session's repository
record and checked against its filesystem root. The GPUI client projects
backend-owned workspace, session, run, approval, session-filesystem, task, and
evidence state into a workspace/session navigator and active-session canvas.
The canvas contains the chronological agent conversation and tool timeline
plus a composer for new tasks, follow-up direction, and answers to agent
questions.

Changed paths, bounded diffs, task artifacts, and repository status are
read-only review projections opened in a drawer or focused overlay. They are
loaded through the canonical session-filesystem, process, and VCS services
and remain scoped to the active session. The client does not own an editable
buffer, tab/pane layout, language-service state, or orchestration state. A
remote client receives the same projections over the authenticated transport.

The project-session and coordinated-agent design is specified in
[project sessions and coordinated sub-agents](project-sessions-design.md).
A project is a root agent session in a workspace; descendants are ordinary
agent sessions with durable project and parent relationships. The backend
owns delegated task state, addressed agent messages, hierarchy limits, child
lifecycle, and worktree integration state. Do not implement orchestration as
prompt-only conventions or client-owned state. Project operations are exposed
through the versioned protocol and capability negotiation.

## Agent runtime

An agent session is a state machine, not an unbounded loop:

```text
planning -> executing <-> evaluating -> completed / failed / cancelled
                |
                +-> awaiting_approval / needs_input / paused
                          |
                          +-> (resume) -> awaiting_approval / needs_input / executing
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
- Project coordination across durable child sessions, with explicit depth,
  message, permission, recovery, and integration rules as defined in the
  project-session design.
- Per-session limits for time, model tokens, tool calls, processes, and cost.
- Checkpoints before risky mutations and a way to restore or inspect them.
- Context compaction that preserves the task, decisions, constraints, and
  unresolved work rather than silently dropping history.
- Provider failover only when the policy allows it; never silently switch
  models during a user-visible action.

The UI may render a conversational transcript, but the backend event stream
is authoritative. Internal provider reasoning is optional display metadata: it
may be carried to the client so a reloaded transcript shows it, but it is never
a requirement of the protocol, and providers return user-visible messages,
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

The persistence layer stores typed SQLite tables covering session state, the
authoritative event journal, serializable agent runtime state, workspace state
and checkpoints, policy decisions, provider health, and usage ledgers. It uses
a single baseline schema; a database written by another revision is rejected
unchanged and must be wiped. A runtime that was executing during a process
crash is recovered in `paused` state so a new connection must explicitly
resume it.

Remote access uses the same domain services through a transport adapter:

```text
HTTP upgrade + bearer credential
 (Authorization header or loom.bearer subprotocol)
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

The repository is a Rust workspace:

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
  loom-local/
  loom-cli/
  loom-ui/
```

Tests live with the crate they cover: unit tests in each crate, protocol
contract tests in `crates/loom-protocol/tests`, and remote transport tests in
`crates/loom-server/tests`. The important boundary is that shared Rust domain
and protocol crates do not depend on a particular window system or browser
API.
