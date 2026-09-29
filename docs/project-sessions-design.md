# Project sessions and coordinated sub-agents

> **Storage note.** Loom uses a single SQLite database with an ordered migration
> ladder recorded in `PRAGMA user_version`; an unknown or newer database is
> rejected unchanged and must be wiped explicitly. Per-run project-agent grants
> are one versioned JSON payload (`run_runtime_config.project_grants`) and
> delegated-task grants are `delegated_tasks.permissions`. See
> [durable storage](storage.md).

## Status and scope

This document turns [GitHub issue #17](https://github.com/bearmuckle/loom/issues/17),
“Project sessions and coordinated sub-agents,” into an implementation design.
It is the design and sequencing plan for the feature. The issue is XL-sized, so
delivery is split into reviewable slices. Direct-child coordination, child
controls, and reviewed worktrees shipped first; independent delegated-task
grants, explicitly authorized branch messaging, and durable manager wait/join
state followed. Child creation is now available only through the run-granted
manager tool: clients cannot create children directly, and the server-bound
tool checks the executing run's persisted grants. Every run-wide message and
activity receives one monotonic ordinal when created, so restored UI entries
merge deterministically and older-page loading is idempotent by transcript
message ordinal. The manager parks at a safe checkpoint, releases its workspace
slot, resumes the original tool continuation once selected children are ready,
blocks premature manager completion, and admits queued work under a
workspace-wide concurrency limit. Failed or cancelled prerequisites leave
dependent tasks blocked with their dependency IDs available for inspection;
terminal manager waits are abandoned during recovery; and code completion
requires a reviewed result and integration when changes exist. Descendant
cascade cancellation, nested permission checks, parent-relative worktree
integration, owner-edge controls, recursive project-tree rendering, descendant
inbox display, and oldest-first admission across ready joins and queued tasks
are implemented. Focused server E2Es validate parked-wait recovery across
restart with exactly-once replay, cap-one and oldest-first admission,
deepest-first cancellation with terminal state persistence, both nested
worktree integration edges, explicitly granted non-adjacent branch messaging,
failed-run prerequisite wakeup, and recovery from interruption during a
cascade. Cascade intent and its ordered subtree snapshot are persisted and
replayed after runtime restoration but before queued-work reconciliation.
Depth-three delegation is enabled after these recovery paths were validated.
Deployments must ensure or force clients to upgrade: the backend rejects
clients that do not negotiate a compatible protocol major before serving the
new contract, and old-client forward compatibility is unsupported.

The implementation also includes durable child/task creation, persisted child
model selection, restart scheduling for queued children, dependency gating,
task-state reconciliation, and durable parent-child message delivery at safe
model-turn boundaries. Project agents can inspect direct-child task and session status;
managers can continue a paused child, retry its failed tool step, or cancel it.
The workspace navigator groups each project root with its direct children and
shows child task summaries and live state; loading from a child resolves the
containing project. Project timelines display durable parent-child messages.
Restored model-context copies of those messages retain project-agent
attribution rather than appearing as user-authored; when the durable project
message card is present, the transcript copy is suppressed. Child context menus
provide pause/resume/interrupt/cancel controls.
Project archive waits until child tasks are terminal, then archives the
descendants with the root. Code-changing children use isolated linked
worktrees; managers can review bounded diffs, fast-forward eligible commits,
and retain or remove child checkouts with an explicit cleanup disposition.
Workspace settings bound delegated-agent parallelism from one to sixteen
(default four); additional tasks remain durable and queued. A project can have
up to fifty queued or active delegated tasks.

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
events; they do not own coordination state. A session with no parent is its
project's root and has no children until it delegates.

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
worktree. The initial implementation accepts exactly one Git repository
attached to the parent session and requires its checkout to be clean. Missing
or multiple repositories and native in-place directory attachments are
rejected. A child worktree is based on its parent's current project branch and
revision. A level-three worktree is based on its level-two parent's branch and
revision. Siblings never share a mutable checkout, and parentage grants no
shared-filesystem access.

The parent reviews a child's result and performs or directs integration into
its own branch. A child never writes or merges into its parent's worktree. The
initial integration mode is fast-forward only: require a clean parent at the
expected base and review of the exact child `HEAD`; if that revision changes,
review it again. Stale or diverged parent revisions remain reviewable and
recoverable; automatic merge and conflict resolution are not supported. Each
level integrates its descendants before returning its result upward; the
project manager owns final integration into the project branch and the
user-facing summary. Record base/result revisions, review and integration
status, conflict state, and cleanup state. Cleanup preserves unmerged work or
records an explicit disposition. Never silently overwrite parent or sibling
changes.

The first implementation should support repositories already represented by
the session repository service. Before extending this to attached native
directories or multi-repository tasks, define whether each child receives an
independent clone/worktree per repository and how local in-place attachments
are handled; a child must never inherit a mutable attachment accidentally.

## Protocol and persistence design

The protocol includes versioned operations and projections for project
snapshots, delegated tasks, addressed messages, agent controls, and worktree
integration: `GetProjectSnapshot` (including lookup from any member session),
`SendProjectAgentMessage`,
`ListProjectAgentMessages`, `ControlProjectChild` (pause/resume/interrupt/cancel),
`GetProjectChildReview`, `IntegrateProjectChild`, and
`CleanupProjectChildWorktree`. Worktree operations require their negotiated
capabilities and project membership checks.
`ProjectSnapshot` carries task IDs, intent, and lifecycle status so clients can
address child controls without deriving identity from labels. Child creation
is not a client request: only the server-bound manager tool can create a child,
and it checks the executing run's persisted grant and limits each child's
grants to that run's authority. Client-supplied session IDs cannot stand in for
the manager's run identity. Client control requests are authorized against the
project root and child membership. Events cover child creation and
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

Persist normalized queryable records for project membership/parentage,
delegated task intent and dependencies, message envelope/body and ordering,
and worktree/integration state. Use foreign keys and transactions so child
creation with its initial task is atomic. Message acceptance and project
ordering commit together; event delivery is a post-commit notification, and
clients recover missed notifications from the durable addressed-message log.
Enforce one root per project, same-project parentage, no cycles, and
maximum depth three in the backend domain service and persistence boundary.
Do not place growing messages or child lists inside session JSON blobs.

Branch messaging is never inferred from the direct-message grant.
Agent-attributed messages must be submitted by the server-bound agent tool; a
client-supplied sender session ID is not an agent identity.

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
2. **Implemented:** represent root sessions as projects with a durable hierarchy
   record.
3. **Implemented:** reject request envelopes with an incompatible protocol
   major before dispatch. `Negotiate` also rejects an incompatible embedded
   client version before returning project-aware schemas. Deployments must
   ensure or force client upgrades; old-client forward compatibility is not
   supported.
4. **Implemented:** recover committed-but-not-launched queued children,
   reconcile delegated-task status from persisted child runs, and schedule
   tasks when dependencies complete. Existing run recovery keeps interrupted
   work paused or blocked rather than starting a duplicate child run.
5. **Implemented:** configure delegated-agent parallelism per workspace. The
   default is four simultaneous running tasks, bounded from one to sixteen;
   durable queued tasks start when a slot becomes available. The pending queue
   is bounded at fifty tasks. Older stored workspace settings default to
   four.

**Exit:** root sessions are represented as projects; unsupported clients are
directed to upgrade before using the new contract; hierarchy invariants are
backend-enforced; snapshots and durable records survive restart and reconnect.
Event replay is rebuilt from authoritative records where a crash occurs after
record commit but before notification persistence.

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
   and group their direct children under the root.
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

1. **Implemented:** limit the first code-task implementation to one Git repository already
   attached to the parent session. Reject missing or multiple repositories and
   native in-place directory attachments. Require a clean parent checkout and
   persist worktree intent before invoking Git.
2. **Implemented:** create a task-derived local branch and linked worktree from the parent's
   recorded `HEAD`; register that checkout only in the child session. Gate the
   child run until its worktree is durably `Ready`.
3. **Implemented:** surface changed paths and bounded diffs for child output without granting
   the parent implicit filesystem access. Require the child result to be a
   commit based on the recorded base before integration. Record the reviewed
   child `HEAD`; if that revision changes, require another review.
4. **Implemented:** add explicit parent review and integration commands. The first integration
   mode is fast-forward only: require review of the exact child revision, a
   clean parent checkout, and an exact expected `HEAD`; capture the resulting
   revision. Surface stale or
   diverged bases without replaying or rewriting commits, and retain the child
   checkout for recovery.
5. **Implemented:** persist creation/recovery and cleanup intent before filesystem mutations.
   Reconcile interrupted setup/removal on restart; never recreate a missing
   `Ready` checkout as an empty one or force-remove changed work implicitly.
6. **Validated in Slice 4:** nested worktree integration follows the owner edge
   at each level: the depth-two manager integrates the depth-three commit into
   its branch, then the project root reviews and integrates the manager's
   branch. Depth-three delegation is enabled after the recovery cases listed
   in Slice 4 passed their end-to-end tests.

**Exit:** met for direct children. Siblings work in isolated worktrees and
their parent can review and fast-forward integrate eligible child commits.
Stale or diverged children remain reviewable and recoverable without
discarding unmerged work; merge and conflict-resolution workflows remain a
later design step.

### Slice 4: deeper hierarchy and branch communication

**Implementation status:** sender
binding, recipient discovery, explicitly granted branch-message routes,
durable manager wait/join, workspace-wide admission, manager completion guards,
descendant cascade cancellation, nested permission and parent-relative
worktree paths, owner-edge controls, recursive project-tree rendering,
descendant inbox display, and oldest-first admission are implemented in the
current worktree. Focused end-to-end tests now cover cap-one wait/join fairness,
oldest-first admission across ready joins and queued tasks, parked-wait recovery
across restart with exactly-once resumption, deepest-first descendant
cancellation with terminal state persistence, parent-relative integration
through both nested worktree edges, explicitly granted non-adjacent branch
messaging, failed-prerequisite wakeup, and interruption recovery during cascade
cancellation. Depth-three delegation is enabled.

1. Persist an explicit, independent child permission set with each delegated
   task. A depth-two agent may create depth-three tasks only when its run has
   the delegation grant; depth-three agents cannot delegate. Code-worktree
   creation, child control, review, integration, inspection, and branch
   messaging remain separate grants. Reject requests that exceed the parent's
   own grants or the maximum depth.
2. Keep direct parent-child messages under the existing direct-message grant.
   Permit non-adjacent branch messages only when both endpoints are project
   members and have the explicit branch-messaging grant. Bind the sender to
   the executing run, keep task context separate from recipient selection,
   reject client-supplied agent identities, and retain per-recipient durable
   inbox ordering.
3. **Implemented and end-to-end validated:** add a durable manager
   wait/join transition before enabling nested agents. A synchronous blocking
   tool is unsafe because it can hold the only concurrency slot while its child
   remains queued. Park a manager at a safe run boundary, persist the wait and
   original continuation, release its workspace slot, and resume it exactly
   once when its direct children are return-ready. Recovery reconciles parked
   managers and queued work without replaying an uncertain external tool
   effect. A restart E2E confirms the continuation and wait survive reopen and
   the result is delivered exactly once. A manager is prevented from reporting
   completion while a child is active or its code result still needs
   integration.
4. **Implemented; cap-one and cross-type fairness end-to-end validated:** apply
   the configured concurrency limit across
   the workspace through one serialized drain. Task creation, retry, resume,
   and recovery all drain ready joins and queued tasks oldest-first before
   consuming capacity. A focused test verifies a ready older join is admitted
   before a newer queued root sibling at cap one. Failed or cancelled
   prerequisites move dependents to blocked and make them return-ready to a
   waiting manager. Focused E2Es validate both cancelled- and failed-
   prerequisite paths and exactly-once manager-wait resumption.
5. **Implemented; deepest-first cancellation end-to-end validated:** keep
   manager control scoped to direct children.
   Cancelling a child now cancels or interrupts its descendants deepest-first;
   pausing or interrupting a manager run remains local. Recursive branch rows
   now show durable task status separately from the child session/run state.
   Owner-edge controls are implemented. A durable cascade marker stores
   the captured post-order task/session list before any member changes. Startup
   replays each member idempotently before admission; scheduling and new child
   creation are fenced while an intent is pending. An interruption E2E verifies
   partial cancellation finishes after restart.
6. **Implemented and end-to-end validated:** preserve upward code
   ownership: a depth-two agent reviews and integrates a
   depth-three commit into its own branch before returning, then the project
   root reviews and integrates that branch. Require a clean exact base and
   review of the exact child `HEAD` at each edge; retain stale or diverged
   work for explicit recovery. The nested worktree E2E confirms each parent's
   `HEAD` advances to the same descendant commit only after review.
7. **Implemented; nested projection and control-state tests added:** project
   sessions render recursively, nest under their persisted parent, and reveal
   their owner chain when selected. Controls, review, and integration actions
   are gated by the owning parent's grants and target its direct child edge.
   Integration is offered after a review result exists. The root activity
   timeline merges successful descendant inbox reads even when another
   recipient read fails. A headless GPUI interaction test renders the full
   three-level tree and selects a depth-three session. A full UI click-through
   for child controls, review, and integration remains the last validation
   item.

**Exit:** three-level projects coordinate safely, recover after restart, keep
workspace concurrency bounded, preserve explicit grants, deliver branch
messages only across authorized routes, and integrate code upward at every
parent boundary. Restart and cap-one wait/join recovery, deepest-first
cancellation and its interruption recovery, both prerequisite outcomes,
oldest-first admission, nested worktree integration, and explicitly granted
non-adjacent branch messaging are validated.

## Verification and rollout

Each slice should add domain transition tests, storage/restart tests,
protocol contract tests, and a deterministic end-to-end fixture at its
boundary. Exercise duplicate child-creation/message requests, inactive
recipients, ordering across reconnect, unauthorized cross-project access,
parallelism limits, cancellation, process restart, depth overflow, and partial
worktree/merge failures. UI work should verify empty, active, blocked, failed,
completed, and stale/reconnecting child states.

The backend rejects clients that negotiate an incompatible protocol major
before serving the current contract, so deployments must ensure or force client
upgrades; old-client forward compatibility is unsupported. Child creation is
manager-only through a server-bound tool granted by the executing run, and the
server validates the persisted grant before creating a child.

## Settled choices and deferred scope

- A project's ID is its root session ID. The hierarchy record is durable, and
  a session with no parent is its own project root.
- Queued child launch and parked-manager continuation use durable task/wait
  state with an atomic wait claim and serialized, oldest-first workspace
  admission. Recovery runs before queued work is admitted.
- The first worktree implementation accepts one clean Git repository attached
  to the parent session. Multi-repository tasks and native in-place directory
  attachments are rejected until their isolation model is designed.
- The first UI lets the user select any project session and message that agent
  directly through its normal session composer. Manager-mediated user messages
  are not required. Agent-to-agent branch messages remain separately
  permission-checked.
- Pausing or interrupting the project manager affects only that run. Cancelling
  a delegated child cascades deepest-first through its descendants, while
  archiving the root is rejected until child tasks are terminal; successful
  archive then archives descendants deepest-first. A separate one-shot
  project-wide cancel or project delete operation is not exposed; define its
  lifecycle and worktree behavior before adding it.
- Child worktrees are not automatically removed when a task or project
  completes. The parent explicitly retains, removes a clean checkout, or
  discards changes. Automatic retention expiry or cleanup is future scope.
