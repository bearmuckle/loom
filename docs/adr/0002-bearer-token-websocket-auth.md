# 0002. Bearer-token WebSocket authentication with scoped capabilities

- Status: Accepted
- Amended: interactive clients sign in after an unauthenticated upgrade; bearer
  tokens remain for operator clients.

## Decision

The remote backend authenticates a WebSocket connection during the HTTP upgrade
with a bearer token in the `Authorization` header, or in a `loom.bearer.<token>`
sub-protocol for browser clients that cannot set handshake headers; the header
wins when both are present, and the token never travels in the URL. Each issued
token carries an `AuthorizationScope` that limits capabilities, sessions,
workspaces, and allowed local repository source roots. Negotiation returns the
intersection of the requested capabilities, the backend's supported
capabilities, and the token grant, and every request re-checks the token, so
revocation takes effect without restarting the service. A plaintext listener
beyond loopback is refused unless the operator passes `--allow-insecure-remote`
or serves TLS.

Bearer tokens are the operator credential. They are not bound to a person and
are meant for headless clients: scripts, CI, and the desktop client's managed
local worker. They stay as described above, and the upgrade that carries one
behaves exactly as it always has.

## Amendment: interactive sign-in

Per-person bearer tokens did not scale to a shared worker: distributing a secret
per person, rotating it, and mapping it back to a stable identity is exactly the
problem the person's GitHub account already solves. So an interactive client no
longer presents a token it was given out of band.

When the operator enables sign-in (`--allow-github-login <login>`, repeatable),
the listener accepts an upgrade that carries **no** credential at all. That
connection starts *pre-login*: it has no participant identity, and it may only
negotiate, discover capabilities, read worker status and the current
participant, and start, poll, or sign out of a GitHub device sign-in. Every
other request fails with `authentication_required`. A successful sign-in binds
the connection to a participant derived from the immutable numeric GitHub
account id, which then has to be granted a session role like any other person.

Properties of the amended decision:

- The listener still refuses a token-less upgrade when sign-in is not enabled,
  so nothing changes for a deployment that does not opt in.
- How many connections may wait to sign in at once is bounded, and a connection
  that stays unsigned past its window is closed.
- The sign-in asks only for the `read:user` scope and never stores the GitHub
  token; it is an identity proof, not a provider credential.
- Operator tokens are unaffected and remain unbound until the connection signs
  in, which is what `--token`/`--token-file` are for now.

## References

- `crates/loom-server/src/auth.rs`: `AuthTokenStore`, `AuthorizationScope`,
  `allows_capability`/`allows_session`/`allows_workspace`.
- `crates/loom-server/src/remote.rs`: `BEARER_SUBPROTOCOL_PREFIX`,
  `request_auth` for the upgrade handshake, and the pre-login upgrade path.
- `crates/loom-server/src/connection/protocol.rs`: per-request `auth.verify()`
  and the `negotiate` capability intersection.
- `crates/loom-server/src/connection/identity.rs`: the pre-login request gate and
  the sign-in binding.
- `crates/loom-server/src/services/identity.rs`: sign-in policy, pending sign-ins,
  and the account-to-participant derivation.
- `crates/loom-core/src/participant.rs`: `github_participant_id` and
  `GitHubSignInStatus`.
- `crates/loom-server/src/deployment.rs`: loopback default, TLS or
  `--allow-insecure-remote` for a plaintext listener beyond loopback.
- `docs/security.md`, `docs/protocol.md`, `docs/deployment.md`.
