# Agent goal command and queued steering

> **Storage note.** Loom uses a single SQLite database with a single baseline
> schema (currently version 2) and no migration ladder; a database written by
> any other revision is rejected unchanged and must be wiped explicitly. Adding
> durable per-session goal state changes the baseline schema and therefore bumps
> the schema version, which requires an explicit state wipe. See
> [durable storage](storage.md) and [the storage design](storage-design.md).

## Status and scope

This document turns [GitHub issue #142](https://github.com/bearmuckle/loom/issues/142),
“Agent goal strategy command,” into a design and sequencing plan. The issue has
no body, so this document records the intended interpretation:

- A single client slash command, `/goal`, sets and displays the durable goal of
  the active session.
- The “strategy” half of the title is dropped: a separate strategy concept
  overlaps with the existing `propose_plan` tool and `AgentPlan` state
  (`crates/loom-agent/src/steps.rs:501`, `crates/loom-protocol/src/agent.rs:118`)
  and does not justify a second user-facing noun.
- While a run is active, `/goal` also steers that run. This depends on fixing
  [GitHub issue #125](https://github.com/bearmuckle/loom/issues/125), where a
  user message sent during an active run fails instead of queueing.

The goal is not a new concept. `docs/product.md:7,22` already makes the user’s
goal the property a project owns, distinct from the concrete task a run is
given. This document gives that existing product noun a persisted, user-editable
form and a command.

This document is a plan only. It contains no implementation. The work can be
delivered as a single change or, when that is more convenient, in iterations.
The [delivery iterations](#delivery-iterations) below are a suggested sequence,
not a required decomposition.

## Problem

Loom has no first-class notion of what a session is trying to achieve.

- The session objective is only the first user message. `send_message`
  (`crates/loom-ui/src/view/runs.rs:4`) derives the session title from that
  message and passes it to `StartSessionAgentRun.task`; it is also cached in
  `session_task_cache` for the transcript.
- The only persisted per-session settings are the approval policy and the
  auto-approve flag (`crates/loom-persistence/src/lib.rs:154`,
  `session_settings` in `crates/loom-persistence/src/schema.rs:404`).
- The system and repository instructions sent with a run are hardcoded in the
  client (`crates/loom-ui/src/view/runs.rs:102`, `crates/loom-ui/src/connection.rs:925`).
- `message_entry_inner` rejects a user message whenever the run is
  `Planning`, `Executing`, or `Evaluating`
  (`crates/loom-agent/src/control.rs:361`). There is no queue, so a user cannot
  send direction while the agent is busy. The run worker already holds the
  runtime lock across a model step (`crates/loom-server/src/backend/runs.rs:143`),
  so an entry is forced to fail or block.

The result is that a user cannot state or inspect a durable objective, and
cannot add direction mid-run.

## Interpreted requirement

`/goal` must do all of the following:

1. **Set** the session goal durably, so it survives reconnect and restart.
2. **Show** the current goal when invoked without an argument.
3. **Apply to the next run**: the goal is included when the next run of the
   session starts.
4. **Steer the active run** when one is running.
5. **Queue, not fail**: sending direction during a busy run must queue and be
   delivered at the next safe model-turn boundary (#125).

The goal is a standing objective for the session. The message that starts a run
remains the concrete task; the goal is layered above it. Changing the goal does
not require starting a new run.

## Design

### Goal as a per-session property

The goal is a bounded, optional, trimmed string owned by the session. `None`,
set through `/goal clear`, clears it. It is:

- **standing**, not per-run: once set it applies to every subsequent run until
  changed, matching the product statement that a project *owns* the user’s goal
  (`docs/product.md:22`);
- **distinct from the task**: the message that starts a run stays the concrete
  task, and the goal is layered above it; changing the goal does not start a run;
- **pinned in context**: folding the goal into `system_instructions` means it is
  never compacted away, because the context assembler always includes system
  instructions in its required set (`crates/loom-context/src/lib.rs:59`);
- persisted in `session_settings`, next to the approval policy;
- exposed in the session projection so any client can render it;
- captured into the run at start, so a goal change never retroactively rewrites
  a running or completed transcript.

### Applying the goal to a run

At run start, the server reads the session goal and includes it in the run’s
system instructions. Folding it into `system_instructions` avoids new agent
runtime state: `DurableRunRuntimeConfig.system_instructions` already persists
(`crates/loom-persistence/src/lib.rs:184`) and is replayed on recovery.

Concretely, in `start_run_with_options`
(`crates/loom-server/src/connection/runs.rs:360`) the effective system
instructions become the caller’s `system_instructions` plus a labeled goal
block when a goal is set. The goal is read server-side rather than composed by
the client so that every client and every replayed run sees the same value.

> **Alternative considered.** Add a `goal` field to `AgentTask`
> (`crates/loom-agent/src/lib.rs:44`) and render it as its own system message in
> `initial_messages` (`crates/loom-agent/src/activity.rs:146`). This gives a
> cleaner transcript distinction but adds agent state, sorting, and projection
> surface for no behavioral gain. Fold into `system_instructions` first; revisit
> if the UI needs to separate goal from instructions.

### Protocol surface

Add to `SessionRequest` (`crates/loom-protocol/src/requests.rs:172`):

```text
SetSessionGoal { session_id: AgentSessionId, goal: Option<String> }
```

Add to `SessionResponse`:

```text
SessionGoal(Option<String>)
```

Add to `AgentSessionSnapshotProjection`
(`crates/loom-protocol/src/lib.rs:145`):

```text
#[serde(default)]
goal: Option<String>
```

`#[serde(default)]` keeps the projection decodable by a client that predates the
field. The new request is only reachable by a client that negotiates a new
enough version; the current constant is `11.1`
(`crates/loom-protocol/src/lib.rs:55`). Bump the protocol minor and rely on the
existing major-version rejection (`crates/loom-core/src/version.rs:14`).
Forward compatibility for old clients is explicitly unsupported
(`docs/roadmap.md:226`).

No new capability is needed: map `SetSessionGoal` to the existing
`ControlAgentSession` capability, the same one that guards rename and archive
(`crates/loom-protocol/src/lib.rs:336`), and add it to `is_retryable_mutation`
so a retried mutation deduplicates by request ID like the other session
settings.

Validation: trim the input; reject a goal longer than a bounded limit (propose
16 KiB, matching the other 16 KiB limits in the schema); `None`, sent by
`/goal clear`, clears the goal. An empty or whitespace-only string is treated as
invalid, not as a clear, so `/goal` with no argument stays show-only.

### Persistence

Add a nullable `goal TEXT` column to `session_settings`
(`crates/loom-persistence/src/schema.rs:404`) with a length check, and extend:

- `DurableSessionSettings` with `goals: BTreeMap<AgentSessionId, String>`
  (`crates/loom-persistence/src/lib.rs:154`);
- the `session_settings` upsert/delete and load paths
  (`crates/loom-persistence/src/catalog.rs:694` and `:118`);
- the backend session service map and its startup/save mirrors
  (`crates/loom-server/src/backend/session_service.rs`,
  `crates/loom-server/src/backend/filesystem.rs:249`,
  `crates/loom-server/src/backend/persistence.rs:597`).

Bump `DATABASE_SCHEMA_VERSION` from 2 to 3
(`crates/loom-persistence/src/lib.rs:73`). Because there is no migration
ladder, this invalidates existing state databases and they must be wiped. The
remaining work in [issue #127](https://github.com/bearmuckle/loom/issues/127)
is the async runtime, neutral-domain persistence types, and test fixtures; it
does not schedule another schema bump, so this change can claim the v3 revision
and land on its own.

Forking copies the goal with the other session settings
(`crates/loom-server/src/dispatch/session.rs:118`).

### Client command and display

Add one `CommandSpec` named `goal` to `COMMANDS`
(`crates/loom-ui/src/view.rs:168`) and handle it in `run_command`
(`crates/loom-ui/src/view/composer.rs:43`):

- `/goal` (no argument): show the current goal, or say none is set.
- `/goal <text>`: persist the goal; if a run is active, also queue a steering
  message (see below). Report the result in the status banner.
- `/goal clear`: send `None` to clear the goal.

Update the `/help` summary text (`crates/loom-ui/src/view/composer.rs:44`).
Store the active goal in the view alongside the other projected session fields
(the view already holds `auto_approve_actions`, `session_task_cache`,
`pending_input`, and similar at `crates/loom-ui/src/view.rs:444-500`) and
refresh it from `AgentSessionSnapshotProjection` in the session load path.

Display: render the active goal as a compact `gpui-kit` chip at the point of
control, next to the composer, and make the chip clickable so it opens the
`/goal` input for editing. Keep it a chip, not a permanent panel. The chip is
absent when no goal is set, and a pending/queued indicator is shown when a
steering message is waiting.

### Steering an active run and queueing (#125)

The durable queue lives in the agent runtime so it is backend-owned,
multi-client, and restart-safe, matching Loom’s state model.

1. Add a `queued_user_messages` field (a `VecDeque` of bounded records) to
   `AgentRuntimeState` (`crates/loom-agent/src/lib.rs:84`) with
   `#[serde(default)]` for state compatibility.
2. Change `message_entry_inner` (`crates/loom-agent/src/control.rs:338`) so that
   when the run is `Planning`/`Executing`/`Evaluating` it enqueues instead of
   returning `InvalidState`. Validation of `attempt_id` and
   `expected_control_revision` still happens first, and the queued record keeps
   its target revision for diagnostics. Emit an
   `AgentEvent::UserMessageQueued { run_id, position }` so clients can render
   the pending state.
3. Drain the whole queue at a safe model-turn boundary in the run worker loop
   (`crates/loom-server/src/backend/runs.rs:102`), next to the existing
   `deliver_project_agent_messages` call. Deliver every queued message in FIFO
   order, each as its own user message, emitting the existing
   `AgentEvent::UserMessage` per message; this clears a backlog in one boundary
   instead of one message per step and keeps each message attributable. Define
   project-message and user-queue ordering explicitly; the simplest rule is
   project messages first, then queued user messages, all before the next step.
4. Update `RunHandle::apply_event` (`crates/loom-server/src/run_handle.rs:321`)
   to track the queued count so snapshots and polling reflect it.
5. Define terminal behavior: on interrupt/cancel, queued messages are dropped
   and a status is reported; on a normal turn boundary they are delivered. A
   pause retains them for resume.
6. The UI should show the queued state explicitly rather than relying on
   `optimistic_messages` (`crates/loom-ui/src/view.rs:447`), which is cleared
   when a user-message event arrives (`crates/loom-ui/src/view/lifecycle/events.rs:210`).

`/goal <text>` on a busy run sets the durable goal and enqueues a labeled
steering message such as `Session goal updated: <text>`. The enqueued message is
what steers the run; the persisted goal is what future runs inherit.

## Delivery iterations

The iterations below are a suggested sequence, not a required decomposition.
The work can be delivered as one change or as any subset; each delivered change
should land with tests.

- **Iteration 1 — Durable session goal (backend + protocol).** `SetSessionGoal`
  request/response, projection field, `session_settings.goal`, schema bump,
  load/save/fork, server dispatch and projection. No UI.
- **Iteration 2 — Goal applied to runs.** Read the session goal at run start and
  fold it into the effective system instructions. Tests prove a new run sees
  the goal and recovery replays it.
- **Iteration 3 — `/goal` command and display.** Command spec, set/show/clear
  handling, view state, clickable `gpui-kit` composer chip, and `/help` text.
- **Iteration 4 — Durable steering queue (#125).** Runtime queue, enqueue
  semantics, worker drain, events, snapshot tracking, terminal behavior.
- **Iteration 5 — Wire `/goal` to a busy run and surface queued state.** Depends
  on iteration 4.

Iterations 1–3 deliver the core value without touching the run loop; iteration 4
can land independently and unblocks issue #125. If a fast partial fix for #125
is wanted first, a client-only queue in the UI is possible, but it is not
durable and not shared across clients, so the backend queue in iteration 4
remains the target.

## Decisions

Settled while preparing this plan:

- **Goal vs task.** Distinct and layered. The goal is the standing session
  objective from `docs/product.md`; a run keeps its own concrete task.
- **Goal scope.** Persistent until changed; it applies to every subsequent run,
  not only the next one.
- **Context survival.** The goal is pinned as a system instruction, so it
  survives context compaction.
- **Schema timing.** The goal changes the baseline schema and claims version 3;
  the remaining #127 work does not schedule a competing bump.
- **Capability.** Reuse `ControlAgentSession`; no new capability, and the
  request is a retryable mutation.
- **Queue delivery.** Drain the entire queue at each model-turn boundary, one
  user message per queued item, in FIFO order.
- **Display and editing.** A clickable `gpui-kit` chip next to the composer
  opens the `/goal` input.
- **Clearing.** Explicit `/goal clear`; `/goal` with no argument only shows.
- **Goal length.** Capped at 16 KiB, matching the other 16 KiB schema bounds.
- **Steering wording.** A labeled message, `Session goal updated: <text>`, so
  the transcript shows why the direction changed.

## Out of scope

- A separate “strategy” concept or command.
- Propagating a goal from a project manager to delegated children.
- Goal templates, history, or multiple goals per session.
- Replacing `propose_plan`; the agent plan remains the agent’s own strategy.
- Editing the goal from the settings dialog.

## Verification

- **Protocol contract** (`crates/loom-protocol/tests/contract.rs`): encode and
  decode `SetSessionGoal`/`SessionGoal`, and confirm a projection without the
  `goal` key still decodes.
- **Persistence** (`crates/loom-persistence/src/tests.rs`): round-trip a goal,
  clear it, and assert the schema version is the new baseline.
- **Server** (`crates/loom-server/src/tests.rs`): set/get a goal, confirm it
  appears in the session projection, is folded into a started run’s system
  instructions, and is copied on fork.
- **Agent** (`crates/loom-agent/src/lib.rs` tests): enqueue while busy, drain at
  a boundary in FIFO order, retain across pause, drop on interrupt, and survive
  state export/restore.
- **UI** (`crates/loom-ui/src/view/tests.rs`): `/goal` shows, `/goal <text>`
  dispatches and reports, `/goal clear` clears, the composer chip reflects the
  goal, and a queued steering message is rendered.
- **Checks:** `cargo fmt --all -- --check`, Clippy, and tests for the affected
  crates, following `.github/workflows/ci.yml`.
