# ADR 0002: Build the M1 agent slice around typed events

## Status

Accepted for M1.

## Decisions

- Use a provider trait returning normalized `ModelStreamEvent` values. The
  deterministic provider drives repeatable tests and demos; the
  OpenAI-compatible adapter normalizes a chat-completions response without
  exposing provider-specific types to the agent runtime.
- Keep the agent runtime as a state machine that pauses before write and
  command tools. Read-only tools run automatically; `apply_patch` and
  `run_command` produce explicit approval events.
- Scope the first tools to a configured workspace root and invoke commands
  directly without a shell. Text patches require an exact replacement so
  an unexpected file version cannot be overwritten silently.
- Pin the first native GPUI client to `gpui = 0.2.2`. The client renders a
  compact workspace shell with a session rail, event timeline, approval card,
  status bar, run inspector, native titlebar, and live controls while
  continuing to use the protocol boundary and in-process backend. Zed is a
  reference for visual density and theme only, not for product functionality.

## Consequences

The M1 provider adapter is synchronous and requests a non-streaming
OpenAI-compatible completion; the normalized agent event model is ready for
provider-native streaming in a later provider milestone. The GPUI client
uses a deterministic demo workspace, while project selection, persistent
terminals, diffs, and browser transport remain later milestones. The visual
scale uses compact workspace conventions rather than a dashboard layout:
12-14px UI text, narrow fixed side panels, subdued separators, and a short
status bar. M5 later narrows the product surface further around agent
sessions and orchestration.
