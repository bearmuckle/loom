# Headless client (`loom-cli`)

`loom-cli` is the headless binary of the `crates/loom-cli` package and a pure
client of the versioned protocol: it embeds no backend, opens no SQLite
database, and manages no worker, and the package does not depend on the
backend crates. The `loom` demo shell lives in the separate `loom-shell`
package:

```sh
cargo run -p loom-cli --bin loom-cli -- doctor
```

The examples below use the installed binary name. In a checkout, replace
`loom-cli` with `cargo run -p loom-cli --bin loom-cli --`.

## Connecting

- `--connect <url>` (or `LOOM_URL`) names the worker WebSocket endpoint.
- `--token-file <path>` (or `LOOM_TOKEN_FILE`) is the only credential path.
  There is deliberately no `--token` flag, so a token never appears on a
  command line, and no diagnostic or output line carries it.
- `--ca-file <pem>` trusts an additional TLS certificate.
- `--allow-insecure-remote` permits plaintext `ws://` to a non-loopback host.
- `--max-frame-bytes <n>` sets the client's frame limit; the default is 4 MiB.
- `--json` prints one JSON envelope for a call instead of text.

## Verbs

| Verb | Purpose |
| --- | --- |
| `doctor` | Negotiated protocol, capability intersection, participant, and worker node. |
| `whoami` | The participant bound to the token. |
| `ls [--workspace <id>] [--archived]` | Workspaces, or the sessions of one workspace. |
| `get <session-id> [--metadata]` | One session snapshot, or that snapshot with its active run metadata. |
| `events --session <id> [--after <seq>] [--cursor-file <path>] [--follow] [--format text\|jsonl] [--max-bytes <n>]` | Session events, followed until the run completes or the caller interrupts. |
| `peek --session <id> [--lines <n>] [--format text\|jsonl] [--max-bytes <n>]` | The tail of the session's current output; 200 lines by default. |
| `run --workspace <id> (--task <text> \| --task-file <path>) [--name <name>] [--model <id>] [--repo <source> [--revision <rev>] [--path <path>]] [--as-directory <path>] [--approval-policy default\|auto] [--request-id <uuid>] [--new]` | Reuse or create the session named `--name`, attach the inputs, and start one run. `--new` always creates a session. |
| `send --run <id> --text <text>` | Follow-up direction to the active run. |
| `answer --run <id> --text <text>` | Answer the question the run is waiting on. |
| `approve --run <id> --tool-call <id>` | Approve the tool call that blocked the run. |
| `deny --run <id> --tool-call <id> [--reason <text>]` | Deny the blocked tool call. |
| `stop --run <id> [--session <id> --archive] [--retry-on-conflict]` | Interrupt the run, optionally archiving its session afterwards. |
| `wait --session <id> [--until blocked\|terminal] [--timeout <seconds>]` | Poll until the run blocks or reaches a terminal state; 60 seconds by default. |
| `status --session <id> [--format text\|verb]` | The session and its latest run; `--format verb` prints one declaration line. |
| `liveness --session <id>` | `alive`, `dead`, `gone`, or `unknown`. |
| `result --session <id> [--task <id>]` | Branch, head, diffstat, changed paths, and task evidence. |

`ls`, `get`, `events`, and `peek` are read-only, issue no mutation, and work
with a read-only-scoped token. `run` starts a run; `send`, `answer`,
`approve`, `deny`, and `stop` control one; `wait`, `status`, `liveness`, and
`result` read one. No verb waits for a run except `wait`, which is always
bounded by `--timeout`.

Every verb negotiates only the capabilities its own requests need. `doctor`
reports the intersection the worker granted, so its capability list is the
smallest set for that verb rather than a client-wide catalogue.

## Output

### `text`

`--format text` is the default and is the documented, byte-stable `loom-text/1`
projection: one line per event as `<seq> <kind> <body>`, with the body escaped
so it never contains a raw newline, both ends of oversized tool output kept with
an exact `[... N bytes omitted ...]` marker, and explicit `pending_approval` and
`question` markers for the states that block a run. The same journal plus the
same cursor produces identical bytes.

`events` and `peek` bound the tool payload of each rendered event to 4096 bytes
by default; `--max-bytes` changes that cap.

### `jsonl`

`--format jsonl` writes one JSON object per line that carries `seq` and
`stream_epoch`, so a caller can resume. Each object also carries the rendered
`kind`/`text` and the full `event` payload.

### `--json`

`--json` prints one envelope per call:

```json
{"ok":true,"verb":"whoami","request_id":"…","data":{…}}
```

A failure uses the same envelope with `ok: false` and an `error` object carrying
`code`, `message`, `retryable`, and `exit_code`. `events` adds
`gap { after, before, missing }` to `data` when a snapshot could not carry every
event after the cursor, and `null` otherwise.

`--json` with `--follow` keeps every followed event in that one envelope, so
prefer `--format jsonl` for a long follow.

## Resume and the cursor file

`events` starts from `--after <seq>` when it is given, otherwise from the local
cursor file when `--cursor-file <path>` is used, otherwise from the current
retained window. The cursor records the URL, the session, the last rendered
sequence, and the backend epoch as JSON, and is replaced atomically (a unique
temporary file and a rename) after each rendered batch. A cursor written for
another URL or session is ignored.

A `SessionEventsSnapshot` cannot page: it carries only the newest part of the
retained window. When the cursor sits below the first sequence that window
carries, the events in between are gone from the response and cannot be fetched
again by this verb. `events` prints the window, reports the missing range as
`warning gap <first>..<last>` on stderr and as a `gap` field in its `--json`
envelope, and resumes at the head of the window rather than claiming a complete
suffix. A first read without a cursor has no gap; starting from the current
window is its documented behaviour.

A changed `stream_epoch` invalidates the stored cursor, because the sequence
numbers no longer name the same events. The client then treats the response in
hand as a snapshot.

The polling cadence, paging, snapshot replacement, and epoch rules are stated in
[the protocol document](protocol.md#headless-polling-client).

## Starting a run

`run` reuses the workspace session whose name matches `--name` (or a name
derived from the task) and otherwise creates it. It attaches `--repo` or
`--as-directory` only when it created the session, so a replay neither
duplicates an attachment nor fails on one that already exists.

The approval policy is set before the run starts, because a run resolves its
policy once when it is created. A created session is pinned to
`--approval-policy` (`default` when the flag is absent); a reused session is
only changed when the flag is given, so a plain run never overwrites a policy a
person set.

`--request-id` is the idempotency key of the run start, so repeating the same
command returns the same run instead of starting a second one. A worker accepts
a run only in an idle session, so reusing a session that already ran reports
`invalid_state` (exit `5`) and creates nothing.

### `run --new`

`--new` creates a session unconditionally. It never lists the workspace and
never matches a session by name, so a retrigger after a completed run does not
reach `invalid_state`. A session name is a label, not an identity: `--new` may
produce a second session with an existing name, and the printed session id and
the `created` flag are the identity. Pass a unique `--name` when the name is
meant to be a handle.

A `--new` command with `--request-id <base>` derives every mutation from that
base so the whole command is replayable: `<base>` starts the run and the
session creation and the input attachments use deterministic ids derived from
it. Repeating the command returns the same session and the same run, with
exactly one of each. The `--json` envelope reports both ids under
`data.request_ids` (`session` and `run`).

## Supervising a run

The control verbs act on a run id, and the tool call id they need comes from
the `pending_approval` event or from `wait`/`status`. They read
`GetRunRevision` first and send that revision with the request.

- `send`, `answer`, `approve`, and `deny` re-read once and retry when the
  worker answers `Conflict`, because their intent is idempotent. A conflict
  that survives the retry is reported with the protocol's `conflict` code
  (exit `1`).
- `stop` refuses a conflict by default, so a stale revision cannot interrupt a
  run whose state the caller has not seen. `--retry-on-conflict` allows the
  same single retry. A run that already finished is `invalid_state` (exit `5`)
  and nothing is archived.
- `stop --archive` archives the session only after the interrupt succeeded,
  and prints `archived <session-id>` after `stopped <run-id> <state>`.

`wait --until blocked` returns exit `3` as soon as the run is
`awaiting_approval` or `needs_input`, with the attempt id and, for an approval,
the tool call id in the `--json` envelope. It does not wait for a person to
decide. The timeout covers only a run that is still working, and a run parked
on a question or approval is likewise reported at once by `--until terminal`,
because it cannot reach a terminal state without a decision. `--until terminal`
exits `0` for `completed` and non-zero for `failed` and `cancelled`; the
envelope's `state` distinguishes them.

`status --format verb` prints the declaration grammar the rest of the ecosystem
uses: `working: …`, `blocked: …`, `paused: …`, `done: …`, or `failed: …`, each
followed by the run state and the run id. The word is derived by this client
from the run state; no worker writes it. A session with no run is
`done: idle -`.

`liveness` never guesses. A session that is archived or in a terminal session
state is `dead` (exit `0`), an id the worker does not know is `gone` (exit
`5`), and a transport failure is `unknown` (exit `7`), never `gone`.

`result` reports each attached repository's branch, head, diffstat (files,
added, removed), and changed paths, plus the evidence links of `--task` when it
is given. A session with no repository and no evidence prints
`nothing to report` and sets `empty` in the `--json` envelope.

## Exit codes

| Exit code | Meaning |
| --- | --- |
| `0` | ok |
| `1` | internal error |
| `2` | usage error |
| `3` | blocked by a pending approval or a missing capability |
| `4` | timeout |
| `5` | not found or gone |
| `6` | protocol or auth mismatch |
| `7` | transport unreachable |

A reader that closes stdout, as in `loom-cli events | head`, is a clean stop
with exit `0` rather than an error.

## Examples

```sh
# Check the worker and the negotiated contract.
loom-cli doctor

# List workspaces, then the sessions of one.
loom-cli ls
loom-cli ls --workspace <workspace-id>

# Follow a session from a persisted cursor.
loom-cli events --session <session-id> --cursor-file .loom-cursor --follow

# Read the tail of a session's output.
loom-cli peek --session <session-id> --lines 40

# Start one run from a task file and print its ids.
loom-cli run --workspace <workspace-id> --name release-check \
  --repo https://github.com/acme/widgets.git --task-file task.md \
  --request-id 0199f0f4-0000-7000-8000-000000000000

# Retrigger CI without colliding with the completed session of the same name.
loom-cli run --workspace <workspace-id> --task-file task.md --new \
  --request-id 0199f0f4-0000-7000-8000-000000000001

# Supervise a run: wait for the approval, decide it, wait for the end.
loom-cli wait --session <session-id> --until blocked --timeout 30
loom-cli approve --run <run-id> --tool-call <tool-call-id>
loom-cli wait --session <session-id> --until terminal --timeout 600
loom-cli result --session <session-id>
```
