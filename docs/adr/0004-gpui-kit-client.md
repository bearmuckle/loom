# 0004. One GPUI client on the published `gpui-kit` crate

- Status: Accepted

## Decision

The native and browser clients are one GPUI application that builds on the
published `gpui-kit` crate (workspace pin `0.8.1`) rather than on a
Chromium-based desktop runtime or a Loom-maintained GPUI fork. `loom-ui` holds
the protocol client, projections, and view, and shares them across targets;
`crates/loom-ui/src/browser.rs` is compiled only for `wasm32` and supplies the
in-browser WebSocket transport. Native-only adapters live in `loom-local` so
the UI does not link the backend implementation. The dependency is a crates.io
pin, so the repository carries no vendored GPUI revision of its own.

## References

- `Cargo.toml`: `gpui-kit = "0.8.1"` in the workspace dependency table;
  `Cargo.lock` resolves it from the crates.io registry.
- `crates/loom-ui/Cargo.toml`: `gpui-kit.workspace = true`, the wasm-only
  dependencies, and the `visual-tests` feature.
- `crates/loom-ui/src/main.rs`: `gpui_kit::App`, the wasm `no_main` wrapper, and
  the native adapters pulled from `loom-local`.
- `crates/loom-ui/src/browser.rs`: wasm-only browser transport and page
  bootstrap settings.
- `docs/architecture.md`, `README.md`.
