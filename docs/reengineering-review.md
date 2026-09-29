# Reengineering review - from-scratch architecture

This document records a point-in-time review that asks a single question: if
Loom were designed again from a clean slate, what would change at the
architectural level? It is a findings and direction document, not a
specification and not a commitment to a rewrite. It complements
[implementation audit](audit.md), which covers agent execution quality, and
[architecture](architecture.md), which states the intended boundaries.

**Reviewed revision:** `1edfe0d` (main).

**Scope:** 15 crates, approximately 91,200 lines of Rust.

## Summary

The documentation is unusually strong and describes the correct target
boundaries. The implementation has drifted from them, and the drift is
concentrated in three god files: `loom-server/src/lib.rs`,
`loom-persistence/src/lib.rs`, and `loom-ui/src/view.rs`. The project was
built infrastructure-first, and the composition root absorbed the domain. The
architecture is therefore already specified correctly in prose; the
reengineering work is making the crate graph and file sizes match the document
and replacing a synchronous, global-mutex core with owned, async services.

A full rewrite is not indicated. The protocol contract, content-addressed
storage, error taxonomy, and test discipline are real assets and should be
preserved.

## Observations that motivate the review

| Area | Observation |
| --- | --- |
| Server | `loom-server/src/lib.rs` is 21,988 lines: ~12,300 lines of production code and ~9,700 lines of embedded tests. `InProcessBackend` has roughly 40 independent `Mutex` fields. `impl InProcessConnection` spans ~6,400 lines; `handle_after_negotiation` alone is ~1,040 lines matching 89 request variants. |
| Persistence | `loom-persistence/src/lib.rs` is 17,466 lines with an 88-method concrete interface, a ~600-line `DATABASE_SCHEMA` string, and raw SQL throughout. |
| Client | `loom-ui/src/view.rs` is 18,375 lines with 388 functions. The view constructs `InProcessBackend` directly and owns schema reset and credential access. |
| Protocol | 89 `ClientRequest` and 73 `ServerResponse` variants in single enums, hand-written codecs, and no schema generation. |
| Layering | `loom-persistence` depends on `loom-providers` and `loom-session`. `loom-ui` links `loom-server`, `loom-persistence`, `loom-providers`, and `loom-vcs` on native targets. |
| Runtime | Providers use blocking `ureq` with a blocking SSE line loop while `loom-server` uses `tokio`. |

Positive findings worth preserving:

- Production code has zero `.unwrap()` and effectively zero `.expect()` in the
  server, persistence, providers, and agent crates; unwraps are confined to
  test modules.
- `loom-core` has a clean, stable `ErrorCode` taxonomy.
- `loom-protocol` is a genuinely backend-independent contract crate, and the
  client abstracts transport behind `ClientConnection`.
- The UI uses `gpui-kit` components rather than hand-rolled controls.
- The content-addressed blob store, typed indexed tables, and WAL/FULL
  transaction design are sound.

## Reengineering directions

### 1. Backend - split the god object

`InProcessBackend` is a struct with one lock per concern, a `Weak<Self>`
self-reference, and multiple admission and gate mutexes. A clean design makes
the composition root own subsystems, each encapsulating its own state:

- `SessionService`, `RunService` (run lifecycle, journal, `RunHandle`),
  `WorkspaceService`, `ProviderService`, `CredentialService`,
  `IdempotencyStore`, and `ProjectCoordinator`.

Each should expose an explicit state machine, matching the
`planning -> executing <-> evaluating` model in
[architecture](architecture.md), instead of free functions mutating shared
maps. `RunHandle` is already a step in this direction and should become the
pattern. The project-coordination tool extension belongs in a dedicated crate,
not in the server.

### 2. Protocol - stop hand-maintaining a monolith RPC

The protocol is one large request enum and one large response enum with an
exhaustive dispatch match in the server. The roadmap already lists
"schema/code-generation approach" as an open decision. That decision should be
settled early: define the contract once in an IDL and generate Rust and
TypeScript, or at minimum split the enums per domain behind request/response
traits so no function requires a thousand-line match. This is the highest
leverage change for both correctness and client work.

### 3. Layering - fix two inversions

- `loom-persistence` should depend only on neutral domain types. Provider
  configuration and session state should not leak into the storage crate.
- The client should depend on `loom-protocol` alone. Backend lifecycle,
  schema reset, and credential access belong in a small native launcher crate,
  so the native and remote paths become symmetric and the UI is genuinely
  protocol-only.

### 4. Runtime - async end to end

The server already runs on `tokio`, but providers use blocking HTTP and a
blocking SSE iterator, and runs get dedicated threads. Committing to `tokio`
across providers, tools, and the agent makes `stream` an async stream, makes
tool execution a bounded async join set with a read/write policy, and removes
bespoke cancellation, timeout, and backpressure plumbing. This subsumes the
sequential-tool and blocking-provider findings in the execution audit.

### 5. Persistence - repositories, a trait, migrations

A fresh design would:

- split the storage crate into per-aggregate repositories (sessions, runs,
  messages and fragments, filesystem, projects, catalog, blobs) behind a
  `Persistence` trait, with file and in-memory implementations that share one
  code path;
- separate the content-addressed blob store from the relational catalog;
- introduce a migration ladder now. Rejecting and wiping an incompatible
  database is acceptable for a preview but is a deliberate dead end that
  becomes expensive after 1.0.

### 6. Client - break up the view

`view.rs` should be split into screen modules (navigator, canvas, composer,
review drawer, settings) with a dedicated view-model layer and a backend host
service that owns connection and backend lifecycle. Only rendering should
remain in the view layer.

### 7. Tests and evidence

Most correctness evidence currently lives in very large in-process integration
tests embedded at the bottom of `lib.rs`. A fresh start would move these into
`tests/` behind a reusable test-backend fixture, add the task-level acceptance
benchmark requested by the execution audit, and property-test the codec and
the hand-rolled path-containment logic.

### 8. Remote security

The remote path places a bearer token in the WebSocket URL and speaks plain
`ws://`. A redesign should perform a token exchange as the first authenticated
frame or via a subprotocol, so credentials never ride in the URL. This is a
real fix independent of the browser WebSocket limitation.

## Suggested target layout

```text
loom-core         ids, errors, capabilities, policy
loom-model        provider traits and token accounting
loom-protocol     generated codecs
loom-persistence  repository traits with sqlite and memory impls
loom-workspace / loom-vcs / loom-process   unchanged
loom-tools        tool definitions
loom-agent        run state machine
loom-project      project coordinator and project tools          [new]
loom-services     session/run/workspace/provider services        [new]
loom-server       transport, auth, connection management
loom-cli          startup and diagnostics
loom-local        native backend embedding and credentials       [new]
loom-ui           protocol-only UI
```

## Suggested sequencing

The execution audit correctly concludes that a full rewrite is not indicated.
Coverage gates, CI, and the existing tests are assets. A pragmatic order is:

1. Extract protocol dispatch into per-domain modules backed by a small set of
   service objects, with no behavior change.
2. Split persistence into repositories behind a trait and add migrations.
3. Fix the two dependency inversions.
4. Move providers and tools to async, then enable read-only tool concurrency
   with the acceptance test from the execution audit.
5. Split the view and move backend lifecycle into a host service.

The through line is simple: make the crate graph and file sizes match the
architecture document, and replace the synchronous, global-mutex core with
owned, async services.
