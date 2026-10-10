# Security and trust model

The backend can execute commands, access valuable repository and session
data, call remote model providers, and use configured credentials. A remote
connection is never implicitly trusted.

## Requirements

- Bind local-only servers to a local transport by default.
- Require explicit opt-in before listening on a network interface.
- Authenticate remote clients and authorize each workspace/session.
- Scope filesystem access to the active session's filesystem root.
- Make command execution policy visible and configurable.
- Treat environment variables, command output, file contents, repository
  instructions, model output, and tool responses as untrusted data.
- Treat prompt injection in repositories, issues, web pages, and tool output
  as untrusted instructions; it must not silently override system policy or
  user approvals.
- Keep provider credentials in a backend secret store or OS credential
  facility and expose only opaque credential IDs to clients.
- Never transmit secrets as part of ordinary workspace snapshots or logs.
- Durable state contains credential references and provider metadata only;
  raw API keys are resolved in memory for a provider call and are not
  serialized in the event journal, health errors, or protocol responses.
- The SQLite state database is durable application state, not an encrypted
  secret vault. The standalone server keeps one instance directory per bind
  address, created owner-only (mode 0700), which defaults to
  `<state-dir>/<instance-name or bind-key>` and holds the database, the
  default token file (mode 0600), and the files derived from the database path:
  the credential file, the session roots, the cached clone mirrors, and the
  owner lock. Deployments must place it and any credential store under the
  backend user's protected data directory and use filesystem permissions
  appropriate to the deployment.
- Provide cancellation and resource limits for processes, streams, model
  calls, and tasks.
- Record security-relevant actions without recording secret values.
- Support revoking a client/session without restarting the backend.

M4's standalone service uses opaque bearer tokens stored as SHA-256 digests
in memory. Native clients present the token in an `Authorization: Bearer`
header; browser `WebSocket` clients, which cannot set handshake headers, pass
it in a `loom.bearer.<token>` subprotocol so credentials never appear in the
connection URL (and therefore not in logs, referrers, or the address bar).
Issued-token debug output is redacted, authentication failures do not echo the
supplied token, and authorization headers are not copied into protocol events. Revocation is checked on every request, including requests
from an already-upgraded WebSocket. Token grants can restrict capabilities,
workspaces, and sessions; an unrestricted grant is an explicit deployment
choice rather than an implicit network default. Without `--token` or
`--token-file` the standalone server keeps its bearer token in
`<instance-dir>/token`: it generates a `loom-<uuid>` value there on first start,
writes it with mode 0600, and reuses it on later starts. The path is always
logged, generation is logged as a warning, and the value itself is printed only
when stdout is a terminal, so it is not captured by `journald` or container
logs.

The service binds to `127.0.0.1` by default. Binding a non-loopback address
requires an explicit `--bind` choice; the bearer token defaults to the instance
token described above, and `--token` or `--token-file` override it. The server
can also terminate TLS itself: `--tls-cert` and `--tls-key` are required
together and make the listener serve `wss://`, with `/health` over TLS as well.
Without TLS, the bind refuses a non-loopback address unless the operator passes
`--allow-insecure-remote`, which logs a warning naming the flag that permitted
the listener. That refusal is a deliberate compatibility change: a plaintext
remote listener, and a native client sending its token to one, now need either
TLS or that explicit opt-in. Loopback is always allowed without either. A
listener that is plaintext because of the opt-in carries the bearer token and
every protocol frame in the clear and must not be exposed directly to an
untrusted network; TLS termination may still be supplied by an external
deployment boundary instead of by the server itself.

A client connecting to a `wss://` worker whose certificate is issued by a
private or self-signed CA adds that CA to its OS trust roots with
`--ca /path/to/ca.pem` or the `LOOM_TLS_CA` environment variable. The addition
is additive: certificate verification is never disabled, and there is
deliberately no accept-any-certificate option. The client likewise refuses
plaintext `ws://` to a non-loopback worker unless it is given
`--allow-insecure-remote`.

## Agent permissions

Tool permissions should be typed and policy-driven rather than inferred from
the UI. At minimum, distinguish:

- Reading files and searching the session filesystem.
- Writing or deleting files.
- Running commands and choosing their environment.
- Accessing the network.
- Reading or forwarding credentials.
- Installing dependencies or changing tool configuration.
- Creating commits, branches, worktrees, or other source-control mutations.
- Starting child agents or spending additional model budget.

The user should see why an action requires approval, which workspace, session,
and repository resources it affects, and what the agent requested. Policies
may allow automatic approval for low-risk actions, but high-risk operations
remain explicit by default.

The first release does not need a complete multi-user identity system, but it
must have an explicit trust boundary so one can be added without redesigning
the protocol.

The backend enforces the local boundary with canonical session roots,
traversal and outside-root symlink rejection, revision-checked edits,
session-scoped task working directories, and session ownership checks for
terminal and task control requests. Workspace membership does not grant
filesystem access by itself. A session root is a path-ownership boundary, not
a promise of OS-level process sandboxing. The default `ApprovalPolicy`
allows reads, pauses writes and commands for approval, requests approval for
network actions, and denies
destructive actions. The policy evaluation is included in the agent event
stream before a tool executes. The UI's optional Auto approve mode allows
non-destructive writes, commands, and network actions, but destructive actions
remain denied.

M3 makes context and resource boundaries visible as structured state. Input
context is assembled from system instructions, repository instructions, task,
summaries, and conversation items; optional conversation items may be
compacted, but required items produce an explicit context-limit error.
Session time, token, tool-call, and cost budgets emit limit events and end a
run rather than silently truncating output, switching providers, or bypassing
approval policy.

The `web_search` tool is a network action and therefore follows the same
default approval requirement. It fetches a bounded server-rendered search page
from the built-in HTML search endpoint, or from an operator-configured
`LOOM_WEB_SEARCH_ENDPOINT`; the endpoint is deployment configuration rather
than model-controlled input. Search results are untrusted remote content, are
returned as bounded structured citations, and may be restricted to explicit
hostnames by the tool request. Page retrieval and arbitrary URL fetching are
not implied by this tool.

The backend owns and canonicalizes each session filesystem root, rejecting
traversal and outside-root symlinks. Workspace authorization governs access to
workspace metadata and session membership; session authorization governs
filesystem, process, and run operations. Deployments must also constrain the
backend process account and filesystem permissions. A full per-user
identity/invitation system and encrypted secret vault remain deferred.

M5 adds no new trust boundary. The client reads only backend-authoritative,
session-scoped filesystem, diff, task, and evidence projections. File previews
and diffs are bounded and cannot mutate the session filesystem; existing
agent tools, approval policies, canonical path checks, and VCS argument
validation remain the authority for mutations.
Repository instructions and context references are displayed as untrusted data
and do not change approval policy.
