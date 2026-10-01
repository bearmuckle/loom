# About pane improvement plan

Status: proposed
Scope: `crates/loom-ui` client presentation. Almost everything below is already
available in the client; the only cross-crate change is retaining the result of
protocol negotiation so version skew can be shown.

## Motivation

Settings → About (`crates/loom-ui/src/view/render/dialogs.rs:1155-1187`) renders
exactly three lines:

- the product name "Loom" (`dialogs.rs:1171`),
- the subtitle "Local agent" (`dialogs.rs:1172-1177`),
- "Version 0.1.0" from `env!("CARGO_PKG_VERSION")` (`dialogs.rs:1178-1184`).

That is the only place in the client where a user can find out what they are
running, and it is the only surface that could answer "which build is this, what
is it connected to, and where do I report a problem". Today it answers none of
those well. The rest of the app assumes the reader already knows the answers:
backend mode is only implied by other panes, the state database path is only
logged at startup (`crates/loom-ui/src/view/lifecycle/init.rs:52-56`), the
license and third-party inventory live in files on disk, and `SECURITY.md` asks
for "the affected revision" that the client cannot display.

## Current implementation map

| Concern | Location |
| --- | --- |
| About pane body | `crates/loom-ui/src/view/render/dialogs.rs:1155-1187` |
| Settings shell (header, nav, scroll container) | `crates/loom-ui/src/view/render/dialogs.rs:609-1200` |
| Section enum and labels | `SettingsSection`, `SETTINGS_SECTIONS` in `crates/loom-ui/src/view.rs:134-149` |
| Section navigation | `settings_nav` in `crates/loom-ui/src/view.rs:817-877` |
| Shared row/card/heading helpers | `crates/loom-ui/src/view.rs:738-812` |
| Open-a-URL helper | `open_external_url` in `crates/loom-ui/src/view/helpers.rs:722-739` |
| Clipboard writes | `cx.write_to_clipboard` in `crates/loom-ui/src/view/providers.rs:64`, `view/render/inspector/changes.rs:467` |
| Render tests | `settings_about_and_providers_panes_render` (`view/tests/loom_view_render_tests.rs:754-766`), `settings_sections_and_dialogs_render` (`view/tests/render_state_tests.rs:19-39`) |
| App license | `license = "AGPL-3.0-only"` in `crates/loom-ui/Cargo.toml:6`; `LICENSES.md`, `COPYRIGHT` |
| Reporting channels | `SECURITY.md` (private advisory form), `README.md` (docs site, releases) |

## Problems

### 1. The subtitle is wrong for most launch modes

"Local agent" is a fixed string. The client can run an in-process backend, a
remote backend (`--remote ws://…`, `crates/loom-local/src/lib.rs:221-232`), the
deterministic demo backend (`--demo`), or, in the browser build, connect to a
remote worker with no local component at all. In the remote and browser cases
the agents are not local, and in demo mode nothing is a real agent. The pane
never consults `demo_workspace` (`view.rs:542`), `browser_demo_mode`
(`view.rs:398-399`), or the worker connection state, so it asserts something
the client cannot back up.

### 2. Version alone does not identify a build

Every crate uses the workspace version (`version = "0.1.0"` in `Cargo.toml`
`[workspace.package]`), and no crate has a `build.rs`, so there is no compiled-in
revision, platform, or profile. `SECURITY.md` asks reporters for "the affected
revision"; the About pane cannot supply one, and the published artifacts are
preview builds with no compatibility guarantees (`README.md`).

### 3. The pane ignores the connection the client actually has

`LoomView` already holds what a useful summary needs: `worker_nodes` with
`connection_state`/`connection_detail`/redacted `url` (`view.rs:356-374, 549`),
`node_names` (`view.rs:411`), `default_backend_node_id` (`view.rs:407`),
`workspace_id` (`view.rs:414`), the browser workspace/model (`view.rs:557-559`),
and the default model. The pane shows none of it. This is the "what am I talking
to" question that a remote-first client should answer in one line, with the
Workers pane (`SettingsSection::Workers`) as the place to manage the details —
About must not duplicate that list.

### 4. The negotiated protocol version and capabilities are discarded

`negotiate` (`crates/loom-ui/src/connection.rs:298-310`) matches
`ControlResponse::Negotiated(_)` and drops the `NegotiationResult`
(`crates/loom-protocol/src/lib.rs:478-482`) that carries the server's
`ProtocolVersion` and `CapabilitySet`. Version skew is a realistic support case
(`unsupported_version_error`, `crates/loom-protocol/src/lib.rs:545-553`), and
the client currently has nothing to display for it. Storing the negotiated
result on the view is a small change that makes a client/server version row
possible.

### 5. No licensing or attribution surface

`loom-ui` and `loom-cli` are distributed under AGPL-3.0-only, the reusable core
crates are GPL-3.0-only, and the crate-by-crate inventory plus attribution live
in `LICENSES.md` and `COPYRIGHT` (`README.md` "License"). None of that is
reachable from the running app; a web build ships the same code with no visible
license or acknowledgements at all.

### 6. No support path

There is no way to learn where Loom keeps state
(`backend_persistence_path()` → `$LOOM_STATE_DIR`/`$XDG_STATE_HOME`/`~/.local/state`
`/loom/state.db`, `crates/loom-local/src/lib.rs:405-417`), no way to copy a
build/environment summary (the clipboard is already used elsewhere), and no link
to the issue tracker or the private vulnerability-reporting form described in
`SECURITY.md`.

### 7. No external links, although the helper exists

`open_external_url` is implemented for native and wasm and already used for the
GitHub device-code flow (`dialogs.rs:466`) and tool result links
(`view/render/tool.rs:458`), but the About pane offers no pointer to the
repository, documentation, releases, or the security policy.

### 8. Undiscoverable entry point

Settings has an About section, but the command palette and `help` command only
know `settings` (`COMMANDS` in `view.rs:170-213`), and there is no shortcut. A
user looking for version information starts in the palette, not in Settings.
`open_providers_from_menu` (`view/providers.rs:935-938`) is the pattern to
mirror for an "About Loom" command.

### 9. Weak test hooks

The About subtree carries no `.test_support()` ids, unlike the surrounding
dialog (`close-settings` at `dialogs.rs:1215`, `settings-content` at
`dialogs.rs:1247`), so today only a "does it render" assertion is possible
(`loom_view_render_tests.rs:754-766`). The pane also bypasses `settings_row`
(`view.rs:761-797`), so it opts out of the phone layout that stacks controls
below their labels.

## Available data for the new rows

| Row | Existing source |
| --- | --- |
| Client version | `env!("CARGO_PKG_VERSION")` (already used) |
| Build revision / profile / target | not available today; needs `crates/loom-ui/build.rs` stamping a short SHA with an `unknown` fallback, plus `option_env!`/`cfg!` |
| Backend mode | `demo_workspace` (`view.rs:542`), `options.remote` (`crates/loom-local/src/lib.rs:159`), `browser_demo_mode` (`view.rs:398-399`) |
| Active backend node | `worker_nodes[ACTIVE_BACKEND_NODE_ENTRY_ID]` (`view.rs:273, 549`), `connection_state`, `connection_detail`, redacted `url` |
| Protocol version | client: `loom_protocol::CURRENT_PROTOCOL_VERSION` (11.1 today, asserted at `crates/loom-protocol/src/lib.rs:561`); server: needs problem 4 fixed |
| Workspace / default model | `workspace_id`, `workspaces`, `default_model` (`view.rs:414-415, 441`) |
| State location (native) | `loom_local::backend_persistence_path()` (`crates/loom-local/src/lib.rs:405-407`) |
| Diagnostics text | composed from the rows above; written with `cx.write_to_clipboard` |
| External links | `open_external_url` (`view/helpers.rs:722-739`) |

## Proposed design

### Identity header (keep, fix the copy)

- Keep the centered hero: name, version, one-line description.
- Replace the fixed "Local agent" with a mode-aware subtitle derived from the
  same state the header/status line already uses, e.g.
  `Local · in-process backend`, `Remote · wss://host:port`,
  `Demo · deterministic provider`, `Browser · connected to wss://host:port`.
- Add a revision suffix when stamped: `Version 0.1.0 · a1b2c3d`.

### "This build" card

Use `settings_row` so the phone layout stacks controls for free (per the
`AGENTS.md` preference for existing gpui-kit components, a `Button` with
`.ghost().small()` and `LoomTooltip` for the copy control):

- **Version** — workspace version plus revision when available.
- **Platform** — `target_os`/`target_arch` and `native`/`browser`, from `cfg!`.
- **Protocol** — client `11.1`, server `11.1` once negotiation is retained;
  render a warning tone on mismatch instead of only failing the next request.

### Backend card

- One summary line: active backend node name, `connection_state`, endpoint with
  credentials stripped (`redact_secret` already exists in
  `crates/loom-ui/src/connection.rs`), workspace name/id, default model.
- A "Manage workers" button that sets `settings_section = SettingsSection::Workers`
  and `settings_open = true`, mirroring `open_providers_for_node`
  (`view/providers.rs:940-951`).
- Native only: state directory path with "Copy path" and "Open folder"
  (`open::that` on the parent directory, tolerating a missing directory).
- Browser: workspace and origin only; no filesystem paths exist there.

### Diagnostics card

- A "Copy diagnostics" control that writes a plain-text block (version,
  revision, platform, protocol, backend mode, workspace, model, state path)
  through `cx.write_to_clipboard`, matching `view/providers.rs:64`.
- The text must never include tokens or credential material; the remote URL is
  already rejected at startup when it embeds a credential
  (`worker_url_embeds_credential`, `view/lifecycle/init.rs:15`), but the
  formatter should redact defensively anyway.
- Keep the formatter a pure function so it can be unit tested without a window.

### Legal and project card

- **License** — "AGPL-3.0-only" for the app, one line noting that the reusable
  core crates are GPL-3.0-only, linking `LICENSES.md` and `COPYRIGHT`.
- **Acknowledgements** — short list (gpui/gpui-kit, Catppuccin palette, the
  provider SDK surface) with the full inventory left in `LICENSES.md`.
- **Experimental software** — the one-line warning from `README.md`, so a
  preview build is not mistaken for a supported product.
- **Links** — Documentation, Repository, Releases, Report a security issue
  (`SECURITY.md` URL; the private advisory form is the documented channel).
  Buttons call `open_external_url`; on wasm a blocked popup should fall back to
  selectable URL text, since `open_external_url` returns an error there
  (`view/helpers.rs:729-739`).

### Discoverability

- Add an `about` entry to `COMMANDS` (`view.rs:170-213`) and an
  `open_about_from_menu` action (`self.settings_section = SettingsSection::About`)
  mirroring `open_providers_from_menu` (`view/providers.rs:935-938`).
- No new keyboard shortcut: the palette plus `help` is the established surface,
  and shortcuts are already crowded.

## Implementation slices

Each slice is independently reviewable and testable.

1. **Copy and structure polish (no new data).** Mode-aware subtitle from
   existing state, revision-ready version line, `.test_support()` ids and
   `accessibility_label`s on the new controls, and the palette/help "About Loom"
   command. Tests: render tests for each backend mode; a real click test that
   the palette command lands on the About section.
2. **Build metadata.** `crates/loom-ui/build.rs` stamping a short git SHA with an
   `unknown` fallback (must not fail on a shallow or git-less checkout), plus
   platform/profile rows. Tests: unit tests for the formatting helpers only.
3. **Backend card.** Retain `NegotiationResult` in `negotiate`/`negotiate_async`
   and store it on the view; render backend mode, endpoint (redacted), workspace,
   model, and protocol versions; add the "Manage workers" action. Tests: mode
   label helper, redaction, protocol-mismatch rendering, section switch.
4. **Diagnostics.** Pure diagnostics formatter plus the copy control. Tests:
   formatter unit tests (including redaction) and a click test guarded by a
   `test_support` id.
5. **Legal and links.** License/acknowledgement/disclaimer rows and link
   buttons. Tests: unit tests for the label/URL constants (so a moved doc path
   or typo is caught) and render tests.
6. **Native state directory row.** Path display, copy, and open-folder action
   behind `cfg(not(target_family = "wasm"))`. Tests: helper tests plus a
   native-only render test.

## Testing and verification

- Unit tests for every new pure helper (mode label, diagnostics text, redaction,
  link constants).
- Render tests per backend mode (`local`, `remote`, `demo`) and per layout
  (desktop and phone), extending `settings_about_and_providers_panes_render`
  (`view/tests/loom_view_render_tests.rs:754-766`); the existing
  `settings_section_navigation_renders_every_pane` test already clicks every
  section by index and will cover the added controls.
- New color literals must be added to `legacy_color_role` in
  `crates/loom-ui/src/theme.rs`, or `every_legacy_color_literal_used_by_the_view_is_mapped`
  fails.
- Gates per `CONTRIBUTING.md`: `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`,
  `cargo test -p loom-ui`, plus `cargo check -p loom-ui --target wasm32-unknown-unknown`
  for the cfg-gated slices (1, 3, 5). Patch coverage must stay at or above 75%
  of changed non-test lines (`cargo llvm-cov` + `diff-cover`).

## Risks and open questions

- **Claims the build cannot back up.** No updater, telemetry, or crash reporting
  exists; the pane must not imply an "check for updates" or "send report"
  capability that is not implemented.
- **Revision stamping.** Depends on git being present at build time; CI and
  release builds may be shallow. The build script must degrade to `unknown`
  rather than failing the build.
- **Secrets.** Endpoint strings are user-supplied; keep redaction in the
  formatter and in the diagnostics text, and never render tokens (they are not
  in view state today, and should not be added).
- **Duplication with Workers.** Keep the node list out of About; one summary
  line plus a link is the intended boundary.
- **Wording churn.** "Local agent" is Loom's self-description; replacing it with
  a mode-aware subtitle is a product-visible copy change and should match
  `docs/product.md` before landing.
- **Logs.** `env_logger` writes to stderr (`crates/loom-ui/src/main.rs:39-45`) and
  there is no file sink, so a diagnostics bundle cannot include a log path
  without a separate logging change. Either treat "copy diagnostics" as
  build/environment only, or add a log file sink as its own change.
- **wasm links.** `window.open` can be blocked; decide whether a blocked link
  shows the URL for manual copying or a status note.
- **Per-crate versions.** All crates currently share the workspace version; if
  that diverges, decide whether About shows the workspace version or the client
  crate version.

## Out of scope

Update checking and downloads, in-app changelog rendering, telemetry or crash
reporting, bundling full license texts in the binary, and moving About out of
the Settings dialog into its own window.
