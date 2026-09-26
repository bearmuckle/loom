# License details

Copyright (C) 2026 Björn Harrtell. See [COPYRIGHT](COPYRIGHT).

Loom's Rust crates declare their license in each crate's `Cargo.toml`:

- Foundational crates that do not depend on AGPL code use `GPL-3.0-only`:
  `loom-core`, `loom-model`, `loom-session`, `loom-persistence`, and
  `loom-providers`.
- `loom-protocol` and crates that depend on it use `AGPL-3.0-only`:
  `loom-agent`, `loom-context`, `loom-process`, `loom-tools`, `loom-vcs`,
  `loom-workspace`, `loom-server`, `loom-cli`, and `loom-ui`.
- The native UI also includes the AGPL-licensed server crate. The combined
  Loom applications are offered under `AGPL-3.0-only`.

The complete license texts are [GPL-3.0-only](LICENSE) and
[AGPL-3.0-only](LICENSE-AGPL). The foundational GPL crates can be reused
subject to GPL-3.0-only. The listed AGPL crates and applications are offered
under AGPL-3.0-only. Dependencies have their own licenses; consult their
package metadata and license notices when redistributing a build.

This file explains the project's intended licensing; it is not legal advice.
