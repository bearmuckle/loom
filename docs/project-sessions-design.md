# Project sessions and coordinated sub-agents

## Status and scope

This document turns [GitHub issue #17](https://github.com/bearmuckle/loom/issues/17),
“Project sessions and coordinated sub-agents,” into an implementation design.
It is a design and sequencing plan, not a claim that project orchestration is
implemented. The issue is XL-sized, so delivery is split into reviewable slices.

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
  expected outcome, code-change intent, owner, and status. It is not merely a
  prompt convention or a fixed planning gate.
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
workspace/backend limit. Dependencies are explicit; a dependent task is not
started until its prerequisites reach an acceptable terminal state. Completion,
failure, cancellation, and blockers all return control to the manager, which
can inspect output and decide whether to retry, redirect, continue, or ask the
user. Project stop/archive behavior must be explicit for descendants and their
worktrees; shutdown or disconnect must not erase coordination state.

Message acceptance means the addressed message has committed durably. Messages
are ordered per project and retain sender, recipient, type, timestamps, and
task association. Delivery may be asynchronous: an inactive recipient sees the
message on resume, while an active recipient is woken after the durable write.
Retryable send commands use request IDs so reconnects do not duplicate
messages. The first slice supports parent-child exchange; routing between
branches is added after direct-child coordination is reliable. Membership and
policy are checked on every send and read.

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
integration. Proposed operation families are `CreateChildAgent`,
`GetProjectSnapshot`, `SendAgentMessage`, `ListAgentMessages`, and
`ControlChildAgent` (pause/resume/interrupt/cancel); names are provisional and
must follow existing request conventions. Events cover child creation and
status transitions, task updates, messages, blockers, worktree changes,
review decisions, integration results, and cleanup disposition. Snapshots
include project/root IDs, parent IDs, depth, task summaries, status, and
per-agent output cursors. Protocol capability negotiation gates clients that
do not understand orchestration.

Persist normalized queryable records for project membership/parentage,
delegated task intent and dependencies, message envelope/body and ordering,
and worktree/integration state. Use foreign keys and transactions so child
creation with its initial task, and message acceptance with its event, are
atomic. Enforce one root per project, same-project parentage, no cycles, and
maximum depth three in the backend domain service and persistence boundary.
Do not place growing messages or child lists inside session JSON blobs.

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

1. Add project/agent hierarchy IDs, depth, delegated-task intent/status,
   message envelope/type, and worktree integration state to shared domain
   types. Define invariants and stable transition/error codes.
2. Add transactional persistence and migrations for project roots, parentage,
   task dependencies, addressed messages, and integration records. Represent
   all existing sessions as roots in projects without children.
3. Add capability-gated protocol snapshots, commands, and events, with
   contract tests for ordering, idempotency, authorization, and depth limits.
4. Add recovery reconciliation for committed-but-not-launched children and
   interrupted child runs.

**Exit:** existing sessions load as projects, hierarchy invariants are
backend-enforced, and snapshots/events survive restart and reconnect.

### Slice 1: direct-child non-code coordination

1. Add a project-manager tool/service to create one bounded child task with
   selected relevant context, limits, and explicit code-change intent.
2. Enforce configurable bounded parallelism and project membership; create
   the child durably before scheduling its independent agent runtime.
3. Implement parent-child durable messaging with accepted/ordered semantics,
   active-recipient wakeup, and queued delivery after resume.
4. Return progress, completion, questions, and blockers to the manager and
   allow it to answer, redirect, continue, retry, or cancel.
5. Add a deterministic end-to-end scenario for investigation/planning that
   completes without worktrees or a fixed document pipeline.

**Exit:** a manager creates a bounded non-code child, exchanges messages while
both agents run, handles a blocker and completion, and resumes after restart.

### Slice 2: project and child control UI

1. Present root sessions as projects in workspace navigation and language;
   preserve existing sessions through the root migration.
2. Add a project agent list/tree with depth, task summary, live status,
   blocker, and latest result. The initial workflow exposes direct children.
3. Add focused child transcript/output inspection and per-child
   pause/resume/interrupt/cancel controls using existing `gpui-kit` components
   where suitable.
4. Show manager-child messages in the project activity timeline, visually
   distinct from user conversation and tool activity. Expose reconnect cursors
   and stale-state refresh through existing protocol projections.
5. Make project stop/archive behavior visible and apply the documented
   descendant policy.

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
- Whether concurrency is configured per workspace, backend, project, or a
  combination, and what default limits apply.
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
