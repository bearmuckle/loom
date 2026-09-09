# ADR 0004: Provider registry and durable orchestration

## Status

Accepted for M3.

## Decisions

- Keep the existing provider-neutral `ModelProvider` boundary and add a
  registry around it. A registry entry identifies a provider kind, endpoint,
  model descriptors, capability flags, pricing metadata, and an optional
  opaque `CredentialRef`; it never contains a raw key in protocol-facing
  state.
- Treat OpenAI-compatible HTTP as the hosted/gateway baseline and expose
  Ollama through the same adapter with a local `/v1/chat/completions`
  endpoint. Deterministic responses remain a first-class fixture. Model
  discovery returns normalized descriptors, while health checks and HTTP
  status classes map to stable Loom errors.
- Use a versioned JSON snapshot with an atomic temporary-file replacement for
  the first persistence engine. It is deliberately dependency-light and
  inspectable for fixtures. The snapshot includes the session/event journal,
  serializable agent runtime state, policies, workspace checkpoints,
  provider health, and model usage. The backend writes after each mutating
  request; malformed or incompatible files fail with a structured persistence
  error.
- Persist messages and runtime cursors, not opaque provider internals. The
  deterministic provider cursor makes fixture runs restartable. An unfinished
  runtime recovered after a process crash is marked `paused`; resuming or
  retrying from the associated workspace checkpoint is explicit and
  idempotent from the client's perspective.
- Assemble context through a separate `loom-context` crate. Required system,
  repository, and task instructions cannot be silently dropped. Conversation
  items can be compacted into an inspectable summary, and token/time/tool/cost
  limits produce structured events instead of hidden truncation or provider
  switching.

## Consequences

The JSON snapshot is suitable for a single backend process and deterministic
restart tests, but it is not a multi-writer database, encrypted credential
vault, or remote event store. File locking, migrations beyond the first
schema, native PTY/process reattachment, streaming transport, and provider
specific cancellation and hard wall-clock deadlines around a blocking HTTP
call remain later work. A provider call that fails during
restart recovery is surfaced as `RecoveryRequired` and
`recovery_required`; the backend does not silently fall back to another
model.

The registry can host multiple OpenAI-compatible configurations, but the
protocol exposes only provider/model descriptors and credential reference
IDs. Clients therefore remain provider-neutral and can select a model without
handling provider secrets.
