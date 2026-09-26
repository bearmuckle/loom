# Loom

> Loom is early experimental software. Interfaces and behavior may change, and
> it is not yet suitable for production or sensitive workloads.

Loom is a cross-platform, remote-first agentic development environment. It
combines a native or browser-based interface built on [GPUI](https://www.gpui.rs/)
with a Rust backend that orchestrates persistent coding-agent sessions,
workspace state, tool execution, and integrations.

The backend can run locally, as a child process, or remotely. Hosted model
providers, OpenAI-compatible endpoints, and local model runtimes should be
usable through the same session and tool model. Native and browser clients
should be interchangeable views of the same durable backend session.

## Why Loom

- **Cross-platform performance:** A GPUI-based Rust interface can run as a
  desktop or browser runtime instead of depending on a Chromium-based desktop runtime such as Electron.
- **Remote-first workflows:** Agents can run near repositories and services,
  without tying development work to a single computer.
- **User-controlled trust:** Loom can run on infrastructure you control, with
  open code and explicit permissions for workspace access, tools, and
  credentials.

## Build and run

Install Rust 1.95 or newer, then run:

```sh
cargo run -p loom-ui
```

By default, Loom uses the directory you launch it from. To use a different
project directory, pass `--project PATH`:

```sh
cargo run -p loom-ui -- --project /path/to/project
```

To try the demo without connecting a model provider:

```sh
cargo run -p loom-ui -- --demo
```

For browser development, run `./scripts/dev-wasm.sh`. This requires Trunk and
the `wasm32-unknown-unknown` Rust target.

## Development checks

Run the full Rust check suite locally:

```sh
./scripts/ci-check.sh
```

To enable the formatting check before every commit, run:

```sh
git config core.hooksPath .githooks
```

The hook runs `cargo fmt --all -- --check`. CI also runs Clippy, tests, and
build checks.

Configure providers with `LOOM_OPENAI_ENDPOINT`, `LOOM_API_KEY`, and
`LOOM_MODEL`, or connect to a remote backend with `LOOM_REMOTE_URL` and
`LOOM_TOKEN`. GitHub Copilot login is available in the UI.

## License

Loom's core libraries and client components are licensed under GPL-3.0-only;
see [LICENSE](LICENSE). The protocol and server-side components in
`crates/loom-protocol` and `crates/loom-server` are licensed under
`AGPL-3.0-only`; see [LICENSE-AGPL](LICENSE-AGPL).
