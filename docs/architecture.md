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

The frontend is a GPUI application, using `gpui-gc` or the maintained GPUI
variant required for browser support. It should be organized into:

- `ui`: views, layout, input handling, commands, and theme.
- `frontend-core`: client-side state projection, caching, routing, and
  protocol requests.
- `agent-ui`: session list, conversation, plan/approval controls, tool
  timeline, context inspector, model selection, usage, and diff review.
- `platform`: native windowing/clipboard/file dialogs and browser bindings.
- `protocol-client`: transport-independent connection and request handling.

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
- `loom-persistence`: atomic versioned state storage used by the in-process
  backend. Its JSON file format is an implementation detail; domain and
  protocol types remain provider-neutral.
- `loom-tools`: typed tool definitions, permission checks, execution policies,
  result normalization, and tool adapters.
- `loom-workspace`: file tree, file contents, watches, edits, and snapshots.
- `loom-process`: commands, terminals, task supervision, output streaming, and
  cancellation.
- `loom-vcs`: source-control abstraction and repository operations.
- `loom-language`: language-server lifecycle, requests, diagnostics, and
  symbols.
- `loom-protocol`: versioned request/response/event schemas and codecs.
- `loom-server`: listeners, authentication, connection management, and
  deployment configuration.
- `loom-cli`: local server startup, diagnostics, and administration.

The backend should expose domain services rather than exposing raw model APIs,
filesystem, and process primitives directly. This keeps provider
normalization, authorization, auditing, cancellation, and future sandboxing
in one place.

M5 keeps `Workspace` as the authoritative root/revision boundary and layers
`EditorWorkspace` on top of it. Editor buffers are an in-memory projection:
they normalize text for editing, retain the original encoding/newline policy,
and save through revision-checked workspace byte writes. A save that observes
an external revision returns `external_change` and never overwrites the
outside edit. Tabs and panes are client-facing layout state; sessions,
workspaces, and agent runs remain the existing backend objects.

`loom-language` currently exposes a lifecycle/capability interface and a
deterministic basic implementation. It is deliberately not a claim that a
full language server exists for every file type: descriptors report
unsupported/unavailable services and operations return stable errors.
`loom-vcs::GitService` scopes direct `git` argv execution to the canonical
workspace root and validates every path before status, diff, index, or commit
operations.

The GPUI client projects these services into a compact activity/file sidebar,
editor center, agent timeline/approval panel, diagnostics/outline and task
result areas, and a Git/status footer. A remote client receives the same
typed projections over M4 transport; no UI-only workspace truth is created.

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

The agent runtime talks to a normalized model interface:

```text
ModelProvider
  list_models()
  describe_model()
  negotiate_capabilities()
  stream_completion(messages, tools, options)
  count_tokens(messages, tools)
  health_check()
  cancel(request_id)
```

Adapters translate provider-specific streaming formats, tool-call schemas,
authentication, errors, and usage metrics into the normalized interface.
Provider configuration belongs to the backend and secrets are referenced by
credential IDs; raw keys must not be sent to or persisted by the frontend.

The initial provider classes are:

- Hosted providers with native APIs and tool calling.
- OpenAI-compatible HTTP endpoints for self-hosted and organization gateways.
- Local inference servers such as Ollama, llama.cpp-compatible servers, or
  other OpenAI-compatible local runtimes.
- Deterministic test providers that replay scripted responses and tool calls.

Local models may have weaker tool calling, smaller context windows, or
different streaming behavior. The capability handshake must let the runtime
adapt prompts and feature availability without making the UI provider-aware.

M3 implements the registry with deterministic, OpenAI-compatible, and Ollama
configurations. Credentials are represented by opaque references; the
registry resolves them only while constructing a backend provider. Usage is
recorded as normalized token/cost records, and transport/status failures are
mapped to stable Loom error codes. Provider configuration summaries never
contain endpoints with credential material or raw keys.

The first durable implementation stores a versioned backend snapshot through
an atomic temporary-file replacement. It includes the session manager state,
authoritative event journal, serializable agent runtime state (including
messages, pending approvals, steps, limits, and usage), workspace state and
checkpoints, policy decisions, provider health, and usage ledgers. A runtime
that was executing during a process crash is recovered in `paused` state so a
new connection must explicitly resume it.

M4 keeps `InProcessBackend` as the single domain service and adds a transport
adapter in `loom-server::remote`:

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
  loom-language/
  loom-protocol/
  loom-server/
  loom-cli/
frontend/
  src/
  assets/
tests/
  protocol/
  integration/
```

The exact layout can change as implementation begins. The important boundary
is that shared Rust domain and protocol crates do not depend on a particular
window system or browser API.
