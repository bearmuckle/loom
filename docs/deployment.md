# Deployment

The standalone `loom-server` backend can run on a host or in a container. This
document records where agent commands execute, which account and environment
they inherit, what durable state survives a restart, and which network access is
intended. It records what the backend and the published image do, not what a
deployment could add on top of them. The trust boundary itself is in the
[security and trust model](security.md).

## The desktop client's local worker

The native `loom-ui` client does not embed a backend. Without `--remote` it
starts `loom-server` as a child process (the managed local worker), so the
sections below apply to it too: agent commands run on the host as the desktop
user, with the client's environment.

- The worker binds `127.0.0.1` on a free port; nothing listens beyond loopback.
- Its bearer token is generated per start and written with mode `0600` to
  `<state-dir>/local-worker/token-<client-pid>`; it is passed with
  `--token-file`, never on the command line.
- Its state database is `<state-dir>/state.db` (`--persistence`), the same file
  the client used before it ran a separate worker, so existing sessions carry
  over. `loom-ui --reset-state` is forwarded as `--reset-state`; without it an
  incompatible database stops the worker and the client reports the failure.
- The binary is, in order: `--server-binary`/`LOOM_SERVER_BINARY`; the
  `loom-server` installed next to the client (release archives, AppImage,
  `.deb`) or on `PATH`; a cached download under
  `<cache>/loom/server/<tag>/` (`LOOM_CACHE_DIR` overrides `<cache>`); a
  download from the `bearmuckle/loom` release matching the client's version, or
  the latest release for a development build. A download is installed only
  after its archive matches the release's `SHA256SUMS`. With no network the
  newest cached version is used.
- The client stops the worker with SIGTERM (then a kill after a grace period)
  when it quits. On Linux the worker also receives SIGTERM if the client dies
  without cleaning up.

A download reaches `api.github.com` and GitHub's release asset hosts. A client
installed from an archive, AppImage, or `.deb` already has `loom-server` next
to it and does not download.

## Shared sessions and GitHub sign-in

A worker can be shared by more than one person. People sign in with their own
GitHub account instead of receiving a bearer token of their own; per-person
tokens are retired. Operator bearer tokens remain, but only for headless
clients such as scripts and CI.

- Sign-in is on by default: an upgrade without a credential becomes a
  pre-login connection, and any GitHub account may sign in and then start with
  no session access until an owner grants a role. `loom-server
  --allow-github-login <login>` (repeatable, matched case-insensitively) narrows
  who may sign in at all, which is what a worker reachable from an untrusted
  network wants. `--no-github-sign-in` turns sign-in off, so every client must
  present an operator bearer token.
- The startup log names whether sign-in is enabled, how many logins are
  allowed (or that any GitHub account may), and where the OAuth client id came
  from. Using the embedded client id logs a warning as well.
- `--github-oauth-client-id <id>` selects the OAuth app the device flow uses.
  Without it the worker uses the public GitHub CLI client id compiled into
  `loom-providers`, which works without registering anything but is shared by
  every deployment.
- The driver of a sign-in is a GitHub OAuth app with the device flow enabled. To
  use your own, create one under *Settings → Developer settings → OAuth Apps*,
  enable device flow, and pass its client id; no client secret is needed or
  stored. The flow asks only for the `read:user` scope.
- The resulting GitHub token is read once to obtain the account's numeric id and
  login, and is then dropped. It is never stored as a provider credential, and a
  provider credential is still configured separately (an operator's
  `ConfigureGitHubCopilot`, or a person's own `StartGitHubCopilotLogin`). A
  provider device login asks for `read:user` as well, so when it completes on a
  connection that has no person, the worker binds that account too: the existing
  GitHub sign-in in Providers is also the worker sign-in for a token connection.
- A signed-in person holds no session access until an owner grants a role, so
  sharing a worker does not share its sessions. The exception is an operator
  connection: when the connection that signs in also holds an unrestricted
  operator token, the token keeps deciding what the connection may do, and the
  GitHub identity is attribution on top of it. The worker also gives that
  person the owner role on every session that has no owner, so a later personal
  connection continues to reach them. Such a token already reaches every
  session, so the ownership follows it instead of granting more. A session that
  already has an owner is unchanged, and a pre-login connection adopts nothing
  because it holds no token to follow.

### Signing in from a client

A client holds no per-person token; it signs in on the connection it already
has. Per-person tokens are retired, and an operator token is the only bearer
credential left.

1. Open Settings → Workers, enter the worker URL (for example
   `ws://127.0.0.1:8765/ws`), and leave the access token empty. An empty token
   sends no credential at all: the native client omits the `Authorization`
   header and the browser client offers no `loom.bearer.` subprotocol.
2. Select Connect. The worker accepts the upgrade as a pre-login connection,
   which may only negotiate, read worker status and its own identity, and sign
   in. The client shows a sign-in prompt where the composer would be, and the
   roster, credential, and run controls stay disabled.
3. Select Sign in with GitHub from the composer prompt (or from the People
   tab) and complete the device flow. The pane shows the user code and the
   verification URL and polls the worker until the account is bound. A refused
   or expired sign-in is reported in the pane with the worker's reason. A
   connection that already holds an access token is not asked to sign in: its
   token is the credential, and it keeps operator authority even if it signs in
   for attribution.
4. The worker binds the GitHub account to the connection. The client then loads
   the workspaces, sessions, and roster it was refused while pre-login, and the
   signed-in login appears in the session header and the People tab. Signing in
   is what puts a person in the roster; an owner still grants the role. When the
   client connected with an unrestricted operator token, the worker also makes
   the person the owner of every session that has no owner, before it reports
   the sign-in, so that client's refresh already shows the sessions it owns.
   The client remembers the person per worker and resumes them on a later
   connection with the same operator token, so a client restart shows them
   online without another device flow. A connection accepted without a token
   cannot resume an identity and asks for sign-in again.
5. Select Sign out to end the binding. The person leaves the roster of every
   session they could see, and a connection accepted without a credential
   returns to the sign-in prompt. An operator connection keeps its authority.
   Select Re-authenticate to replace the bound account: the client signs out,
   then starts a fresh device flow, because the worker refuses a second sign-in
   while an identity is bound.

#### Which GitHub flow to use

Three GitHub device flows exist, and the client labels them apart because they
acquire different things:

- **Sign in to this worker** — the sign-in that binds a person to the
  connection. The People tab, the prompt that stands in for the composer, and
  the WORKERS settings row offer it. It grants no repository or model access;
  the person starts read-only until an owner grants a role.
- **Sign in to GitHub Copilot** — Settings → Providers, "GitHub Copilot". It
  gives the worker a GitHub Copilot model-provider credential. It is not a way
  to join the roster: a connection that is waiting for a sign-in is refused the
  provider requests, so the flow cannot complete before someone signs in, and
  the credential belongs to the worker rather than to the person.
- **Authorize GitHub repository access** — Settings → Providers, "GitHub
  repository access". It lets the worker clone, push, and open pull requests
  through the GitHub CLI app. It does not sign the person in to the worker.

A worker sign-in binds the person to one connection, because the client stores
no credential for them. Reloading the page, reconnecting, or opening the worker
in a new tab on a connection accepted without an operator token repeats the
device flow. The client does remember the login that signed in last on a worker
and names them in the sign-in prompt ("Sign in again as …"), but that is a hint
for the person, not a resumed session; only an operator connection with an
unrestricted token resumes an identity, as described in step 4 above.

An operator token still behaves exactly as it did: enter it after the URL, and
the connection has unrestricted authority with no person bound. It may also
sign in on top of the token, and the worker refuses a second sign-in while one
identity is bound, so sign out before switching accounts. Signing in on top of
an unrestricted operator token does not narrow the token: the connection keeps
worker-wide access, repository browsing, and provider administration, and the
person is attributed as the author of what it does. The worker also makes that
person the owner of every session that has no owner, so a later personal
connection continues to reach them.

The browser client is configured with `?remote=<url>` and an optional
`&token=<token>`; a URL alone connects unsigned, and the token no longer has to
appear in the query string. Both clients press the same device flow, which needs
a secure connection (`wss://`, or `ws://` on loopback) because the device code
and the resulting identity travel over it.

Egress required for sign-in, from the backend itself:

- `github.com` — the device authorization (`/login/device/code`) and token
  exchange (`/login/oauth/access_token`).
- `api.github.com` — reading the signed-in account (`/user`).

Both are already part of the intended egress for GitHub remote operations, so a
worker that clones or pushes to GitHub needs no new exception. A deployment that
blocks either endpoint keeps working for operator bearer tokens; only interactive
sign-in fails, with the reason surfaced as a failed sign-in status.

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
- The persistence fail-stop record, `state.db.fail-stop`, which only exists once
  a durable state save failed; see [Persistence fail-stop and
  reopen](#persistence-fail-stop-and-reopen).

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

## Archived sessions and retention

Archived sessions stay in durable state until they are deleted. Deleting an
archived session removes its database rows, its filesystem root, and its linked
worktrees; deleting an archived project root removes every descendant session of
that project in one operation. A linked worktree with changes or a lock refuses
deletion unless the caller forces it. The clone cache beside the session roots
is shared and is never deleted with a session.

`--archive-retention <duration>`, or `LOOM_ARCHIVE_RETENTION`, automatically
deletes archived projects once they are older than the duration, measured from
the archive time. It is disabled by default, so an absent or zero value never
auto-deletes. A duration is `<n><unit>` with unit `ms`, `s`, `m`, `h`, `d`, or
`w`; a bare integer means seconds, and `off` and `never` mean disabled:

```sh
./loom-server --bind 127.0.0.1:8765 --archive-retention 14d
```

The sweep runs at startup and every 15 minutes. It only considers a fully
archived project tree: a descendant that is not archived or a child task that is
not terminal skips the project. `--archive-retention-force-discard`, or
`LOOM_ARCHIVE_RETENTION_FORCE_DISCARD` (default off), lets the sweep discard
dirty or locked worktrees; without it, the sweep skips such a project, logs the
reason, and retries later. The native desktop client runs the same sweep for the
durable backend it embeds, so a retention policy set from the client acts even
without a separate `loom-server` process.

## Persistence fail-stop and reopen

A durable state save can fail for a reason that clears on its own, most often a
filesystem that filled up. The backend then latches a persistence fail-stop: it
keeps the last committed state, refuses every durable request with a
`persistence` error, and stops writing. The latch is deliberate. A
request-scoped save can fail after the handler already changed in-memory state,
so the backend must not keep serving from a state the database does not hold,
and the latch does not clear itself.

`/health` answers `503 persistence fail-stop` while the latch is set, and the
standalone server also exits non-zero so a service manager reopens it from the
last committed state: `Restart=on-failure` in the sample unit, or
`--restart unless-stopped` under Docker. The reopened process resumes what was
last written, so only the work of the failed save is lost, and runs resume from
their last checkpoint.

The exit is delayed, and the delay grows with the number of fail-stops recorded
in `state.db.fail-stop` during the past hour: 30 seconds for the first, doubling
up to a five minute cap. A failure that is still present therefore keeps
retrying at a steady rate instead of restarting in a loop that trips the service
manager's start limit and leaves the unit stopped. Recorded fail-stops expire on
their own, so an instance that has been healthy for an hour reopens at the
shortest delay again. A record that cannot be written at all - a full filesystem
is a likely reason - falls back to that shortest delay, which still spaces the
restarts out.

The native shell's `--serve` mode is started by a person rather than by a
supervisor, so it does not exit: it keeps refusing durable requests and reports
`503` on `/health`.

## Network policy

The intended egress for the backend account is DNS, the configured model
provider endpoints, GitHub, and package registries. Loom does not filter
egress: there is no proxy, allow-list, or per-command network restriction in the
backend. Outbound access from the execution context is what the deployment
grants, and restricting it is the operator's boundary.

Git remote operations are bounded and follow the deployment's proxy:

- Every binary sets libgit2's process-wide socket connect and transfer timeouts
  at startup, before its async runtime exists. A remote that cannot be reached
  within 30 seconds, or that stops transferring for 120 seconds, fails with a Git
  error instead of holding the backend thread that issued the operation until the
  operating system's own TCP timeout. The transfer bound is per stalled read or
  write, so a large clone on a slow but working link still finishes. It bounds a
  stalled remote, it does not retry one, and SSH remotes, which libgit2 runs
  through an external `ssh` process, are not covered.
- A clone, fetch, or push auto-detects the proxy from Git configuration
  (`http.<url>.proxy`) and from `http_proxy`, `https_proxy`, and `no_proxy`,
  exactly as the `git` command line does. This is proxy configuration, not an
  egress filter: Loom still does not restrict what the backend may reach.

The listener side is separate from egress:

- The default bind is loopback, `127.0.0.1:8765`.
- A non-loopback bind needs TLS (`--tls-cert` with `--tls-key`) or the explicit
  `--allow-insecure-remote` opt-in, which logs a warning naming the flag that
  permitted the bind.
- One WebSocket frame is limited to 4 MiB per side. `--max-frame-bytes <bytes>`,
  or `LOOM_MAX_FRAME_BYTES`, changes that bound for the listener and for a
  native client that reads the same variable. The protocol has no chunking, so a
  request or response above the limit fails as `payload_too_large` naming the
  request, the received byte size, and the configured limit; a frame that names
  no request closes the connection with the same explanation. Raise the limit
  when a workspace listing, file read, transcript page, or tool result
  legitimately exceeds it.
- A container's network namespace is whatever the runtime gives it, which is
  the `--network` the container was started with.

Approval policy is not an egress filter. Network-classified tool actions
require approval under the default `ApprovalPolicy`, and command actions do
too, so a command that reaches the network is gated by the command decision
rather than by a network check. Auto approve mode allows writes,
commands, and network actions without prompting, so approval gating does not
restrict egress either.

Destructive actions are allowed in both modes, so they are an egress path:
`github_push_branch` and `github_remove_labels` are classified as destructive
and the default policy and auto approve both allow them. An operator who wants
to gate either tool sets the destructive dimension to `RequireApproval` for the
session with the `SetSessionApprovalPolicy` request, which then prompts per
instance, or to `Deny` for a hard refusal; `run_command` stays a command action
so builds and tests keep working under both policies.

An operator can verify the policy inside the execution context:

```sh
getent hosts github.com
curl -fsS -o /dev/null https://api.github.com/
curl -fsS -o /dev/null https://index.crates.io/config.json
```

The package-registry check targets the crates.io index that Cargo itself uses
rather than the website root: `crates.io` answers `403` to non-browser clients,
which proves reachability but fails `curl -f`.

Run them in the context that executes agent commands, for example with
`docker exec loom-server ...` for a container deployment or as the backend
account on a host. The release pipeline's image smoke test runs these checks in
the published image, so the documented policy is verified against the shipped
image.

## Telling deployments apart in diagnostics

`loom-server --diagnostics` prints one `key: value` line per fact, after a first
line naming the server version, and exits without touching state: it resolves
paths without creating them, opens no database, and never generates a token. A
containerized deployment whose state directory is not on a filesystem of its own
also gets a final `warning:` line, because its workspaces are removed with the
container. It reports:

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
- `state-directory-filesystem`: whether the state directory is on a
  filesystem of its own, which is what a mounted volume looks like, or on the
  same filesystem as its parent, which is the container's own writable layer.
  The line reads `separate filesystem`, `same filesystem as its parent`, or
  `unknown` when the comparison cannot be made.
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
overridden `--bind` is followed instead of a hardcoded 8765, and an override that
adds `--tls-cert` is probed over HTTPS.

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
