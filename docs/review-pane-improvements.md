# Right-pane (inspector) improvement plan

Status: proposed
Scope: `crates/loom-ui` client presentation. No backend protocol changes are
required; every panel below is backed by an existing request/response or event.

## Motivation

The right pane is presented as the "Changes" review drawer. It is the only
auxiliary surface in the client, yet the session has other inspectable state
that is currently scattered or invisible:

- Agent/run state is repeated across the session header, the composer status
  line, sidebar pills, and the timeline.
- Context assembly (`ContextInspection`) is reduced to one line above the
  composer (`crates/loom-ui/src/view/render/composer.rs:66`).
- Spend/usage is not shown at all. `UsageRequest::GetSessionUsage` and
  `GetRunUsage` exist, but the client never sends them, and
  `AgentEvent::RunUsage` / `RunUsageUpdated` are explicitly ignored
  (`crates/loom-ui/src/view/lifecycle/events.rs:378`).
- The diff view itself is a first-pass spike (`docs/diff-review-spike.md`) with
  known limitations.

The pane should become a small tabbed **inspector** rather than a single
"Changes" view.

## Current implementation map

| Concern | Location |
| --- | --- |
| Pane rendering | `crates/loom-ui/src/view/render/review.rs` |
| Actions and data loading | `crates/loom-ui/src/view/review.rs` |
| State model | `ReviewState`, `ReviewRow`, `ReviewPanel` in `crates/loom-ui/src/state.rs` |
| Panel split / toggle | `crates/loom-ui/src/view/render/root.rs:236-585` |
| Diff line classification | `crates/loom-ui/src/syntax.rs:480-540` |
| File viewer request | `open_review_file` in `crates/loom-ui/src/view/review.rs:150` |
| Context inspection storage | `context_inspection` field in `crates/loom-ui/src/view.rs:439` |
| Tests | `review_panel_*` in `crates/loom-ui/src/view/tests.rs:3074+` |

`ReviewPanel` has a single variant (`Changes`), so the "Show changes" button in
the header (`render/review.rs:367-379`) is effectively dead and was clearly
left as a tab hook.

## Problems with the current Changes view

### File list

- Status is rendered with `Debug`: `format!("{:?}  {}", file.worktree, ...)`
  (`render/review.rs:100`), producing `Modified`, `Unknown`, etc. instead of
  compact status glyphs.
- Staged and working changes are two unrelated rows in the same flat list
  (`render/review.rs:112-133`); the relationship is unclear.
- Long paths do not truncate or scroll; the sidebar is fixed at 220px
  (`render/review.rs:17`).
- Repositories, repository changes, staged rows, and "other workspace files"
  share one narrow column with weak hierarchy.
- No filter/search, no grouping, no per-file status color, no conflict marker,
  no file count or aggregate add/del summary.

### Diff

- Lines use diff colors only; no syntax highlighting even though
  `syntax::line_highlights` / `syntax::highlight` already exist and are used in
  the transcript (`docs/diff-review-spike.md` records this as a known gap).
- Fixed 22px rows (`render/review.rs:422`) with no wrapping or horizontal
  scroll; long lines are clipped.
- No intra-line (word-level) highlighting, no split/side-by-side mode, no
  whitespace toggle, no collapsible hunks, no expand-context.
- Header shows status text but offers only Previous/Next hunk
  (`render/review.rs:235-260`): no copy patch/path, no open-file, no wrap.
- Truncation (`MAX_REVIEW_DIFF`, 48 KB) is reported but context cannot be
  expanded.

### File viewer

- Dumped as one `SelectableText` blob (`render/review.rs:298-309`); no line
  numbers, no syntax highlighting, no wrap toggle, no position/scroll restore.
- Content is capped at `MAX_REVIEW_DIFF` (48 KB) with no indicator.

## Available data for new panels

| Panel | Existing source |
| --- | --- |
| Agent | `active_run: AgentRunSnapshot`, `run_state`, `session_state`, `activity_records`, `project_snapshot` (agents, delegated tasks, worktrees) |
| Context | `ContextInspection` (`items`, `total/included/omitted_tokens`, `ContextBudget`, `compacted`, `summary`) via `AgentEvent::ContextInspected` and `ContextRequest::InspectAgentContext { run_id }` |
| Spend | `UsageSnapshot` (input/output/cached tokens, `tool_calls`, `cost_micros`, `elapsed_ms`) + `ProviderUsageSummary`; `UsageRequest::GetSessionUsage`, `GetRunUsage`; live `AgentEvent::RunUsage` / `RunUsageUpdated` |
| Files | `FilesystemRequest::GetSessionFilesystemSnapshot`, `ReadSessionFile`, `session_directories`, `session_repositories` |
| Changes | `GitRepositoryStatus`, `GetSessionVcsDiff`, `SessionFilesystemChange` |

`session_id_for_request` already routes both usage requests
(`crates/loom-ui/src/view/helpers.rs:608,638`), so only the view wiring is
missing.

## Proposed design

### Tabbed inspector

Replace `ReviewPanel` with a tab enum and render a gpui-kit `TabBar`
(`gpui_kit::component::tab::{TabBar, Tab, TabVariant}`, `Segmented` or
`Underline`) under the inspector header. Prefer the component over a custom
strip per `AGENTS.md`.

```rust
pub(crate) enum InspectorTab {
    Changes,
    Agent,
    Context,
    Files,
}
```

- Persist the active tab across sessions (`ReviewState`), defaulting to
  `Changes`.
- Retitle the header to the active tab; keep the close control.
- On phone layouts the tab strip scrolls horizontally; the existing stacked
  list/detail layout is preserved per tab.
- Add a small tab badge: changed-file count on `Changes`, attention count on
  `Agent` (pending approval/input), context-usage warning on `Context`.

### Changes tab (immediate polish)

File list:

- Add `GitFileStatusKind::label()`/`glyph()` (M/A/D/R/?/U) and color mapping;
  stop using `Debug`.
- Render one row per file with a status glyph, a path where the directory is
  muted and the file name is primary, wrapping/truncating paths, and colored
  `+n −n` counts. Show a conflict indicator when `file.conflicted`.
- Represent staged vs working as a compact toggle (Working / Staged) or a
  per-row badge instead of duplicate rows.
- Add a filter input (reuse the `InputState` + `subscribe` pattern from
  `render/root.rs:89-98`) and an aggregate summary
  (`n files · +a −d`).
- Group by repository and by status; keep the repository picker.
- Virtualize once above a threshold (the list is currently unvirtualized).

Diff:

- Syntax-highlight each line by reusing `syntax::highlight` /
  `line_highlights`, layered over the existing added/removed backgrounds.
- Sticky file header: path, staged/working badge, add/del summary, copy path,
  copy patch, "Open in Files", whitespace toggle, wrap toggle,
  unified/split toggle.
- Collapsible hunks and "expand context" via an additional `ReadSessionFile`
  fetch; keep Previous/Next hunk and add keyboard shortcuts.
- Word-level highlighting for removed/added line pairs.
- Horizontal scroll for unwrapped lines; explicit binary/large-file states.

Keep diffs read-only (roadmap M5 non-goal: staging/commit controls).

### Agent tab

Summary of the active session and run, reusing existing projections:

- Session/run header: name, `session_status_pill` state, model, branch/clean.
- Current run: task, state, started/updated/elapsed (live tick), summary,
  evidence links (already actionable in the timeline).
- Plan checklist from the latest `TimelineItem::Plan`.
- Tool activity counts by `ToolPartStatus` and pending approval/input
  indicators.

No new backend calls required.

### Context tab

- Budget bar: included tokens vs `budget.effective_input_tokens` (or context
  window), with `reserved_output_tokens` shown.
- Itemized `ContextItem` rows: kind, label, estimated tokens, included/omitted,
  and `omission_reason`.
- Compaction card when `compacted` / `summary.is_some()`: source message count,
  projection version, created time, and the existing status note.
- Refresh action dispatching `ContextRequest::InspectAgentContext { run_id }`
  for the active run; handle `ContextResponse::ContextInspection`.

### Files tab

- Source picker from `session_directories` / `session_repositories`, and a
  file list from `FilesystemRequest::GetSessionFilesystemSnapshot`.
- Viewer with line numbers, syntax highlighting by extension, wrap toggle, copy
  path/copy contents, and follow-to-line.
- Reuse `open_review_file`; widen the bound or make truncation explicit and
  pageable.

### Spend

Surface usage where it belongs contextually rather than as a fifth tab:

- `Context` tab shows session and current-run `UsageSnapshot`
  (`input`/`output`/`cached` tokens, `tool_calls`, `elapsed_ms`) and
  `ProviderUsageSummary` cost (`cost_micros`).
- `Agent` tab shows elapsed/tool calls for the active run.
- Update live from `AgentEvent::RunUsage` / `RunUsageUpdated`, and fetch on
  tab open / session switch via the usage requests.
- Format money from `cost_micros` with a shared helper; show "unavailable"
  when the provider reports no cost.

## Implementation slices

Each slice should be independently reviewable and testable.

1. **Tabbed shell.** `InspectorTab` enum, `TabBar` rendering, persistence,
   badge counts, header retitle, close behavior. Move the existing Changes
   body under the tab unchanged.
2. **Changes file-list polish.** Status glyph/label/color helpers, single-row
   staged badge, path truncation, add/del summary, filter. Update
   `review_panel_renders_*` tests.
3. **Diff polish.** Syntax highlighting, wrapping/horizontal scroll, sticky
   header controls (copy/wrap/open), collapsible hunks, word-level diff.
4. **Agent tab.** Read-only summary from existing snapshots; controls reuse
   pause/resume/interrupt/cancel commands.
5. **Context tab.** Budget bar, item list, compaction card, refresh; remove the
   composer context line or reduce it to a compact link once the tab exists.
6. **Spend.** Usage requests + live event handling + formatting; tests for
   `UsageResponse` and event application.
7. **Files tab.** Snapshot listing, viewer with line numbers/highlighting/wrap,
   truncation handling.
8. **Mobile/accessibility pass.** Horizontal tab strip, per-tab phone layout,
   keyboard navigation, `accessibility_label`s, tooltips.

## Suggested file layout

- `crates/loom-ui/src/state.rs`: `InspectorTab`, `UsageState`, `FileViewState`.
- `crates/loom-ui/src/view/render/review.rs` → split into
  `inspector.rs` (tab shell), `inspector/changes.rs`, `inspector/agent.rs`,
  `inspector/context.rs`, `inspector/files.rs`.
- `crates/loom-ui/src/view/review.rs`: keep data-loading actions; add
  `refresh_usage`, `refresh_context`, `load_file_snapshot`, `select_tab`.
- `crates/loom-ui/src/view/lifecycle/events.rs`: apply
  `RunUsage` / `RunUsageUpdated`.

## Testing and verification

- Unit-test status glyph/label mapping, path truncation helpers, and
  `UsageSnapshot` formatting.
- Render tests per tab, mirroring
  `review_panel_renders_workspace_and_git_changes` and
  `review_panel_renders_loading_file_and_binary_diff_states`.
- Test `UsageResponse` handling and live `RunUsage` event application.
- Test context inspection rendering for inside/over budget, omitted items, and
  compacted history.
- Keep existing hunk navigation and selection-revision races covered.
- Run the standard Rust gates for the crate: `cargo fmt --check`,
  `cargo clippy -p loom-ui`, and the `loom-ui` tests; include the wasm check if
  the split touches cfg-gated code.

## Risks and open questions

- **Tab default.** Keep `Changes` as default for continuity, or open the tab
  that needs attention (e.g. `Agent` while awaiting approval). Recommendation:
  default `Changes`, badge the others.
- **Composer context line.** Replacing it with a tab link may reduce at-a-glance
  awareness; consider keeping a compact budget indicator.
- **Diff rendering cost.** Syntax highlighting plus large diffs can be costly;
  highlight only visible rows, consistent with the existing virtualized list.
- **Split view.** Side-by-side alignment needs a row model that pairs
  removed/added lines; treat as a later slice if unified rendering lands first.
- **Files tab scope.** A full workspace tree needs a directory-listing request
  (`GetSessionFilesystemSnapshot` is the current coarse source); confirm its
  payload/size before committing to a deep tree.
- **Usage capability.** Usage requests are gated by `Capability::ReadUsage`;
  the UI must degrade gracefully when the capability or cost data is absent.

## Implementation status

This first increment is implemented on branch `ui/review-pane-improvements`:

- **Tab shell.** `InspectorTab { Changes, Agent, Context, Files }` replaces the
  single-variant `ReviewPanel`; the header renders a gpui-kit `TabBar` with
  badges for changed files and pending agent attention. Selecting a tab loads
  its data lazily. The pane lives in
  `crates/loom-ui/src/view/render/inspector/`.
- **Changes.** Status glyphs/colors and labels replace `Debug` output; the file
  list shows conflict badges, colored add/del counts, an aggregate
  `n · +a −d` summary, and a distinct staged row; relative paths truncate. The
  diff header adds wrap and copy-path controls plus the existing hunk
  navigation, and unwrapped diffs scroll horizontally. Workspace files render
  with line numbers and syntax highlighting.
- **Agent.** Run summary (session, state, model, task, elapsed), shared usage
  card, plan checklist, tool-activity counts, and evidence links.
- **Context.** Budget bar with included/effective tokens and over-budget
  coloring, reserved/omitted/total figures, item list with omission reasons,
  compaction summary, refresh action, and the shared usage card.
- **Files.** Workspace snapshot listing and a read-only viewer with line
  numbers, syntax highlighting, wrap, and copy-contents.
- **Spend.** `GetSessionUsage` / `GetRunUsage` are fetched on tab view and pane
  open, and `RunUsage` / `RunUsageUpdated` events update the run snapshot live.

Deferred to later slices: diff syntax highlighting and word-level highlights,
side-by-side mode, collapsible hunks/expand-context, the Changes filter input,
a deep file tree, and keyboard navigation.
