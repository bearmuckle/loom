# Agent presentation: from run timeline to conversation

This note explains why the agent surface reads as an orchestration console
rather than a conversation, and defines the client-side presentation model that
brings it closer to a modern coding-agent transcript. It covers the projection
and rendering changes only; runtime changes are listed as deferred work.

## Problem

The backend models an agent as an orchestration object: a run with a nine-state
machine, model-turn "steps", an activity timeline, durable interactions,
context inspections, evidence links, checkpoints, limits, and worker nodes.
The client then renders that domain model almost one-to-one. The result is that
the audit trail is the primary surface and the conversation is one projection
among many.

Concrete symptoms:

- Assistant text has no streaming affordance and is indistinguishable from
  finished text.
- A single tool call is rendered as up to four separate rows
  (`ToolRequested`, `ToolStarted`, `ToolOutput`, `ToolCompleted`); once activity
  records arrive those rows are deleted and replaced by collapsed activity
  groups, so the visual language changes mid-run.
- Tool output is silently truncated (32 KB for legacy rows, 420 characters for
  activity rows, head/tail for commands) with no way to expand it, and there is
  no code highlighting or inline diff rendering.
- The same run state is repeated in the header, footer, activity group headers,
  activity rows, and sidebar child labels.
- Provider and role labels ("GitHub Copilot", "You") are repeated on every
  item.
- Evidence links are inert text, the plan is an append-only card stack, and
  project message cards compete with the conversation.

None of these are required by the backend contract. The client is simply
rendering the wrong object.

## Root cause

opencode models a session as a conversation of messages, where an assistant
message is an ordered list of **parts** (text, reasoning, tool, patch, todo).
The transcript is a projection of that list and every tool affordance falls out
of the part model. Loom already collects nearly the same information in
`AgentActivityRecord` and `AgentEvent`, but renders the runtime/audit model
instead. The fix is a projection change: derive a message-part model on the
client and render that, keeping the activity timeline as an optional diagnostic
source rather than the render source.

## Target model

```text
Transcript
  Turn::User { text }
  Turn::Assistant { parts: Vec<Part>, streaming: bool }
  Turn::System { text }          // status, errors, needs-input
  Turn::Plan { steps, completed, active }
  Turn::ChildMessage { ... }     // project message, de-emphasized

Part
  Text { markdown, streaming }
  Reasoning { text, collapsed }  // reserved; no runtime support yet
  Tool {
    id, name, title, status, elapsed_ms,
    detail { Command | File | Search | Patch | Generic },
    output { text, truncated, expanded },
    approval { pending, resolved },
  }
```

### Tool parts

Each tool call becomes exactly one part, replacing both the four-row legacy
lifecycle and the collapsed `ActivitySection` grouping. The part carries:

- **Title**: a human intent derived from the tool name and arguments, for
  example `Read src/main.rs`, `Run cargo test`, `Search "TODO"`,
  `Edit src/lib.rs`. Falls back to the raw tool name for unknown tools.
- **Status**: `queued`, `running`, `awaiting_approval`, `awaiting_input`,
  `completed`, `failed`, or `cancelled`, shown once on the part header.
- **Detail**: structured arguments rendered compactly (command line, path,
  pattern, patch summary).
- **Output**: collapsed by default, expandable, with the full bounded output
  and an explicit truncation notice. Active, failed, and approval-gated parts
  stay open; finished successes collapse. Command output keeps an in-progress
  affordance while running.
- **Approval**: inline approve/reject controls when the part is awaiting a
  decision, reusing the existing approval commands.

A patch preview inline in the tool part is not implemented in this pass; the
existing review drawer remains the place to inspect a full diff. Output is
rendered with the existing markdown/code treatment rather than a language
highlighter.

### Streaming

The active assistant turn renders a cursor while deltas are arriving, and the
turn is marked `streaming`. This removes the need for the composer's
"Working..." label to carry all progress feedback, though the composer keeps a
compact running indicator.

### Status consolidation

Run/session state is shown once, in the session header. The footer, activity
group headers, activity rows, and sidebar labels stop repeating it. Sidebar
child rows keep a single status dot plus name.

### Plan

`AgentPlan` renders as a single live checklist for the active run instead of an
append-only card per `PlanProposed`, with `✓`/`>`/`○` markers updated by
step events. Completed runs keep their final checklist, but a new run replaces
it rather than stacking.

### Evidence and project messages

Evidence entries become actionable (open the link) when they look like URLs.
Project message cards are visually de-emphasized: a single compact header line
with sender, kind, and sequence, and no heavy border/background, so they read as
context rather than interruptions.

### Composer

The composer gains an explicit send control and a visible stop/interrupt
control tied to the existing pause/interrupt commands, so the primary actions
are discoverable without keyboard knowledge.

## Mapping from existing primitives

| Target | Existing source |
| --- | --- |
| `Turn::Assistant.parts[].Text` | `AgentEvent::AssistantMessageDelta` |
| `Part::Tool` | `ToolCallRequested/Started/Output/Completed` + `AgentActivityRecord` |
| Tool title | `AgentActivityData::{File,Search,Command,ToolCall}` payload |
| Tool output | `ToolOutputChunk` / activity result |
| `Turn::Plan` | `AgentPlan` + `StepStarted`/`StepCompleted` |
| Approval | `ToolApprovalRequired` / `ToolApprovalDecided` |
| `Turn::ChildMessage` | `AgentMessageRecord` |
| `Turn::System` | `Status`, `Error`, `NeedsInput` items |

The existing `TimelineItem` enum stays as the wire-facing accumulator during
the transition; a projection step folds it into `Turn`/`Part` for rendering.
Once the projection is proven, `TimelineItem` can be retired.

## Out of scope

This note covers client presentation. The following runtime changes would
further close the gap and are tracked separately:

- Execute all tool calls from a completion (and run independent calls
  concurrently) instead of stopping at the first `ToolCallDelta`.
- Stream tool output incrementally instead of emitting one chunk after
  synchronous execution.
- Treat a rejected tool as a tool error fed back to the model instead of
  failing the run, and let `ask_user` resume conversationally.
- Add reasoning/thinking stream events and provider parsing.
- Promote message parts into `loom-protocol` as the canonical representation.

## Runtime slices

The following runtime changes are implemented:

- **Multi-tool turns.** A completion whose tool calls are all permitted runs
  every call in sequence instead of stopping at the first one. Calls that need
  approval, `propose_plan`, `ask_user`, and deferred project-join tools still
  use the single-call path because they pause or park the run. A step
  interrupted between the model response and its tool result is detected on
  recovery and surfaced as `RecoveryRequired` rather than replayed.
- **Non-fatal rejection.** Rejecting a tool records a failed tool result and
  lets the model react instead of failing the run. The rejected call signature
  is remembered, so an identical re-request is answered without prompting
  again.
- **Reasoning.** `ModelStreamEvent::ReasoningDelta` and
  `AgentEvent::ReasoningDelta` carry optional provider reasoning summaries.
  Chat-completions encoders parse `reasoning_content`/`reasoning`; the
  Responses encoder parses reasoning summary text. Reasoning is kept on the
  assistant turn and echoed back to chat-completions providers that require it
  (DeepSeek thinking mode), including across persistence and resume; providers
  that do not emit or accept it are unaffected. A thinking-mode provider returns
  an empty `reasoning_content` about half the time; the empty field is preserved
  and echoed rather than dropped, because omitting it makes the provider reject
  the next request with HTTP 400. Reasoning is also carried to the client in
  transcript pages so a reloaded transcript shows it, but it stays optional
  display metadata.

Parallel execution of independent calls and incremental suppression of
duplicate denials across restarts remain open.

## Status

The projection and rendering changes above are implemented in `loom-ui`:
message-part assistant turns, unified tool blocks, collapsed status chrome,
streaming cursor, composer send/stop, actionable evidence, single-run plans,
de-emphasized project messages, and an opt-in collapsed reasoning disclosure
(hidden by default; enabled under Settings → Appearance).

## Presentation polish

The projection and rendering code lives under `crates/loom-ui/src/view` and
`crates/loom-ui/src/syntax.rs`:

- **Code legibility.** Tool results render in the theme's monospace family with
  lightweight syntax highlighting for the common transcript languages.
  Unified diffs are detected and rendered with added/removed/hunk colors inline
  in the tool block, and the review panel's diff lines are monospace. Results
  that only restate the workspace—`read_file`, `search_text`, `glob`, and
  `list_files`—render as a pointer rather than a body: the action already names
  the file or query, so the contents stay behind the file viewers. A search
  goes one step further and shows its query with the number of hits on the tool
  row, so the outcome is readable without expanding it and without rendering the
  matching lines. A command renders as its own command line, cut to one line.
  A result that exists nowhere else—`web_search`, `apply_patch`, GitHub, review,
  and project state—is shown; everything else is not. Failed output is never
  shown for any tool: the row reports the failed status, and whether a failure
  matters for the user is the agent's judgement, reported in its answer. A block
  that would reveal nothing when expanded offers no disclosure control, and a
  settled result starts collapsed.
- **Progress feedback.** A single status line above the composer carries an
  animated spinner, the run state, elapsed time, and the `esc to interrupt`
  hint. Run state is no longer repeated in the session header.
- **Composer.** The input auto-grows with its content and offers inline `/`
  command and `@` file completions, `↵ send`/`⇧↵ newline` hints, and a
  command-palette affordance.
- **Command palette.** `⌘K`/`Ctrl+K` opens a filterable palette over the same
  command set as the slash menu.
- **Transcript.** Assistant turns carry a neutral `Agent` gutter marker (user
  turns use `You`); tool blocks carry a type icon, monospace title, patch
  summary, copy control, and a language label. A model response that thinks and
  calls tools several times stays one agent entry: every reasoning part is
  gathered into a single disclosure at the top, and all of its tool calls share
  one usage line that lists each tool type with its invocation count (for
  example `Read ×2 · Search ×1`). Expanding the usage line reveals the detailed
  presentation, where consecutive same-kind tool calls still collapse into one
  expandable summary row. Active and approval-gated runs stay open. A run that
  recovered from a failed call reads as done with the failure count called out
  (`done · 1 failed`); only a run where nothing succeeded reads as failed. The
  transcript scrolls through
  gpui-kit's `MessageScroller`, which keeps the live edge pinned and provides
  the scrollbar, bottom fade, and jump-to-latest control.
- **Navigation.** Session rows show relative update times and an activity dot
  for running background sessions, with a filter field above the tree.
- **Empty states.** The no-project and empty-transcript states describe the
  available commands and primary action.

Remaining gaps: a tree-sitter-backed highlighter, per-tool timestamps, and
shell-mode (`!`) execution are not implemented. The `@` completion lists
changed files and attached sources rather than a full workspace file index.


## Verification

- Unit-test the projection: text, reasoning, and tool deltas produce one
  assistant turn with ordered parts; late tool results update the existing part
  in place; streaming state clears on completion.
- Preserve the existing snapshot-replay and paging behavior: restored
  transcripts produce the same parts as streamed events.
- Confirm approvals remain functional from the inline part controls and that
  the composer can still start, pause, and interrupt runs.
