# ADR 0005: M4 remote backend control

## Status

Accepted for M4.

## Context

M3 already has the authoritative in-process backend, durable JSON state, typed
protocol envelopes, and a journal that can be queried by sequence. M4 needs a
standalone service without creating a second session/runtime model. Browser and
native clients must be able to reconnect to the same run, while remote
connections must not receive workspace or provider capabilities before they
are authenticated and authorized.

## Decision

1. **Transport:** implement WebSocket over HTTP using `axum` on the server and
   `tokio-tungstenite` on the native fixture. The wire payload remains the
   existing JSON request/response/event envelopes. `WebSocketTransport` is a
   transport adapter, not a new domain API. WebSocket ping/pong frames provide
   heartbeats.
2. **Authentication:** require a bearer token during the HTTP upgrade. The
   in-memory token store keeps only a SHA-256 digest and an opaque token ID.
   An authenticated connection revalidates its token for every request, so
   revocation takes effect without closing/restarting the service.
3. **Authorization:** token grants may restrict capabilities, projects, and
   sessions. The backend intersects negotiated capabilities with the grant
   and checks the project/session/run referenced by every operation.
4. **Reconnect:** clients use the existing global `EventSequence` cursor with
   `GetSessionEvents`. The bounded journal returns
   `SessionEventsSnapshot` when the cursor predates retained history. The
   client replaces its projection and resumes from the returned latest
   sequence.
5. **Mutation retries:** retryable mutation request IDs are cached and
   persisted with the backend snapshot. Reusing a request ID with a different
   request is a conflict, preventing accidental duplicate writes or approvals.
6. **Failure boundaries:** request deadlines and cancellation stop transport
   work, and bounded outbound queues close a lagging connection. Work already
   executing in the backend is not owned by a WebSocket task; process/agent
   execution and journaling continue after disconnect.

## Consequences and limitations

- JSON is easy to inspect and contract-test but is not yet optimized for large
  terminal/file streams. A future binary codec can be added behind the same
  typed transport boundary.
- The current token store is process-local and not an identity provider. It is
  suitable for local/controlled deployments and tests, not a hosted multi-user
  service.
- The listener is plain `ws://`; remote deployments need TLS termination and
  network policy at the deployment boundary.
- Event retention and idempotency caches are bounded. Snapshot fallback keeps
  the current session projection but does not reconstruct discarded historical
  events.
- A timed-out transport task cannot forcibly interrupt arbitrary synchronous
  provider/tool code. The runtime remains durable and can be inspected or
  controlled by a new connection.
