# Loom

Loom is a cross-platform, remote-first agentic development environment. It
combines a native or browser-based interface with a Rust backend that
orchestrates persistent coding-agent sessions, workspace state, tool
execution, and integrations.

The primary product is an agent orchestration application with a code
workspace. A user gives an agent a repository task, watches it inspect,
plan, edit, run tools, respond to failures, and produce a reviewable result.
The user remains in control through approvals, interruption, feedback,
checkpoints, and explicit permission policies.

The backend can run locally, as a child process, or remotely. Hosted model
providers, OpenAI-compatible endpoints, and local model runtimes should be
usable through the same session and tool model. Native and browser clients
should be interchangeable views of the same durable backend session.

## Specification

The project specification is split into focused documents:

- [Product scope and use cases](docs/product.md) - target users, workflows,
  feature areas, principles, and the first end-to-end experience.
- [Architecture](docs/architecture.md) - frontend, Rust backend, agent
  runtime, model providers, tools, and repository structure.
- [Communication protocol](docs/protocol.md) - transports, event model,
  reconnect behavior, provider-neutral operations, and streaming.
- [Security model](docs/security.md) - trust boundaries, permissions,
  credentials, prompt injection, and resource limits.
- [Known competitors and prior art](docs/competitors.md) - comparable
  products, useful patterns, near-misses, and design lessons.
- [Roadmap and quality bar](docs/roadmap.md) - milestones, exit conditions,
  performance, testing, decisions, and first-release non-goals.

These documents describe the intended first implementation. Each milestone
should produce a demonstrable vertical slice, and architecture decisions
should be recorded as implementation settles details such as the GPUI
variant, schema format, persistence engine, and supported deployment targets.

## M0 foundation and M1 vertical slice

The M0 foundation is a Rust workspace with shared domain types, provider-
neutral model types, a versioned JSON protocol, an in-process backend, and a
native command-line shell. Run the vertical slice with:

```sh
cargo run -p loom-cli -- --name "Foundation demo"
```

The shell negotiates protocol capabilities, creates an empty agent session,
and renders the authoritative session event stream.

M1 adds the deterministic provider, an OpenAI-compatible adapter, the
workspace tools, approvals, interruption, retry, streamed agent events, and
the GPUI three-pane native client:

```sh
# Text event-stream client with automatic approvals
cargo run -p loom-cli -- --task "make a small repository change"

# GPUI client against the isolated M1 demo workspace
cargo run -p loom-ui
```

The GPUI client starts against an isolated temporary workspace and pauses on
write/command approvals. See [ADR 0001](docs/decisions/0001-foundation.md) and
[ADR 0002](docs/decisions/0002-m1-agent-vertical-slice.md) for the
implementation decisions.

## M2 workspace and human control

M2 adds workspace, process, and recovery services while keeping the M1
in-process protocol and approval flow compatible:

- `loom-workspace` opens a canonical, workspace-scoped root and provides
  bounded file snapshots, safe reads/edits, polling change events, checkpoints,
  undo/revert, conflict detection, and an explicit user takeover boundary.
- `loom-process` provides persistent standard-process terminals with output
  event streams, input, recorded resize state, exit status, cancellation, and
  bounded build/test/lint-style task supervision with artifact metadata.
- Typed `ApprovalPolicy` decisions cover reads, writes, commands, network, and
  destructive actions. Agent policy evaluations are journaled next to the
  existing approval prompts, so automatic, approval-required, and denied
  actions remain visible to clients.

The native shell can exercise the M2 surfaces after an agent run:

```sh
cargo run -p loom-cli -- --m2-demo --manual-approval
```

The GPUI client displays the workspace snapshot count and the same approval,
reject, interrupt, and retry controls. See
[ADR 0003](docs/decisions/0003-m2-workspace-and-human-control.md) for the
implementation tradeoffs and deferred PTY, persistence, and remote-watch
work.

## M3 providers and durable orchestration

M3 keeps the M2 protocol and workspace boundaries while adding a provider
registry and restart-safe orchestration:

- deterministic fixtures, arbitrary OpenAI-compatible HTTP configurations, and
  an Ollama-compatible local runtime share normalized model descriptors,
  capability negotiation, usage ledgers, health state, and rate-limit/error
  codes;
- provider configuration stores only opaque credential references in
  descriptors, events, and durable state. Secret material is resolved by the
  backend credential store and is never returned by `ListProviders`;
- sessions, event envelopes, prompts/messages, run state, steps, tool and
  approval events, policies, workspace checkpoints, and model usage are saved
  in an atomic, versioned JSON state file when the backend is opened with
  `InProcessBackend::new_persistent`;
- context inspection reports instruction/conversation items, token budgets,
  summaries, compaction, and explicit omissions. Session limits cover elapsed
  time, input/output tokens, tool calls, and cost without silently switching
  providers or truncating required context;
- pause/resume, fork, retry-from-checkpoint, and recovery after restart are
  typed protocol operations. A disconnected client does not own the runtime;
  another connection can resume the same backend state.

Run the deterministic provider, list the deterministic and local provider
configurations, then close and reopen the backend from the persisted transcript:

```sh
cargo run -p loom-cli -- --m3-demo
```

The local-compatible fixture uses `http://127.0.0.1:11434/v1/chat/completions`
and model `llama3.2`; pass `--model llama3.2` when an Ollama server is
available. The persistence file can be selected explicitly with
`--persistence <path>`. See
[ADR 0004](docs/decisions/0004-m3-providers-and-durable-orchestration.md) for
the persistence, provider, context, and recovery tradeoffs.

## M4 remote backend control

M4 adds an opt-in standalone service without changing the in-process protocol:

- `loom-server` exposes the existing typed request/response envelopes over a
  JSON WebSocket transport suitable for native and browser clients;
- the default bind address is loopback. Network binding is explicit and the
  service requires a bearer token before protocol negotiation, capability
  discovery, workspace access, or provider access;
- tokens can be scoped to capabilities, projects, and sessions and are
  revocable while connections are open. Tokens and provider secrets are never
  written to event journals or request logs;
- bounded journal retention returns a typed session snapshot fallback when a
  reconnecting cursor is older than retained history. Retryable mutations use
  request IDs as durable idempotency keys;
- heartbeats, request deadlines, cancellation frames, malformed-payload
  errors, and bounded outbound queues make transport failure explicit while
  the backend runtime continues independently of a frontend connection.

Start a local service with an explicit token and optional durable state:

```sh
cargo run -p loom-cli -- --serve --token "$LOOM_TOKEN" \
  --bind 127.0.0.1:8765 --persistence ~/.local/share/loom/state.json
```

The deterministic two-client fixture starts a loopback server, disconnects the
first client while a run is awaiting approval, reconnects a second client,
resumes events, approves the pending actions, and retrieves the result:

```sh
cargo run -p loom-cli -- --m4-demo
```

See [ADR 0005](docs/decisions/0005-m4-remote-backend-control.md) for the
transport/authentication/reconnect choices and their current limitations.

## M5 coding workspace

M5 turns the GPUI shell into a coding workspace while keeping the M4
backend-owned workspace and remote protocol boundaries:

- `loom-workspace::EditorWorkspace` provides multi-file buffers, tabs,
  splits, undo/redo, manual/focus-loss/idle autosave, newline and UTF-8/UTF-16
  preservation, large-file limits, external-edit conflicts, agent markers,
  file-tree navigation, fuzzy open, literal project search, and repository
  instruction/context references.
- `loom-language` provides explicit lifecycle and capability descriptors plus
  a deterministic basic service for diagnostics, symbols, definitions, and
  references. Unsupported languages return structured capability errors.
- `loom-vcs` runs scoped Git commands with argument vectors (never shell
  concatenation) for status, diffs, staging, unstaging, commits, branches,
  and conflict visibility.
- `loom-process` task snapshots include bounded build/test/lint output and
  stable `loom://task/...` evidence links. Agent run snapshots can attach
  evidence links to final summaries.
- The GPUI layout now has activity/file navigation, an editor center with
  tabs and unsaved/conflict indicators, an agent timeline/approval panel,
  diagnostics/outline and task-result areas, and a Git/status footer.

M5 protocol requests are additive and capability-gated, so in-process and
WebSocket clients use the same editor, language, VCS, navigation, and task
evidence operations. See [ADR 0006](docs/decisions/0006-m5-coding-workspace.md)
for the choices and deferred limitations.
