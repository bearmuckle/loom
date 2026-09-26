# Diff review primitive spike (issue 21)

The VCS response is the authoritative source for repository changes. Its old
`patch` text lacks hunk headers because `loom-vcs` prints only line content and
origin markers. `similar` can build a diff from two complete buffers, but the
client does not have both Git versions and should not reconstruct Git state.
The VCS layer now exposes typed hunks and old/new line numbers from libgit2's
patch callback. The legacy patch remains for existing clients.

`gpui-kit`'s Editor handles a single text buffer. It has a line-number gutter,
`set_readonly`, and a syntax-highlighter adapter; highlighting requires a
language provider. An immutable before or after buffer is feasible, but its
gutter numbers are positions in that buffer. A unified diff needs separate
before/after numbers, and added and removed rows need distinct backgrounds.
Two editor instances also have independent wrapping and scrolling, so they do
not provide reliable side-by-side alignment. The first review uses GPUI's
virtualized `list` with custom rows and keeps the scope unified. The left
navigator, canvas, and right review pane share GPUI Kit's `h_resizable` group,
which supplies both drag dividers. Code rows use `SelectableText` for copying.
The unified rows currently use diff colors rather than language syntax colors.

The `rgitui_diff` reference demonstrates a separate diff row model and
virtualized rendering. Those layout ideas are used without importing its older
GPUI revision or application dependencies. The remaining custom pieces are
row colors, dual line-number gutters, hunk navigation, and the navigator. The
server bounds typed rows to the review size limit; the UI reports truncation
and binary files explicitly.
