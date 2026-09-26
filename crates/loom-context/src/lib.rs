use loom_core::{ErrorCode, LoomError, Result, Timestamp};
pub use loom_model::estimate_message_tokens;
use loom_model::{MessageRole, ModelMessage};
pub use loom_protocol::{
    ContextAssemblyOptions, ContextBudget, ContextInspection, ContextItem, ContextItemKind,
    ContextSummary,
};
use serde::{Deserialize, Serialize};

const DEFAULT_OUTPUT_RESERVE: u64 = 1_024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextAssembly {
    pub messages: Vec<ModelMessage>,
    pub inspection: ContextInspection,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextInput {
    pub system_instructions: Option<String>,
    pub repository_instructions: Option<String>,
    pub task: String,
    pub conversation: Vec<ModelMessage>,
    pub existing_summary: Option<String>,
    /// Latest user direction when it lies before the saved summary boundary.
    #[serde(default)]
    pub latest_user_message: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ContextAssembler;

impl ContextAssembler {
    pub const fn new() -> Self {
        Self
    }

    pub fn assemble(
        input: &ContextInput,
        options: &ContextAssemblyOptions,
    ) -> Result<ContextAssembly> {
        Self::assemble_with_counter(input, options, estimate_message_tokens)
    }

    /// Assemble with the active provider's token estimator. Tool schema overhead
    /// must already have been deducted from the supplied message budget.
    pub fn assemble_with_counter(
        input: &ContextInput,
        options: &ContextAssemblyOptions,
        count: impl Fn(&ModelMessage) -> u64,
    ) -> Result<ContextAssembly> {
        if input.task.trim().is_empty() {
            return Err(LoomError::invalid_request("context task must not be empty"));
        }
        let reserve = output_reserve(options);
        let budget = ContextBudget::new(options.context_window, options.max_input_tokens, reserve)?;
        let mut messages = Vec::new();
        let mut items = Vec::new();
        for (kind, label, content) in [
            (
                ContextItemKind::SystemInstructions,
                "system instructions",
                input.system_instructions.clone(),
            ),
            (
                ContextItemKind::RepositoryInstructions,
                "repository instructions",
                input
                    .repository_instructions
                    .as_ref()
                    .map(|text| format!("Repository instructions:\n{text}")),
            ),
            (ContextItemKind::Task, "task", Some(input.task.clone())),
        ] {
            if let Some(content) = content.filter(|text| !text.trim().is_empty()) {
                let role = if kind == ContextItemKind::Task {
                    MessageRole::User
                } else {
                    MessageRole::System
                };
                let message = ModelMessage::new(role, content);
                items.push(context_item(kind, label, count(&message), true));
                messages.push(message);
            }
        }
        let mut pinned_user = input
            .latest_user_message
            .as_ref()
            .map(|text| ModelMessage::new(MessageRole::User, text));
        let required =
            messages.iter().map(&count).sum::<u64>() + pinned_user.as_ref().map_or(0, &count);
        let limit = budget.effective_input_tokens.unwrap_or(u64::MAX);
        if required > limit {
            return Err(context_limit(
                "required instructions and task do not fit in the context budget",
            ));
        }
        let prior = input
            .existing_summary
            .as_deref()
            .filter(|text| !text.is_empty());
        let prior_message = prior.map(summary_message);
        let original_tokens = input.conversation.iter().map(&count).sum::<u64>();
        let prior_tokens = prior_message.as_ref().map_or(0, &count);
        let total_tokens = required
            .saturating_add(original_tokens)
            .saturating_add(prior_tokens);
        let trigger = limit.saturating_sub(limit / 10);
        let mut conversation = input.conversation.clone();
        let mut start = 0;
        let mut compacted = false;
        let mut summary = prior.map(|text| ContextSummary {
            text: text.to_owned(),
            source_message_count: 0,
            created_at: Timestamp::now(),
        });
        if total_tokens > trigger {
            let outcome = (|| -> Result<()> {
                // Leave headroom for the next exchange, but always try to retain the
                // newest exchange. Assistant calls and their outputs form one group.
                let latest_user = conversation
                    .iter()
                    .rposition(|message| message.role == MessageRole::User);
                let user_tokens = latest_user.map_or(0, |index| count(&conversation[index]));
                let available = limit.saturating_sub(required).saturating_sub(user_tokens);
                let mut boundaries = vec![0];
                for (index, message) in conversation.iter().enumerate().skip(1) {
                    if message.role != MessageRole::Tool {
                        boundaries.push(index);
                    }
                }
                let newest_start = boundaries.last().copied().unwrap_or(0);
                let minimum_recent = conversation
                    .iter()
                    .enumerate()
                    .skip(newest_start)
                    .filter(|(index, _)| Some(*index) != latest_user)
                    .map(|(_, message)| {
                        if message.role == MessageRole::Tool {
                            let mut minimal = message.clone();
                            minimal.content = "[... omitted ...]".to_owned();
                            count(&minimal).min(count(message))
                        } else {
                            count(message)
                        }
                    })
                    .sum::<u64>();
                let summary_budget = (available / 4)
                    .clamp(16, 1_024)
                    .min(available.saturating_sub(minimum_recent));
                let recent_budget = available.saturating_sub(summary_budget);
                let target = recent_budget.saturating_sub(recent_budget / 10);
                let mut retained = original_tokens.saturating_sub(user_tokens);
                for next in boundaries.iter().copied().skip(1) {
                    if retained <= target {
                        break;
                    }
                    retained = retained.saturating_sub(
                        (start..next)
                            .filter(|index| Some(*index) != latest_user)
                            .map(|index| count(&conversation[index]))
                            .sum::<u64>(),
                    );
                    start = next;
                }
                // Large tool results are reduced in place without losing call IDs
                // or arguments. Never silently truncate a user's latest request.
                if retained > recent_budget {
                    for message in conversation.iter_mut().skip(start) {
                        if message.role != MessageRole::Tool {
                            continue;
                        }
                        let before = count(message);
                        let allowance =
                            before.saturating_sub(retained.saturating_sub(recent_budget));
                        let mut shortened = message.clone();
                        shortened.content = "[... omitted ...]".to_owned();
                        let allowance = allowance.max(count(&shortened));
                        let original = &message.content;
                        shortened.content = fit_text(original, allowance, |text| {
                            let mut message = shortened.clone();
                            message.content = text.to_owned();
                            count(&message)
                        });
                        retained = retained
                            .saturating_sub(before)
                            .saturating_add(count(&shortened));
                        *message = shortened;
                        compacted = true;
                        if retained <= recent_budget {
                            break;
                        }
                    }
                }
                if retained > recent_budget {
                    return Err(context_limit(
                        "the latest exchange cannot fit alongside instructions and a context summary",
                    ));
                }
                if let Some(index) = latest_user.filter(|index| *index < start) {
                    pinned_user = Some(conversation[index].clone());
                }
                if start > 0 || prior.is_some() || compacted {
                    let excerpts = compact_messages(&input.conversation[..start]);
                    let text = match prior {
                    Some(previous) => format!("{}\n{}", bounded_excerpt(previous, 2_048), excerpts.text),
                    None if start == 0 => "Older tool output was shortened. Read source files again when details are needed.".to_owned(),
                    None => excerpts.text,
                };
                    let text =
                        fit_text(&text, summary_budget, |text| count(&summary_message(text)));
                    if text.is_empty() || count(&summary_message(&text)) > summary_budget {
                        return Err(context_limit(
                            "context budget leaves no room for a compaction summary",
                        ));
                    }
                    compacted |= start > 0 || prior != Some(text.as_str());
                    summary = Some(ContextSummary {
                        text,
                        source_message_count: start,
                        created_at: Timestamp::now(),
                    });
                }
                Ok(())
            })();
            if let Err(error) = outcome {
                if total_tokens > limit {
                    return Err(error);
                }
                // Headroom is a preference. Do not reject a valid request just
                // because its newest exchange cannot be compacted further.
                conversation = input.conversation.clone();
                start = 0;
                compacted = false;
                pinned_user = input
                    .latest_user_message
                    .as_ref()
                    .map(|text| ModelMessage::new(MessageRole::User, text));
                summary = prior.map(|text| ContextSummary {
                    text: text.to_owned(),
                    source_message_count: 0,
                    created_at: Timestamp::now(),
                });
            }
        }
        if let Some(summary) = &summary {
            let message = summary_message(&summary.text);
            items.push(context_item(
                ContextItemKind::Summary,
                "conversation summary (bounded excerpts)",
                count(&message),
                true,
            ));
            messages.push(message);
        }
        if let Some(message) = &pinned_user {
            items.push(context_item(
                ContextItemKind::Conversation,
                "latest user direction (preserved)",
                count(message),
                true,
            ));
            messages.push(message.clone());
        }
        for (index, original) in input.conversation.iter().enumerate() {
            let included = index >= start;
            let mut item = context_item(
                ContextItemKind::Conversation,
                &format!("{:?} message", original.role),
                count(original),
                included,
            );
            if !included {
                item.omission_reason =
                    Some("older exchange compacted into bounded excerpts".to_owned());
            } else {
                let message = &conversation[index];
                if message != original {
                    item.omission_reason =
                        Some("large tool output shortened to fit context".to_owned());
                }
                messages.push(message.clone());
            }
            items.push(item);
        }
        let included_tokens = messages.iter().map(&count).sum::<u64>();
        if included_tokens > limit {
            return Err(context_limit("assembled context exceeds the input budget"));
        }
        let omitted_tokens = input.conversation[..start].iter().map(&count).sum::<u64>()
            + input.conversation[start..]
                .iter()
                .zip(&conversation[start..])
                .map(|(original, shortened)| count(original).saturating_sub(count(shortened)))
                .sum::<u64>();
        Ok(ContextAssembly {
            messages,
            inspection: ContextInspection {
                items,
                total_tokens,
                included_tokens,
                omitted_tokens,
                budget,
                compacted,
                summary,
            },
        })
    }

    pub fn inspect(
        input: &ContextInput,
        options: &ContextAssemblyOptions,
    ) -> Result<ContextInspection> {
        Ok(Self::assemble(input, options)?.inspection)
    }
}

pub fn assemble_context(
    input: &ContextInput,
    options: &ContextAssemblyOptions,
) -> Result<ContextAssembly> {
    ContextAssembler::assemble(input, options)
}

pub fn inspect_context(
    input: &ContextInput,
    options: &ContextAssemblyOptions,
) -> Result<ContextInspection> {
    ContextAssembler::inspect(input, options)
}

pub fn output_reserve(options: &ContextAssemblyOptions) -> u64 {
    options.reserved_output_tokens.unwrap_or_else(|| {
        options
            .context_window
            .map_or(DEFAULT_OUTPUT_RESERVE, |window| {
                (window / 4).clamp(1, DEFAULT_OUTPUT_RESERVE)
            })
    })
}

fn context_limit(message: &str) -> LoomError {
    LoomError::new(ErrorCode::ContextLimitExceeded, message, false)
}

fn context_item(
    kind: ContextItemKind,
    label: &str,
    estimated_tokens: u64,
    included: bool,
) -> ContextItem {
    ContextItem {
        kind,
        label: label.to_owned(),
        estimated_tokens,
        included,
        omission_reason: None,
    }
}

fn summary_message(text: &str) -> ModelMessage {
    // History is data, not a new system instruction.
    ModelMessage::new(
        MessageRole::User,
        format!(
            "Earlier conversation (lossy excerpts; consult original sources for missing details):\n{text}"
        ),
    )
}

fn bounded_excerpt(text: &str, max_chars: usize) -> String {
    let chars: Vec<_> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_owned();
    }
    let half = max_chars / 2;
    format!(
        "{}\n[... omitted ...]\n{}",
        chars[..half].iter().collect::<String>(),
        chars[chars.len() - half..].iter().collect::<String>()
    )
}

fn fit_text(text: &str, budget: u64, count: impl Fn(&str) -> u64) -> String {
    if count(text) <= budget {
        return text.to_owned();
    }
    let mut length = text.chars().count() / 2;
    while length > 0 {
        let excerpt = bounded_excerpt(text, length);
        if count(&excerpt) <= budget {
            return excerpt;
        }
        length /= 2;
    }
    let marker = "[... omitted ...]";
    if count(marker) <= budget {
        marker.to_owned()
    } else {
        String::new()
    }
}

/// Deterministic, explicitly lossy compaction that also works offline. Bound
/// individual excerpts and the total summary so repeated compaction cannot grow
/// without limit. Preserve both ends, where goals and recent outcomes occur.
pub fn compact_messages(messages: &[ModelMessage]) -> ContextSummary {
    let text = messages
        .iter()
        .map(|message| {
            let calls = message
                .tool_calls
                .iter()
                .map(|call| format!("{} {}", call.name, call.arguments))
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "{:?}: {} {}",
                message.role,
                bounded_excerpt(&message.content.replace('\n', " "), 512),
                bounded_excerpt(&calls, 256)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    ContextSummary {
        text: bounded_excerpt(&text, 4_096),
        source_message_count: messages.len(),
        created_at: Timestamp::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ContextInput {
        ContextInput {
            system_instructions: Some("System".to_owned()),
            repository_instructions: Some("Keep changes focused".to_owned()),
            task: "Implement the feature".to_owned(),
            conversation: vec![
                ModelMessage::new(MessageRole::Assistant, "old information ".repeat(500)),
                ModelMessage::new(MessageRole::Assistant, "recent result"),
            ],
            existing_summary: None,
            latest_user_message: None,
        }
    }

    #[test]
    fn inspection_reports_compaction_and_budget_decisions() {
        let assembled = ContextAssembler::assemble(
            &input(),
            &ContextAssemblyOptions {
                max_input_tokens: Some(200),
                reserved_output_tokens: Some(2),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(assembled.inspection.compacted);
        assert!(assembled.inspection.omitted_tokens > 0);
        assert!(
            assembled
                .inspection
                .items
                .iter()
                .any(|item| item.omission_reason.is_some())
        );
    }

    #[test]
    fn required_context_fails_instead_of_silently_truncating() {
        let error = ContextAssembler::assemble(
            &input(),
            &ContextAssemblyOptions {
                max_input_tokens: Some(1),
                reserved_output_tokens: Some(1),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ContextLimitExceeded);
    }

    #[test]
    fn default_output_reserve_scales_for_small_model_windows() {
        let assembled = ContextAssembler::assemble(
            &ContextInput {
                task: "small".to_owned(),
                ..Default::default()
            },
            &ContextAssemblyOptions {
                context_window: Some(64),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(assembled.inspection.budget.effective_input_tokens.unwrap() > 0);
    }
    fn options(limit: u64) -> ContextAssemblyOptions {
        ContextAssemblyOptions {
            max_input_tokens: Some(limit),
            reserved_output_tokens: Some(32),
            ..Default::default()
        }
    }

    fn exchange(output: &str) -> Vec<ModelMessage> {
        let call = loom_model::ToolCall {
            id: loom_core::ToolCallId::new(),
            name: "read_file".to_owned(),
            arguments: Default::default(),
        };
        vec![
            ModelMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                name: None,
                tool_call_id: None,
                tool_calls: vec![call.clone()],
            },
            ModelMessage {
                role: MessageRole::Tool,
                content: output.to_owned(),
                name: Some(call.name),
                tool_call_id: Some(call.id),
                tool_calls: Vec::new(),
            },
        ]
    }

    #[test]
    fn retains_recent_exchanges_and_includes_a_bounded_summary() {
        let mut input = ContextInput {
            task: "Fix the bug".to_owned(),
            ..Default::default()
        };
        input
            .conversation
            .extend(exchange(&"old detail ".repeat(1000)));
        let recent = exchange("latest result");
        input.conversation.extend(recent.clone());
        let assembly = assemble_context(&input, &options(300)).unwrap();
        assert!(assembly.messages.ends_with(&recent));
        assert_eq!(
            assembly
                .inspection
                .summary
                .as_ref()
                .unwrap()
                .source_message_count,
            2
        );
        assert!(assembly.inspection.within_budget());
        assert!(
            assembly
                .inspection
                .items
                .iter()
                .any(|item| item.kind == ContextItemKind::Summary && item.included)
        );
        assert!(
            !assembly
                .messages
                .iter()
                .any(|message| message.tool_calls == input.conversation[0].tool_calls)
        );
    }

    #[test]
    fn shortens_multiple_large_outputs_without_breaking_the_exchange() {
        let mut conversation = exchange(&"first huge result ".repeat(1000));
        let second = exchange(&"second huge result ".repeat(1000));
        conversation[0]
            .tool_calls
            .extend(second[0].tool_calls.clone());
        conversation.push(second[1].clone());
        let input = ContextInput {
            task: "Read both files".to_owned(),
            conversation,
            ..Default::default()
        };
        let assembly = assemble_context(&input, &options(400)).unwrap();
        assert!(assembly.inspection.compacted);
        assert!(assembly.inspection.within_budget());
        let call_message = assembly
            .messages
            .iter()
            .find(|message| !message.tool_calls.is_empty())
            .unwrap();
        assert_eq!(call_message.tool_calls.len(), 2);
        for call in &call_message.tool_calls {
            let result = assembly
                .messages
                .iter()
                .find(|message| message.tool_call_id == Some(call.id))
                .unwrap();
            assert!(result.content.contains("omitted"));
            assert!(!result.content.is_empty());
        }
    }

    #[test]
    fn preserves_latest_user_direction_even_when_compacting_past_it() {
        let input = ContextInput {
            task: "Implement feature".to_owned(),
            conversation: vec![
                ModelMessage::new(MessageRole::User, "Keep the public API unchanged."),
                ModelMessage::new(MessageRole::Assistant, "work details ".repeat(1000)),
                ModelMessage::new(MessageRole::Assistant, "latest result"),
            ],
            ..Default::default()
        };
        let assembly = assemble_context(&input, &options(300)).unwrap();
        assert!(assembly.messages.contains(&input.conversation[0]));
        assert_eq!(assembly.messages.last(), input.conversation.last());
        assert!(assembly.inspection.within_budget());
    }

    #[test]
    fn refuses_to_discard_an_oversized_latest_user_request() {
        let input = ContextInput {
            task: "Task".to_owned(),
            conversation: vec![ModelMessage::new(
                MessageRole::User,
                "important direction ".repeat(1000),
            )],
            ..Default::default()
        };
        assert_eq!(
            assemble_context(&input, &options(100)).unwrap_err().code,
            ErrorCode::ContextLimitExceeded
        );
    }

    #[test]
    fn uses_supplied_counter_and_preserves_unicode() {
        let input = ContextInput {
            task: "Task".to_owned(),
            conversation: vec![
                ModelMessage::new(MessageRole::Assistant, "日本語 🦀 ".repeat(1000)),
                ModelMessage::new(MessageRole::Assistant, "latest"),
            ],
            ..Default::default()
        };
        let count = |message: &ModelMessage| message.content.len() as u64 + 10;
        let assembly =
            ContextAssembler::assemble_with_counter(&input, &options(800), count).unwrap();
        assert_eq!(
            assembly.inspection.included_tokens,
            assembly.messages.iter().map(count).sum::<u64>()
        );
        assert!(assembly.inspection.within_budget());
        assert!(
            assembly
                .inspection
                .summary
                .unwrap()
                .text
                .contains("omitted")
        );
    }

    #[test]
    fn keeps_existing_summary_when_no_new_compaction_is_needed() {
        let input = ContextInput {
            task: "Task".to_owned(),
            existing_summary: Some("Preserve the API; tests passed.".to_owned()),
            latest_user_message: Some("Do not change signatures.".to_owned()),
            conversation: vec![ModelMessage::new(MessageRole::Assistant, "latest")],
            ..Default::default()
        };
        let assembly = assemble_context(&input, &options(500)).unwrap();
        assert!(!assembly.inspection.compacted);
        assert_eq!(
            assembly.inspection.summary.unwrap().text,
            input.existing_summary.unwrap()
        );
        assert!(
            assembly
                .messages
                .iter()
                .any(|message| message.content == "Do not change signatures.")
        );
    }

    #[test]
    fn required_context_can_use_headroom_without_history() {
        let input = ContextInput {
            task: "Task".to_owned(),
            ..Default::default()
        };
        let count = estimate_message_tokens(&ModelMessage::new(MessageRole::User, "Task"));
        let assembly = assemble_context(&input, &options(count)).unwrap();
        assert_eq!(assembly.inspection.included_tokens, count);
        assert!(!assembly.inspection.compacted);
    }
    #[test]
    fn headroom_is_optional_when_the_latest_exchange_already_fits() {
        let input = ContextInput {
            task: "task".to_owned(),
            conversation: vec![ModelMessage::new(
                MessageRole::Assistant,
                "large answer ".repeat(100),
            )],
            ..Default::default()
        };
        let tokens = estimate_message_tokens(&ModelMessage::new(MessageRole::User, &input.task))
            + estimate_message_tokens(&input.conversation[0]);
        let assembly = assemble_context(&input, &options(tokens)).unwrap();
        assert_eq!(assembly.messages.last(), input.conversation.last());
        assert!(!assembly.inspection.compacted);
        assert_eq!(assembly.inspection.included_tokens, tokens);
    }

    #[test]
    fn reduces_summary_allocation_to_preserve_a_large_recent_answer() {
        let input = ContextInput {
            task: "task".to_owned(),
            conversation: vec![
                ModelMessage::new(MessageRole::Assistant, "old answer ".repeat(1000)),
                ModelMessage::new(MessageRole::Assistant, "recent answer ".repeat(100)),
            ],
            ..Default::default()
        };
        let recent = estimate_message_tokens(input.conversation.last().unwrap());
        let assembly = assemble_context(&input, &options(recent + 60)).unwrap();
        assert!(assembly.inspection.compacted);
        assert_eq!(assembly.messages.last(), input.conversation.last());
        assert!(assembly.inspection.within_budget());
    }
}
