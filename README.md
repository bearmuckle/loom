# Loom

Loom is a remote-first coding-agent environment. A Rust backend owns agent
sessions, tool execution, workspace state, and model integrations; a GPUI client
runs natively or in the browser and attaches to a local or remote backend.

Loom is pre-1.0, so interfaces, configuration, and on-disk state can still
change between releases. Review the [security and trust model](docs/security.md)
before using it with sensitive data or exposing it beyond a trusted boundary.

Try the deterministic demo, which needs no provider or backend:
https://bearmuckle.github.io/loom/?demo=true

## What you get

- One GPUI client for native desktop and the browser.
- Backend-owned sessions that survive disconnects, reconnects, and restarts.
- Approval-gated tools with workspace, diff, and task review.
- Providers: OpenAI, DeepSeek, OpenAI-compatible endpoints, Ollama, and GitHub
  Copilot login.
- Coordinated child agents on isolated worktrees.

## Build and run

Install Rust 1.95 or newer. On Linux, the native UI also needs the system
packages listed in [CI](.github/workflows/ci.yml).

```sh
cargo run -p loom-ui
```

Loom uses the directory you launch it from. Point it at another project with
`--project PATH`, or start the provider-free demo with `--demo`:

```sh
cargo run -p loom-ui -- --project /path/to/project
cargo run -p loom-ui -- --demo
```

For browser development, run `./scripts/dev-wasm.sh`. It needs Trunk and the
`wasm32-unknown-unknown` Rust target, serves both services on loopback, and puts
a development bearer token in the browser URL, so keep it local. Pushes to
`main` publish the browser client to https://bearmuckle.github.io/loom/.

## Standalone server

Tagged releases and the container image also ship `loom-server`, the backend the
client embeds, as a standalone process. It needs no `--persistence` path or
token: it generates a bearer token in its instance directory and keeps durable
state below the state root, so a stable bind address resumes where it left off.

```sh
tar -xzf loom-server-<tag>-linux-x86_64.tar.gz && chmod +x loom-server
./loom-server --bind 127.0.0.1:8765
```

A non-loopback listener requires TLS (`--tls-cert` and `--tls-key`) or the
explicit `--allow-insecure-remote` opt-in. See [deployment](docs/deployment.md)
for the state layout, token handling, retention, TLS, egress, and the container
image:

```sh
docker run --rm -p 127.0.0.1:8765:8765 ghcr.io/<owner>/loom-server:<tag>
```

## Providers

Configure OpenAI, DeepSeek, or GitHub Copilot in the Providers dialog; the
default models are `gpt-6-luna` and `deepseek-flash`, overridable with
`LOOM_OPENAI_MODEL` and `LOOM_DEEPSEEK_MODEL`. For an OpenAI-compatible gateway,
set `LOOM_OPENAI_ENDPOINT`, `LOOM_API_KEY`, and `LOOM_MODEL`. API keys are stored
in a credential file beside the backend's SQLite database and are used by the
backend; treat them as sensitive and note that state storage is not an encrypted
vault.

## Documentation

- [Architecture](docs/architecture.md)
- [Deployment](docs/deployment.md)
- [Security and trust model](docs/security.md)
- [Product scope](docs/product.md)
- [Protocol](docs/protocol.md)

## Development

Run the full Rust check suite with `./scripts/ci-check.sh`. To check formatting
on every commit, point Git at the bundled hooks:

```sh
git config core.hooksPath .githooks
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the workflow. Report security issues
privately per [SECURITY.md](SECURITY.md).

## License

Individual crates declare their license in their `Cargo.toml`. The reusable core
crates are GPL-3.0-only; `loom-protocol` and `loom-server` are AGPL-3.0-only.
The `loom-ui` and `loom-cli` applications depend on the AGPL-licensed protocol,
so Loom application distributions are offered under AGPL-3.0-only. See
[LICENSES.md](LICENSES.md) and [COPYRIGHT](COPYRIGHT) for details.
