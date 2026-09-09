# Loom

Loom is a cross-platform, remote-first agentic development environment. It
combines a native or browser-based interface with a Rust backend that
orchestrates persistent coding-agent sessions, workspace state, tool
execution, and integrations.

The primary product is an agent orchestration application with a code
workspace. A user gives an agent a repository task, watches it inspect,
plan, edit, run tools, respond to failures, and produce a reviewable result.
The user remains in control through approvals, interruption, feedback,
checkpoints, and explicit permission policies.

The backend can run locally, as a child process, or remotely. Hosted model
providers, OpenAI-compatible endpoints, and local model runtimes should be
usable through the same session and tool model. Native and browser clients
should be interchangeable views of the same durable backend session.

## Specification

The project specification is split into focused documents:

- [Product scope and use cases](docs/product.md) - target users, workflows,
  feature areas, principles, and the first end-to-end experience.
- [Architecture](docs/architecture.md) - frontend, Rust backend, agent
  runtime, model providers, tools, and repository structure.
- [Communication protocol](docs/protocol.md) - transports, event model,
  reconnect behavior, provider-neutral operations, and streaming.
- [Security model](docs/security.md) - trust boundaries, permissions,
  credentials, prompt injection, and resource limits.
- [Roadmap and quality bar](docs/roadmap.md) - milestones, exit conditions,
  performance, testing, decisions, and first-release non-goals.

These documents describe the intended first implementation. Each milestone
should produce a demonstrable vertical slice, and architecture decisions
should be recorded as implementation settles details such as the GPUI
variant, schema format, persistence engine, and supported deployment targets.

## M0 foundation and M1 vertical slice

The M0 foundation is a Rust workspace with shared domain types, provider-
neutral model types, a versioned JSON protocol, an in-process backend, and a
native command-line shell. Run the vertical slice with:

```sh
cargo run -p loom-cli -- --name "Foundation demo"
```

The shell negotiates protocol capabilities, creates an empty agent session,
and renders the authoritative session event stream.

M1 adds the deterministic provider, an OpenAI-compatible adapter, the
workspace tools, approvals, interruption, retry, streamed agent events, and
the GPUI three-pane native client:

```sh
# Text event-stream client with automatic approvals
cargo run -p loom-cli -- --task "make a small repository change"

# GPUI client against the isolated M1 demo workspace
cargo run -p loom-ui
```

The GPUI client starts against an isolated temporary workspace and pauses on
write/command approvals. See [ADR 0001](docs/decisions/0001-foundation.md) and
[ADR 0002](docs/decisions/0002-m1-agent-vertical-slice.md) for the
implementation decisions.
