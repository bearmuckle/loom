# Vendored GPUI Kit patch

This directory contains the `gpui-base` and `gpui-component` 0.6.6 crates
from crates.io, with the web clipboard paste fix from GPUI Kit PR #3244
applied: <https://github.com/longbridge/gpui-kit/pull/3244>.

The PR was merged upstream after 0.6.6 was published. Its stale-paste guard
uses `document_revision`, which is not present in 0.6.6, so this backport
compares a text snapshot and selections instead. Remove this patch when a
published GPUI Kit release containing the fix is available.
