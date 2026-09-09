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
- Provide cancellation and resource limits for processes, streams, model
  calls, and tasks.
- Record security-relevant actions without recording secret values.
- Support revoking a client/session without restarting the backend.

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
