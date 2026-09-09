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

## Foundation implementation

The M0 foundation is a Rust workspace with shared domain types, provider-
neutral model types, a versioned JSON protocol, an in-process backend, and a
native command-line shell. Run the vertical slice with:

```sh
cargo run -p loom-cli -- --name "Foundation demo"
```

The shell negotiates protocol capabilities, creates an empty agent session,
and renders the authoritative session event stream. See
[ADR 0001](docs/decisions/0001-foundation.md) for the initial implementation
decisions and deferred frontend choices.
