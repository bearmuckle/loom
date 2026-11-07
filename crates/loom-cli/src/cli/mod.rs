//! `loom-cli`: the command-line client contract.
//!
//! # Surface
//!
//! Global flags are accepted anywhere on the command line and in both the
//! `--flag value` and `--flag=value` forms. The first non-flag token names the
//! verb.
//!
//! `--help` (or `-h`) prints the global manual. When the line also names a verb,
//! as in `loom-cli run --help` or `loom-cli project worktree cleanup --help`, it
//! prints only that verb's usage, its flags, and a few copy-and-paste examples,
//! so one command answers without the whole table; a verb the per-verb table does
//! not know falls back to the global manual, and help always wins over a token
//! that would otherwise be rejected.
//!
//! | verb | requests |
//! | --- | --- |
//! | `doctor` | `Control/DiscoverCapabilities`, `Control/Negotiate`, `Access/GetCurrentParticipant`, `Control/GetWorkerNodeStatus` |
//! | `node` | `Control/GetWorkerNodeStatus` |
//! | `whoami` | `Access/GetCurrentParticipant` |
//! | `ls` | `Workspace/ListWorkspaces`, or `Workspace/ListWorkspaceSessions` with `--workspace` |
//! | `get` | `Session/GetAgentSession`, plus `Session/GetAgentSessionSnapshotMetadata` with `--metadata` |
//! | `events` | `Events/GetSessionEvents` |
//! | `peek` | `Events/GetRecentSessionEvents` |
//! | `run` | `Control/Negotiate`, `Workspace/ListWorkspaceSessions`, `Workspace/CreateAgentSessionInWorkspace`, `Repository/AttachSessionRepository`, `Filesystem/AttachSessionDirectory`, `Session/SetSessionApprovalPolicy`, `Run/StartSessionAgentRunWithOptions`, plus `Provider/ListProviders` when `--model` is absent |
//! | `send` | `Collaboration/GetRunRevision`, `Collaboration/DirectAgent` |
//! | `project send` | `Project/GetProjectSnapshotForSession`, `Session/GetAgentSessionSnapshotMetadata`, `Collaboration/GetRunRevision`, `Collaboration/DirectAgent` |
//! | `answer` | `Collaboration/GetRunRevision`, `Collaboration/AnswerAgentQuestion` |
//! | `approve`, `deny` | `Collaboration/GetRunRevision`, `Collaboration/ControlRun` with `Approve`/`Reject` |
//! | `stop` | `Collaboration/GetRunRevision`, `Collaboration/ControlRun` with `Interrupt`, plus `Session/ArchiveAgentSession` with `--archive` |
//! | `wait` | `Collaboration/GetRunRevision`, `Session/GetAgentSessionSnapshotMetadata` polling |
//! | `status` | `Session/GetAgentSessionSnapshotMetadata`, plus `Project/GetProjectSnapshotForSession` and one `Session/GetAgentSessionSnapshotMetadata` per blocked agent with `--tree` |
//! | `liveness` | `Session/GetAgentSessionSnapshotMetadata` |
//! | `result` | `Repository/ListSessionRepositories`, `Repository/GetSessionVcsStatus`, `Repository/GetSessionVcsDiff`, `Task/GetSessionTaskEvidence` |
//! | `retention get` | `Control/GetArchiveRetentionPolicy` |
//! | `retention set` | `Control/GetArchiveRetentionPolicy`, `Control/SetArchiveRetentionPolicy` |
//! | `providers list` | `Provider/ListProviders` |
//! | `models list` | `Provider/ListProviders` |
//! | `providers set-key` | `Provider/ConfigureApiKeyProvider` |
//! | `delete` | `Project/GetProjectSnapshotForSession`, `Session/DeleteAgentSession` |
//! | `session archive` | `Project/GetProjectSnapshotForSession`, `Session/ArchiveAgentSession` |
//! | `session delete` | `Project/GetProjectSnapshotForSession`, `Session/DeleteAgentSession` |
//! | `project get`, `project tree`, `project tasks` | `Project/GetProjectSnapshotForSession` |
//! | `project children` | `Project/GetProjectSnapshotForSession`, `Session/GetAgentSessionSnapshotMetadata` per direct child |
//! | `project messages` | `Project/GetProjectSnapshotForSession`, `Project/ListProjectAgentMessages` |
//! | `project child` | `Project/GetProjectSnapshotForSession`, `Project/ControlProjectChild` |
//! | `project archive` | `Project/GetProjectSnapshotForSession`, `Session/ArchiveAgentSession` |
//! | `project delete` | `Project/GetProjectSnapshotForSession`, `Session/DeleteAgentSession` |
//! | `project worktrees` | `Project/GetProjectSnapshotForSession` |
//! | `project worktree get` | `Project/GetProjectSnapshotForSession` |
//! | `project worktree diff` | `Project/GetProjectSnapshotForSession`, `Project/GetProjectChildReview` |
//! | `project worktree integrate` | `Project/GetProjectSnapshotForSession`, `Project/IntegrateProjectChild` |
//! | `project worktree cleanup` | `Project/GetProjectSnapshotForSession`, `Project/CleanupProjectChildWorktree` |
//!
//! The mutating requests of `run`, `send`, `project send`, `answer`, `approve`,
//! `deny`, `stop`, `delete`, `session archive`, `session delete`, `project
//! archive`, `project delete`, `project child`, `project worktree integrate`,
//! and `project worktree cleanup` are the ones `--request-id` keys. Everything
//! else a verb sends, such as
//! `Collaboration/GetRunRevision` and `Project/GetProjectSnapshotForSession`,
//! is a read with a fresh id, and only the verb's own mutation carries the
//! caller's key:
//!
//! | verb | keyed request | derived request |
//! | --- | --- | --- |
//! | `run` | `Run/StartSessionAgentRunWithOptions` | `--new` also derives the session creation, the input attachments, and the policy change |
//! | `send` | `Collaboration/DirectAgent` | — |
//! | `project send` | `Collaboration/DirectAgent` | — |
//! | `answer` | `Collaboration/AnswerAgentQuestion` | — |
//! | `approve`, `deny` | `Collaboration/ControlRun` | — |
//! | `stop` | `Collaboration/ControlRun` | `Session/ArchiveAgentSession` with `--archive` |
//! | `delete`, `session delete` | `Session/DeleteAgentSession` | — |
//! | `session archive`, `project archive` | `Session/ArchiveAgentSession` | — |
//! | `project delete` | `Session/DeleteAgentSession` | — |
//! | `project child` | `Project/ControlProjectChild` | — |
//! | `project worktree integrate` | `Project/IntegrateProjectChild` | — |
//! | `project worktree cleanup` | `Project/CleanupProjectChildWorktree` | — |
//!
//! Every verb first sends `Control/Negotiate` with the version this build
//! speaks and with only the capabilities it uses, because a worker refuses an
//! unnegotiated connection. `node`, `whoami`, `ls`, `get`, `events`, `peek`,
//! `retention get`, `providers list`, `models list`, and every `project` read
//! are the read-only verbs: they send nothing that mutates and negotiate only
//! the capabilities their reads need, so a read-only-scoped token can run them.
//! `retention set`, `providers set-key`, and `delete` are operator-level
//! mutations: a token without `configure_archive_retention`,
//! `configure_providers`, or `delete_agent_session` is refused at negotiation
//! with exit `3`.
//!
//! `whoami` and `doctor` report the connection's participant: the identity
//! object of the person the token belongs to, or `{"kind":"operator"}` for an
//! operator token, which is the same connection the text line calls
//! `participant operator`. Neither verb invents a person.
//!
//! `session archive`, `session delete` (and its `delete` alias), `project
//! archive`, `project delete`, and `project worktree cleanup` are the
//! operator-level lifecycle verbs, and they are separate operations: archiving
//! moves a session to the archived state and keeps its transcript, checkpoints,
//! and every checkout, deletion removes the durable state, and worktree cleanup
//! decides the fate of one delegated task's checkout. Each one reads
//! `Project/GetProjectSnapshotForSession` before it mutates anything, because an
//! id alone does not say whether it names a project root whose operation reaches
//! other sessions.
//!
//! `session archive` accepts any session and requires `--yes` when the id is a
//! project root whose archive the worker cascades to its live members, so the
//! blast radius is one explicit decision. `project archive` and `project delete`
//! require the id to name such a root and spell the scope themselves; `project
//! delete` keeps the `--yes` requirement because deletion is irreversible and
//! `--force` still has to be confirmed. The worker stays the authority on which
//! sessions may change state: archiving a project root while a child task is
//! unfinished or a child session is active is refused as `invalid_state` (exit
//! `5`), and a deletion of a live child is cancelled by the worker rather than
//! by this client.
//!
//! `project worktree cleanup` resolves the project, the manager session, and the
//! worktree record from the snapshot by `--task`, so the caller never supplies
//! an id the worker did not report. The disposition is explicit: `--keep`
//! retains the checkout, `--remove` removes it and is refused while it holds
//! changes, and `--discard` removes it with those changes and therefore also
//! needs `--yes`. A checkout is never removed without one of those flags.
//!
//! Listing, inspection, review, integration, and cleanup are separate operations
//! over the same worker-owned records. `project worktrees` lists the checkouts a
//! project's snapshot holds, `project worktree get` reports one durable record
//! without opening it, and `project worktree diff` asks the worker for its own
//! bounded review of one checkout, so the client never re-bounds a diff it did
//! not compute. `project worktree integrate` fast-forwards the parent with the
//! task's result under the revision the checkout was created from (or
//! `--expected-parent-revision`); the worker keeps its ancestor and conflict
//! rules, and the client sends exactly one integration request, so a conflict is
//! reported as the failure it is rather than retried, and repeating the command
//! with the same `--request-id` is answered from the durable cache instead of
//! integrating twice. Every one of them resolves
//! the project and the manager session from the snapshot by `--task`.
//!
//! `node` prints one line per field of `WorkerNodeStatus`, with absent health
//! samples as `unknown`. `providers list` prints one line per provider with the
//! labeled fields `id`, `kind`, `credential`, and `api-key-configurable`, and
//! never a secret or a credential reference. `models list` prints one line per
//! model as `provider=<provider-id> model=<model-id> default=<true|false>` for
//! every configured provider, or for the one `--provider` id, and marks exactly
//! one default per provider: the provider kind's declared default when that
//! model is present, otherwise the first model in sorted order. A `--provider`
//! id the worker does not have is an empty list with exit `0`, not an error.
//! Its `--json` payload is `provider` (the filter or `null`) and `models`, whose
//! entries carry `provider`, `id`, `display_name`, `default`, `context_window`,
//! and `max_output_tokens`.
//!
//! `providers set-key` reads the key from `--from-env` or
//! `--key-file` and sends `Provider/ConfigureApiKeyProvider`; the worker stores
//! the secret, and the command prints only `provider <id> credential set`.
//! `retention get` prints `window`, `force-discard`, and
//! `sub-agent`, with a disabled window as `off`. `retention set` merges the
//! flags it was given into the policy the worker already stores (a flag that is
//! absent keeps its stored value), and reports the stored result followed by
//! `stored for this worker; the next sweep applies it`: changing the policy
//! never starts a sweep, and a session already archived is deleted by that next
//! round rather than by the command.
//!
//! `delete` reads `Project/GetProjectSnapshotForSession` before it sends the
//! mutation, because the id alone does not say whether it names a project root
//! that cascades. A root with another member or a delegated task removes its
//! whole tree, so `--yes` is required when that tree is larger than the target
//! itself; `--force` requires `--yes` as well, because it discards dirty or
//! locked worktrees. Both checks happen before `Session/DeleteAgentSession` is
//! sent. The worker remains the authority on which session states may be
//! deleted: a non-archived session that is not a project root is refused with
//! `invalid_state` (exit `5`) and nothing changes.
//!
//! # Projects and child agents
//!
//! The `project` verbs expose the project and child-agent lifecycle that the
//! worker already stores, alongside the project-scoped `archive`, `delete`, and
//! `worktree cleanup` described above. Every project read is addressed by the id
//! of any project member session: a project root's session id is also its
//! project id, and a child's id resolves to the project it belongs to, so the
//! same command works from the root and from a child. `project get` and
//! `project tree` report the members, `project tasks` the delegated tasks,
//! `project children` the direct children of the addressed session,
//! `project messages` one page of the project's agent message history, and
//! `project send` a direction to a delegated child's active run. Those reads
//! negotiate `read_project`, `project children` also `read_agent_session`, and
//! `project messages` also `read_project_agent_messages`, so a
//! read-only-scoped token can run them.
//!
//! The project id every later request needs comes from
//! `Project/GetProjectSnapshotForSession`, which is the authoritative projection
//! of the hierarchy and its tasks. `project messages` pages
//! `Project/ListProjectAgentMessages` with `--after` as
//! `after_project_sequence` and `--limit` (1 to 512, 100 by default) as the page
//! size, and prints a final `next <sequence>` line only when the worker sets
//! `next_after_project_sequence`. Message history is read-only here, and
//! `project send` is not a message: an agent-attributed message is reserved for
//! the server-bound agent tool, and the worker refuses
//! `Project/SendProjectAgentMessage` for every client connection, so a person
//! directs a live child through `Collaboration/DirectAgent` instead.
//!
//! `project send <session-id> (--text <text> | --text-file <path>)` reads one
//! `Project/GetProjectSnapshotForSession` and requires the session to be the
//! target of a task in it, otherwise it is a usage error naming `project
//! children`. It then reads the child's
//! `Session/GetAgentSessionSnapshotMetadata` and directs the run that metadata
//! reports through the same `Collaboration/GetRunRevision` and
//! `Collaboration/DirectAgent` exchange as `send`: the same
//! `sent <run-id> <state>` text, the same `action`, `run`, and `revision`
//! payload, and the same single retry after a `Conflict`. A child whose session
//! reports no active run is `invalid_state` (exit `5`) with the metadata
//! request id, and no direction is sent. It negotiates `read_project`,
//! `read_agent_session`, `read_agent_run`, and `control_agent_run`.
//!
//! `project child pause|resume|interrupt|cancel` maps the sub-verb onto
//! `Project/ControlProjectChild` as `Pause`, `Continue`, `Interrupt`, and
//! `Cancel`. The manager session id is never taken from a flag: the task is
//! looked up in the snapshot, and its `requester_session_id` is sent as the
//! manager, because only the server-owned task record says which run owns the
//! child. A task id the project does not name is reported as `not_found` (exit
//! `5`) from the snapshot, without a control request. A worker refusal keeps its
//! own mapping: a denied capability is exit `3` and an invalid child state is
//! exit `5`. `--request-id` keys the control request alone while the snapshot
//! pre-read keeps a fresh id; there is no retry, so one key never names a second
//! request.
//!
//! `run` starts a run, and it is described here because its requests are the
//! longest sequence the client sends:
//!
//! Its model is `--model` when the caller named one. Otherwise it sends the same
//! `Provider/ListProviders` read that `providers list` sends and starts the
//! first provider, in the order the worker returned them, that has a credential
//! and a known default model, selected as
//! `loom:model:v1:["<provider-id>","<model>"]` - a selection, not a raw id, so
//! the worker cannot resolve the model to another provider. With no such
//! provider it warns on stderr and starts `deterministic/demo`, and a worker
//! that refuses the catalog read as a missing capability falls back the same
//! way, because a run with the demo model is better than no run; a transport
//! failure is never caught.
//!
//! 1. `Workspace/ListWorkspaceSessions { include_archived: false }` resolves the
//!    workspace's live sessions. The first session whose name equals the
//!    resolved name (`--name`, or one derived from the task) is reused;
//!    otherwise the session is created with
//!    `Workspace/CreateAgentSessionInWorkspace`. `--new` skips the listing
//!    entirely and always creates a session, so a name is a label rather than
//!    an identity.
//! 2. The inputs are attached **only when this invocation created the session**:
//!    `--repo` sends `Repository/AttachSessionRepository` and `--as-directory`
//!    sends `Filesystem/AttachSessionDirectory`. A replay of the same command
//!    therefore neither duplicates an attachment nor fails on one that already
//!    exists.
//! 3. `Session/SetSessionApprovalPolicy` pins `--approval-policy` before the run
//!    starts, because a run resolves its policy once, when it is created. A
//!    session this invocation creates is pinned to the requested preset, or the
//!    default one when the flag is absent; a reused session is only changed when
//!    the flag was given, so a plain run never overwrites a person's choice.
//! 4. `Run/StartSessionAgentRunWithOptions` starts the run under the caller's
//!    `--request-id` (or a freshly generated id), so repeating the same id is
//!    answered from the worker's durable idempotency cache with the same run
//!    instead of starting a second one. The idempotency key is the run start
//!    alone; the session creation, the attachments, and the policy change use
//!    fresh ids.
//!
//! The verb returns as soon as `AgentRunStarted` arrives: it never polls and
//! never waits for the run, and `events --follow` is the verb that watches one.
//! Its text output is exactly two lines, `session <session-id> <session-name>`
//! and `run <run-id> <run-state>`, and it prints neither when the run did not
//! start.
//!
//! # Control verbs
//!
//! `send`, `answer`, `approve`, and `deny` read `Collaboration/GetRunRevision`,
//! send their collaboration request with that revision, and on `Conflict`
//! re-read once and retry, because their intent is idempotent. `project send` is
//! the same exchange with a delegated child's run, so it keeps the same output,
//! payload, and single retry. `stop` sends
//! `Interrupt` and refuses the same conflict unless `--retry-on-conflict` is
//! given, so a stale revision cannot interrupt a run the caller has not seen.
//! A conflict that survives the allowed retry is reported with the protocol's
//! `conflict` code, which maps to exit `1` because it is neither a usage error
//! nor a missing resource. A key never turns a conflict into a success: the
//! re-read revision makes the retry a different request, so it is sent as a new
//! one, a command the worker already answered under that key is refused as a
//! key conflict rather than replayed, and the run still refuses what it would
//! refuse without a key.
//!
//! `wait` polls `Session/GetAgentSessionSnapshotMetadata`, and reads
//! `Collaboration/GetRunRevision` once it has something to report. It is always
//! time-bounded: `--timeout` (60 seconds by default) ends the poll with
//! `deadline_exceeded` and exit `4`. A run parked on an approval or a question
//! is reported at once with exit `3`, and the `--json` envelope carries the
//! attempt id and, for an approval, the tool call id. Any terminal state is
//! reported at once with exit `0`, whether the run completed, failed, or was
//! cancelled: `data.state` and `terminal: true` name the outcome. The timeout
//! therefore covers only a run that is still working, never a person's decision.
//!
//! `status` derives its declaration from the run state: `working` for planning,
//! executing, and evaluating, `blocked` for awaiting approval and needs input,
//! `paused`, `done` for completed, and `failed` for failed and cancelled. The
//! line is derived by this client from the run state; no worker writes it.
//!
//! `status --tree` additionally projects the agent session's project: the
//! project verdict, its agents with a verdict each, its delegated tasks, its
//! worktrees with their conflict paths, and one decision per agent waiting on an
//! approval or a question. The records come from
//! `Project/GetProjectSnapshotForSession` unchanged; this client derives the
//! verdict words, which are the `status` word set plus `archived`, and reads the
//! metadata of every agent whose project record is `awaiting_approval` or
//! `needs_input` to learn which decision it waits on. The requested session
//! reuses its own projection, so its pending decision appears once as a
//! `decision` line instead of the plain `pending` line.
//!
//! `liveness` never guesses: an archived or terminal session is `dead`, a
//! session the worker does not know is `gone` with exit `5`, a transport
//! failure is `unknown` with exit `7`, and anything else is `alive`.
//!
//! # `loom-text/1`
//!
//! Text output is a byte-stable projection: one physical line per event,
//! `<seq> <kind> <body>\n`. `<seq>` is the decimal `EventSequence`, `<kind>` is
//! a fixed lowercase word, and `<body>` is escaped so it can never contain a
//! raw newline: `\\`, `\n`, `\r`, and `\t` become their two-character
//! escapes, other C0 controls and `0x7f` become `\xNN`, and every other UTF-8
//! byte is passed through unchanged. An empty body leaves no trailing space, so
//! the line is `<seq> <kind>\n`.
//!
//! Fields of a body that are empty are omitted, which is how
//! `run_completed` reports a run without a summary. `tool_done` carries the
//! outcome word and, when the tool produced one, its bounded output.
//!
//! Text bounding: a string longer than the byte budget keeps its first and last
//! half, rounded to UTF-8 character boundaries, with the exact marker
//! `[... N bytes omitted ...]` between them, where N is the number of omitted
//! bytes. The budget of a verb's `--max-bytes` applies to tool output, tool
//! arguments, and tool results, so one line never grows unbounded with a tool
//! payload. Bounding runs on the raw text before escaping, so the omitted count
//! is a count of the original bytes. Replaying the same events with the same
//! budget produces identical bytes.
//!
//! Nothing in the projection uses Rust's `Debug` formatting: every variant
//! name is an explicit match arm spelled exactly like the protocol's snake_case
//! serde name, so a wire change cannot silently change the text contract.
//!
//! # `--json`
//!
//! `--json` prints exactly one envelope on stdout. Success is
//! `{"ok":true,"verb":...,"request_id":...,"replayed":...,"data":...}` with
//! the requested payload in `data`; failure is
//! `{"ok":false,"verb":...,"request_id":...,"error":{"code":...,"message":...,"retryable":...,"exit_code":...}}`.
//! `request_id` is the id of the verb's primary request (its first request, or
//! its handshake), or `null` when no request was sent. `run` reports its run
//! start instead, because that is the id its caller chose and the key a replay
//! reuses. `replayed` is `true` only when the worker answered a mutating
//! request of this invocation from its idempotency cache instead of acting
//! again, which happens only when the caller repeated a `--request-id`; it is
//! how a repeated archive, deletion, integration, cleanup, child control, or
//! child direction reports that the mutation was applied once. Every other
//! success reports `false`, and a failure envelope has no such field.
//! `events` adds `gap` (`after`, `before`, `missing`) when a snapshot
//! could not carry every event after the cursor, and `null` otherwise.
//!
//! `events --format jsonl` and `peek --format jsonl` are a different shape:
//! without `--json` they print one object per event,
//! `{"seq":...,"stream_epoch":...,"session_id":...,"kind":...,"text":...,"event":...}`,
//! where `kind` and `text` are exactly the text projection of that event.
//!
//! A verb whose documented exit code reports something other than success is
//! still a successful determination: a blocked `wait` is `ok:true` with exit
//! `3`, a terminal `wait` is `ok:true` with exit `0` whatever the run outcome,
//! a `json` `events --follow` that reached its deadline is `ok:true` with exit
//! `4`, and a `liveness` verdict of `gone` or `unknown` is `ok:true` with exit
//! `5` or `7`. The `data` payload carries the verdict, state, attempt id, and
//! tool call id, so a caller never has to parse the message.
//!
//! # Exit codes
//!
//! `0` ok, `1` internal, `2` usage, `3` blocked (`approval_required`,
//! `capability_denied`, `authorization_denied`, `workspace_access_denied`), `4`
//! timeout (`deadline_exceeded`, `backpressure`), `5` not found or gone
//! (`not_found`, `invalid_state`), `6` protocol or authentication mismatch
//! (`unsupported_protocol`, `authentication_required`,
//! `authentication_failed`), and `7` transport unreachable, meaning a
//! connection or transport failure before or during a request. Every other
//! error maps to `1`.
//!
//! # Cursor file
//!
//! `events --follow --timeout <seconds>` bounds the follow. The cursor file is
//! written after every fully rendered batch, so when the bound elapses the last
//! delivered sequence is already on disk: the verb stops and the same command
//! with the same cursor file resumes exactly there. Text and `--format jsonl`
//! stream every event as it arrives, so they report the elapsed bound as the
//! `deadline_exceeded` failure with exit `4`. `--json` buffers the events, so a
//! bounded follow that reaches its deadline reports `ok: true` with
//! `data.timed_out: true`, the events it delivered, and exit `4`, rather than a
//! failure that would drop events the cursor file already points past. The
//! deadline is retryable either way. Without `--timeout` a follow is unbounded,
//! because it still stops at the run's `run_completed` event, and `--timeout`
//! without `--follow` is a usage error.
//!
//! `events --cursor-file <path>` keeps
//! `{"version":1,"url":...,"session_id":...,"after":...,"stream_epoch":...}`
//! so a later run resumes without reprinting history. A cursor written for a
//! different URL or session is ignored. The file is replaced atomically (a
//! temporary file in the same directory, then a rename) after every fully
//! rendered batch. An event whose sequence is at or below the persisted cursor
//! is never printed; a `SessionEventsSnapshot` response prints the window it
//! carries, reports any range between the cursor and that window as `gap` (on
//! stderr and in the `--json` envelope) because a snapshot cannot page it back,
//! and then persists `latest_sequence` so a later run resumes at the head; and a
//! changed `stream_epoch` discards the stored cursor first, because a new epoch
//! means the sequences no longer name the same events.
//!
//! # Token handling
//!
//! The bearer token may only come from a file: `--token-file <path>` or
//! `LOOM_TOKEN_FILE`. `--token` is rejected as a usage error that says so,
//! because a token on a command line is visible to every process on the node.
//! No diagnostic, envelope, or log line ever carries the token.
//!
//! The worker URL is `--connect <url>`, then `LOOM_URL`, then
//! `ws://127.0.0.1:8765/ws`, so a local worker needs no flags at all. When neither
//! `--token-file` nor `LOOM_TOKEN_FILE` names a file, the token path is derived
//! from that URL as `<state directory>/<host>_<port>/token`, which is exactly
//! where a standalone worker stores the token of the instance bound to that
//! address: the state directory honours `LOOM_STATE_DIR`, then
//! `XDG_STATE_HOME`, then `$HOME/.local/state`, and the host and port key is the
//! worker's own bind key (`ws` defaults to port 80 and `wss` to 443). A leading
//! `~/` in a named path is expanded with `HOME`. A token read failure names the
//! path that was tried and, for a derived path, the URL it was derived for, and
//! it is a usage error with exit `2` like every other local failure of the
//! command line.

mod args;
mod client;
mod cursor;
mod exit;
mod text;
mod verbs;

#[cfg(test)]
mod tests;

use std::io::Write;

pub use client::{Client, RemoteClient};

use args::{Invocation, Parsed};

/// Entry point of the `loom-cli` binary: parses the arguments, connects, runs
/// the verb, and maps every outcome to an exit code. A usage error never
/// panics on user input.
pub fn main(arguments: Vec<String>) -> i32 {
    let json = args::wants_json(&arguments);
    let mut invocation = match args::parse(&arguments) {
        Ok(Parsed::Help(text)) => return print_help(text),
        Ok(Parsed::Run(invocation)) => invocation,
        Err(message) => return report_usage_stdio(&message, json, args::named_verb(&arguments)),
    };
    // The environment is consulted exactly once, here, and the result is stored
    // on the invocation so the URL a cursor file is scoped to is the one the
    // connection uses. Parsing itself stays a pure function of its arguments.
    let url = args::resolve_connect(
        invocation.globals.connect.take(),
        std::env::var("LOOM_URL").ok(),
    );
    let state_directory = loom_core::state_dir();
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let token_file = match args::resolve_token_file(
        invocation.globals.token_file.clone(),
        std::env::var_os("LOOM_TOKEN_FILE").map(std::path::PathBuf::from),
        home.as_deref(),
        &state_directory,
        &url,
    ) {
        Ok(token_file) => token_file,
        Err(message) => {
            return report_usage_stdio(&message, json, args::named_verb(&arguments));
        }
    };
    invocation.globals.connect = Some(url.clone());
    invocation.globals.token_file = Some(token_file.path.clone());
    let settings = &invocation.globals;
    let token = match std::fs::read_to_string(&token_file.path) {
        Ok(token) => token.trim().to_owned(),
        Err(error) => {
            // The message names the path that was tried, and, when the caller
            // named no file, the URL the default was derived from.
            let message = match &token_file.derived_for {
                Some(worker) => format!(
                    "could not read the token file '{}' (default for {worker}): {error}",
                    token_file.path.display()
                ),
                None => format!(
                    "could not read the token file '{}': {error}",
                    token_file.path.display()
                ),
            };
            return report_usage_stdio(&message, settings.json, args::verb_name(&invocation.verb));
        }
    };
    if token.is_empty() {
        return report_usage_stdio(
            "the token file is empty",
            settings.json,
            args::verb_name(&invocation.verb),
        );
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let mut out = std::io::stdout();
            let mut err = std::io::stderr();
            let failure =
                verbs::Failure::Internal(format!("could not start the async runtime: {error}"));
            return report_failure(
                &failure,
                settings.json,
                args::verb_name(&invocation.verb),
                &mut out,
                &mut err,
            );
        }
    };
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = runtime.block_on(async {
        let mut transport = loom_local::WebSocketTransport::new(url, token)
            .with_insecure_remote_allowed(settings.allow_insecure_remote);
        if let Some(max_frame_bytes) = settings.max_frame_bytes {
            transport = match transport.with_max_frame_bytes(max_frame_bytes) {
                Ok(transport) => transport,
                Err(error) => {
                    return report_failure(
                        &verbs::Failure::Usage(error.message),
                        settings.json,
                        args::verb_name(&invocation.verb),
                        &mut out,
                        &mut err,
                    );
                }
            };
        }
        if let Some(ca_file) = &settings.ca_file {
            transport = match transport.with_ca_certificate_file(ca_file) {
                Ok(transport) => transport,
                Err(error) => {
                    return report_failure(
                        &verbs::Failure::Usage(error.message),
                        settings.json,
                        args::verb_name(&invocation.verb),
                        &mut out,
                        &mut err,
                    );
                }
            };
        }
        match transport.connect().await {
            Ok(connection) => {
                let mut client = RemoteClient::new(connection);
                // The verb runs through the same entry point the tests use, so
                // there is one execution path for both, and the arguments were
                // parsed once, so a task file is read once.
                run_parsed(&mut client, &invocation, &mut out, &mut err).await
            }
            // The worker was never reached: an authentication refusal is a
            // protocol or auth mismatch, and anything else is unreachable.
            Err(error) => {
                let code = exit::for_connect_error(&error);
                report_transport(
                    &error,
                    code,
                    settings.json,
                    args::verb_name(&invocation.verb),
                    &mut out,
                    &mut err,
                )
            }
        }
    });
    let _ = out.flush();
    let _ = err.flush();
    code
}

/// Runs one verb against an already connected client, returning its exit code
/// without ever exiting the process. This is the seam the tests drive with an
/// in-process backend; the binary parses once and calls [`run_parsed`].
#[cfg(test)]
pub async fn run_command<C: Client>(
    client: &mut C,
    arguments: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let json = args::wants_json(arguments);
    match args::parse(arguments) {
        Ok(Parsed::Help(text)) => {
            let _ = out.write_all(text.as_bytes());
            exit::OK
        }
        Ok(Parsed::Run(invocation)) => run_parsed(client, &invocation, out, err).await,
        Err(message) => report_usage(&message, json, args::named_verb(arguments), out, err),
    }
}

/// Runs an already parsed invocation, so a caller that needed the parsed flags
/// before connecting does not parse (and read a task file) twice.
pub async fn run_parsed<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    run_invocation(client, invocation, out, err).await
}

async fn run_invocation<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let json = invocation.globals.json;
    let verb = args::verb_name(&invocation.verb);
    match verbs::run(client, invocation, out, err).await {
        Ok(outcome) => {
            if json {
                let envelope = serde_json::json!({
                    "ok": true,
                    "verb": verb,
                    "request_id": outcome.request_id.map(|id| id.to_string()),
                    "replayed": outcome.replayed,
                    "data": outcome.data,
                });
                write_json(out, &envelope);
            }
            outcome.exit_code
        }
        Err(failure) => report_failure(&failure, json, verb, out, err),
    }
}

fn print_help(text: &str) -> i32 {
    let mut out = std::io::stdout();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
    exit::OK
}

/// Reports a usage error, in the JSON envelope when `--json` was asked for and
/// on stderr otherwise.
fn report_usage(
    message: &str,
    json: bool,
    verb: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    report_failure(
        &verbs::Failure::Usage(message.to_owned()),
        json,
        verb,
        out,
        err,
    )
}

/// Reports a usage error found before any client exists, on the process streams.
fn report_usage_stdio(message: &str, json: bool, verb: &str) -> i32 {
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    let code = report_usage(message, json, verb, &mut out, &mut err);
    let _ = out.flush();
    let _ = err.flush();
    code
}

fn report_failure(
    failure: &verbs::Failure,
    json: bool,
    verb: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // A reader that closed the pipe is not a failure and has nowhere to read a
    // report, so stop quietly with the successful code.
    if matches!(failure, verbs::Failure::BrokenPipe) {
        return exit::OK;
    }
    let code = failure.exit_code();
    if json {
        write_json(out, &failure.envelope(verb));
    } else {
        let _ = writeln!(err, "error {} {}", failure.code(), failure.message());
    }
    code
}

fn report_transport(
    error: &loom_core::LoomError,
    code: i32,
    json: bool,
    verb: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if json {
        write_json(
            out,
            &verbs::Failure::Transport {
                request_id: None,
                error: error.clone(),
            }
            .envelope_with_code(verb, code),
        );
    } else {
        let _ = writeln!(err, "error {} {}", error.code, error.message);
    }
    code
}

pub(crate) fn write_json(out: &mut dyn Write, value: &serde_json::Value) {
    let _ = writeln!(out, "{value}");
}
