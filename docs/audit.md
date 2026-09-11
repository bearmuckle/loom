# Implementation audit

This document records a point-in-time audit of the implementation against
the specification in [product.md](product.md),
[architecture.md](architecture.md), [protocol.md](protocol.md), and
[roadmap.md](roadmap.md). It is a findings
document, not a specification. Once a finding is resolved, remove it here and
update the specification document it refers to.

**Audited revision:** `a2c6a73` (M5 client, after the M0-M6 roadmap rescope),
with the remediation of A1-A6, A9, and A11 applied on top.

**Scope:** approximately 23k lines across 15 crates.

## Summary

The workspace type-checks cleanly, CI covers formatting, clippy, tests, and
builds, and the infrastructure crates are real implementations rather than
stubs. The crate boundaries are the right ones and the dependency direction
now runs leaf-ward: `loom-protocol` carries the contract and its data-transfer
types and depends only on `loom-core` and `loom-model`, while the backend
crates depend on the contract.

The remaining findings are feature gaps and client limitations rather than
structural problems.

| ID | Severity | Area | Finding |
| --- | --- | --- | --- |
| A7 | Medium | Process | Terminals are piped stdio, not PTYs |
| A8 | Medium | Workspace | No file watching |
| A10 | Medium | UI, server | Remote is not a first-class mode |
| A12 | Low | Providers | Hardcoded model default and client impersonation |
| A13 | Low | UI | The client still reaches around its own protocol boundary |

## Implementation gaps

### A7 - Terminals are piped stdio, not PTYs

`loom-process` spawns real child processes with piped stdin, stdout, and
stderr (`crates/loom-process/src/lib.rs:136-142`). There is no pseudo-terminal
and no termios handling, so `resize` only records rows and columns on a
snapshot without informing the child.

Programs that detect a TTY will disable colour, alter buffering, or suppress
progress output, and interactive programs will not work. The M2 milestone
describes "persistent terminals with output streaming, resize, input, and
cancellation", which overstates the current behaviour.

### A8 - No file watching

`loom-workspace` uses the real filesystem but has no watch mechanism; change
detection is pull-based re-reading and revision comparison.
[roadmap.md](roadmap.md) now describes M2 accordingly, so this is scheduled
feature work rather than drift.

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

### A12 - Hardcoded model default and client impersonation

`loom-providers` pins `GITHUB_COPILOT_DEFAULT_MODEL` to a specific model name
and sends fixed editor, plugin, and user-agent strings identifying the client
as a particular VS Code and Copilot Chat build
(`crates/loom-providers/src/lib.rs:18-29`). This is brittle across upstream
changes and carries terms-of-service risk.

Also noted: the build compiles two versions of `tokio-tungstenite` (0.27
pinned by the workspace, 0.29 pulled in through `axum`).

### A13 - The client still reaches around its own protocol boundary

`crates/loom-ui/src/platform.rs` calls `loom_vcs::GitService::init` to
bootstrap a demo workspace, and `crates/loom-ui/src/view.rs` uses
`FileCredentialStore` and `GitHubCopilotAuthenticator` directly for the
device-flow login. Both are native-only paths, and they are the reason A10
cannot be closed by the client alone. They are isolated in the platform and
view modules so a browser target can replace them, but the login and
workspace-bootstrap flows should become protocol operations.

## What the audit confirmed as sound

These are recorded so they are not accidentally "fixed" later.

- The infrastructure crates are real, not simulations. `loom-process` spawns
  real operating-system processes, `loom-vcs` invokes the real `git`
  executable and is tested against real repositories, and `loom-workspace`
  performs real filesystem access with a symlink-escape guard.
- Capability negotiation, the bounded event journal with a snapshot resume
  cursor, and the idempotency cache for retryable mutations are implemented
  as specified. Idempotency is now serialized per request id instead of
  through one global mutation lock.
- Credentials are referenced by opaque ID. The device-flow token is stored in
  an owner-only local file and does not appear in backend state, protocol
  responses, logs, or the session timeline.
- Protocol JSON contract tests exist and cover version negotiation.
- The crate decomposition itself is appropriate.

## Remediation applied since the audited revision

Recorded for traceability; the specification documents describe the resulting
behaviour.

1. **A3** - `ModelProvider` and the provider metadata types moved into
   `loom-model`. The serializable agent, context, tool, workspace, process,
   and VCS types moved into `loom-protocol`, which now depends only on
   `loom-core` and `loom-model`; the backend crates depend on the contract and
   re-export the types they produce.
2. **A1** - `ModelProvider::stream` emits events through a sink as they are
   decoded, the OpenAI-compatible and GitHub Copilot adapters consume
   server-sent events, and a `CancellationToken` is observed between chunks.
3. **A2** - A run is registered before it starts, executes on a backend-owned
   worker one step at a time, and is controlled through a `RunControl` handle.
   Pause and interrupt cancel the in-flight model stream instead of waiting
   for the runtime lock.
4. **A4, A5, A6** - The editor surface, `loom-language`, and the VCS index and
   commit mutations were removed. Repository instruction loading moved to
   `loom-workspace`.
5. **A9, A11** - `loom-ui` was split into `connection`, `state`, `view`,
   `text_input`, `theme`, and `platform` modules, and backend requests are
   submitted to a connection worker thread instead of running on the GPUI
   thread. [architecture.md](architecture.md) now describes the layout that
   exists.
