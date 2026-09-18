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

Requires Rust 1.85 or newer.

```text
cargo build
cargo run -p loom-ui -- --workspace /path/to/project
```

Demo mode:

```text
cargo run -p loom-ui -- --demo
```

Browser target (GPUI currently requires a nightly compiler for wasm atomics):

```text
RUSTC_BOOTSTRAP=1 cargo check -p loom-ui --target wasm32-unknown-unknown
```

The wasm binary is a browser-hosted GPUI client; native-only workspace,
process, and credential integrations remain available through the remote
backend boundary.

Configure providers with `LOOM_OPENAI_ENDPOINT`, `LOOM_API_KEY`, and
`LOOM_MODEL`, or connect to a remote backend with `LOOM_REMOTE_URL` and
`LOOM_TOKEN`. GitHub Copilot login is available in the UI.
