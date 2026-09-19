# Security and trust model

The backend can execute commands, access valuable project data, call remote
model providers, and use configured credentials. A remote connection is never
implicitly trusted.

## Requirements

- Bind local-only servers to a local transport by default.
- Require explicit opt-in before listening on a network interface.
- Authenticate remote clients and authorize each workspace/session.
- Scope filesystem access to configured workspace roots.
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
  secret vault. Deployments must place it and any credential store under the
  backend user's protected data directory and use filesystem permissions
  appropriate to the deployment.
- Provide cancellation and resource limits for processes, streams, model
  calls, and tasks.
- Record security-relevant actions without recording secret values.
- Support revoking a client/session without restarting the backend.

M4's standalone service uses opaque bearer tokens stored as SHA-256 digests
in memory. Issued-token debug output is redacted, authentication failures do
not echo the supplied token, and authorization headers are not copied into
protocol events. Revocation is checked on every request, including requests
from an already-upgraded WebSocket. Token grants can restrict capabilities,
projects, and sessions; an unrestricted grant is an explicit deployment
choice rather than an implicit network default.
Scope builders may also pin a project to a canonical workspace root before
the first `OpenWorkspace`.

The service binds to `127.0.0.1` by default. Binding a non-loopback address
requires an explicit `--bind` choice and a token. TLS termination is expected
to be supplied by the deployment boundary for remote use; the current
standalone listener is plain WebSocket (`ws://`) and must not be exposed
directly to an untrusted network.

## Agent permissions

Tool permissions should be typed and policy-driven rather than inferred from
the UI. At minimum, distinguish:

- Reading files and searching the workspace.
- Writing or deleting files.
- Running commands and choosing their environment.
- Accessing the network.
- Reading or forwarding credentials.
- Installing dependencies or changing tool configuration.
- Creating commits, branches, worktrees, or other source-control mutations.
- Starting child agents or spending additional model budget.

The user should see why an action requires approval, which workspace and
resources it affects, and what the agent requested. Policies may allow
automatic approval for low-risk actions, but high-risk operations remain
explicit by default.

The first release does not need a complete multi-user identity system, but it
must have an explicit trust boundary so one can be added without redesigning
the protocol.

M2 implements the local boundary with canonical workspace roots, traversal and
outside-root symlink rejection, revision-checked edits, workspace-scoped task
working directories, and project ownership checks for terminal/task control
requests. Its default `ApprovalPolicy` allows reads, pauses writes and
commands for approval, requests approval for network actions, and denies
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

Workspace roots remain backend-owned after the first `OpenWorkspace` for a
project and canonical workspace checks from M2 still reject traversal and
outside-root symlinks. M4 project/session authorization prevents a token from
using another project's IDs; deployments that allow a token to select a
project's initial root must additionally constrain the process account and
filesystem permissions. A full per-user identity/invitation system and
encrypted secret vault remain deferred.

M5 adds no new trust boundary. The client reads only backend-authoritative,
project-scoped session, workspace-change, diff, task, and evidence
projections. File previews and diffs are bounded and cannot mutate the
workspace; existing agent tools, approval policies, canonical workspace
checks, and VCS argument validation remain the authority for mutations.
Repository instructions and context references are displayed as untrusted data
and do not change approval policy.
