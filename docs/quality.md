# Task-level quality gate

This document defines the representative agent-task fixtures and the measured
thresholds that guard user-facing execution quality.

## Running the gate

```sh
cargo test -p loom-server --test agent_quality -- --nocapture
```

The test runs every fixture end to end on the deterministic provider, prints
the measurements, and fails when a signal crosses its threshold. Because it is
a normal integration test, a regression is visible in CI on every pull request
and push alongside the rest of the workspace tests.

## Fixtures

The fixtures in `crates/loom-server/tests/agent_quality.rs` are small but
representative slices of real work:

| Fixture | Shape | What it protects |
| --- | --- | --- |
| `inspect_workspace` | Read-only exploration (`list_files`, `read_file`) then a summary turn | Read-only concurrency and the exploration loop |
| `edit_and_validate` | A mutating `apply_patch`, then `run_command` validation, then a summary turn | The write/execute path and multi-turn completion |
| `recover_from_tool_error` | A failing `apply_patch`, then a corrected call, then a summary turn | Tool-error feedback and recovery rather than a stuck run |
| `recover_from_undecodable_call` | An `InvalidToolCall` whose argument payload cannot be decoded, a valid call in the same turn, then a summary turn | In-turn tool error for an undecodable call so the model resends it instead of the run failing |

A separate search fixture writes a bounded workspace and measures
`search_text` latency, and asserts the search actually returns the needle so the
latency bound cannot pass vacuously.

## Signals and thresholds

| Signal | Measurement | Threshold |
| --- | --- | --- |
| Successful-completion rate | Fixtures that reach `Completed` / all fixtures | `>= 1.00` |
| Tool-error recovery rate | Fixtures with a failed tool result that still complete / fixtures with a failure | `>= 1.00` |
| Turns per task | Maximum `StepStarted` events in any fixture run | `<= 8` |
| Time to first streamed output | Maximum time from run start to the first `AssistantMessageDelta` | `<= 10 s` |
| Search latency | Median `search_text` call over the fixture workspace | `<= 2 s` |

The thresholds are deliberately loose enough for loaded CI runners while still
catching an order-of-magnitude regression. Rate thresholds are exact because
the provider is deterministic.

## Changing the gate

When a new interaction becomes important, add a fixture and, if it introduces a
new signal, a row to both tables above. Raise a threshold only with a linked
explanation; the point of the gate is to make a regression visible, not to
accommodate it silently.
