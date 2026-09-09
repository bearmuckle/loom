# Known competitors and prior art

This document records products and open-source projects that are useful
comparisons for Loom. It is intended to guide product and architecture
decisions, not to serve as an exhaustive market survey or an endorsement.

This snapshot was reviewed on 2026-09-09. Product capabilities, licensing,
and deployment options change, so claims should be rechecked before they
become implementation constraints.

## Evaluation criteria

Loom has two hard requirements when evaluating comparable products:

- **Remote control:** a second browser, native client, terminal, or messaging
  client can observe and steer the same backend-owned session or run. Starting
  a new job or receiving a completed pull request is not enough.
- **Portable backend:** the agent runtime and its control plane can run on
  user-controlled infrastructure such as a workstation, VM, container,
  Kubernetes cluster, or private cloud. A customer-controlled workspace behind
  a vendor-only control plane is a partial match, not a fully portable
  backend.

No product literally supports every infrastructure target. The useful
comparison is whether the product has a clear runtime boundary and deployment
adapters rather than making the frontend, provider, or vendor-hosted
execution service the source of truth.

## Closest matches

| Product | What exists today | Useful prior art for Loom | Important limitation |
| --- | --- | --- | --- |
| [OpenHands Agent Canvas](https://docs.openhands.dev/openhands/usage/agent-canvas/backends) | A browser frontend connects to an Agent Server backend by host URL and API key. The backend owns conversations, tools, workspaces, events, and automations. It can run locally, on a VM or container, or through the [Kubernetes Helm deployment](https://docs.openhands.dev/openhands/usage/agent-canvas/backend-setup/kubernetes). | Explicit frontend/backend separation, backend switching, persistent workspaces, pause/resume, and a browser-first remote workflow. | The open self-hosted Helm deployment is single-tenant and unauthenticated; hardened authentication, RBAC, and multi-tenancy are an enterprise boundary. |
| [Coder Agents](https://coder.com/docs/ai-coder/agents/architecture) | The agent loop runs as a background job in the self-hosted Coder control plane and reaches Coder workspaces through the same connection path used by IDEs, web terminals, and SSH. Coder supports [Kubernetes control planes and VM or Kubernetes workspaces](https://coder.com/docs/admin/infrastructure). | Workspace abstraction, message queuing, central policies, identity, auditability, and a control-plane-owned agent loop. | It is tightly coupled to the Coder platform and is less useful as a general-purpose client/protocol reference. |
| [OpenCode](https://opencode.ai/docs/server/) | A standalone HTTP server exposes sessions and events. The browser client and a terminal TUI can attach to the same server and share sessions and state through the [web client](https://opencode.ai/docs/web/). | A small, explicit client/server protocol; OpenAPI generation; multiple clients; HTTP authentication; provider and permission APIs. | Durable session history is clear, but crash-safe recovery and exactly-once handling for an in-flight model or tool turn are less explicit than Loom's target. |
| [Open SWE](https://github.com/langchain-ai/open-swe) | An asynchronous software factory accepts work from a dashboard, GitHub, Slack, Linear, schedules, and an experimental desktop client. Each thread has a persistent sandbox and can be deployed with local or pluggable sandbox providers. | Durable threads, isolated sandboxes, follow-up work, CI monitoring, plan mode, review agents, and trigger adapters. | It is optimized for issue-to-pull-request automation rather than a continuously interactive coding workspace. Production self-hosting uses the standalone LangGraph Agent Server and requires a license. |
| [AiderDesk](https://aiderdesk.hotovo.com/docs/advanced/docker) | A desktop and browser coding environment can run headlessly in Docker on a remote server. Its [REST/SSE API](https://aiderdesk.hotovo.com/docs/integrations/rest-api) exposes prompts, task state, questions, sessions, worktrees, and queued prompts. | Task-oriented sessions, worktree isolation, diff review, approval gates, streaming events, and a practical remote deployment path. | Authentication, multi-user isolation, and recovery guarantees are simpler than Loom's intended protocol and security model. |

## Adjacent control-plane and remote-control patterns

These products are valuable prior art even when they are not a complete
replacement for Loom's integrated coding workspace.

| Product | Useful pattern | What Loom should examine carefully |
| --- | --- | --- |
| [Paperclip](https://github.com/paperclipai/paperclip) | A self-hosted control plane manages heterogeneous agents, workspaces, approvals, budgets, logs, and schedules. Its [durable continuation scheduler](https://github.com/paperclipai/paperclip/blob/master/doc/architecture/durable-continuation-scheduler.md) reconciles work after process restarts. | Persisting continuation intent separately from a process is a strong model for crash recovery. Paperclip orchestrates other coding agents rather than supplying Loom's full editor and workspace. |
| [Claude Code Remote Control](https://code.claude.com/docs/en/remote-control) | Browser, phone, and terminal surfaces stay synchronized with a locally running coding-agent process. Filesystem, MCP servers, tools, and permissions remain on the user's machine. | The remote-control UX and reconnection behavior are excellent references. The relay and model service are vendor-hosted, so this is not an end-to-end self-hosted control plane or a provider-neutral architecture. |
| [Cline connectors and hub](https://docs.cline.bot/cli/connectors) | Messaging clients such as Telegram, Slack, Discord, and WhatsApp can steer active local sessions. A hub can run anywhere Node.js runs and supports schedules and long-running work. | Remote steering should be possible without making the full editor the only control surface. Messaging is a useful supplementary client, but not a sufficient primary workspace UI. |
| [Ona (formerly Gitpod)](https://ona.com/docs/ona/getting-started.md) | Background agents run in reproducible Dev Container environments, with schedules, pull-request triggers, browser/editor access, and private [AWS](https://ona.com/docs/ona/runners/aws/overview) or [GCP](https://ona.com/docs/ona/runners/gcp/overview) runners. | Dev-container configuration, private runner boundaries, and background automation are useful patterns. Its documented private execution targets do not make the full control plane an arbitrary self-hosted backend. |
| [OpenClaw](https://docs.openclaw.ai/gateway/remote) | A gateway owns sessions, state, channels, authentication, and background work. Browser, CLI, mobile, messaging, and SSH clients connect to the same gateway, which can run locally or in Docker/Kubernetes. | The single gateway owner and resumable multi-client model are close to Loom's protocol goals. It is a general agent system, so its broad tool and security surface should not be copied without coding-workspace constraints. |
| [DeerFlow](https://github.com/bytedance/deer-flow) | A long-horizon agent harness combines LangGraph persistence, sandboxes, skills, schedules, browser and messaging clients, and TUI/web synchronization. | Durable checkpoints, scheduled work, and pluggable sandboxes are useful. It is a general work-agent product rather than a source-control and editor-centric workspace. |
| [Agent Zero](https://github.com/agent0ai/agent-zero) | A self-hosted web agent offers projects, files, terminals, browser automation, persistent memory, scheduling, and live human intervention. | The full-desktop interaction model is useful for broad automation, but it has less of Loom's typed coding-session, diff, approval, and protocol focus. |

## Notable near-misses

These products are relevant but fail one of the hard requirements under the
strict interpretation above:

- **GitHub Copilot cloud agent, Cursor background agents, Devin, OpenAI Codex
  cloud, and Jules:** strong remote and asynchronous coding workflows, but no
  generally deployable self-hosted backend.
- **Goose, Continue, Aider, SWE-agent, and mini-SWE-agent:** portable local
  coding runtimes, but no clearly documented second-client attachment to the
  same live backend-owned session.
- **Ona's hosted control plane:** customer-controlled AWS or GCP runners are
  useful, but a private runner is not the same as a portable Loom-style
  control plane.

Near-misses should not be ignored. They expose the tradeoff boundaries Loom
must keep explicit instead of allowing a hosted service, a local CLI, or a
workspace provider to silently redefine the product.

## What Loom should borrow

1. **Backend-owned truth and multiple clients.** Follow OpenHands, OpenCode,
   Coder, and OpenClaw by making sessions, workspaces, permissions, and event
   history backend-owned objects. Native, browser, terminal, and supplementary
   clients should be projections over the same protocol.
2. **A real deployment boundary.** Follow OpenHands, OpenCode, AiderDesk, and
   Coder by making the backend a standalone service or library with explicit
   transport, storage, authentication, and workspace adapters.
3. **Durable work, not just durable transcripts.** Follow Coder, Open SWE, and
   Paperclip by persisting queued messages, continuation intent, approvals,
   tool state, checkpoints, and recovery information separately from the
   frontend connection and agent process.
4. **Pluggable execution.** Follow Open SWE and Ona by treating sandboxes,
   workspaces, dev containers, and private runners as adapters behind
   capability negotiation rather than hard-coding one hosting provider.
5. **Human control at every surface.** Follow the approval and interruption
   patterns in OpenHands, AiderDesk, Coder, Claude Code, and Cline. Remote
   clients must be able to inspect pending actions, approve or deny them,
   redirect work, and take over safely.
6. **Configurable automation without hiding side effects.** Follow Open SWE,
   Ona, and Paperclip by supporting schedules and external triggers while
   keeping plans, policies, logs, artifacts, and resulting diffs reviewable.

## What Loom should not copy

1. **A vendor-hosted control plane as a hidden dependency.** A customer-owned
   runner does not satisfy Loom's portability goal if session state,
   authorization, and orchestration remain unavailable outside the vendor.
2. **A messaging connector as the primary product.** Chat integrations are
   useful clients, but they cannot replace structured plans, tool timelines,
   approvals, terminal output, diagnostics, and diff review.
3. **Unauthenticated self-hosting by default.** A convenient single-tenant
   deployment is useful for development, but remote exposure must require
   explicit authentication, authorization, encryption, and workspace scope.
4. **Request-lifetime execution.** A model turn or HTTP request must not own
   the only copy of agent progress. Client disconnects, process restarts, and
   reconnects must preserve valid backend state.
5. **Provider- or editor-shaped domain models.** Provider-specific streaming
   formats, IDE assumptions, and tool schemas belong behind adapters. The
   session, event, approval, and workspace model must remain provider-neutral.
6. **Calling a sandbox a complete product.** Daytona, E2B, Modal, Runloop,
   dev containers, and workspace platforms solve execution isolation or
   lifecycle. Loom still needs durable orchestration, permissions, review,
   client protocol, and a coherent coding workspace.
7. **Promising arbitrary infrastructure without capability boundaries.** Every
   deployment target has different filesystem, process, network, identity,
   persistence, and security properties. Loom should advertise supported
   capabilities and limits rather than pretending all targets behave
   identically.

The resulting design target is a portable backend with a stable protocol,
durable event and execution state, interchangeable clients, explicit
capability negotiation, and a coding workspace that remains useful when the
agent is paused, disconnected, redirected, or replaced.
