# ADR 0003: Workspace-scoped process control and human recovery

## Status

Accepted for M2.

## Decisions

- Add `loom-workspace` and `loom-process` as provider-neutral domain services
  rather than placing filesystem and process primitives in the protocol or
  UI crates.
- Canonicalize configured workspace roots and reject absolute paths, parent
  traversal, and symlinks that resolve outside the root. File edits carry a
  content revision and fail with a structured conflict instead of silently
  overwriting a newer version.
- Use standard-library child processes with piped input/output and worker
  threads for the first terminal implementation. Terminal output and task
  events are bounded queues, task output is capped, and all long-running
  operations have explicit cancellation and exit states.
- Keep approval policy in `loom-core`. The safe default allows reads,
  requires approval for writes and commands, and denies destructive actions.
  Policy evaluation is an agent event before execution, preserving the M1
  approval request/decision events while making automatic and denied decisions
  inspectable.
- Create a workspace checkpoint before each agent run. Agent edits update the
  checkpoint's expected revisions; user edits do not. Revert and undo therefore
  refuse to overwrite a user change, and taking user control pauses subsequent
  agent writes.

## Consequences

Workspace and process services are usable in-process and through typed JSON
requests without coupling clients to a provider or a platform shell. The
terminal resize operation records the requested dimensions but does not yet
allocate a native PTY, so applications that require terminal-specific control
sequences remain deferred. At the M2 boundary, event journals, checkpoints,
and task metadata are in memory and are lost when the backend exits. M3 adds
a durable backend snapshot for sessions, agent events, checkpoints, and model
usage; reconnecting process ownership remains M4 work. Workspace watches use
snapshot polling, which is deterministic and portable but does not provide
kernel-native notifications or push delivery until a transport layer is added.

## Deferred work

- Native PTY allocation and platform-specific resize/signal behavior.
- Durable session, checkpoint, task, and event storage.
- Backpressure-aware remote transports and reconnect-safe process handles.
- Configurable command allowlists, network proxying, and a credential broker.
