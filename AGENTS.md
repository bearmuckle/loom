# Agent instructions

- Keep PR verification sections terse: list each verification command once, with a brief pass or fail result. Do not include verbose test output.
- For UI work, prefer existing `gpui-kit` components when they fit the interaction; build custom controls only when the component library does not provide a suitable option.
- Before opening a PR, check workspace line coverage locally with `cargo llvm-cov --workspace --all-features --locked --fail-under-lines 80`; coverage must be at least 80%.
