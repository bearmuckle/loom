# Headless client (`loom-cli`)

`loom-cli` is the headless binary of the `crates/loom-cli` package and a pure
client of the versioned protocol: it embeds no backend, opens no SQLite
database, and manages no worker. The package ships it beside the `loom` demo
shell:

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
| `run --workspace <id> (--task <text> \| --task-file <path>) [--name <name>] [--model <id>] [--repo <source> [--revision <rev>] [--path <path>]] [--as-directory <path>] [--approval-policy default\|auto] [--request-id <uuid>]` | Reuse or create the session named `--name`, attach the inputs, and start one run. |

`ls`, `get`, `events`, and `peek` are read-only, issue no mutation, and work
with a read-only-scoped token. `run` is the only verb in this slice that changes
worker state, and no verb waits for a run to finish.

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
```
