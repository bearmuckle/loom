# Loom

Loom is a remote-first coding-agent environment. Its Rust backend manages agent
sessions, workspace state, tool execution, and model integrations. The GPUI
client runs natively or in a browser and can connect to a local or remote
backend.

Loom is under active development. It is usable today, but it has not reached
1.0, so interfaces, configuration, protocol, and on-disk state may still change
between releases.

Tagged GitHub releases ship two binaries, the `loom-ui` desktop client and the
standalone `loom-server` backend. Both report the release tag for `--version`
and print usage for `--help`. Both arrive as archives for Linux x86_64 and
aarch64 and for Windows x86_64, and as one universal binary for macOS that runs
on Apple silicon and Intel. The Linux x86_64 and aarch64 builds of the client
also come as an AppImage and a `.deb` package. Every release additionally
carries a CycloneDX SBOM of the tagged dependencies, `SHA256SUMS`, signed SLSA
build provenance for each artifact, and a `ghcr.io/<owner>/loom-server`
container image.

On Linux, the AppImage is the self-contained path and needs no installation,
while the `.deb` installs the client together with a desktop entry and icon.
The x86_64 Linux artifacts are built on `ubuntu-22.04` and need glibc 2.35 or
newer. The aarch64 artifacts come from a newer arm64 runner image, so they need
a newer glibc than the x86_64 ones; there is no 22.04 arm64 image to align them
with. The macOS and Windows builds are neither notarized nor code signed, so
Gatekeeper and SmartScreen warn about them on first run.

CI builds and tests on Linux; other operating systems and browsers are not yet
documented as supported targets.

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
trusted TLS boundary. The standalone backend terminates TLS itself when given
`--tls-cert` and `--tls-key`, and refuses a plaintext listener beyond loopback
unless the operator passes `--allow-insecure-remote`. A plaintext listener must
not be exposed directly to an untrusted network.

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

## Standalone server

Released archives and the container image also carry `loom-server`, the backend
the client embeds, as a separate process. Its archives unpack the same way as
the client's, and it also reports `--version` and `--help`:

```sh
tar -xzf loom-server-<tag>-linux-x86_64.tar.gz
chmod +x loom-server
./loom-server --version
```

Neither `--persistence` nor a token is required. Without `--persistence`, a
bind address with a stable port keeps durable state in
`<state-dir>/<instance-name or bind-key>/state.db`. An explicit `--persistence`
path still wins over that default. `<state-dir>` is `LOOM_STATE_DIR` used
verbatim, else `$XDG_STATE_HOME`, else `$HOME/.local/state`, else a directory in
the system temp folder, with a `loom` subdirectory appended in every case. The
bind key is the bind address with `:` replaced by `_`, so `127.0.0.1:8765`
becomes `127.0.0.1_8765`:

```sh
./loom-server --bind 127.0.0.1:8765
```

The instance directory is created with mode `0700` and holds the database, the
token, and the files derived from the database path: the credential file, the
session roots, the cached clone mirrors, and the owner lock. `--instance-name`
pins one directory for a host-aliased bind, so `0.0.0.0:8765` and
`127.0.0.1:8765` can share it:

```sh
./loom-server --bind 0.0.0.0:8765 --allow-insecure-remote \
  --instance-name loom-server
```

A bind port of 0 has no stable identity: the backend stays in memory and the
default token file falls back to `<state-dir>/token`.

Archived sessions stay in that durable state until they are deleted. Deleting
an archived session removes its database rows, its filesystem root, and its
linked worktrees; deleting an archived project root removes every descendant
session of that project in one operation. A linked worktree with changes or a
lock refuses the deletion unless the caller asks for it to be forced. The
node-level clone cache beside the session roots is shared and is never deleted
with a session.

Without `--token` or `--token-file` the server reads `<instance-dir>/token` and,
when that file is missing or empty, generates a `loom-<uuid>` token there. The
file is written atomically with mode `0600`. The path is always logged, a
generated token is logged as a warning, and the value itself is printed only
when stdout is a terminal, so it does not land in `journald` or `docker logs`:

```sh
./loom-server --bind 127.0.0.1:8765
# log line: Reusing the bearer token in '<instance-dir>/token'
```

`--token` and `--token-file` remain explicit, mutually exclusive overrides.
`--token-file` reads the trimmed contents of a file instead of taking the token
on the command line, where other local users could read it through `/proc`.
`--reset-state` wipes an incompatible state database instead of refusing to
start; an instance with no durable database has nothing to reset and rejects
the flag.

`--archive-retention <duration>`, or `LOOM_ARCHIVE_RETENTION`, deletes archived
projects automatically once they are older than the duration, measured from
the archive time. Retention is disabled by default, so an absent or zero value
never auto-deletes. A duration is `<n><unit>` with unit `ms`, `s`, `m`, `h`,
`d`, or `w`; a bare integer means seconds, and `off` and `never` mean disabled:

```sh
./loom-server --bind 127.0.0.1:8765 --archive-retention 14d
```

The sweep runs at startup and periodically. It only considers a fully archived
project tree: a descendant that is not archived or a child task that is not
terminal skips the project. `--archive-retention-force-discard`, or
`LOOM_ARCHIVE_RETENTION_FORCE_DISCARD` (default off), lets the sweep discard
dirty or locked worktrees; without it, the sweep skips such a project, logs
the reason, and retries later.

The unauthenticated `GET /health` endpoint answers `ok`. A bind address that is
not loopback is refused unless the listener serves TLS or the operator opts in
to plaintext. `--tls-cert` and `--tls-key` are required together and make the
listener serve `wss://`, with `/health` over TLS as well:

```sh
./loom-server --bind 0.0.0.0:8765 \
  --tls-cert /path/to/cert.pem --tls-key /path/to/key.pem
```

`--allow-insecure-remote` accepts the risk of a plaintext listener instead: the
bearer token and every protocol frame travel unencrypted, and the server logs a
warning naming the flag that permitted the bind. This is a deliberate
compatibility change: a plaintext remote setup that worked before now needs
either TLS or that flag. A native client that connects to a plaintext
non-loopback worker refuses it for the same reason and takes the same flag,
while loopback connections never need it. When a worker presents a private or
self-signed certificate, the client adds that CA to its OS trust roots with
`--ca /path/to/ca.pem` (or `LOOM_TLS_CA`); the addition is additive and
certificate verification is never disabled.

`loom --serve` resolves the same defaults through the same resolver and also
accepts `--instance-name` and `--token-file`, so an explicit token is no longer
necessary there either.

`packaging/loom-server.service` is a sample systemd unit that runs the server
as a non-root user with `LOOM_STATE_DIR=/var/lib/loom`, restarts it on failure,
and starts it after the network is up.

A container image is published for the same tag:

```sh
docker pull ghcr.io/<owner>/loom-server:<tag>
docker run --rm -p 127.0.0.1:8765:8765 \
  ghcr.io/<owner>/loom-server:<tag>
```

The image's default command binds `0.0.0.0:8765` inside the container with
`--allow-insecure-remote` and pins `--instance-name loom-server` below
`LOOM_STATE_DIR=/var/lib/loom`, because that is what makes a published port
reachable. The first start generates the token in that instance directory, so
`docker exec <container> cat /var/lib/loom/loom/loom-server/token` reads it, or
set a fixed value with `--token` or `--token-file` instead. Publish the port
only to trusted peers, or pass your own TLS material with `--tls-cert` and
`--tls-key` and drop the opt-in. The image's `HEALTHCHECK` derives its host, port
and scheme from the server's own command line, so an overridden `--bind` or a TLS
override is followed without overriding the probe.

Where agent commands execute, which account and environment they inherit, what
the state root keeps across a restart, and the intended network policy are
documented in [deployment](docs/deployment.md).

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
