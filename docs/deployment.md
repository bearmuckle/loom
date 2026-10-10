# Deployment

The standalone `loom-server` backend can run on a host or in a container. This
document records where agent commands execute, which account and environment
they inherit, what durable state survives a restart, and which network access is
intended. It records what the backend and the published image do, not what a
deployment could add on top of them. The trust boundary itself is in the
[security and trust model](security.md).

## Where agent commands execute

Loom has no separate command sandbox. An agent command runs as a child process
of the backend, in the same context the backend runs in.

- On a host deployment, which is an unpacked binary or the sample systemd unit,
  a command runs on the host. It runs as the backend account, inherits the
  backend process environment, and starts with the session filesystem root as
  its working directory.
- In the published container image, the backend and therefore every agent
  command it starts run inside that one container. The container's filesystem,
  users, and network namespace are the boundary; there is no second container
  per command, and the image does not add a sandbox of its own.

The task supervisor starts a task with `Command::new`, resolves the working
directory below the session filesystem root, and does not clear the
environment, so a child process inherits the backend's environment as-is. The
command tool starts its process the same way, and sets no environment of its
own.

A session filesystem root is a path-ownership boundary, not an OS sandbox. It
constrains which paths Loom's own tools may read and write through canonical
path checks and outside-root symlink rejection. It does not restrict what a
spawned process may reach through the filesystem it inherits. See the
[security and trust model](security.md).

## Which account and environment

- The backend runs as one account. The sample systemd unit runs it as the
  dedicated non-root `loom` user and group; the container image creates a
  system `loom` user and starts the backend as that user. `loom-server` does not
  drop privileges, change users, or change groups, either for itself or per
  command.
- The deployment must constrain that account's filesystem permissions. The
  account's reach is the reach of every agent command, because Loom neither
  sandboxes nor re-credentializes a command.
- A command inherits the backend process environment. That includes any
  credential-bearing variable the operator exported for the backend, such as
  `LOOM_API_KEY`, `LOOM_OPENAI_ENDPOINT`, or a repository token placed in the
  environment. Anything visible to the backend process is visible to a command.
- Loom does not drop privileges per command. There is no per-command user,
  group, capability set, or environment filter. Treat the backend account and
  its environment as the privilege of the agent.

## Persistence and lifecycle

The state root is resolved in this order, and a `loom` subdirectory is appended
to it in every case:

1. `LOOM_STATE_DIR`, used verbatim as the state root.
2. `$XDG_STATE_HOME`.
3. `$HOME/.local/state`.
4. A directory in the system temp folder.

The instance directory lives below that state directory. It is named by
`--instance-name` when one is given, and otherwise by the bind key, which is the
bind address with `:` replaced by `_`, so `127.0.0.1:8765` becomes
`127.0.0.1_8765`. The instance directory and the parent of the default token
file are created with mode `0700`. A bind port of 0 has no stable identity, so
the backend stays in memory and the default token file falls back to
`<state-dir>/token`.

The instance directory holds:

- The state database, `state.db`.
- The bearer token file, `token`, written with mode `0600` and reused on later
  starts.
- The credential file, `state.credentials.json`.
- The session filesystem roots under `state.session-roots`.
- The cached clone mirrors under `state.clone-cache`.
- The owner lock, `state.db.loom-owner.lock`.

An explicit `--persistence` names the database path instead, and the credential
file, session roots, clone cache, and owner lock are then derived from that
path.

Without a mounted volume, the instance directory is the container's writable
layer. `docker rm`, `docker run --rm`, or replacing the image with a newer tag
removes that layer, and with it the database, the workspaces, and the session
roots. A named volume or a bind mount of the state root is what makes them
outlive the container. The image sets `LOOM_STATE_DIR=/var/lib/loom`, so the
path to mount is `/var/lib/loom`.

- Use `--restart unless-stopped` so the backend comes back after a host reboot
  or a crash.
- `--rm` removes the container and its writable layer when it exits. It does
  not remove named volumes, so a mounted state root survives it.
- `--reset-state` wipes a state database this build cannot open and starts
  empty. It is the documented recovery for an incompatible database and has
  nothing to reset on an in-memory instance.
- A second start on the same mounted state root reuses the existing token in
  `token`; it is not regenerated. A generated token is logged as a warning and
  its value is printed only when stdout is a terminal.

An ephemeral deployment, where everything is removed with the container:

```sh
docker run --rm -p 127.0.0.1:8765:8765 \
  ghcr.io/<owner>/loom-server:<tag>
```

A persistent deployment with a named volume:

```sh
docker volume create loom-state
docker run -d --name loom-server --restart unless-stopped \
  -p 127.0.0.1:8765:8765 \
  -v loom-state:/var/lib/loom \
  ghcr.io/<owner>/loom-server:<tag>
```

A persistent deployment with a bind mount, where the token is read from the
state root on the host:

```sh
mkdir -p /srv/loom-state
docker run -d --name loom-server --restart unless-stopped \
  -p 127.0.0.1:8765:8765 \
  -v /srv/loom-state:/var/lib/loom \
  ghcr.io/<owner>/loom-server:<tag>

sudo cat /srv/loom-state/loom/loom-server/token
```

The token can also be read from inside the container:

```sh
docker exec loom-server cat /var/lib/loom/loom/loom-server/token
```

A fixed token avoids reading the generated one, and `--token-file` is preferred
over `--token`, whose value any local user could read from `/proc`. The named
file must already exist and be readable by the backend account, because an
operator-named token file is read and never created:

```sh
docker run -d --name loom-server --restart unless-stopped \
  -p 127.0.0.1:8765:8765 -v loom-state:/var/lib/loom \
  ghcr.io/<owner>/loom-server:<tag> --token-file /run/secrets/loom-token
```

## Network policy

The intended egress for the backend account is DNS, the configured model
provider endpoints, GitHub, and package registries. Loom does not filter
egress: there is no proxy, allow-list, or per-command network restriction in the
backend. Outbound access from the execution context is what the deployment
grants, and restricting it is the operator's boundary.

The listener side is separate from egress:

- The default bind is loopback, `127.0.0.1:8765`.
- A non-loopback bind needs TLS (`--tls-cert` with `--tls-key`) or the explicit
  `--allow-insecure-remote` opt-in, which logs a warning naming the flag that
  permitted the bind.
- A container's network namespace is whatever the runtime gives it, which is
  the `--network` the container was started with.

Approval policy is not an egress filter. Network-classified tool actions
require approval under the default `ApprovalPolicy`, and command actions do
too, so a command that reaches the network is gated by the command decision
rather than by a network check. Auto approve mode allows non-destructive writes,
commands, and network actions without prompting, so approval gating does not
restrict egress either.

An operator can verify the policy inside the execution context:

```sh
getent hosts github.com
curl -fsS -o /dev/null https://api.github.com/
curl -fsS -o /dev/null https://crates.io/
```

Run them in the context that executes agent commands, for example with
`docker exec loom-server ...` for a container deployment or as the backend
account on a host. The release pipeline's image smoke test runs these checks in
the published image, so the documented policy is verified against the shipped
image.

## Telling deployments apart in diagnostics

`loom-server --diagnostics` prints one `key: value` line per fact and exits
without touching state. It reports:

- `execution-context`: whether the backend runs on a host or in a container.
- `container-runtime`: the container runtime that was detected, when there is
  one.
- `deployment-role`: the role this deployment declares.
- `process-account`: the user the backend process runs as.
- `working-directory`: the backend process working directory.
- `state-root`: the resolved state root.
- `state-directory`: Loom's own state directory below that root.
- `state-persistence`: whether durable state exists or the backend is in
  memory.
- `state-directory-filesystem`: the filesystem type holding the state root.
- `instance-directory`: the instance directory for this bind address.
- `state-database`: the state database path, or the in-memory backend.
- `agent-commands`: where agent commands execute.
- `agent-command-environment`: what environment an agent command inherits.
- `bind-address`: the listener bind address.
- `remote-transport`: whether the listener serves `ws://` or `wss://`.
- `remote-exposure`: whether the listener is loopback or reachable beyond it.
- `bearer-token`: the token file path and where the token came from. The value
  is never printed.
- `network-egress`: the egress this deployment intends to allow.

A container is labelled so that a real agent execution container can be told
apart from a one-off smoke test. Containers started from the image carry
`loom.deployment-role=agent-worker`. The release pipeline's server-image smoke
tests run labelled `loom.deployment-role=server-image-smoke-test` and with
`LOOM_DEPLOYMENT_ROLE=server-image-smoke-test` set, so a filter separates them:

```sh
docker ps --filter label=loom.deployment-role=agent-worker
docker ps --filter label=loom.deployment-role=server-image-smoke-test
```

The image health check probes the port from the server's own command line, so an
overridden `--bind` is followed instead of a hardcoded 8765. An override that
changes the TLS scheme still needs its own probe.

## Operator checklist

1. Pick a host or container deployment deliberately, because it fixes the
   execution context and the privilege of every agent command.
2. Pin the state root to a volume, or to a directory the backend account owns
   on a host, so workspaces, session roots, and the database survive a restart
   or an image replacement.
3. Set the token explicitly, preferably with `--token-file`, or read the
   generated one from the state root and give it only to intended clients.
4. Keep the listener on loopback, or terminate TLS with `--tls-cert` and
   `--tls-key`, and treat `--allow-insecure-remote` as a temporary choice on a
   trusted network.
5. Decide the egress boundary yourself, since Loom does not filter egress.
6. Confirm the deployed context with `loom-server --diagnostics` and re-run the
   verification commands from the network policy section in that context.
