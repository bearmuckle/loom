# Implementation audit

This document records a point-in-time audit of the implementation against
the specification in [product.md](product.md),
[architecture.md](architecture.md), [protocol.md](protocol.md), and
[roadmap.md](roadmap.md). It is a findings
document, not a specification. Once a finding is resolved, remove it here and
update the specification document it refers to.

**Audited revision:** `b365327` (M5 client, after the M0-M6 roadmap rescope).

**Scope:** approximately 24k lines across 16 crates.

## Summary

The workspace type-checks cleanly, CI covers formatting, clippy, tests, and
builds, and the infrastructure crates are real implementations rather than
stubs. The crate boundaries are broadly the right ones.

Three structural problems undermine the product's central claims: model
streaming is buffered rather than incremental, agent runs cannot be
interrupted, and the protocol crate depends on the whole backend domain.
Separately, roughly 2,000 lines of editor and language-service code remain
wired into the backend and protocol after the milestones that justified them
were descoped.

| ID | Severity | Area | Finding |
| --- | --- | --- | --- |
| A1 | Critical | Providers, agent | Streaming is buffered, not incremental |
| A2 | Critical | Server, agent | Runs cannot be interrupted or paused |
| A3 | Critical | Protocol, model | Contract crate depends on backend domain |
| A4 | High | Workspace, protocol | Editor surface is descoped and unused |
| A5 | High | Language | `loom-language` is a heuristic, not a service |
| A6 | Medium | VCS | Index and commit surface exceeds current scope |
| A7 | Medium | Process | Terminals are piped stdio, not PTYs |
| A8 | Medium | Workspace | No file watching |
| A9 | Medium | UI | Client is monolithic and blocks its own UI thread |
| A10 | Medium | UI, server | Remote is not a first-class mode |
| A11 | Low | Docs | Specification describes a layout that does not exist |
| A12 | Low | Providers | Hardcoded model default and client impersonation |

## Critical findings

### A1 - Streaming is buffered, not incremental

`ModelProvider::stream` returns `Result<Vec<ModelStreamEvent>>`
(`crates/loom-providers/src/lib.rs:34`). The provider performs a blocking
`ureq` call, collects the entire completion, and returns it as a vector. The
agent runtime then iterates that vector and emits `AssistantMessageDelta`
events from it (`crates/loom-agent/src/lib.rs:843-875`).

No text reaches a client before the full completion has arrived. The delta
events are a replay of an already-complete response.

This contradicts the product promise that the agent "streams user-visible
messages, tool calls, output, progress, and errors", and makes the roadmap's
"time to first streamed model output" metric meaningless as written.

**Direction:** the provider interface should yield events incrementally, for
example through a callback, an iterator, or a channel, so that the runtime
can journal each delta as it arrives.

### A2 - Agent runs cannot be interrupted or paused

`StartAgentRun` drives the agent loop to completion synchronously inside the
request handler. `AgentRuntime::start` calls `advance`
(`crates/loom-agent/src/lib.rs:489-508`), and `advance` is a loop that issues
blocking model calls and executes tools until the run needs approval, needs
input, or finishes (`crates/loom-agent/src/lib.rs:778`).

Three independent mechanisms each make interruption impossible while a run is
in flight:

1. `InProcessBackend` holds a single `mutation_lock: Mutex<()>`
   (`crates/loom-server/src/lib.rs:202`) that is acquired for every retryable
   mutation (`crates/loom-server/src/lib.rs:1093`). `InterruptAgentRun`,
   `PauseAgentRun`, and `ResumeAgentRun` are all classified as retryable
   mutations (`crates/loom-protocol/src/lib.rs:547-556`), so they block on
   the lock the running request already holds.
2. The runtime is inserted into the run map only after `runtime.start()`
   returns (`crates/loom-server/src/lib.rs:2301`). During the run the run ID
   is not registered, so a control request would fail with `not_found` even
   if it acquired the lock.
3. There is no cancellation primitive in the agent or provider path.
   `AgentRuntime::interrupt` (`crates/loom-agent/src/lib.rs:571`) only
   transitions a state enum; it cannot stop an in-flight blocking HTTP call.
   The provider interface has no `cancel` operation, although
   [architecture.md](architecture.md) specifies one.

This breaks the guiding principle that "automation must be interruptible,
permissioned, and reviewable", and it invalidates the M4 exit condition,
which assumes a second client can observe and control a running agent.

**Direction:** register the runtime before starting it, move run execution
onto an owned worker rather than the request handler, replace the global
mutation lock with per-run locking so control requests are always
serviceable, and introduce a cancellation token honoured by the provider.

### A3 - The contract crate depends on the backend domain

`loom-protocol` depends on `loom-agent`, `loom-providers`, `loom-workspace`,
`loom-language`, `loom-vcs`, and `loom-process`. The resulting dependency
chain is `loom-protocol -> loom-providers -> ureq -> ring`.

Any client that speaks the protocol therefore compiles a blocking, native
HTTP and TLS stack. This blocks the M6 browser target before any wasm work
begins, and it means a frontend cannot depend on the contract without
depending on the backend implementation.

The root cause is that the `ModelProvider` trait is defined in
`loom-providers` (`crates/loom-providers/src/lib.rs:31`) rather than in
`loom-model`. [architecture.md](architecture.md) places the normalized model
interface in `loom-model` and adapters in `loom-providers`. Because the
abstraction lives in the adapter crate, `loom-agent` depends on the concrete
adapters, and the dependency propagates outward into the protocol.

**Direction:** move `ModelProvider` and the related abstractions into
`loom-model`, and reduce `loom-protocol` to `loom-core` plus serializable
data-transfer types so the contract is a leaf-ward dependency.

## Descoped code still present

### A4 - The editor surface is descoped and unused

[ADR 0006](decisions/0006-m5-coding-workspace.md) and
[architecture.md](architecture.md) both state that M5 adds no editable
buffers, tabs, splits, or autosave, and that `loom-workspace` must not become
a second editor authority. The implementation does both.

- `crates/loom-workspace/src/editor.rs` is 1,250 lines implementing
  `EditorBuffer`, `EditorTab`, `EditorPane`, `SplitDirection`, autosave
  policy, fuzzy find, and search.
- `InProcessBackend` owns `editors: Mutex<BTreeMap<ProjectId, EditorWorkspace>>`
  (`crates/loom-server/src/lib.rs:190`).
- `loom-protocol` exposes twelve editor requests, including
  `OpenEditorBuffer`, `EditEditorBuffer`, `UndoEditorBuffer`,
  `RedoEditorBuffer`, `SaveEditorBuffer`, `ReloadEditorBuffer`,
  `GetEditorLayout`, `SplitEditor`, `FocusEditorPane`, `FocusEditorTab`, and
  `CloseEditorBuffer` (`crates/loom-protocol/src/lib.rs:290-343`).

The M5 client issues none of these requests. The entire surface is exercised
only by its own tests. The one genuine dependency is
`editor(project_id).instruction_text()`, used to load repository instructions
when starting a run (`crates/loom-server/src/lib.rs:2278`).

Two specific problems inside the surface are worth recording even if the code
is removed:

- `SplitEditor`, `FocusEditorPane`, and `FocusEditorTab` place frontend
  layout state in the backend. Pane and tab focus are not durable session
  truth, and storing them inverts the "backend owns truth" principle by
  making the backend own presentation.
- Undo is implemented by pushing a full `String` clone of the document on
  every edit (`crates/loom-workspace/src/editor.rs:410`), so memory cost is
  proportional to document size per edit.

**Direction:** move `instruction_text` into `loom-workspace` or
`loom-context`, then remove `editor.rs`, the backend editor state, and the
editor protocol requests and capabilities.

### A5 - `loom-language` is a heuristic, not a language service

The crate spawns no process and implements no language-server protocol.
`basic_diagnostics` reports a hint when a line contains the substring `TODO`,
an error when it contains `ERROR`, and otherwise checks bracket balance
(`crates/loom-language/src/lib.rs:456-508`). `basic_symbols` matches line
prefixes such as `fn `, `struct `, and `enum `. Starting and stopping a
"language service" only toggles a descriptor field.

These results are exposed through the protocol as `GetDiagnostics` and
`GetSymbols`, backed by the `ReadDiagnostics`, `ReadSymbols`,
`GoToDefinition`, `FindReferences`, and `LanguageServiceLifecycle`
capabilities, which advertises semantic analysis the backend does not
perform. The M5 client consumes none of it.

With M7 removed from the roadmap and language-server UX explicitly out of
scope for M5, this is 736 lines of unused scope that also contributes to the
`loom-protocol` dependency problem in A3.

**Direction:** delete the crate and its protocol and capability surface.
Reintroduce it behind a real language-server process model if and when the
deferred extensibility work is scheduled.

### A6 - VCS index and commit surface exceeds current scope

The `MutateVcsIndex` and `CreateVcsCommit` capabilities
(`crates/loom-core/src/capability.rs:46-47`) and the corresponding
`loom-vcs` operations remain, although ADR 0006 defers staging, commit
creation, and branch management. This is smaller and less coupled than A4 and
A5, but it is the same class of drift.

## Implementation gaps

### A7 - Terminals are piped stdio, not PTYs

`loom-process` spawns real child processes with piped stdin, stdout, and
stderr (`crates/loom-process/src/lib.rs:136-142`). There is no pseudo-terminal
and no termios handling, so `resize` only records rows and columns on a
snapshot (`crates/loom-process/src/lib.rs:257`) without informing the child.

Programs that detect a TTY will disable colour, alter buffering, or suppress
progress output, and interactive programs will not work. The M2 milestone
describes "persistent terminals with output streaming, resize, input, and
cancellation", which overstates the current behaviour.

### A8 - No file watching

`loom-workspace` uses the real filesystem but has no watch mechanism; change
detection is pull-based re-reading and revision comparison. The M2 milestone
describes exposing "file snapshots and watches".

### A9 - The client is monolithic and blocks its own UI thread

[architecture.md](architecture.md) describes a frontend split into `ui`,
`frontend-core`, `agent-ui`, `platform`, and `protocol-client`. The
implementation is a single 4,615-line `crates/loom-ui/src/main.rs` built
around a `LoomView` struct with roughly 40 fields that mixes state
projection, protocol requests, and GPUI rendering in one type, plus a
hand-written text input element.

`ClientConnection::request` is synchronous, and several handlers call it
directly on the UI thread, including rename, archive, model refresh, review
loading, and the startup sequence. The remote path additionally performs
`runtime.block_on` while holding a `Mutex`, so remote latency becomes a UI
freeze. Some operations correctly use `cx.background_spawn`, but their
completion handlers then issue further blocking requests on the UI thread.

The client also bypasses its own protocol boundary: it calls
`loom_vcs::GitService::init` and `FileCredentialStore` directly and stores
`loom_vcs::GitDiff` and `loom_workspace::WorkspaceChange` domain types in UI
state rather than protocol projections.

### A10 - Remote is not a first-class mode

Workspace opening and creation are gated on the connection being in-process,
so a remote client must select an already-open project and otherwise reports
that the remote backend has no open projects. GitHub login is disabled
entirely when remote, because the device-flow token is written to a local
credential file that only an in-process backend can read.

The protocol treats both transports equivalently, so this is a client
limitation rather than a protocol one, but it does not yet meet the
"remote should be a first-class mode" principle.

## Minor findings

### A11 - Specification describes a layout that does not exist

[architecture.md](architecture.md) documents a `frontend/` directory and
`tests/protocol/` and `tests/integration/` directories. None exist; the
client is a crate and all tests are per-crate. Either the layout or the
document should change.

### A12 - Hardcoded model default and client impersonation

`loom-providers` pins `GITHUB_COPILOT_DEFAULT_MODEL` to a specific model name
(`crates/loom-providers/src/lib.rs:20`) and sends fixed editor, plugin, and
user-agent strings identifying the client as a particular VS Code and Copilot
Chat build (`crates/loom-providers/src/lib.rs:26-28`). This is brittle across
upstream changes and carries terms-of-service risk.

Also noted: the build compiles two versions of `tokio-tungstenite` (0.27
pinned by the workspace, 0.29 pulled in through `axum`).

## What the audit confirmed as sound

These are recorded so they are not accidentally "fixed" later.

- The infrastructure crates are real, not simulations. `loom-process` spawns
  real operating-system processes, `loom-vcs` invokes the real `git`
  executable and is tested against real repositories, and `loom-workspace`
  performs real filesystem access with a symlink-escape guard.
- Capability negotiation, the bounded event journal with a snapshot resume
  cursor, and the idempotency cache for retryable mutations are implemented
  as specified.
- Credentials are referenced by opaque ID. The device-flow token is stored in
  an owner-only local file and does not appear in backend state, protocol
  responses, logs, or the session timeline.
- Protocol JSON contract tests exist and cover version negotiation.
- The crate decomposition itself is appropriate. A3 is a problem of
  dependency direction, not of where the boundaries were drawn.

## Suggested sequence

1. Move `ModelProvider` into `loom-model` and reduce `loom-protocol` to
   `loom-core` plus data-transfer types (A3). This unblocks M6 and removes
   the native HTTP dependency from the contract.
2. Make provider streaming incremental and introduce a run cancellation
   token (A1).
3. Move run execution off the request handler, register runtimes before
   starting them, and replace the global mutation lock with per-run locking
   (A2).
4. Remove the editor and language surfaces, preserving `instruction_text`
   (A4, A5, A6).
5. Split the client into modules and move backend calls off the UI thread
   (A9, A10).

Items A7 and A8 are feature work rather than corrections, and should be
scheduled against the milestone that needs them or removed from the M2
description.
