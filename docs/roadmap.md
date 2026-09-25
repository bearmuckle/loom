# Roadmap and quality bar

Each milestone should end with a demonstrable vertical slice. Avoid building
an entire layer in isolation before proving the end-to-end path.

## Milestones

### M0 - Foundations

- Create the Rust workspace and CI for formatting, linting, unit tests, and
  supported target builds.
- Choose the GPUI/`gpui-gc` revision and record native and wasm constraints.
- Define agent/session IDs, errors, event envelopes, capability negotiation,
  protocol versioning, and provider-neutral model types.
- Add a minimal native shell and a protocol test harness.

**Exit condition:** a native shell connects to an in-process backend, creates
an empty agent session, and renders its state and event stream.

### M1 - Single-agent vertical slice

- Implement one deterministic test provider and one real provider adapter.
- Start a session with a task, system/repository instructions, and a selected
  model.
- Implement the agent state machine, streamed assistant messages, typed tool
  calls, tool results, interruption, retry, and completion.
- Add the first tools: list files, read file, search text, apply patch, and
  run a command.
- Render a conversation, plan, tool timeline, approval prompt, and final
  summary.

**Exit condition:** a user can ask an agent to make a small repository change,
approve the plan and patch, run a test command, and inspect the final diff.

### M2 - Session filesystems and human control

- Create sessions with backend-managed isolated filesystem roots and expose
  file snapshots. Attach one or more repository sources to a session, each
  checked out at a stable relative path. Change detection is pull-based
  revision comparison; a watch mechanism is not implemented yet.
- Add persistent terminals with output streaming, resize, input, and
  cancellation.
- Add task supervision, bounded output buffers, backpressure, exit status, and
  artifact links.
- Add approval policies for reads, writes, commands, network access, and
  destructive operations.
- Add checkpoints, undo/revert for agent edits, conflict detection, and
  user takeover.

**Exit condition:** an agent can complete a multi-file task using searches,
patches, tests, and a terminal while the user can stop or reject any action.

### M3 - Providers and durable orchestration

- Add multiple hosted providers, OpenAI-compatible endpoints, and at least one
  local model runtime.
- Add provider/model discovery, capability negotiation, credential references,
  token accounting, rate-limit handling, and provider health.
- Persist sessions, prompts, tool calls, approvals, checkpoints, and model
  usage so sessions can be resumed after a process restart.
- Add context inspection, summaries, compaction, token budgets, and per-session
  time/tool/cost limits.
- Add session fork, retry from checkpoint, pause/resume, and background work.

Implementation notes for the first vertical slice:

- `loom-providers` supplies deterministic, OpenAI-compatible, and Ollama
  configurations with normalized discovery, capability intersection,
  credential references, usage records, rate-limit/error normalization, and
  health state.
- `loom-persistence` stores sectioned SQLite state atomically. The sections
  cover the session/event journal, serializable runtime state, checkpoints,
  policies, provider health, and usage. Recovery pauses unfinished runs for
  explicit resume.
- `loom-context` reports assembly decisions, compaction summaries, omissions,
  and budgets. Runtime limits are represented by typed usage/limit events.

**Exit condition:** the same task can be run with deterministic and
OpenAI-compatible/local configurations, and a disconnected/restarted backend
resumes the session without losing the transcript or workspace state. The
CLI `--m3-demo` exercises provider discovery and a persistent restart fixture;
the GPUI shell exposes provider count and pause/resume controls.

### M4 - Remote backend control

- Run the backend as a standalone local or remote service.
- Implement WebSocket transport, authentication, reconnect, and event resume.
- Add capability discovery and explicit workspace/session permissions.
- Add protocol compatibility and migration tests.
- Keep agent execution and event journaling independent of frontend lifetime.

M4 implementation notes:

- `loom-server` provides an opt-in, loopback-by-default JSON WebSocket service
  and a native `WebSocketTransport` fixture. Bearer tokens are scoped to
  capabilities/projects/sessions and are checked on every request.
- `GetSessionEvents` resumes from a global sequence. Bounded retention returns
  `SessionEventsSnapshot` with a resume cursor when history is unavailable, and
  retryable mutations are deduplicated by durable request ID.
- Heartbeats, request deadlines, cancellation frames, malformed-payload
  responses, and bounded outbound queues are transport protections; runtime
  and journal state remain backend-owned after disconnect.
- `cargo run -p loom-cli -- --m4-demo` demonstrates a second client
  reconnecting, observing steps, approving both deterministic tool actions,
  and retrieving the completed result.

**Exit condition:** a second native client can securely reconnect to a running
agent, observe its current step, approve an action, and retrieve its result.

### M5 - Agent workspace and orchestration surface

M5 is the first focused native client for the product's primary object: a
durable agent session. It should feel close to the GitHub Copilot app in
information architecture and interaction model, while retaining Loom's
backend-owned state, provider neutrality, and remote-control boundary.

- Provide a small workspace/session navigator with new-session, rename, resume,
  archive, and connection-status actions.
- Make the active session the main canvas: task prompt, streamed assistant
  messages, proposed plan, step progress, tool calls, bounded command output,
  approval requests, errors, and the final summary are one chronological,
  collapsible run view.
- Keep human control in the primary flow. The composer can start a run, send
  follow-up direction, answer an agent question, and continue a paused run.
  Pause, interrupt, retry, approve, and reject actions are visible where they
  apply.
- Add a small review surface for changed-file lists, read-only diffs, task
  results, and evidence links. Details can open in a drawer or focused
  overlay; there is no permanent IDE-style panel grid.
- Preserve reconnect and resume behavior. Reopening a session reconstructs
  the current run from backend snapshots and event history rather than from
  client-local UI state.
- Use a restrained, compact visual system: clear hierarchy, dense but
  readable typography, subdued separators, and a calm dark/light theme.
  Zed is a reference for visual tone only, not for editor features,
  navigation, or layout behavior.

The M5 client should reuse the existing session-filesystem, process, task,
VCS, and protocol services. Add only the narrow session, run, review, and
evidence projections that the UI needs; do not introduce an editor buffer
authority or a second orchestration model.

**Exit condition:** a user can open a workspace, start an agent session, watch a
plan and live tool timeline, approve or interrupt work, provide follow-up
direction, reconnect to a paused or running session, and review changed files,
diffs, and validation evidence before continuing or handing off the task.

M5 deliberately does not include editable buffers, tabs or splits, autosave,
language-server UX, fuzzy project navigation, an interactive terminal,
staging/commit controls, branch management, or a rich multi-agent topology
view. Those capabilities may be added later without changing the
session-centric product model.

### M6 - Browser and wasm target

- Build the shared frontend for wasm with the selected GPUI variant.
- Implement browser transport, authentication handoff, and reconnect UX.
- Replace native-only APIs with platform adapters.
- Add browser-specific file, clipboard, download, and accessibility behavior.

**Exit condition:** the browser client can complete the M1-M5 agent workflows
against a remote backend using supported browsers.

## Quality bar

### Workspace/session model transition

The current implementation uses project IDs to couple a local directory,
workspace services, configuration, and sessions. This is a transitional
bootstrap model, not the product model. Evolve it without silently changing
existing persisted or protocol identities:

- Introduce workspace records that group sessions without requiring a
  filesystem path or repository.
- Move session ownership from project IDs to workspace IDs.
- Give every session its own managed filesystem root and repository
  attachment list; keep mutable checkouts session-exclusive.
- Scope filesystem, process, terminal, checkpoint, and VCS services to the
  session. Keep any shared clone cache immutable and backend-internal.
- Migrate the CLI's current folder-backed `--workspace` bootstrap as a
  compatibility path, then replace it with explicit workspace and
  session-repository operations.

Before implementation, settle repository attachment lifecycle and checkout
choices (clone versus worktree) at the service boundary. Those choices must
not change workspace identity or permit one session's tools to mutate another
session's working tree.

### Correctness

- Workspace edits are atomic where possible and detect conflicts.
- Every request has a defined success, failure, and cancellation path.
- Reconnect never duplicates non-idempotent mutations.
- Backend state remains valid if a client disappears at any point.

### Performance

Measure the paths users feel:

- Time to first usable project view.
- Time to first streamed model output.
- Keystroke-to-render latency for ordinary files.
- Search latency on representative repositories.
- Terminal and tool-output throughput and memory use.
- Reconnect and snapshot time.
- CPU, memory, and binary size for native and wasm builds.

Use representative repositories and slow-network profiles rather than
optimizing only loopback benchmarks.

### Testing

- Unit-test domain services and state transitions.
- Contract-test every protocol message and version negotiation path.
- Integration-test a real backend through each transport.
- Test provider streaming, tool-call normalization, cancellation, reconnect,
  backpressure, malformed payloads, and permission failures.
- Run a small browser compatibility matrix once wasm work begins.
- Keep deterministic fixtures for files, processes, VCS state, model
  responses, and tool calls, including event-stream fixtures that prove a
  delta is journaled before a completion ends and that a run can be
  interrupted while a model call is open.

## Decisions to settle early

These choices should be recorded as architecture decision records when made:

- The exact GPUI/`gpui-gc` fork and supported native platforms.
- The first wasm/browser support target and minimum browser versions.
- MessagePack versus CBOR, and the schema/code-generation approach.
- Whether QUIC is a first-release transport or a later optimization.
- The persistence engine for sessions, event journals, and configuration.
- The minimum authentication mechanism for remote deployments.
- The supported source-control providers beyond Git.
- The provider adapter contract and local model compatibility baseline.

## Non-goals for the first release

- Replacing every existing developer tool.
- Supporting arbitrary plugins with unrestricted native code execution.
- Promising identical behavior across native and browser environments when
  platform security models differ.
- Building a hosted service before the standalone backend and protocol are
  reliable.
- Hiding destructive or privileged actions behind automatic behavior.
- Extensibility, collaboration, and language-server integration are deferred
  until the M0-M6 product loop has proven stable.
