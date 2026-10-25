# Communication protocol

The protocol is a public boundary between the frontend and backend, even when
both run in one process. It must be deterministic, inspectable, versioned,
and testable without a UI.

## Transport strategy

Use a transport abstraction with these initial implementations:

| Mode | Preferred transport | Fallback |
| --- | --- | --- |
| Native client and local backend | In-process channel or Unix domain socket / named pipe | Loopback TCP |
| Browser client and local/remote backend | WebSocket over HTTP(S) | HTTP request/stream endpoints where required by a host |
| Native client and remote backend | QUIC when deployment supports it | WebSocket or HTTP/2-compatible stream |

The protocol payload is independent of the transport. M4 implements the
initial JSON WebSocket adapter through `loom-server::WebSocketTransport`;
in-process clients continue to use the same typed envelopes. JSON is
intentionally retained as the inspectable contract until payload benchmarks
settle a MessagePack/CBOR migration. A future codec can sit behind the same
transport boundary without changing request IDs or event sequences.

## Protocol requirements

- Request IDs and typed responses.
- Server events with monotonically increasing sequence numbers.
- Session IDs and resumable subscriptions.
- Heartbeats, deadlines, cancellation, and backpressure.
- Capability and protocol-version negotiation.
- Compression for large payloads, without compressing already-compressed data.
- Chunked file, model, terminal, and tool-output streams.
- Idempotency keys for retryable mutations.
- Structured errors with stable codes and user-facing context.
- Authentication and authorization before workspace or provider capabilities
  are exposed.
- Redaction rules for secrets in logs and event history.

## Workspace, session, and repository ownership

The target domain model separates organization from execution:

- A `Workspace` groups sessions and workspace-level settings. It has an ID
  and lifecycle independent of any local directory or Git repository.
- An `AgentSession` belongs to a workspace and owns an isolated filesystem
  root on its backend. The host path is backend-internal and is not returned
  to remote clients. This defines filesystem ownership and path scope; it
  does not by itself guarantee OS-level process sandboxing.
- A session may attach zero or more repository sources. Each attachment
  identifies a source, requested revision, and stable relative checkout path
  within that root. The backend materializes a session-owned clone or
  worktree; mutable working trees are not shared between sessions.
- File, process, terminal, checkpoint, change, and VCS operations are scoped
  to a session. Repository paths are interpreted relative to that session's
  checkout, while root-relative paths can address files elsewhere in the
  session filesystem.

The protocol exposes workspace lifecycle/list operations,
workspace-scoped session listing/creation, and session-scoped repository
attachment and filesystem operations. Workspace IDs are independent of paths
and repository IDs. The contract includes `DiscoverCapabilities`,
`ClientFrame`/`ServerFrame` codec types, and `SessionEventsSnapshot`.
Version compatibility is major-version based: a client may negotiate a newer
minor version within the same major, while an incompatible major returns the
existing `unsupported_protocol` error. Deployments must ensure or force clients
to upgrade; old-client forward compatibility is not supported.

Approval policies are scoped to an agent session. Session snapshots
include the effective approval policy and the session's `auto_approve_actions`
preference; the session-scoped `SetSessionApprovalPolicy` request updates both
for subsequent runs. A run resolves its policy once, when it is created, and
evaluates every step against that pinned copy, which the run snapshot reports in
its `approval_policy` field (absent for runs recorded before it was reported),
so a client can compare it with the session's own policy. A policy denial states
that pin and what changes it, and the `SetSessionApprovalPolicy` response reports
the scope the change applies in (`applies_from: "next_run"`), because a run that
is already executing keeps the decision it started with.

A session snapshot also carries an optional `goal`, the trimmed, non-empty text
that states what the session is for. The session-scoped `SetAgentSessionGoal`
request sets it and returns the updated snapshot as
`AgentSessionGoalChanged`. The same request with no goal (`goal: null`) clears
it, so clearing is explicit rather than an empty string; blank or over-long
text is still rejected before any state change, and an archived session or an
unknown session ID is rejected without a partial update. A goal change is
journalled as `AgentSessionGoalChanged`, which carries no goal when the goal was
cleared, so a connected client removes or replaces the goal it shows. A session
gains a goal only through this request: a run's task is never promoted into a
goal. The goal is stated on every model request, which keeps it outside the
compactable transcript and across a restart that resumes the run, and the
durable session record is re-read before each model turn, so a goal changed
while a run executes reaches that run's next turn instead of waiting for a new
run.

A run with a goal is goal-driven. Its request advertises `claim_goal_reached`,
a control tool answered by the runtime, and no other tool use is needed for it.
A turn that ends without a claim is answered with a bounded continuation that
tells the agent to continue or claim; after the continuation budget the run
completes with a summary that says the goal was not claimed, so an unproductive
model cannot loop. A run blocked on `ask_user` stays resumable as usual. A goal
cleared during a run stops driving it at the next model turn, and a clear leaves
the run's task text untouched. The root session's goal is the project goal, and
the manager is told it owns it. Protocol 11.12 adds the field, request, response,
and event; 11.13 adds clearing with a null goal. A snapshot written before the
field existed still decodes with no goal.

The WebSocket service authenticates during the HTTP upgrade using a bearer
token supplied in the `Authorization` header, or in a `loom.bearer.<token>`
subprotocol for browser clients that cannot set handshake headers, then
creates an authenticated view of the existing in-process connection. Each request re-checks the token so revocation takes effect
without restarting the service. Capability negotiation is intersected with the token grant, and every
workspace/session/run request is checked against the token's explicit scope.
A worker that allows GitHub sign-in also accepts an upgrade with no credential
at all; that connection is pre-login and may only sign in, as described under
[multi-user sessions](#multi-user-sessions).

The backend should journal enough event metadata to replay the current
projection after reconnecting, while avoiding unbounded memory growth. A
client that falls behind must be able to request a fresh snapshot and resume
from a known sequence.

Workspace-scoped `GetSessionEvents` requests require the
`SubscribeWorkspaceEvents` capability. The server returns typed workspace
event responses for these requests; it does not fall back to a session-only
response shape.

Workspace-scoped `GetSessionEvents` requests from capable clients return `WorkspaceEvents` when the
cursor is current and `WorkspaceEventsSnapshot` when the client must resync.
Their entries are a union of session event envelopes and workspace event
envelopes. Workspace rename and configuration
revision changes carry a workspace ID directly and never use a synthetic
session ID. The workspace sequence is shared with session events so clients can
resume one ordered workspace feed.

The server retains a bounded event journal per session and per workspace (4096
events each by default). If a session-specific cursor is older than the
retained range, `GetSessionEvents` returns `SessionEventsSnapshot` with the
current session projection, retained events, the oldest available sequence, and
the latest sequence. The client must replace its projection and resume from the
latest sequence in the response. Requests with a stable `RequestId` are
idempotent for retryable mutations; the bounded idempotency cache is persisted
with durable backend state.

The server sends WebSocket ping heartbeats, applies a configured request
deadline, accepts cancellation frames for pending transport tasks, and uses a
bounded outbound queue. A full queue closes the connection rather than
unboundedly buffering output. Disconnecting a client never cancels the
backend's agent, process, terminal, or journal work: a run executes on a
backend-owned worker, not on the connection that started it.

## Example domain operations

These names are illustrative; the schema should use explicit request and
event types rather than a generic "run arbitrary method" envelope.

```text
CreateWorkspace / ListWorkspaces
CreateAgentSessionInWorkspace / ListWorkspaceSessions
AttachSessionRepository / DetachSessionRepository / ListSessionRepositories
StartSessionAgentRun
PauseAgentRun
ResumeAgentRun
InterruptAgentRun
ApproveAgentAction
RejectAgentAction
RetryAgentStep
ForkAgentSession
ListProviders
ListModels
SetSessionModel
GetSessionContext
GetSessionFilesystemSnapshot
ReadSessionFile
ApplySessionEdit
SubscribeSessionFilesystemEvents
OpenTerminal
WriteTerminalInput
ResizeTerminal
StartTask
CancelTask
GetRepositoryStatus
```

M2 adds typed session-filesystem and process requests rather than exposing
filesystem or process primitives directly:

```text
AttachSessionRepository / DetachSessionRepository / ListSessionRepositories
GetSessionFilesystemSnapshot / GetSessionFilesystemChanges
ReadSessionFile / ApplySessionFilesystemEdit / TakeSessionFilesystemControl
CreateSessionCheckpoint / RevertSessionCheckpoint / UndoSessionEdit
SetSessionApprovalPolicy
OpenSessionTerminal / WriteSessionTerminalInput / ResizeSessionTerminal
GetSessionTerminalEvents / CancelSessionTerminal
StartSessionTask / GetSessionTask / GetSessionTaskEvents / CancelSessionTask
```

Session filesystem edits carry an optional content revision and return a
structured conflict when it no longer matches. A session's filesystem change
history is polled with `GetSessionFilesystemChanges` and an `after_sequence`
cursor; the same changes also arrive as `SessionFilesystemChanged` events on
the session event stream (`crates/loom-protocol/src/requests.rs`,
`crates/loom-model/src/events.rs`). Terminal and task output is
delivered as bounded, sequence-numbered records; snapshots remain available
after a client disconnects. Policy evaluations are agent events before a
tool executes, and the existing approval request/decision events remain
authoritative for approval-required actions.

Agent and Edit modes allow read, write, command, and network actions without
prompting by default; the session settings can opt out, which restores explicit
approval for writes, commands, and network actions. **Auto approve** remains an
explicit mode. The default policy allows destructive actions; exactly two
built-in tools are classified as destructive, `github_push_branch` and
`github_remove_labels`, and a client that wants to gate them sets the
destructive dimension to `RequireApproval`, which prompts per instance. `Deny`
on that dimension stays available to a deployment that wants a hard refusal,
and `run_command` is a command action that stays runnable in automatic mode.

M3 adds typed provider and durable-orchestration requests:

```text
StartAgentRunWithOptions
PauseAgentRun / ResumeAgentRun
ForkAgentSession
RetryAgentFromCheckpoint
ListProviders / ListModels
DiscoverProviderModels
GetProviderHealth
ConfigureGitHubCopilot
ConfigureApiKeyProvider / RemoveApiKeyProvider
StartGitHubCopilotLogin / GetGitHubCopilotLoginStatus
GetRunUsage
InspectAgentContext
```

`StartAgentRunWithOptions` carries `SessionLimits` and
`ContextAssemblyOptions`. Limits are reported in `RunLimitReached` and
`RunUsageUpdated` events; a context budget failure is a structured
`context_limit_exceeded` failure rather than silent truncation or provider
failover. `ContextInspected` events identify required, included, omitted, and
compacted context items. Before each model call, the runtime clamps configured
windows to the active model's advertised limit, counts tool schemas once, and
sends the reserved output limit to the provider. Models without a known window
use a visible 8,192-token fallback unless a window is configured explicitly.

At 90% of the input budget, context assembly keeps recent exchanges together
and replaces older history with bounded, explicitly lossy excerpts. System and
repository instructions, the original task, and the latest user direction remain
intact. Large tool outputs may be shortened with omission markers; call IDs and
arguments are preserved. If required context or the latest exchange still cannot
fit, the run reports a context-limit error. Compaction uses the active provider's
token estimator; these counts remain estimates, not server-side token guarantees.

When a provider ends a turn because it reached the model's output-token limit
without requesting a tool, the runtime keeps the partial response and resumes
with a bounded continuation prompt instead of failing the run. If the model
keeps reaching the limit, the run fails with an explicit output-token error
after a small number of attempts.

The runtime persists the summary and its history boundary across recovery,
retaining the full transcript separately. The UI shows estimated input usage and
output reserve, and records compaction in the timeline. Excerpts preserve the
beginning and end of older content; they are not model-generated semantic
summaries and may omit intermediate decisions or details.

Provider summaries contain provider/model IDs, capabilities, credential
reference IDs, and health state, never raw credentials. Normalized provider
authentication, rate-limit, invalid-response, and unavailable errors retain
retryability without echoing response bodies or request headers.
`ProviderSummary.api_key_configurable` is optional and defaults to false so
new clients do not offer setup on older workers. `ConfigureApiKeyProvider`
accepts a provider ID and API key for a registered API-key provider and returns
only `ProviderConfigured`; the secret-bearing request is excluded from the
durable idempotency journal. The client requires WSS for remote workers
(loopback WS is allowed) before sending the key. `RemoveApiKeyProvider` takes
only the provider ID and returns `ProviderConfigured`; it drops the stored
secret and leaves the provider in the catalog so it can be configured again.

Provider configuration remains in each backend's SQLite database. Newly
entered API keys are stored in a sibling `<database-stem>.credentials.json`
file, with owner-only permissions on Unix. The provider config stores only a
random credential reference into that backend-specific file, so databases on
the same host do not discover or reuse each other's new keys. Existing
OpenAI-compatible configs that point into the legacy host credential file are
migrated lazily when their backend opens: the key is copied into that
backend's credential file and the SQLite config is updated. The old entry is
retained so another existing backend can migrate independently; existing
provider-specific credentials such as GitHub Copilot remain in their current
store.

`ConfigureGitHubCopilot` accepts a GitHub device-flow access token only over an
authenticated connection, stores it in the worker's credential store, and
returns no credential material. It requires the separate `ConfigureProviders`
capability; tokens are not included in workspace configuration or provider
summaries. Clients must use WSS for remote workers (loopback WS is allowed)
when sending the credential. Browser clients instead use
`StartGitHubCopilotLogin` and `GetGitHubCopilotLoginStatus`: the worker performs
the device-code exchange and stores the resulting credential itself, so no
OAuth access token is sent through or persisted by the browser.

A device flow is also how a person signs in (`Access/StartGitHubSignIn`, see
[multi-user sessions](#multi-user-sessions)). That sign-in is not provider
configuration: it asks only for `read:user`, uses the token to read the GitHub
account once, and never stores it. Worker-wide provider configuration, including
`ConfigureGitHubCopilot`, stays with operator connections, while a person's
`StartGitHubCopilotLogin` adds a version of their own credential. A provider
device login also asks for `read:user`, so when it completes on a connection
with no person the worker binds that account too: the provider sign-in doubles
as the worker sign-in for an operator token.

Important event families include `AgentMessageDelta`,
`AgentPlanProposed`, `AgentStepStarted`, `ToolCallRequested`,
`ToolApprovalRequired`, `ToolCallStarted`, `ToolOutputChunk`,
`AgentStepCompleted`, `AgentNeedsInput`, `AgentRunUsage`,
`SessionFilesystemChanged`, and `AgentRunCompleted`. Events must identify the
session, run, step, tool call, and sequence number so a client can render
partial progress and recover a consistent view.

Agent activity history is additionally represented by additive
`ActivityRecorded` events. Each record has a stable activity ID, an
optional parent activity and step, status, timestamps, and elapsed duration.
Model activities identify only the model turn; they do not invent or expose
private model reasoning. Tool activities retain the observable call
arguments and result, with typed file, search, and command details inferred
from the existing tool schemas. Existing lifecycle and tool events remain
authoritative and are preserved for older clients.

M3 also journals `StepStarted`, `StepCompleted`, `ContextInspected`,
`RunUsageUpdated`, `RunLimitReached`, approval/policy events, and forked
session history. `ProviderError`, `ContextError`, and `RecoveryRequired`
events retain normalized failure reasons without secret material. The backend
persists these events before acknowledging the
mutating request when durable mode is enabled. On restart, unfinished
provider calls are not replayed implicitly: recoverable runtime state is
paused and the client explicitly resumes or retries from its workspace
checkpoint. If a referenced credential is unavailable, the run remains
inspectable in `paused` state and receives `RecoveryRequired`; resuming then
returns the normalized provider authentication error rather than switching
models.

M5 adds only the capability-gated projections required for the focused agent
client:

```text
ListWorkspaces / ListWorkspaceSessions / GetAgentSessionSnapshot
GetAgentRunSnapshot / GetSessionEvents
GetSessionFilesystemChanges / ReadSessionFile / GetSessionVcsDiff
ListSessionTasks / GetSessionTask / GetSessionTaskEvidence
```

Worker-node management adds workspace-scoped
`GetWorkspaceConfigForWorkspace` and `SetWorkspaceConfigForWorkspace`
requests. The persisted config contains a revision,
worker WebSocket URLs, a CPU pulse threshold for session-card node indicators
(default 5%), the parallel project-agent limit, the auto-archive-completed-agents
switch, the automatic-project-titles switch (default on), and the run queue
timeout (default 30 minutes); credentials, workspace
files, and session state are not included. Connected peers receive the updated config when nodes join or
are removed. Sessions remain owned by the node that created them; config
distribution does not migrate sessions.
Provider settings are maintained independently by each worker. The client can
open provider settings for any connected worker, and signing in configures that
worker rather than the client host or other workers.

The UI aggregates sessions from connected nodes and offers a node choice when
creating a session on a multi-node setup, defaulting to the startup backend.
Session and run requests are sent to the recorded owner. If that node becomes
unavailable or is removed, the session remains pinned there and is never
silently moved to another backend.

`GetWorkerNodeStatus` reports CPU utilization and RAM usage as integer
percentages when available. CPU utilization is sampled by the host system
monitor between status requests (rather than inferred from CPU count or load
average); the first sample and unsupported measurements are unavailable.
Memory usage is calculated from total memory minus the operating system's
available memory. Each backend instance uses a UUID node ID that remains
stable for that instance, and its display name includes the host name plus a
short ID suffix so separate backend processes on one host remain distinguishable.
Clients should render unavailable values as `n/a`.

`GetArchiveRetentionPolicy` is read-only and returns
`archive_retention_policy { retention_ms, force_discard_worktrees }`. It
requires the `read_worker_node_status` capability. `retention_ms` is `null` or
zero when retention is disabled.

`SetArchiveRetentionPolicy { policy }` replaces that policy and answers with the
normalized policy that was stored, so the caller sees exactly what the worker
will apply from its next sweep. It is a retryable mutation and requires the
separate `configure_archive_retention` capability, so a client that may only
read the worker status cannot turn automatic deletion on. A zero or absent
`retention_ms` disables retention while `force_discard_worktrees` is kept as
sent. The policy is stored in durable state and survives a restart; changing it
takes effect at the next sweep, because a change does not start one, and it
requires durable storage, so an in-memory worker refuses the request with
`unsupported_capability`.

A worker resolves its retention policy at startup in one order: an explicit
`--archive-retention`/`LOOM_ARCHIVE_RETENTION` window (with
`--archive-retention-force-discard`/`LOOM_ARCHIVE_RETENTION_FORCE_DISCARD`) wins
over the persisted policy and is persisted in turn, otherwise the policy loaded
when the durable state was opened applies, otherwise retention is disabled. The
startup log names the source that won. Automatic deletion itself remains an
internal backend operation rather than a client request: a client reads or sets
the active policy and observes the resulting session removals.

Session-card indicators pulse smoothly only while CPU usage is above the
workspace-configured threshold. The online color changes to red only after
three consecutive 10-second status polls report both CPU and RAM above 90%;
either metric at or below 90%, unavailable metrics, or a failed poll resets
the severe-load streak.

The existing M1-M4 mutations remain the control surface:

```text
CreateAgentSession / RenameAgentSession / ArchiveAgentSession
SetAgentSessionTitleSource
DeleteAgentSession
StartAgentRun / SendAgentMessage
PauseAgentRun / ResumeAgentRun / InterruptAgentRun
ApproveAgentAction / RejectAgentAction / RetryAgentStep
```

A project is titled automatically. A project's row in the sidebar is rendered
from its root agent session's `name`, so the backend chooses that name from the
work: the `title` the agent passes to `propose_plan` (3-6 words, no paths and no
quotes), or a title derived from the run's goal when the run never proposes
one. The title is sanitised (no paths or URLs, at most six words and 48
characters, first letter upper-case) and is only replaced when it diverges
enough from the current name. Only a project root is titled; a delegated sub
agent keeps the name its manager gave it.

`AgentSessionSnapshot.title_source` says who chose the current name: `auto`
when the backend chose it, `manual` after a person renamed the session. A
manual rename pins the title, so automation stops for that project.
`SetAgentSessionTitleSource { session_id, source }` changes the source without
renaming, returning `agent_session(snapshot)`, and requires the same
`control_agent_session` capability and owner role as `RenameAgentSession`. A
rename reports its source in `AgentSessionRenamed { session_id, name, source }`
(absent in older payloads, which read as `auto`), and a source change is
journaled as `AgentSessionTitleSourceChanged { session_id, source }`, so a
client can render the state from the event stream. A workspace turns the
behaviour off with `auto_title_projects: false`; the default is on, so an
upgraded workspace titles new projects without an operator action.

`ArchiveAgentSession` interrupts a non-terminal run for the session before
archiving it, so clients do not need to issue a separate interrupt request.

`DeleteAgentSession { session_id, force }` deletes an archived session and
returns `agent_session_deleted { session_id }`. It is a retryable mutation and
requires the separate `delete_agent_session` capability. Only archived sessions
can be deleted, and `Archived` is terminal: there is no restore or unarchive,
so an archived session is immutable until it is deleted. Deleting a project
root deletes every descendant session of that project in one operation, using
the same project snapshot and cascade order as archiving. The `force` flag
allows discarding a dirty or locked linked worktree that would otherwise
refuse the deletion; without it, such a project is refused, and the caller
decides whether to force it.

Session and run snapshots must be sufficient to render the active
conversation, plan, step state, tool calls, approvals, bounded output,
questions, errors, and final summary after reconnect. Review responses are
read-only, session-scoped, and bounded; diffs and task artifacts retain stable
references without exposing credentials or shell strings.

The protocol has no editor-buffer, editor-layout, or language-service
requests, and no VCS index or commit mutations. VCS status, diff, branch, and
conflict reads remain available. Those descoped surfaces would be reintroduced
with the milestone that needs them.
All M5 operations work through both `InProcessConnection` and the
authenticated WebSocket adapter after normal version and capability
negotiation.

## Run execution and control

`StartAgentRun` registers the run and returns its snapshot as soon as the run
is observable; the run itself proceeds on a backend worker. Clients follow
progress through the session event stream, which receives assistant deltas
while a model call is still open.

A session accepts a new run only while it is `idle`. A run that completed,
failed, was cancelled, or is waiting for approval leaves the session in that
state, so a later start in the same session is refused with `invalid_state`
rather than replacing or restarting the previous run. A headless client that
wants to start work therefore resolves an idle session or creates a new one;
`loom-ctl run` does exactly that and reports the refusal instead of retrying
under another session name.

`PauseAgentRun` and `InterruptAgentRun` raise a control flag and cancel the
in-flight model stream rather than waiting for it, so they are answered while
a model call is open. Requests that need exclusive access to a busy run
(`ApproveAgentAction`, `SendAgentMessage`, `RetryAgentStep`, and the
checkpoint retry) return a retryable `conflict` instead of blocking.

Run projections (`GetAgentRunSnapshot`, `GetRunUsage`, `InspectAgentContext`)
are served from the backend's cached run state. Run state, usage, pending
approval, and pending input are updated from events as they are journaled; the
message transcript in a projection is refreshed at each step boundary, so a
transcript may lag the event stream by one in-flight model call.

## Activity detail

Protocol 11.7 adds `activity_detail` to the requests that carry transcript
content: `GetSessionEvents`, `GetRecentSessionEvents`, `GetAgentRunSnapshot`,
`GetAgentRunTranscriptPage`, `GetAgentSessionSnapshot`,
`GetAgentSessionSnapshotMetadata`, and `GetAgentSessionInitialState`. It has
three values:

- `replies`: the agent's replies plus a blocking approval or input prompt, with
  no provider reasoning and no tool content.
- `activity`: replies and tool activity, without provider reasoning.
- `full`: everything.

The value is a projection, not a permission. A reduced level empties content
fields but never drops an event or message record, because sequence numbers,
message ordinals, page cursors, and client idempotency depend on the records
staying. Cursors, ordinals, and page boundaries are therefore identical at
every level, and a client can change levels without losing its place. A
blocking approval or input prompt keeps its tool arguments at every level,
because those arguments are the control the person answers. Run control state,
usage, the plan, and the run summary are likewise returned unchanged.

An omitted field means `full`, so a request that does not carry it (including
one from an older client) is answered exactly as before.

Changing the level re-reads the run: content the client did not fetch cannot be
shown later, so a level that renders more than the last read has to ask again.
Levels that need the same projection change only the display.

## Multi-user sessions

Protocol 11.3 adds identity, roles, and presence; 11.4 adds attributed chat
and ordered run actions. Provider credential selection uses the same 11.4
version. 11.5 adds access requests. 11.6 adds GitHub sign-in. 11.7 adds the
activity-detail projection. 11.8 adds provider API-key removal. 11.9 adds
resuming a remembered GitHub identity. 11.10 adds creating a sub project from
the project it branches out of. 11.11 adds automatic project titles,
`SetAgentSessionTitleSource`, and the title source a rename reports. 11.12 adds
the session goal field, request, response, and event; 11.13 adds clearing it.
11.14 adds the one-time sign-in credential. Every addition is a new enum
variant, a capability, or a field with a compatible default (an omitted field
keeps the older behavior), so clients of an older 11.x minor keep working; they
simply never send the new requests, negotiate the new capability, or ask for
the new behavior.

### Signing in with GitHub

An interactive client is not expected to hold a per-person bearer token. GitHub
sign-in is enabled on a worker unless the operator switches it off
(`loom-server --no-github-sign-in`), and `--allow-github-login <login>`
(repeatable) narrows who may sign in; with no allowlist any GitHub account may.
A WebSocket upgrade that carries **no** credential at all is accepted and the
connection starts *pre-login*: it has no participant identity and no session
grants. A pre-login connection may only:

- `Negotiate` and `DiscoverCapabilities` (capability `git_hub_sign_in`);
- `GetWorkerNodeStatus`, so a client can show which worker it reached;
- `Access/GetCurrentParticipant`, which reports `participant: null`;
- `Access/StartGitHubSignIn`, `Access/GetGitHubSignInStatus`, and
  `Access/SignOutGitHub`.

Every other request fails with `authentication_required`. When sign-in is
disabled (`--no-github-sign-in`), or when a credential is presented, the
upgrade behaves exactly as before: an operator bearer token is required. The
listener bounds how many connections may wait to sign in at once, releases that
slot once a person is bound, and closes a connection that is still unsigned
after its sign-in window.

A sign-in is a GitHub device flow:

1. `StartGitHubSignIn` → `GitHubSignInStarted { login_id, user_code,
   verification_uri, expires_in, interval }`. The client shows the user code and
   verification URI. The worker asks GitHub only for the `read:user` scope and
   never stores the resulting GitHub token as a provider credential.
2. The client polls `GetGitHubSignInStatus { login_id }` →
   `GitHubSignInStatus { status }`, where `status` is `pending`,
   `signed_in { participant }`, or `failed { message }`. The person's identity is
   derived from the immutable numeric GitHub account id, so it survives a login
   rename and a second sign-in; the display name is the GitHub login. Only the
   connection that started a sign-in reads it.
3. `signed_in` means this connection is now bound to a person. If the GitHub
   login is not on the worker's allowlist, the sign-in fails and the connection
   stays pre-login.
4. `SignOutGitHub` → `SignedOutGitHub` ends the binding: the person's presence
   is dropped from every roster they appear in, and a connection that was
   accepted without a credential returns to the pre-login state.
5. A client remembers the participant id per worker, keyed by a durable worker
   identity — a remote worker's address, or the state database the managed local
   worker keeps — and never by `WorkerNodeStatus::node_id`, which names one
   worker process and is regenerated whenever that process starts. A later
   connection with an unrestricted operator token may send
   `ResumeGitHubIdentity { participant_id }` → `GitHubIdentityResumed {
   participant }`, which binds presence again without a new device flow. The
   worker resumes only a person already in its directory and only on an operator
   token; a connection accepted without a credential still has to sign in.

The poll that first reports `signed_in` may also carry `resume_token`, the
person's own revocable credential. The client stores it and presents it as the
bearer credential on a later connection; the worker then binds that connection
to the same person, subject to the session roles that person holds. It is not
an operator token: it reaches only what that person is granted. The worker
keeps only the credential's digest, and a sign-out revokes it. An older worker
sends no field at all, so a client treats it as optional.

Anyone with a GitHub account *may* complete the device flow, but a signed-in
person holds no session grants until an owner grants one (see access requests).
An operator bearer token stays what it was: an unrestricted, unbound client for
headless use. It may sign in too, and is then both an operator and a person.

### Identity and roles

The worker binds a participant identity to a connection; no request names its
own author. A participant id derived from a GitHub account id is stable across
renames and re-authentication, and an operator token is not bound to a person
and is governed by token scope only.

Each session grants every person one role:

| Role | Allowed |
| --- | --- |
| `observer` | Read the session, its runs, roster, revisions, and credential metadata. |
| `participant` | Everything an observer can, plus peer chat, agent direction and answers, approve/reject, pause/resume/interrupt/retry, terminals, tasks, and edits. |
| `owner` | Everything a participant can, plus grants and roles, credential selection, repository search and attach/detach, rename, archive, delete, and approval policy. |

`ClientRequest::required_session_role` returns the least role a request needs.
New grants start as `observer`. The session creator is its owner. The last
owner cannot be demoted or revoked. Revocation fails the person's later
requests and closes their WebSocket connections with close code 1008.

A sign-in on a connection that also holds an unrestricted operator token — a
scope that names neither a workspace nor a session — makes the signed-in person
the `owner` of every session that has no owner, and that is persisted before the
sign-in is reported as `signed_in`. The token already reaches every session, so
the ownership follows it rather than granting more. Everything else is
unchanged: a session with any grant is left untouched, because its last owner
can be neither demoted nor revoked; a pre-login connection holds no token and
adopts nothing; and a token restricted to named workspaces or sessions adopts
nothing either. Role enforcement is unchanged — the sign-in adds grants, it does
not bypass a check.

### Access requests

`ClientRequest::Access(AccessRequest)`:

- `GetCurrentParticipant` → `CurrentParticipant { participant }` (`None` for
  an operator connection).
- `ListParticipants` → `Participants`.
- `GetSessionRoster { session_id }` → `SessionRoster { entries }`. One
  `RosterEntry` per person: identity, role, presence (`online`, `away`,
  `offline`), connection count, last activity. A connection silent for 45 s is
  stale; a live person idle for 5 min is away.
- `GrantSessionAccess`, `SetSessionRole` → `SessionGrant`;
  `RevokeSessionAccess` → `SessionAccessRevoked`. Owner only.
- `ListProviderCredentials` → `ProviderCredentials`: the caller's own
  credentials plus the worker default. Metadata only (provider, owner label,
  version, revoked); secrets never leave the worker.
- `SelectSessionCredential { session_id, credential_id }` →
  `SessionCredential`. Owner only. Applies to later runs.
- `GetSessionCredential { session_id }` → `SessionCredential { selection,
  credential }`; both `None` means the worker default.
- `GetRunCredential { run_id }` → `RunCredential { pin }`. Each run pins
  credential id, version, provider, and model when it starts; `pin` is `None`
  for runs started before pinning.
- `RevokeProviderCredential { credential_id }` →
  `ProviderCredentialRevoked`. Runs pinned to it fail their next provider call.

- `RequestSessionAccess { session_id, requested_role, note }` →
  `SessionAccessRequestPending(SessionAccessRequest)`. Needs only a
  participant-bound connection (capability `RequestSessionAccess`); no grant
  is required. One pending request per person and session: asking again
  replaces role and note and keeps the id. A note is at most 1 KiB. Asking for
  a role you already hold (or lower) fails with `invalid_request`.
- `ListSessionAccessRequests { session_id }` → `SessionAccessRequests`.
  Owner only. Oldest first.
- `ApproveSessionAccessRequest { session_id, request_id, role }` →
  `SessionAccessRequestResolved { request, decision, grant }`. Owner only.
  Grants `role`, or the requested role when `role` is `None`.
- `DenySessionAccessRequest { session_id, request_id, reason }` →
  `SessionAccessRequestResolved`. Owner only. Access is unchanged.
- `WithdrawSessionAccessRequest { session_id, request_id }` →
  `SessionAccessRequestResolved` with `decision: withdrawn`. Only the
  requester; anyone else gets `not_found`.

Pending requests persist (table `session_access_requests`, schema v11) and are
removed when decided or withdrawn.

The same `Access` domain carries the GitHub sign-in requests described under
[signing in with GitHub](#signing-in-with-github): `StartGitHubSignIn`,
`GetGitHubSignInStatus { login_id }`, `SignOutGitHub`, and
`ResumeGitHubIdentity { participant_id }`. They need no session and no grant;
resuming additionally needs an unrestricted operator token.

A missing, revoked, or mismatched credential fails with
`provider_authentication` and a recovery hint (the owner signs in again or
selects another credential). There is no fallback to another account.

`RepositoryRequest::SearchGitHubRepositories` takes an optional `session_id`.
A participant-bound connection must name a session it owns; operator
connections may omit it.

### Collaboration requests

`ClientRequest::Collaboration(CollaborationRequest)`:

- `SendPeerMessage { session_id, text }` → `PeerMessageSent { entry }`. Peer
  chat is stored in the session history, is never added to model context, and
  never starts or continues a run. The protocol has no per-person delivery:
  every peer message goes to the session's people. Clients pick the recipient
  from a leading mention, so no modal state can misroute a message: `@agent`
  sends the rest of the text to the agent, `@people` sends peer chat, and
  `@<name>` sends peer chat addressed to the roster person whose name matches.
  Any other leading `@` word is a file reference and leaves the text on the
  agent path.
- `DirectAgent { run_id, expected, text }`,
  `AnswerAgentQuestion { run_id, expected, text }`, and
  `ControlRun { run_id, expected, action }` → `RunAction(RunActionOutcome)`.
- `GetRunRevision { run_id }` → `RunRevision { revision }`. Observer role.
- `ListSessionCollaboration { session_id, after_sequence, limit }` →
  `SessionCollaboration { entries, next_after_sequence }`. Observer role. The
  retained `CollaborationEntry` records of the session, oldest first, each
  with its feed sequence; at most 500 per page. Clients read it on session
  load and reconnect to rebuild peer chat and attribution, and de-duplicate
  it against live `Collaboration` events.

Sends need the participant role. Text is limited to 16 KiB.

### Ordering and conflicts

`RunRevision` is `{ attempt_id, control_revision, action_sequence }`. The
server serializes every direction, answer, and control action on a run. An
action is applied only if its `expected` revision equals the current one; it
then advances `action_sequence` and answers `Accepted { entry, revision, run }`.
Otherwise it answers `Conflict { expected, current, run }` and changes
nothing. `action_sequence` is persisted per run (table
`run_action_sequences`, schema v11) with every state save, so it survives a
backend restart and a revision issued before the restart never matches a
different state after it. Two people acting on the same revision get one acceptance and one
conflict, in the order the server received them.

Clients send the revision they decided against (from `GetRunRevision`, the
last outcome, or the last `Collaboration` event). On `Conflict` a client
should keep the user's draft, adopt `current`, show the new run state, and let
the user decide again. Legacy `RunRequest` variants pass through the same gate
without a revision check, so they are ordered and attributed too.

### Events

- `ServerEvent::Collaboration { entry }`: a `CollaborationEntry` with
  `author` (`participant` with identity and role, or `operator`), `recipient`
  (`agent` or `people`), `kind` (`peer_message`, `agent_direction`,
  `agent_answer`, `run_control`, `access_changed`, `access_request_decided`),
  optional `run_id` and `revision`, and `created_at`. Persisted in the session
  history. Grants, role changes, revokes, and approve/deny decisions are all
  recorded here with their author.
- `ServerEvent::SessionRosterChanged { entries }`: the full roster after a
  connect, disconnect, grant, role change, or revoke. Clients replace their
  copy. Presence is live state, not history: only the latest roster per
  session is kept, outside the retained window, and it is never written to
  the durable feed, so presence churn never evicts history events. After a
  restart or for the current state, read `GetSessionRoster`.
- `ServerEvent::SessionAccessRequested { request }`: a request was filed or
  replaced. Delivered to the session's owners and the requester only.
- `ServerEvent::SessionAccessRequestResolved { request, decision }`: a
  request was approved, denied, or withdrawn. Delivered to the owners and the
  requester (the requester sees it on the cross-session stream even before
  holding a grant).

## Headless polling client

`loom-ctl` reads the journal with one pull loop; this slice adds no push
subscription. It sends `GetSessionEvents` with `after_sequence` and never waits
for unsolicited server events.

- **Polling.** The interval starts at 250 ms and doubles per empty poll to a
  2000 ms ceiling; the first response that carries an event resets it to 250 ms.
- **Paging.** While a response names `next_after_sequence`, the client requests
  the next page at once with that cursor, so one logical batch is not split
  across polls. A response that names no continuation has reached the latest
  journaled event.
- **Snapshot replaces the cursor.** A `SessionEventsSnapshot` answer means the
  requested cursor is no longer retained. The client replaces its local
  projection state, renders only events newer than the highest sequence it has
  already rendered, and resumes from `latest_sequence` in that response. It
  never reprints an event it already emitted and never skips one between the
  snapshot's `oldest_sequence` and `latest_sequence`.
- **Epoch.** `stream_epoch` names one backend instance. A change of epoch
  invalidates the cursor: the client discards its cursor and treats the next
  response as a snapshot.
- **Cursor persistence.** An optional local cursor file records the URL, the
  session, the last rendered sequence, and the epoch. It is written atomically,
  through a temporary file plus a rename, after each rendered batch. The cursor
  file is client-local; the backend stores no per-client cursor.
- **Read-only verbs.** `ls`, `get`, `events`, and `peek` issue no mutation, so a
  read-only-scoped token can run them.
- **Anchoring.** The durable journal and `RequestId` idempotency remain the only
  authorities for resume and retries. `loom-ctl run` starts a run and returns its
  ids without waiting, so waiting for completion stays a client concern.

## Reconnect and consistency

The backend event stream is authoritative. A client connection should:

1. Authenticate and negotiate protocol/capability versions.
2. Subscribe to one or more session streams from a known sequence.
3. Apply a snapshot when its sequence is no longer retained.
4. Resume event delivery without duplicating mutations.
5. Reconcile pending approvals and client commands after reconnect.

Long-running model calls, tool calls, processes, and child agents must not
depend on a frontend connection. A frontend can disconnect, reconnect from a
different device, and continue controlling the same backend session.

A session's events are journaled by whoever acts on it, so a client of a
worker that more than one person can reach follows the open session's stream
whether or not it has a run of its own: another person's run arrives there and
nowhere else. That follow poll may be slower than the poll of a run this
client started, because nothing is streaming yet, but it keeps running while a
shared session is open.
