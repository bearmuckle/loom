# Token usage and compression

Status: investigation for [#289](https://github.com/bearmuckle/loom/issues/289).
No behavior changed except recommendation 1, which now declares context
windows for the hosted OpenAI families (see
[Recommendations](#recommendations-in-order)). Measurements are from a debug
build of `12c5f3a` in this
checkout; the throwaway measurement tests were removed after recording the
numbers below (see [Measurements](#measurements)).

## Summary

- The fixed part of every request is large relative to the default budget: a
  normal agent run on this repository pays about **2,294 tokens** before the
  task text or conversation, and a project-manager run about **3,734 tokens**.
- One maximum-size tool result (64 KiB) is **~12,600 tokens** of Rust source.
- An OpenAI provider configured before recommendation 1 was implemented ran on
  the **8,192-token fallback window**, because its model catalog recorded no
  `context_window` and discovery has nothing to read. That left roughly 3,800
  tokens for the conversation, so a single large tool result forced compaction,
  which drops history (the model re-reads) and rewrites the prompt prefix
  (cache misses). The declared windows in recommendation 1 remove that.
- Cached input tokens are recorded but priced at the full input rate, and
  DeepSeek's cache-hit counters are not parsed, so caching is currently
  invisible in cost and in the UI.
- No high-quality Rust crate provides safe generic prompt compression. The
  high-quality crates that exist (`tree-sitter`, `text-splitter`) only support
  structural reductions of code and text.

## Where the tokens go

| Item | Today | Measured | Source |
| --- | --- | --- | --- |
| Workspace tool schemas | 9 tools, advertised whenever the model supports tool calls | 785 tokens, 4,086 bytes JSON | `crates/loom-tools/src/lib.rs` (`tool_definitions`) |
| Project-manager tool schemas | 9 more tools for a manager run | 1,440 tokens, 7,482 bytes JSON | `crates/loom-server/src/project_tools.rs` |
| System instructions | general block plus tool-derived guidance | 332 tokens (109 general, 223 guidance) | `crates/loom-agent/src/steps.rs` (`system_instructions_with_builtin_guidance`), `crates/loom-tools/src/lib.rs` (`tool_guidance`) |
| Repository instructions | every `AGENTS.md`, `CLAUDE.md`, `LOOM.md`, `CONTRIBUTING.md`, `.github/copilot-instructions.md` in the workspace root, each up to 2 MiB, concatenated and always included | 1,177 tokens, 5,070 bytes for this repository | `crates/loom-sessionfs/src/instructions.rs`, `crates/loom-server/src/util.rs` (`merge_repository_instructions`) |
| One tool result | capped at 64 KiB, head and tail kept | 12,568 tokens (Rust, 5.21 bytes/token), 13,070 tokens (markdown) | `crates/loom-tools/src/lib.rs` (`DEFAULT_MAX_OUTPUT_BYTES`, `truncate_middle`) |
| Conversation | append-only; every step re-sends the retained history | grows until compaction | `crates/loom-context/src/lib.rs` |
| Reasoning | stored and replayed on assistant turns when tools are present | part of the prompt on every later step | `crates/loom-agent/src/activity.rs` (`echo_reasoning_on_assistant_turns`), `crates/loom-providers/src/response.rs` (`message_json`) |

### Budget math on the default window

With no advertised model window, `crates/loom-agent/src/steps.rs` substitutes
`8_192` tokens. With no explicit completion cap that gives:

1. `reserved_output_tokens` = `8192 / 4` = **2,048** (`crates/loom-context/src/lib.rs`).
2. `effective_input_tokens` = `8192 - 2048` = **6,144** (`crates/loom-core/src/context.rs`).
3. Tool schemas are charged first: `6144 - 785` = **5,359** for messages.
4. Required messages (system 332 + repository instructions 1,177 + task) leave
   about **3,800 tokens** for conversation.
5. Compaction starts at 90% of the message budget (`limit - limit / 10`), so
   roughly **3,300 tokens** of conversation.

A single 64 KiB tool result is ~12,600 tokens, so on this budget it can never
be retained: the assembler either shortens it in place or folds the whole
conversation into bounded excerpts on the same step the result arrives. On a
correctly declared 128k-1M window the same result is 1-10% of the input
budget.

### Compaction behavior

- Older turns become one deterministic excerpt summary
  (`compact_messages`); large tool results inside the retained window are
  shortened in place, preserving call ids and arguments.
- The summary is heuristic, not model-written: the role and the first 512
  characters of each message, joined and bounded to 4,096 characters. Tool-call
  arguments are cut to 256 characters, so decisions recorded only in arguments
  (patch context, search queries) are lost.
- Because compaction is triggered by size and rewrites the prefix, every
  compaction invalidates a provider prompt cache from the summary onward and
  makes the model re-read sources to recover detail.

## Prompt caching

OpenAI, DeepSeek, and Copilot-proxied models cache an identical prompt prefix
and bill cached input tokens at a lower rate. OpenAI reports
`prompt_tokens_details.cached_tokens`; DeepSeek reports top-level
`prompt_cache_hit_tokens` and `prompt_cache_miss_tokens`. Loom parses the
OpenAI shape (`crates/loom-providers/src/response.rs`) but not DeepSeek's.

What Loom does today:

- Order is stable: system instructions, repository instructions, task, optional
  summary, optional pinned user direction, conversation. The conversation is
  append-only, so the prefix is cacheable between steps.
- `cached_input_tokens` is recorded and persisted, but `cost_for_usage`
  (`crates/loom-providers/src/errors.rs`) multiplies all `input_tokens` by the
  full input price and ignores the cached subset, so reported cost overstates
  spend exactly when caching works.
- Two write paths break prefix stability: compaction (summary inserted and
  older turns dropped) and `echo_reasoning_on_assistant_turns`, which fills
  `reasoning_content: ""` into *earlier* assistant turns once any reasoning
  appears. `message_json` emits that field, so the serialized prefix changes
  after the first reasoning turn. The empty field exists because thinking-mode
  providers were observed to reject a follow-up that drops it; whether the
  empty-string form is still required by every supported provider is unverified.

Nothing in the request carries explicit cache hints. That is fine for the
automatic-caching providers Loom supports; it would matter if an explicit
`cache_control`-style provider were added.

## Measurements

| Measurement | Value |
| --- | --- |
| Workspace tool schemas | 785 tokens, 4,086 bytes (9 tools) |
| Largest workspace tool schemas | `apply_patch` 135 tokens, `search_text` 131, `read_file` 98 |
| Project-manager tool schemas | 1,440 tokens, 7,482 bytes (9 tools) |
| Largest project tool schemas | `delegate_project_task` 300 tokens, `delegate_project_code_task` 284 |
| System instructions | 332 tokens |
| Repository instructions, this repository | 1,177 tokens, 5,070 bytes |
| 64 KiB tool result | 12,568 tokens (Rust), 13,070 tokens (markdown) |
| Fixed overhead, normal run | ~2,294 tokens |
| Fixed overhead, manager run | ~3,734 tokens |
| `cl100k_base` vs `o200k_base` on this code | within about 0.5%; `o200k` was not smaller |
| Unknown model names (`gpt-6-luna`) | fall back to `cl100k_base`; `gpt-5*` maps to `o200k_base` |
| Tokenizer throughput (debug build) | ~1.1 MB/s |

The tokenizer result matters: swapping tokenizer families is not a saving on
this workload, and the default `gpt-6-*` model name is only mis-mapped by a
fraction of a percent. The useful move is to calibrate estimates against the
provider-reported `input_tokens` instead of chasing tokenizer families.

## Crate survey

The issue asks whether a high-quality compression crate exists. Findings:

| Crate | Version | Downloads (all-time / recent) | Verdict |
| --- | --- | --- | --- |
| `tiktoken-rs` | 0.12.1 | 21.1M / 9.0M | Already used for counting. No replacement needed; extend the model map when new families appear. |
| `tree-sitter` | 0.27.1 | 46.9M / 18.2M | High quality. Only useful for an AST-based "signatures and doc comments" read mode for very large files; needs a grammar dependency per language and a new `read_file` contract. |
| `text-splitter` | 0.33.0 | 2.4M / 683k | High quality. Token-aware chunking; marginally better than the existing head/tail cut when a large result must be split into linked parts. |
| `compression-prompt` | 0.1.2 | 2,977 / 765 | Not acceptable now. First published 2025-10-22, single author, no independent evaluation, and it drops tokens statistically from arbitrary text. For a code agent that breaks exact-text `apply_patch` matching and identifiers. |
| LLMLingua | - | - | Python only; no maintained Rust port found. |

No crate provides safe, general prompt compression. The honest answer to the
issue's "if any high quality crates are available do use" is: use
`tiktoken-rs` (already in use) and, only if structural reduction is pursued,
`tree-sitter`; do not add a lossy compressor crate.

## Recommendations, in order

1. **Declare context windows for hosted models (S). Implemented.**
   `declared_context_window` (`crates/loom-model/src/config.rs`) declares the
   input windows for the families `openai_model_supported` already lists:
   `gpt-4.1`/`gpt-4.1-mini`/`gpt-4.1-nano` = 1,047,576; `gpt-4o`/`gpt-4o-mini`
   = 128,000; `gpt-5`/`gpt-5-mini`/`gpt-5-nano` = 400,000;
   `o3`/`o3-mini`/`o4-mini` = 200,000; and `gpt-6-astra`/`gpt-6-sol`/`gpt-6-luna`
   = 128,000 (a conservative floor consistent with the Copilot descriptor).
   Names match exactly or with a versioned suffix (`gpt-4.1-2025-04-14`).
   `ProviderConfig::openai` records the value, and OpenAI discovery falls back
   to the table when the endpoint publishes no window and the configured
   descriptor has none; a discovered or client-supplied window still wins. The
   table is not applied to `OpenAiCompatible` or `Ollama`, where a gateway may
   serve the same name with a different real window, so the 8,192 fallback
   stays for genuinely unknown models. This is the largest saving available: it
   removes the re-read and compaction loop on every OpenAI run without changing
   prompt content. Still open: surface the `context_options.context_window`
   override in the client; it is persisted today but not reachable from the UI.
2. **Make usage accounting honest (S).** Price cached input tokens separately
   (or discount them), parse DeepSeek's `prompt_cache_hit_tokens` and
   `prompt_cache_miss_tokens`, and expose a cache-hit ratio in the usage
   summary. Token work cannot be steered while spend is misreported.
3. **Make the tool-result cap window-relative (S).** Replace the flat 64 KiB
   with `min(64 KiB, share of the effective input budget)`, expressed in bytes
   via the measured ~5 bytes/token, and keep head and tail. A single result
   should never exceed about a tenth of the input budget.
4. **Bound repository instructions (S).** Cap the concatenated instruction
   text by tokens (for example 8k), prefer `AGENTS.md`, and skip files whose
   content hash matches one already included. Today a 2 MiB instruction file is
   accepted and then fails the run with `ContextLimitExceeded`, because
   required instructions may not be trimmed.
5. **Keep the prefix stable (S/M).** Verify that each supported thinking
   provider still needs the empty `reasoning_content` echo; if not, stop
   rewriting earlier turns. Place a pinned user direction after the conversation
   it follows rather than before it. Both remove avoidable cache misses.
6. **Replace excerpt compaction with a structured work-state summary (M).**
   The event stream already knows which files were read or changed, which
   commands ran and how they exited, and which checks failed. A deterministic
   summary of that state is more useful than 512-character message excerpts and
   avoids re-reads; a model-written summary of the middle of a long run can be
   added later as an option.
7. **Guard token efficiency in the quality gate (S).** Add to
   `crates/loom-server/tests/agent_quality.rs` a per-fixture measurement of
   input tokens and compaction count, with loose thresholds. Without it every
   change above is unmeasured, and the roadmap's performance list has no token
   signal at all.
8. **Optional: structural reads (L).** A `tree-sitter` mode that returns
   signatures, types, and doc comments for very large files. Worth doing only
   after (1) and (3), and only with a benchmark showing fewer total tokens for
   a complete task, not just a smaller single result.

## Non-recommendations

- **Lossy prompt compressors** (token dropping) for anything that will be
  edited: `apply_patch` requires exact `old_text` matches, and shortened
  identifiers or paths make the model's edits fail.
- **General-purpose compressors** (`zstd`, `brotli`) for model input: they
  reduce bytes on the wire or on disk, not tokens in the prompt.
- **Swapping tokenizer libraries**: measured at well under 1% difference on
  this code; the error that matters is the unverified window, not the tokenizer
  family.
- **Trimming tool descriptions** as a first step: the whole workspace schema is
  785 tokens, about a third of the fixed overhead, and the descriptions carry
  behavioral guidance that has been tuned deliberately.

## Open questions

- Do DeepSeek and Copilot-proxied models still require the echoed
  `reasoning_content` field, and is it billed as input?
- Does OpenAI's automatic prefix caching cover the `tools` array, or only the
  message list? It decides whether tool-schema changes are cache-relevant.
- What is the real distribution of tool-result sizes in use? There is no
  telemetry, so the threshold in (3) is an estimate.
- How much does per-step tokenization cost in a release build? A debug build
  tokenizes at ~1.1 MB/s, and the assembler counts each message plus the whole
  request per step, so a long session may spend noticeable CPU re-counting
  unchanged history.

## How the numbers were produced

Each number above comes from a temporary `#[test]` that called the production
functions and printed counts. The harness is small enough to recreate:

```rust
let tools = loom_tools::tool_definitions();
let tokens = loom_model::estimate_tokens(&loom_model::ModelRequest {
    model: loom_model::ModelId::new("gpt-4"),
    messages: Vec::new(),
    tools,
    options: Default::default(),
});
```

Project-manager schemas come from `ProjectAgentTools::definitions` with every
grant enabled, repository instructions from
`loom_sessionfs::Workspace::instruction_text` over this checkout, and the
64 KiB figure from `loom_model::estimate_message_tokens` over a real source
file padded to the cap. Tokenizer comparisons used
`tiktoken_rs::{cl100k_base_singleton, o200k_base_singleton}`.
