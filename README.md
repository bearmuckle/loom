# Loom

> Loom is early experimental software. Interfaces and behavior may change. It
> is not suitable for production or sensitive workloads.

Loom is an experimental remote-first coding-agent environment. Its Rust
backend manages agent sessions, workspace state, tool execution, and model
integrations. The GPUI client runs natively or in a browser and can connect to
a local or remote backend.

The current project is a developer preview, not a supported general-purpose
product. It is under active development; behavior and configuration may change
between commits. Tagged GitHub releases include native UI client binaries for
Linux x86_64, macOS x86_64 and arm64, and Windows x86_64. These preview builds
have no compatibility guarantees. CI currently builds on Linux; other
operating systems and browsers have not been documented as supported targets.

## Current capabilities and limitations

The repository currently includes a native client, a browser client, a
deterministic demo mode, persistent sessions, approval-gated agent tools,
workspace and diff review, and a standalone WebSocket backend. Provider code
includes a deterministic provider, OpenAI-compatible and Ollama adapters, and
GitHub Copilot login.

The browser client is still a development surface. Remote browser connections
pass a bearer token in the WebSocket URL because browser WebSocket APIs cannot
set authorization headers. URLs can be retained in browser history and
observed by infrastructure logs; use short-lived, narrowly scoped credentials
and a trusted TLS deployment boundary. The standalone backend speaks plain
`ws://` and must not be exposed directly to an untrusted network. See the
[security and trust model](docs/security.md).

Loom does not provide OS-level process sandboxing or a complete multi-user
identity system. A session filesystem root is a path ownership boundary, not
a sandbox for commands. The SQLite database is not an encrypted credential
vault. Do not use Loom with sensitive workloads or treat it as a hardened
multi-user service.

## Why Loom

- **Cross-platform performance:** A GPUI-based Rust interface can run as a
  desktop or browser runtime instead of depending on a Chromium-based desktop runtime such as Electron.
- **Remote-first workflows:** Agents can run near repositories and services,
  without tying development work to a single computer.
- **User-controlled trust:** Loom can run on infrastructure you control, with
  open code and explicit permissions for workspace access, tools, and
  credentials.

## Build and run

Install Rust 1.95 or newer. On Linux, the native UI build also needs the system
packages listed in [CI](.github/workflows/ci.yml). Then run:

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
the `wasm32-unknown-unknown` Rust target. The helper starts both services on
loopback by default and places its development bearer token in the browser
URL. Keep it local; do not expose this setup to a network. For a token-free
browser walkthrough, build the WASM client and open it with `?demo=true`.
Pushes to `main` also publish the browser client to
https://bearmuckle.github.io/loom/ through GitHub Pages. Add `?demo=true` to
the URL to open the deterministic demo without a backend.

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
`LOOM_TOKEN`. GitHub Copilot login is available in the UI. Treat provider
credentials as sensitive; they are used by the backend, and backend state
storage is not an encrypted secret vault.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow and
reporting expectations. Security issues should be reported privately; see
[SECURITY.md](SECURITY.md).

## License

Individual crates declare their license in their `Cargo.toml`. The reusable
core crates are GPL-3.0-only; `loom-protocol` and `loom-server` are
AGPL-3.0-only. The distributable `loom-ui` and `loom-cli` applications depend
on the AGPL-licensed protocol, and the native client also includes the server;
Loom application distributions are therefore offered under AGPL-3.0-only.
See [LICENSES.md](LICENSES.md) for the crate-by-crate breakdown and
[COPYRIGHT](COPYRIGHT) for project attribution.
