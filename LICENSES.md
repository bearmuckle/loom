# License details

Copyright (C) 2026 Björn Harrtell. See [COPYRIGHT](COPYRIGHT).

Loom's Rust crates declare their license in each crate's `Cargo.toml`:

- Crates that do not depend on AGPL code use `GPL-3.0-only`: `loom-build`,
  `loom-core`, `loom-model`, `loom-session`, `loom-persistence`,
  `loom-providers`, `loom-agent`, `loom-context`, `loom-process`,
  `loom-tools`, `loom-vcs`, and `loom-sessionfs`.
- `loom-protocol` and crates that depend on it use `AGPL-3.0-only`:
  `loom-local`, `loom-server`, `loom-cli`, and `loom-ui`.
- The native UI also includes the AGPL-licensed server crate. The combined
  Loom applications are offered under `AGPL-3.0-only`.

The complete license texts are [GPL-3.0-only](LICENSE) and
[AGPL-3.0-only](LICENSE-AGPL). The GPL crates can be reused
subject to GPL-3.0-only. The listed AGPL crates and applications are offered
under AGPL-3.0-only. Dependencies have their own licenses; consult their
package metadata and license notices when redistributing a build.

The `deny` job in [`.github/workflows/ci.yml`](.github/workflows/ci.yml) checks
the license of every crate in `Cargo.lock` against the allow list in
[`deny.toml`](deny.toml), so a license that is not listed there fails the build.

This file explains the project's intended licensing; it is not legal advice.
