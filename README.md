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
- [Known competitors and prior art](docs/competitors.md) - comparable
  products, useful patterns, near-misses, and design lessons.
- [Roadmap and quality bar](docs/roadmap.md) - milestones, exit conditions,
  performance, testing, decisions, and first-release non-goals.
- [M5 agent workspace and orchestration surface](docs/decisions/0006-m5-coding-workspace.md) -
  the focused session-first client specification.

These documents describe the intended first implementation. The roadmap and
architecture decision records are the source of truth for scope and
implementation tradeoffs; this README intentionally keeps the product
direction and document index concise.
