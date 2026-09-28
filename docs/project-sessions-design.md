# Project sessions and coordinated sub-agents

## Status and scope

This document turns [GitHub issue #17](https://github.com/bearmuckle/loom/issues/17),
“Project sessions and coordinated sub-agents,” into an implementation design.
It is the design and sequencing plan for the feature. The issue is XL-sized, so
delivery is split into reviewable slices. The current draft implementation has
landed the project hierarchy, protocol 6.0 contract, forward v41-to-v46 SQLite
migrations, durable child/task creation, persisted child model selection,
restart scheduling for queued children, dependency gating, reconciliation of
task status from persisted runs, and durable parent-child message delivery at
safe model-turn boundaries. Root managers can create bounded non-code child
tasks through a write-approved agent tool; separate delegation, messaging,
inspection, and child-control grants survive run recovery. Project agents can
send durable direct-parent/direct-child messages and inspect direct-child task
and session status. A manager tool can continue a paused child, retry its
failed tool step, or cancel it. The workspace navigator now groups a project root with its direct
children and shows child task summaries and live state; loading the navigator
from a child session resolves the containing project. The project message
timeline now displays durable parent-child messages with separate project
activity styling. Child context menus provide pause/resume/interrupt/cancel
controls, and project-root views refresh from workspace event cursors after
reconnect. Project archive waits until child tasks are terminal, then archives
the descendants with the root. Worktree-backed code tasks and integration
remain future slices. Workspace
settings configure the maximum number of delegated agents running in parallel
(default four, range one to sixteen); additional tasks remain durable and
queued. A project can have up to fifty queued or active delegated tasks.

The feature makes a root agent session a **project**: the durable owner of a
user goal and the root of an agent hierarchy. A project manager may delegate
bounded code or non-code tasks to child sessions, observe their durable state,
communicate with them, and integrate their results. Workspaces remain the
grouping and settings boundary and may contain multiple projects. There is no
standalone session workflow: every agent session is either a project's root or
a descendant.

## Product and domain model

- **Workspace** groups project roots and shared settings. It has no filesystem
  root and is not a repository or security boundary.
- **Project** is a root agent session with no parent. It owns the user's goal,
  coordinates descendants, and is accountable for the final outcome. Persist
  project identity independently from workspace identity, even if the first
  implementation uses the root session ID as the project ID.
- **Agent session** is one executing or completed agent context. It belongs to
  exactly one project and has either no parent (the root) or one parent agent.
- **Delegated task** records bounded intent, context references, dependencies,
  expected outcome, selected model, code-change intent, owner, and status. The
  selected model is stored so retries and restart recovery keep the same model.
  A delegated task is not merely a prompt convention or a fixed planning gate.
- **Agent message** is a durable, ordered, attributed message addressed to an
  agent in the same project: progress, result, question, blocker, direction, or
  answer. It is distinct from user conversation and tool activity.
- **Agent worktree** records an isolated checkout for code-changing work,
  including owner, parent base revision, child result revision, integration
  state, conflicts, and cleanup disposition.

The hierarchy has at most three levels: project manager, sub-agent, and
sub-sub-agent. The domain and protocol represent all three levels from the
start; the first UI and orchestration slice creates only direct children.
Level-three agents cannot delegate. Every descendant remains under the root
project's authority and cannot become a root project implicitly.

Agent status, messages, task intent, permissions, worktree state, integration,
and lifecycle are backend-authoritative. Clients render snapshots and ordered
events; they do not own coordination state. Existing persisted sessions must
be represented as project roots with no children when the model is introduced.

## Coordination and lifecycle behavior

The project manager receives explicit role instructions: it owns the overall
goal, delegates bounded work, monitors outcomes and blockers, synthesizes
results, escalates decisions to the user, and remains responsible for final
delivery. Each child receives its bounded task, relevant context, project and
parent identity, allowed delegation depth, permissions, and reporting
expectations.

The manager may run independent tasks concurrently up to a configured
workspace/backend limit. Start with a default cap of four active child agents
per project; a later settings surface may configure a lower cap, but disabling
the bound is not supported. Dependencies are explicit; a dependent task is not
started until its prerequisites reach an acceptable terminal state. Completion,
failure, cancellation, and blockers all return control to the manager, which
can inspect output and decide whether to retry, redirect, continue, or ask the
user. Pausing or interrupting the project manager affects only the manager run;
child runs remain independent. Archiving a project is rejected while any child
task is queued, blocked, or running. Once all child tasks are terminal,
archiving the root also archives its descendant sessions. Shutdown or
disconnect must not erase coordination state.

Message acceptance means the addressed message has committed durably. Messages
are ordered per project and retain sender, recipient, type, timestamps, and
task association. Active run workers read the durable inbox between model
turns; a message never interrupts an in-flight provider request or pending
tool/approval action. The inbox cursor and injected transcript entry are saved
in the same run checkpoint. A blocked or inactive recipient keeps messages in
the durable inbox until its normal resume/next run. Retryable send commands use
request IDs so reconnects do not duplicate messages. The first slice supports
parent-child exchange; routing between branches is added after direct-child
coordination is reliable. Membership and policy are checked on every send and
read.

Human review remains available at consequential decisions, particularly code
integration and policy-sensitive actions. Projects are flexible coordinators,
not mandatory plan-document-to-issue-to-build pipelines.

## Code task isolation and integration

Each code-changing agent receives its own Git worktree; non-code work gets no
worktree. A child worktree is based on its parent's current project branch and
revision. A level-three worktree is based on its level-two parent's branch and
revision. Siblings never share a mutable checkout, and parentage grants no
shared-filesystem access.

The parent reviews a child's result and performs or directs integration into
its own branch. A child never writes or merges into its parent's worktree. Each
level integrates its descendants before returning its result upward; the
project manager owns final integration into the project branch and the
user-facing summary. Record base/result revisions, review and integration
status, conflict state, and cleanup state. Stale bases and conflicts become
visible coordination states with reviewable diffs and messages. Cleanup must
preserve unmerged work or record an explicit disposition. Never silently
overwrite parent or sibling changes.

The first implementation should support repositories already represented by
the session repository service. Before extending this to attached native
directories or multi-repository tasks, define whether each child receives an
independent clone/worktree per repository and how local in-place attachments
are handled; a child must never inherit a mutable attachment accidentally.

## Protocol and persistence design

Add versioned protocol operations and projections for project snapshots,
children, delegated tasks, addressed messages, agent controls, and worktree
integration. The current contract includes `CreateProjectChild`,
`GetProjectSnapshot` (including lookup from any member session),
`SendProjectAgentMessage`, `ListProjectAgentMessages`, and
`ControlProjectChild` (pause/resume/interrupt/cancel); future worktree
operations remain provisional and must follow existing request conventions.
`ProjectSnapshot` carries task IDs, intent, and lifecycle status so clients can
address child controls without deriving identity from labels. The client
control request is authorized against the project root and child membership;
the manager agent tool continues to require its persisted run grant. Events cover child creation and
status transitions, task updates, messages, blockers, worktree changes,
review decisions, integration results, and cleanup disposition. Snapshots
include project/root IDs, parent IDs, depth, task summaries, status, and
per-agent output cursors. Protocol capability negotiation gates clients that
do not understand orchestration.

Project orchestration is an intentional client/backend compatibility boundary:
Loom does not need to keep old clients working against a project-enabled
backend. Before a client can use the project protocol, require it to upgrade to
a release that understands the protocol version and project schemas; enforce
this at negotiation/connection setup rather than relying only on the UI to
hide new operations. Reject an outdated client with the existing
`UnsupportedProtocol` error, including the required version in its message,
before sending any new project capabilities or schemas. Do not send new
capability values, request/response variants, or event variants to a client
that has not passed that gate. Once admitted, capability checks still govern
which project operations that client may use. This avoids requiring old-client
forward compatibility: deployments may ensure or force the client upgrade,
and the backend rejects any client that has not upgraded. The
pre-feature version/error envelope must remain sufficient to deliver this
rejection; do not add a new error enum value that an old client would need to
decode.

Protocol 6.0 also carries the workspace-level project-agent concurrency
setting. Because this protocol version is part of the coordinated project
release, clients are upgraded or rejected at negotiation before using the new
field. The persisted workspace-config JSON uses a serde default of four when
the field is absent, so existing stored settings do not need a separate SQLite
schema migration.

Advance the protocol version for the project contract. Prefer a major version
change if the new required domain semantics or enum variants are not backward
compatible; update all supported native/browser clients in the same release
window. Keep the existing session request surface for root-session operations
where practical, but clients admitted to the new protocol must treat roots as
projects. Define the minimum client version and error/update path before
enabling the backend feature. Backend downgrades to pre-project
protocol/storage versions are unsupported, including when no child agents
have been created.

Persist normalized queryable records for project membership/parentage,
delegated task intent and dependencies, message envelope/body and ordering,
and worktree/integration state. Use foreign keys and transactions so child
creation with its initial task is atomic. Message acceptance and project
ordering commit together; event delivery is a post-commit notification, and
clients recover missed notifications from the durable addressed-message log.
Enforce one root per project, same-project parentage, no cycles, and
maximum depth three in the backend domain service and persistence boundary.
Do not place growing messages or child lists inside session JSON blobs.

The project foundation requires forward SQLite migrations from schema version
41 through version 46. The v41-to-v42 migration adds the
normalized project, membership/parentage, delegated-task, addressed-message,
and worktree/integration structures, then backfill each existing session as
the root of a project while preserving its session ID, workspace, transcript,
events, runs, approvals, and filesystem references. Existing session IDs
remain stable; if project IDs are separate, assign them once and persist the
mapping. Set `user_version` to 42 only after the backfill and invariants pass.
The v42-to-v43 migration adds the per-run project-message cursor used to
checkpoint inbox delivery atomically with the agent transcript. The v43-to-v44
migration adds the per-run project-delegation grant. The v44-to-v45 migration
adds separate per-run messaging and inspection grants, both defaulting off for
existing runs. The v45-to-v46 migration adds the per-run child-control grant,
also defaulting off. A recovered run retains only the project tool
authorization captured when it started; delegation, messaging, inspection, and
control grants do not imply one another. Each migration commits its resulting
schema version with its schema change and can be retried safely after
interruption. This is a forward-only transition: a backend release that
supports schemas only through v45 cannot open the database after it has
migrated to v46, and no schema downgrade is provided. If an upgrade must
be rolled back, restore a pre-upgrade backup or move forward with a fix.

Child creation is idempotent and commits its session, task, project link, and
initial event before execution is scheduled. A crash after commit but before
launch is recovered by a durable continuation scan. In-flight children recover
to a resumable paused/unknown state, never replay an uncertain tool effect
automatically. Resume rehydrates the child's own runtime and filesystem; it
does not depend on a manager process staying alive. Project snapshots plus
ordered event cursors reconstruct the UI after reconnect or backend restart.

Authorization is evaluated for project membership, agent control, message
send/read, repository access, and integration. Parent-child links alone do not
grant unrestricted access. Preserve existing workspace/session scoping and
approval policy boundaries; record who requested a consequential integration
and its decision.

## Delivery plan

### Slice 0: domain and protocol foundation

1. **Implemented:** add project/agent hierarchy IDs, depth, delegated-task
   intent/status, message envelope/type, and worktree integration state to
   shared domain types; enforce the core hierarchy invariants.
2. **Implemented:** add the forward v41-to-v46 migrations and represent
   existing sessions as project roots.
3. **Implemented:** advance to protocol 6.0 and reject clients that do not
   meet the supported protocol version during negotiation, before serving
   project-aware schemas. Deployments must ensure or force client upgrades;
   old-client forward compatibility is not supported.
4. **Implemented:** recover committed-but-not-launched queued children,
   reconcile delegated-task status from persisted child runs, and schedule
   tasks when dependencies complete. Existing run recovery keeps interrupted
   work paused or blocked rather than starting a duplicate child run.
5. **Implemented:** configure delegated-agent parallelism per workspace. The
   default is four simultaneous running tasks, bounded from one to sixteen;
   durable queued tasks start when a slot becomes available. The pending queue
   is bounded at fifty tasks. Older stored workspace settings default to four
   without a SQLite schema migration.

**Exit:** v41 data migrates through v46 with existing sessions represented as
projects; unsupported clients are directed to upgrade before using the new
contract; hierarchy invariants are backend-enforced; snapshots and durable
records survive restart and reconnect. Backend downgrade is unsupported and
documented. Event replay is rebuilt from authoritative records where a crash
occurs after record commit but before notification persistence.

### Slice 1: direct-child non-code coordination

1. **Implemented:** expose a root-only `delegate_project_task` agent tool with
   bounded non-code inputs, optional model selection, and durable retry
   identity. Child creation remains behind the existing write approval policy.
   Persist its capability grant with the run so recovery restores the same
   tool surface without retaining backend or caller objects.
2. **Implemented:** enforce configurable bounded parallelism and project
   membership; create each child durably before scheduling its independent
   agent runtime.
3. **Implemented:** deliver parent-child messages from the durable inbox at
   model-turn boundaries, checkpoint the inbox cursor with the transcript,
   and retain messages for blocked or inactive recipients until resume.
4. **Implemented:** add run-granted `send_project_agent_message` and
   `list_project_children` tools. A child reports to its parent; a parent
   identifies a child by its delegated task ID. Terminal recipients are
   rejected because they have no current resume path.
5. **Implemented:** return progress, completion, questions, and blockers to the
   manager and allow it to answer, redirect, continue a paused child, retry its
   failed tool step, or cancel it. Retry repeats only the failed tool step; it
   does not create a fresh task attempt. Whole-task retry remains unsupported
   until task attempt identity, history, and idempotency semantics are designed.
6. **Implemented:** add a deterministic end-to-end investigation scenario that
   delegates a non-code task, exchanges progress/questions/directions/results,
   completes both runs, and verifies project state, messages, transcripts,
   inbox cursors, and run grants after reopening the backend.

**Exit:** a manager creates a bounded non-code child, exchanges messages while
both agents run, handles a blocker and completion, and project state survives
restart. Existing run-recovery behavior remains covered by its restart tests.

### Slice 2: project and child control UI

1. **Implemented:** label root sessions as projects in workspace navigation
   and group their direct children under the root; the root migration preserves
   existing sessions.
2. **Implemented:** show direct-child task summaries and live session state in
   the project tree. Blocker and result messages are highlighted in the
   activity timeline.
3. **Implemented:** selecting a child opens its existing transcript/output
   view; the child context menu provides pause, resume, interrupt, failed-step
   retry, and cancel actions using existing `gpui-kit` menu controls.
4. **Implemented:** show manager-child messages in the project activity
   timeline, visually distinct from user conversation and tool activity.
   Read the durable inbox for each direct project member with an independent
   per-recipient cursor, merge messages by project sequence, and use the
   workspace event feed cursor to refresh after reconnect or new activity.
5. **Implemented:** pausing or interrupting the manager leaves child runs
   independent. Project archive is rejected while any child task is queued,
   blocked, or running; after tasks are terminal, archive the descendant
   sessions with the root.

**Exit:** users can inspect, message through the manager, and control each
child independently; project state reconstructs after reconnect.

### Slice 3: code tasks and reviewed integration

1. Add a backend worktree service that creates child worktrees from the
   parent's recorded branch/revision, with ownership and cleanup records.
2. Surface changed paths and bounded diffs for child output without granting
   the parent implicit filesystem access.
3. Add explicit parent review and integration commands. Integrate into the
   parent's branch only after review; capture resulting revision or conflict.
4. Handle stale bases, conflicts, retries, interruption, and failed cleanup
   as durable statuses with recoverable work.
5. Verify nested integration semantics at level two before enabling level
   three in user workflows.

**Exit:** siblings work in isolated worktrees and their parent can review,
integrate, resolve conflicts, and report the resulting revision without
discarding unmerged work.

### Slice 4: deeper hierarchy and branch communication

1. Enable level-two delegation with level-three agents; backend rejects any
   request beyond three total levels.
2. Add project-scoped branch-to-branch routing with policy and membership
   checks, keeping direct manager/child messaging as the simpler default.
3. Add hierarchy-wide dependency coordination, concurrency accounting,
   cancellation semantics, and progress roll-up while the project manager
   retains final accountability.
4. Validate nested worktree ancestry and integrate each level upward before
   project-branch integration.

**Exit:** three-level projects coordinate safely, recover after restart, and
preserve explicit ownership and review at each integration boundary.

## Verification and rollout

Each slice should add domain transition tests, SQLite migration/restart tests,
protocol contract tests, and a deterministic end-to-end fixture at its
boundary. Exercise duplicate child-creation/message requests, inactive
recipients, ordering across reconnect, unauthorized cross-project access,
parallelism limits, cancellation, process restart, depth overflow, and partial
worktree/merge failures. UI work should verify empty, active, blocked, failed,
completed, and stale/reconnecting child states.

Gate new client behavior on negotiated capabilities. Ship root-as-project
compatibility before enabling child creation, then enable non-code direct
children before code worktrees. This keeps the migration independently
reviewable and makes each new authority boundary observable before it can
modify repositories.

## Open decisions

- Whether project identity is a separate persisted ID or initially the root
  session ID with a durable project record.
- Exact continuation scheduling/claim mechanism for children committed but
  not yet started.
- Worktree creation and cleanup behavior for multi-repository tasks and native
  local directory attachments.
- Whether user messages can address a child directly or must pass through the
  project manager in the first UI.
- Descendant behavior for project stop, archive, and deletion, including
  retention of unmerged child work.
- Protocol compatibility window and database migration policy for existing
  installations.
