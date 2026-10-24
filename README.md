# Loom

Loom is a remote-first coding-agent environment. A Rust backend owns agent
sessions, tool execution, workspace state, and model integrations; a GPUI client
runs natively or in the browser and attaches to a local or remote backend.

Loom is pre-1.0, so interfaces, configuration, and on-disk state can still
change between releases. Review the [security and trust model](docs/security.md)
before using it with sensitive data or exposing it beyond a trusted boundary.

Try the deterministic demo, which needs no provider or backend:
https://bearmuckle.github.io/loom/?demo=true

## What you get

- One GPUI client for native desktop and the browser.
- Backend-owned sessions that survive disconnects, reconnects, and restarts.
- Approval-gated tools with workspace, diff, and task review.
- Providers: OpenAI, DeepSeek, OpenAI-compatible endpoints, Ollama, and GitHub
  Copilot login.
- Coordinated child agents on isolated worktrees.
- A durable project goal, set or read with `/goal`, that the project manager is
  told on every run.

## Build and run

Install Rust 1.99 or newer. On Linux, the native UI also needs the system
packages listed in [CI](.github/workflows/ci.yml).

```sh
cargo run -p loom-ui
```

Loom uses the directory you launch it from. Point it at another project with
`--project PATH`, or start the provider-free demo with `--demo`:

```sh
cargo run -p loom-ui -- --project /path/to/project
cargo run -p loom-ui -- --demo
```

For browser development, run `./scripts/dev-wasm.sh`. It needs Trunk and the
`wasm32-unknown-unknown` Rust target, serves both services on loopback, and puts
a development bearer token in the browser URL, so keep it local. Pushes to
`main` publish the browser client to https://bearmuckle.github.io/loom/.

## Standalone server

Tagged releases and the container image also ship `loom-server`, the backend the
client embeds, as a standalone process. It needs no `--persistence` path or
token: it generates a bearer token in its instance directory and keeps durable
state below the state root, so a stable bind address resumes where it left off.

```sh
tar -xzf loom-server-<tag>-linux-x86_64.tar.gz && chmod +x loom-server
./loom-server --bind 127.0.0.1:8765
```

A non-loopback listener requires TLS (`--tls-cert` and `--tls-key`) or the
explicit `--allow-insecure-remote` opt-in. See [deployment](docs/deployment.md)
for the state layout, token handling, retention, TLS, egress, and the container
image:

```sh
docker run --rm -p 127.0.0.1:8765:8765 ghcr.io/<owner>/loom-server:<tag>
```

`--archive-retention <duration>` (or `LOOM_ARCHIVE_RETENTION`), with
`--archive-retention-force-discard`, sets a startup default that overrides the
policy stored in the worker's durable state and is stored in turn: a worker
started without the flag resumes the policy it stored, and a client can change
the window and the force-discard switch at runtime through
`SetArchiveRetentionPolicy`. Retention is disabled when no flag, variable, or
stored policy enables it. Archived sub-agent sessions have their own window,
`--archive-retention-sub-agent <duration>` (or `LOOM_ARCHIVE_RETENTION_SUB_AGENT`),
defaulting to 10 minutes, applied even when general retention is off.
[deployment](docs/deployment.md) documents the
duration syntax and the sweep.

## Headless client

`loom-cli` is the headless binary of the `crates/loom-cli` package and a pure
client of the versioned protocol: it embeds no backend, opens no SQLite
database, and manages no worker. The package ships it beside the `loom` demo
shell:

```sh
cargo run -p loom-cli --bin loom-cli -- doctor
```

Point it at a worker with `--connect <url>` (or `LOOM_URL`). The bearer token
comes only from `--token-file <path>` (or `LOOM_TOKEN_FILE`); there is
deliberately no `--token` flag, so a token never appears on a command line.
`--ca-file` adds TLS roots, and a plaintext `ws://` URL to a non-loopback host
needs the explicit `--allow-insecure-remote` opt-in.

| Verb | Purpose |
| --- | --- |
| `doctor` | Negotiated protocol, capability intersection, participant, and worker node. |
| `whoami` | The participant bound to the token. |
| `ls [--workspace <id>]` | Workspaces, or the sessions of one workspace. |
| `get <session-id> [--metadata]` | One session snapshot, or only its metadata. |
| `events --session <id> [--after <seq>] [--cursor-file <path>] [--follow] [--format text\|jsonl] [--max-bytes <n>]` | Session events, followed until the run completes or the caller interrupts. |
| `peek --session <id> [--lines <n>] [--format text\|jsonl] [--max-bytes <n>]` | The tail of the session's current output; 200 lines by default. |
| `run --workspace <id> (--task <text> \| --task-file <path>) [--name <name>] [--model <id>] [--repo <source> [--revision <rev>] [--path <path>]] [--as-directory <path>] [--approval-policy default\|auto] [--request-id <uuid>]` | Reuse or create the session named `--name`, attach the inputs, set the approval policy, and start one run. |

`events` and `peek` bound the tool payload of each rendered event to 4096 bytes
by default; `--max-bytes` changes that cap.

`--format text` is the default and is the documented, byte-stable `loom-text/1`
projection: one line per event as `<seq> <kind> <body>`, with the body escaped
so it never contains a raw newline, both ends of oversized tool output kept with
an exact `[... N bytes omitted ...]` marker, and explicit `pending_approval` and
`question` markers for the states that block a run. The same journal plus the
same cursor produces identical bytes.

`--format jsonl` writes one JSON object per line that carries `seq` and
`stream_epoch`, so a caller can resume. `--json` prints one envelope containing
the request id the call used.

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

`ls`, `get`, `events`, and `peek` are read-only and work with a
read-only-scoped token. `run` starts a run and prints its ids; no verb in this
slice waits for a run to finish.

`run` is the one mutation. It reuses the workspace session whose name matches
`--name` (or a name derived from the task) and otherwise creates it, attaches
`--repo` or `--as-directory` only when it created the session, and pins
`--approval-policy` before the run starts. `--request-id` is the idempotency key
of the run start, so repeating the same command returns the same run. A worker
accepts a run only in an idle session, so reusing a session that already ran
reports `invalid_state` (exit `5`) and creates nothing.

## Providers

Configure OpenAI, DeepSeek, or GitHub Copilot in the Providers dialog; the
default models are `gpt-6-luna` and `deepseek-flash`, overridable with
`LOOM_OPENAI_MODEL` and `LOOM_DEEPSEEK_MODEL`. For an OpenAI-compatible gateway,
set `LOOM_OPENAI_ENDPOINT`, `LOOM_API_KEY`, and `LOOM_MODEL`. API keys are stored
in a credential file beside the backend's SQLite database and are used by the
backend; treat them as sensitive and note that state storage is not an encrypted
vault.

## Documentation

- [Architecture](docs/architecture.md)
- [Deployment](docs/deployment.md)
- [Security and trust model](docs/security.md)
- [Product scope](docs/product.md)
- [Protocol](docs/protocol.md)

## Development

Run the full Rust check suite with `./scripts/ci-check.sh`. To check formatting
on every commit, point Git at the bundled hooks:

```sh
git config core.hooksPath .githooks
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the workflow. Report security issues
privately per [SECURITY.md](SECURITY.md).

## License

Individual crates declare their license in their `Cargo.toml`. The reusable core
crates are GPL-3.0-only; `loom-protocol` and `loom-server` are AGPL-3.0-only.
The `loom-ui` and `loom-cli` applications depend on the AGPL-licensed protocol,
so Loom application distributions are offered under AGPL-3.0-only. See
[LICENSES.md](LICENSES.md) and [COPYRIGHT](COPYRIGHT) for details.
