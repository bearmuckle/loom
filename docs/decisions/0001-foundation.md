# ADR 0001: Establish the protocol-first foundation

## Status

Accepted for the M0 foundation.

## Decisions

- Use a Cargo workspace with separate `loom-core`, `loom-model`,
  `loom-protocol`, `loom-session`, `loom-server`, and `loom-cli` crates.
- Keep the backend authoritative. The in-process backend owns session state and
  the monotonic event journal; clients only submit typed requests and render
  responses and events.
- Use JSON as the first protocol codec. It is deliberately inspectable and
  supports the protocol test harness while the binary transport choice remains
  a measured follow-up decision.
- Use UUID-backed domain identifiers and a numeric protocol version with
  capability intersection during connection negotiation.
- Use a terminal native shell for M0. It exercises the same request/event seam
  that a GPUI client will use without coupling the backend foundation to a
  windowing or browser target before the browser-capable GPUI variant is
  selected.

## Consequences

The initial session manager is in-memory, so restart persistence, remote
transports, model providers, and the GPUI frontend remain later milestones.
Those features can be added behind the protocol and domain boundaries without
changing the native shell's session/event flow.
