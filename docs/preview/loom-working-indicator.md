# Working indicator: before and after

`loom-working-indicator.gif` compares the run status line's working indicator
side by side:

- **left** — gpui-kit's `Spinner::small()`, the rotating loader the client used
  before commit `f0b1014` (`feat(ui): weave the Loom mark while a run is
  working`).
- **right** — the Loom mark from the app icon: the warp and weft bars, with the
  weft riding up the warp while the pair dims.

Each panel holds the 14 px indicator in the status row, and the same indicator
at 4x underneath so the motion is visible.

## How it was rendered

Real GPUI frames, not a mockup: GPUI's headless renderer (Mesa lavapipe) drew
the actual `Spinner` and the actual `loom_mark` in the client's dark colours.
The capture ran for one 2.4 s weave — 48 frames at 50 ms, which is also an exact
multiple of the spinner's 0.8 s rotation — so the GIF loops without a seam. The
framing harness was a throwaway test and is not committed; the animation it
captured is the code on `main`.
