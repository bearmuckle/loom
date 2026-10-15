# 0002. Bearer-token WebSocket authentication with scoped capabilities

- Status: Accepted

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

## References

- `crates/loom-server/src/auth.rs`: `AuthTokenStore`, `AuthorizationScope`,
  `allows_capability`/`allows_session`/`allows_workspace`.
- `crates/loom-server/src/remote.rs`: `BEARER_SUBPROTOCOL_PREFIX`,
  `request_auth` for the upgrade handshake.
- `crates/loom-server/src/connection/protocol.rs`: per-request `auth.verify()`
  and the `negotiate` capability intersection.
- `crates/loom-server/src/deployment.rs`: loopback default, TLS or
  `--allow-insecure-remote` for a plaintext listener beyond loopback.
- `docs/security.md`, `docs/protocol.md`.
