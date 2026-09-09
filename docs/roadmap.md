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

### M2 - Workspace tools and human control

- Open a configured project root and expose file snapshots and watches.
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
- `loom-persistence` stores a versioned JSON snapshot atomically. The
  snapshot covers the session/event journal, serializable runtime state,
  checkpoints, policies, provider health, and usage. Recovery pauses
  unfinished runs for explicit resume.
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

**Exit condition:** a second native client can securely reconnect to a running
agent, observe its current step, approve an action, and retrieve its result.

### M5 - Full coding workspace

- Add the editor with tabs, splits, unsaved state, agent change markers, and
  conflict-aware saves.
- Integrate language-server lifecycle management, diagnostics, symbols,
  definitions, and references.
- Add fuzzy navigation, project-wide search, repository instructions, and
  context file references.
- Show repository status and diffs; add staging, unstaging, commit creation,
  branch information, and conflict visibility.
- Add test/build/lint result views and evidence links in the final agent
  summary.

**Exit condition:** a user can complete a realistic issue-driven coding task
from prompt through agent plan, implementation, validation, diff review, and
commit preparation.

### M6 - Browser and wasm target

- Build the shared frontend for wasm with the selected GPUI variant.
- Implement browser transport, authentication handoff, and reconnect UX.
- Replace native-only APIs with platform adapters.
- Add browser-specific file, clipboard, download, and accessibility behavior.

**Exit condition:** the browser client can complete the M1-M5 workflows
against a remote backend using supported browsers.

### M7 - Extensibility and collaboration

- Publish protocol, model-provider, tool, and extension interfaces.
- Add parallel child agents, isolated worktrees, session handoff, comparison,
  and selective merge/discard.
- Add provider adapters, additional VCS systems, language servers, task
  runners, and deployment targets.
- Add session sharing, invitations, revocation, and audit history.
- Add performance tooling and deployment documentation.

**Exit condition:** a third-party adapter can be added without modifying
frontend views or the backend session model; two authorized clients can work
against one session predictably; and a multi-agent task has inspectable
dependencies and outcomes.

## Quality bar

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
- Keep deterministic fixtures for files, processes, diagnostics, VCS state,
  model responses, and tool calls.

## Decisions to settle early

These choices should be recorded as architecture decision records when made:

- The exact GPUI/`gpui-gc` fork and supported native platforms.
- The first wasm/browser support target and minimum browser versions.
- MessagePack versus CBOR, and the schema/code-generation approach.
- Whether QUIC is a first-release transport or a later optimization.
- The persistence engine for sessions, event journals, and configuration.
- The language-server process model and sandboxing strategy.
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
