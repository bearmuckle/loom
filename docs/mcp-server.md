# MCP control plane (`loom-mcp`)

`loom-mcp` is an MCP server that mirrors `loom-cli` over streamable HTTP, with a
stdio transport for a local MCP host. It is a client of the existing versioned
WebSocket protocol: it embeds no backend, opens no SQLite database, and changes
nothing about the worker or its auto-update path. Each MCP tool call runs one
`loom-cli` verb through the same client seam `loom-cli` itself uses, so
behaviour and exit meanings match [`docs/headless-client.md`](headless-client.md)
byte for byte, including the no-secrets rule.

## What it is not

- It is not a second backend and not a second protocol. The worker's WebSocket
  surface stays the single control plane; this server is one more client of it,
  like `loom-cli` and `loom-ui`.
- It does not replace `loom-cli` or the worker WebSocket.
- Destructive verbs are out of the first slice: `session delete`/`delete`,
  `retention set`, `providers set-key`, and `providers remove` are not exposed.

## Run

```sh
cargo run -p loom-mcp --bin loom-mcp -- \
  --connect ws://127.0.0.1:8765/ws --token-file ~/.config/loom/token
```

The worker URL and token file resolve exactly like `loom-cli`: `--connect` /
`LOOM_URL` (default `ws://127.0.0.1:8765/ws`), and `--token-file` /
`LOOM_TOKEN_FILE` (default `<state dir>/<host>_<port>/token`). There is
deliberately no `--token` flag, so the secret never appears on a command line.

## Transport

- **Streamable HTTP** (default): a stateless `POST /mcp` JSON-RPC endpoint.
  Responses are `application/json`; there are no server-initiated
  notifications, so a client polls `status` and the event resources the way
  `loom-cli` documents. MCP notifications for block/terminal state are left for
  a later slice.
- **stdio**: `--stdio` serves newline-delimited JSON-RPC on stdin/stdout for a
  local MCP host. Stdio trusts the local caller, because a process that can
  launch the binary can already read the token file; the bearer check belongs
  to the HTTP listener.

## Bind and TLS

The default bind is loopback, `127.0.0.1:8766`. A non-loopback bind is an
explicit operator decision and requires either TLS or an explicit plaintext
opt-in:

```sh
# Tailnet / WireGuard address (CGNAT range), with TLS:
loom-mcp --bind 100.64.0.1:8766 --tls-cert cert.pem --tls-key key.pem \
  --connect wss://worker.example/ws --token-file token

# Tailnet / WireGuard address, plaintext on a trusted overlay only:
loom-mcp --bind 100.64.0.1:8766 --allow-insecure-remote \
  --connect ws://100.64.0.2:8765/ws --token-file token
```

`--tls-cert` and `--tls-key` have the same shape as the worker and must be given
together. `--allow-insecure-remote` logs a warning and also permits a plaintext
`ws://` connection to a non-loopback worker. The unspecified address
(`0.0.0.0`/`::`) is never a silent default and follows the same non-loopback
rule. To bind a specific overlay interface, bind its address rather than
`0.0.0.0`; `ip -brief addr` shows the interface addresses.

## Auth and secrets

The only credential is the operator bearer token from the same `--token-file`
path `loom-cli` uses. No second, weaker secret is minted.

- HTTP clients present `Authorization: Bearer <token>` on `/mcp`; the listener
  compares it in constant time against the operator token and returns `401`
  otherwise. `/health` stays open.
- The same token authenticates the worker WebSocket connection.
- The token and provider API keys are never printed, logged, returned in a tool
  result or resource, or named in a tool schema. `providers list` and
  `models list` print no secrets.

## Tools

One tool per first-slice verb:

| Tool | `loom-cli` verb | Mutating |
| --- | --- | --- |
| `ls` | `ls` | no |
| `status` | `status` | no |
| `run` | `run` | yes |
| `send` | `send` | yes |
| `approve` | `approve` | yes |
| `deny` | `deny` | yes |
| `archive` | `session archive` | yes |
| `models_list` | `models list` | no |
| `providers_list` | `providers list` | no |

Every tool result carries `exit_code` and `ok` in its structured content, with
the verb's text output in `output`, so the exit meaning is the one
`loom-cli` documents. Mutating tools are annotated `destructiveHint` and read
tools `readOnlyHint`, so the MCP host can approve or reject each mutating call;
authentication never auto-allows `approve`/`deny`/`run`/`send`/`archive`.

## Resources

Session logs are MCP resource templates, equivalent to `events` and `peek`:

- `loom://sessions/{session_id}/events` (optional `?after=<sequence>`), with a
  `gap` report when a snapshot cannot carry the whole window.
- `loom://sessions/{session_id}/peek` (optional `?lines=<n>`).

They return the same JSON objects `loom-cli events|peek --format jsonl`
produces, without any credential.

## Read-only mode

`--read-only` exposes only `ls`, `status`, `models_list`, `providers_list`, and
the log resources. `run`, `send`, `approve`, `deny`, and `archive` are removed
from the tool list and refused if called anyway.
