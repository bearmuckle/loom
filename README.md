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

Requires Rust 1.95 or newer.

```text
cargo build
cargo run -p loom-ui -- --workspace /path/to/project
```

Demo mode:

```text
cargo run -p loom-ui -- --demo
```

Run the full Rust check suite locally:

```text
./scripts/ci-check.sh
```

To check formatting automatically before every commit, enable the repository
hook once:

```text
git config core.hooksPath .githooks
```

The commit hook runs `cargo fmt --all -- --check` without compiling Rust. The
full Clippy, test, and build checks run in CI and can be run locally with
`./scripts/ci-check.sh`.

Browser target (GPUI currently requires a nightly compiler for wasm atomics):

```text
RUSTC_BOOTSTRAP=1 cargo check -p loom-ui --target wasm32-unknown-unknown
```

The wasm binary is a browser-hosted GPUI client; native-only workspace,
process, and credential integrations remain available through the remote
backend boundary.

Run the backend and browser client together for local WASM development:

```text
./scripts/dev-wasm.sh
```

The launcher starts a deterministic/demo backend on `127.0.0.1:8765`, waits
for its health endpoint, then starts Trunk from `crates/loom-ui` on
`127.0.0.1:8080`. It prints a complete URL containing the WebSocket token and
workspace path; open that URL in a browser. Press `Ctrl-C` to stop both
processes. Set `LOOM_MODEL`, `LOOM_TOKEN`, `LOOM_BACKEND_BIND`,
`LOOM_FRONTEND_PORT`, or `LOOM_WORKSPACE` to override the local defaults.
Without a URL or saved worker connection, the full client opens disconnected;
connect a worker from Settings. Worker settings are saved in browser storage.

The browser client is deployed to Vercel on pushes to `main` and can also be
published manually from the Actions tab. Configure a Vercel project and the
`VERCEL_TOKEN`, `VERCEL_ORG_ID`, and `VERCEL_PROJECT_ID` GitHub Actions secrets
to enable deployment. The client requires a separately running Loom backend.

Configure providers with `LOOM_OPENAI_ENDPOINT`, `LOOM_API_KEY`, and
`LOOM_MODEL`, or connect to a remote backend with `LOOM_REMOTE_URL` and
`LOOM_TOKEN`. GitHub Copilot login is available in the UI.
