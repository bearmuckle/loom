# Architecture decision records

An architecture decision record (ADR) fixes a choice that the code and the rest
of the documentation already depend on. Records are added when the choice is
made, never beforehand: an `Accepted` record describes behaviour that exists in
this checkout and cites the file that implements it.

## Process

1. Add the next free number as `docs/adr/NNNN-<slug>.md`.
2. Give the record a status: `Accepted` (implemented and cited here), `Open`
   (still being decided), `Superseded`, or `Rejected`.
3. An `Accepted` record names the decision and cites the code or documentation
   path that shows it. Do not record a plan as a decision.
4. Add the record to the index below in the same change; keep `Open` entries in
   the roadmap's list until a decision is made.
5. Do not rewrite the meaning of an accepted record. Supersede it with a new
   record and mark the old one `Superseded by NNNN`.

## Index

| ADR | Status | Decision |
| --- | --- | --- |
| [0001](0001-sqlite-single-database-persistence.md) | Accepted | Backend state lives in one SQLite database at schema version 4 |
| [0002](0002-bearer-token-websocket-auth.md) | Accepted | WebSocket authentication uses a bearer token with scoped capabilities |
| [0003](0003-protocol-version-negotiation.md) | Accepted | Protocol version 11.1 with major-version negotiation |
| [0004](0004-gpui-kit-client.md) | Accepted | One GPUI client on the published `gpui-kit` crate for native and wasm |

## Open

These come from the roadmap's [decisions to settle early](../roadmap.md) and
have no recorded outcome yet:

- The exact GPUI/`gpui-gc` fork and supported native platforms. The current
  client depends on the published `gpui-kit` crate (ADR 0004), which is a
  dependency pin, not a recorded fork or platform list.
- The first wasm/browser support target and minimum browser versions.
- MessagePack versus CBOR, and the schema/code-generation approach.
- Whether QUIC is a first-release transport or a later optimization.
- The supported source-control providers beyond Git.
- The provider adapter contract and local model compatibility baseline.
