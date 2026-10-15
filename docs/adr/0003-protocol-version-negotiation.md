# 0003. Protocol version 11.1 with major-version negotiation

- Status: Accepted

## Decision

The protocol carries an explicit major/minor version and the current contract is
11.1. Compatibility is major-version based: a client may negotiate a newer minor
version within the same major, and an incompatible major fails with the
`unsupported_protocol` error before the request is served. Old-client forward
compatibility is not supported. Negotiation returns the backend's current
version together with the capability intersection, and persisted event records
stamp the version from the neutral domain crate so they do not depend on the
protocol contract.

## References

- `crates/loom-core/src/version.rs`: `CURRENT_PROTOCOL_VERSION`,
  `ProtocolVersion::is_compatible_with`.
- `crates/loom-server/src/connection/protocol.rs`: version check on every
  request and in `negotiate`.
- `crates/loom-protocol/src/lib.rs`: `unsupported_version_error` and the version
  test.
- `docs/protocol.md`, `docs/roadmap.md`.
