# Antigravity Backend via `agy acp` (ACP Host)

Loom can run Google's official, unmodified Antigravity ACP agent (`agy acp` / `antigravity-acp`) as an external Agent Client Protocol (ACP v1) harness inside the worker. This is the first concrete harness slice for ACP support (#20) and implements the Antigravity backend (#448).

## How it works

Loom acts as a thin ACP **host** in the worker (`loom-harness::acp` + `loom-server::acp_host`), rather than adding a new HTTP model vendor inside `loom-providers`:

- **Binary discovery & override**: On the worker, Loom looks for `agy` (invoked as `agy acp --stdio`) and then `antigravity-acp` (invoked as `antigravity-acp --stdio`) on `PATH`. You can specify a manual command override either per session or via `LOOM_ANTIGRAVITY_ACP_COMMAND`. Loom never ships, patches, or forks the Antigravity binary.
- **One supervised process per session**: Each ACP session spawns one piped-stdio JSON-RPC 2.0 child process (newline-delimited JSON, never a PTY), rooted at the session working directory (`cwd`), with bounded stdout/stderr buffering and clean child shutdown on archive, delete, or worker exit.
- **Isolated profile directory**: Loom chooses and creates an isolated per-session directory (`0700`) under the worker's state tree and passes it to `agy acp` via `--app-data-dir` and `--gemini-dir` (and `LOOM_ACP_PROFILE_DIR`). Loom **only chooses the directory path and never reads, lists, or copies any file inside it**.
- **ACP v1 flow**:
  1. `initialize` — Loom sends its own `clientInfo` (`name: "loom"`, version from crate metadata). Loom never impersonates T3 Code or any other product.
  2. `authenticate` — Only methods advertised by the agent in `initialize` are accepted (default `oauth-personal`; explicit ToS-safe option `gemini-api-key`).
  3. `session/new` — Rooted at the session working directory. Models come from the harness catalog (`models.availableModels` / `configOptions`), not a hard-coded list.
  4. `session/prompt` / `session/cancel` — One Loom run maps to one ACP prompt turn; cancelling the run cancels any pending permission prompt and sends `session/cancel`.
- **No Loom prompt/tool injection**: Antigravity is the agent. Loom does not inject Loom's system prompt, tool list, or `loom-context` assembly into the harness, and does not enforce `SessionLimits` on ACP sessions (usage reported by the harness is displayed only).
- **Event & approval mapping**:
  - `agent_message_chunk` / `agent_thought_chunk` → Loom transcript / reasoning deltas.
  - `tool_call` / `tool_call_update` → Loom tool activity (`AgentActivityRecord`).
  - `session/request_permission` → Loom's existing approval UI (`AgentRunState::AwaitingApproval`), answered live with the ACP `optionId` (and `{ outcome: "cancelled" }` on cancel).
  - `usage_update` → `UsageSnapshot`.
  - `plan` → `AgentPlan`.
  - Harness stderr, startup failures, missing binary, auth errors, and crashes → visible session activity (`AgentActivityRecord`), never silent hangs.

## Setup

1. Install Google's official Antigravity CLI (`agy`) on the worker host so `agy` (or `antigravity-acp`) is on `PATH`, or set `LOOM_ANTIGRAVITY_ACP_COMMAND=/path/to/agy`.
2. Connect a client that negotiates the `acp_agent_sessions` capability to a worker that supports it. Older clients and workers that do not negotiate `acp_agent_sessions` refuse ACP sessions cleanly.
3. Choose an authentication method:
   - **`oauth-personal` (default)**: Uses `agy acp`'s own interactive browser / terminal sign-in flow during ACP `authenticate`. Sign-in stays inside Google's binary.
   - **`gemini-api-key` (ToS-safe option)**: Export `GEMINI_API_KEY` in the worker process environment before starting the worker, then create the ACP session with `auth_method: "gemini-api-key"`. The key reaches `agy` solely via inherited process environment — Loom never reads, logs, persists, or echoes `GEMINI_API_KEY`.
4. If `agy` is missing, the requested auth method is not advertised, or sign-in fails, Loom fails the turn immediately with visible session activity and a hint to use `gemini-api-key`.

## Honest resume and durability

The ACP harness owns its conversation state. Loom records the backend descriptor (`antigravity`, harness command, negotiated ACP protocol version, and advertised capabilities) on the session record (`AgentSessionSnapshot.backend`):

- Real session resume (`session/load` or `session/resume`) is attempted **only** when the harness advertises `loadSession` or `sessionCapabilities.resume` in `initialize`.
- When the harness does not advertise resume capability, Loom does **not** claim durable journal replay; restarting the harness begins a fresh ACP session and surfaces that honestly in session activity and in the Run inspector.

## Policy limits, Terms risk, and disclosures

This backend is **not risk-free** under Google's written terms:

1. **Terms risk (Antigravity ToS §6 & FAQ)**: The Antigravity Additional Terms of Service (§6) and Antigravity FAQ forbid using third-party software with an Antigravity consumer OAuth login and note that violations can lead to account suspension. Google's documented ToS-safe path for third-party agents is an API key (`authenticate("gemini-api-key")` with `GEMINI_API_KEY` in the worker environment, or Gemini Enterprise / Vertex).
2. **Interactions / training note**: On consumer Antigravity plans, Google may use Interactions to evaluate and improve Google machine-learning technologies unless the user disables that option in Google's in-product settings.
3. **Not a Google-registered ACP client**: Loom is not a Google-registered ACP client and is not covered by any third-party product's registration or assurance. Loom identifies itself honestly as `loom` in ACP `initialize`.
4. **Interactive-only**: Antigravity ACP sessions are strictly **person-present interactive runs**. Loom refuses headless/unattended sessions (`interactive: false`), auto-approve policies (`auto_approve_actions: true` or `ApprovalPolicy::auto_approve()`), delegated/background/scheduled project runs, and CI/unattended worker environments (`CI`, `GITHUB_ACTIONS`, `LOOM_UNATTENDED`, `LOOM_HEADLESS`, `LOOM_SCHEDULED`).

## What this is NOT

- **No OAuth proxy or token harvesting**: Loom never reads, copies, greps, logs, or stores `~/.gemini`, `~/.antigravity*`, `~/.config/*antigravity*`, keyrings, OAuth tokens, or `GEMINI_API_KEY`.
- **No `agy -p` wrapper**: Loom never wraps `agy -p` as a generic chat endpoint inside `loom-providers`.
- **No T3 Code client-id spoofing**: Loom never sends T3 Code's client identity or any other product's `clientInfo` to `agy acp`.
- **No credential scraping or billing/quota pass-through**: Subscription sign-in and inference stay entirely inside Google's unmodified binary.

## Opt-in live `agy` test

Automated CI tests run exclusively against a compiled stdio JSON-RPC stub ACP harness. No real sign-in or live `agy` invocation runs in CI.
