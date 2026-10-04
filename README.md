# Loom

Loom is a remote-first coding-agent environment. Its Rust backend manages agent
sessions, workspace state, tool execution, and model integrations. The GPUI
client runs natively or in a browser and can connect to a local or remote
backend.

Loom is under active development. It is usable today, but it has not reached
1.0, so interfaces, configuration, protocol, and on-disk state may still change
between releases. Tagged GitHub releases include native UI client binaries for Linux x86_64, macOS arm64 (Apple silicon),
and Windows x86_64. CI builds and tests on Linux; other operating systems and
browsers are not yet documented as supported targets.

## What is included

Loom provides a native client, a browser client, a deterministic demo mode,
persistent sessions, approval-gated agent tools, workspace and diff review, and
a standalone WebSocket backend. Provider support includes a deterministic
provider, OpenAI, DeepSeek, OpenAI-compatible and Ollama adapters, and GitHub
Copilot login. Projects can coordinate delegated child agents on isolated
worktrees.

## Current limitations

Loom does not provide OS-level process sandboxing or a complete multi-user
identity system. A session filesystem root is a path ownership boundary, not a
sandbox for commands. The SQLite database is not an encrypted credential vault.
Review the [security and trust model](docs/security.md) before using Loom with
sensitive data or exposing it beyond a trusted boundary.

The browser client cannot set authorization headers on a `WebSocket`, so remote
browser connections carry the bearer token in a `loom.bearer.<token>`
subprotocol instead of the connection URL, keeping it out of browser history
and infrastructure logs. Use short-lived, narrowly scoped credentials and a
trusted TLS deployment boundary. The standalone backend speaks plain `ws://`
and must not be exposed directly to an untrusted network.

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

When no `--project` is given and Loom is launched from inside its own state
directory (for example by a desktop environment that picks an internal
session root as the working directory), Loom skips that directory and opens the
empty project picker instead of adopting it as a project.

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

## Providers

Configure the official OpenAI provider by entering an API key for the selected
worker in the Providers dialog. OpenAI models can then be discovered and
selected for agent runs. The default model is `gpt-6-luna`; override it with
`LOOM_OPENAI_MODEL`. The official DeepSeek provider is configured the same way;
its default model is `deepseek-flash`, overridable with `LOOM_DEEPSEEK_MODEL`.
For an OpenAI-compatible gateway, configure
`LOOM_OPENAI_ENDPOINT`, `LOOM_API_KEY`, and `LOOM_MODEL`. API keys entered in
the dialog are stored in a credential file next
to that backend's SQLite database, separately for each backend installation.
GitHub Copilot login is available in the UI. Treat provider credentials as
sensitive; they are used by the backend, and backend state storage is not an
encrypted secret vault.

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
