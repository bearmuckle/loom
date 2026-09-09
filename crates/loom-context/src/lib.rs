use loom_core::{ErrorCode, LoomError, Result, Timestamp};
use loom_model::{MessageRole, ModelMessage};
use serde::{Deserialize, Serialize};

const DEFAULT_OUTPUT_RESERVE: u64 = 1_024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextItemKind {
    SystemInstructions,
    RepositoryInstructions,
    Task,
    Summary,
    Conversation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextItem {
    pub kind: ContextItemKind,
    pub label: String,
    pub estimated_tokens: u64,
    pub included: bool,
    pub omission_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextBudget {
    pub context_window: Option<u64>,
    pub requested_input_tokens: Option<u64>,
    pub reserved_output_tokens: u64,
    pub effective_input_tokens: Option<u64>,
}

impl ContextBudget {
    pub fn new(
        context_window: Option<u64>,
        requested_input_tokens: Option<u64>,
        reserved_output_tokens: u64,
    ) -> Result<Self> {
        if reserved_output_tokens == 0 {
            return Err(LoomError::invalid_request(
                "context output reserve must be greater than zero",
            ));
        }
        let effective_input_tokens = match (context_window, requested_input_tokens) {
            (Some(window), Some(requested)) => {
                Some(window.saturating_sub(reserved_output_tokens).min(requested))
            }
            (Some(window), None) => Some(window.saturating_sub(reserved_output_tokens)),
            (None, Some(requested)) => Some(requested),
            (None, None) => None,
        };
        if effective_input_tokens == Some(0) {
            return Err(LoomError::new(
                ErrorCode::ContextLimitExceeded,
                "context budget leaves no room for input",
                false,
            ));
        }
        Ok(Self {
            context_window,
            requested_input_tokens,
            reserved_output_tokens,
            effective_input_tokens,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextInspection {
    pub items: Vec<ContextItem>,
    pub total_tokens: u64,
    pub included_tokens: u64,
    pub omitted_tokens: u64,
    pub budget: ContextBudget,
    pub compacted: bool,
    pub summary: Option<ContextSummary>,
}

impl ContextInspection {
    pub fn within_budget(&self) -> bool {
        self.budget
            .effective_input_tokens
            .is_none_or(|budget| self.included_tokens <= budget)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextSummary {
    pub text: String,
    pub source_message_count: usize,
    pub created_at: Timestamp,
}

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
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextAssemblyOptions {
    pub context_window: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub reserved_output_tokens: Option<u64>,
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
        if input.task.trim().is_empty() {
            return Err(LoomError::invalid_request("context task must not be empty"));
        }
        let reserved_output_tokens = options.reserved_output_tokens.unwrap_or_else(|| {
            options
                .context_window
                .map_or(DEFAULT_OUTPUT_RESERVE, |window| {
                    (window / 4).clamp(1, DEFAULT_OUTPUT_RESERVE)
                })
        });
        let budget = ContextBudget::new(
            options.context_window,
            options.max_input_tokens,
            reserved_output_tokens,
        )?;
        let mut candidates = Vec::new();
        if let Some(system) = input
            .system_instructions
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            candidates.push((
                ContextItemKind::SystemInstructions,
                "system instructions".to_owned(),
                ModelMessage::new(MessageRole::System, system),
            ));
        }
        if let Some(repository) = input
            .repository_instructions
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            candidates.push((
                ContextItemKind::RepositoryInstructions,
                "repository instructions".to_owned(),
                ModelMessage::new(
                    MessageRole::System,
                    format!("Repository instructions:\n{repository}"),
                ),
            ));
        }
        candidates.push((
            ContextItemKind::Task,
            "task".to_owned(),
            ModelMessage::new(MessageRole::User, &input.task),
        ));
        if let Some(summary) = input
            .existing_summary
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            candidates.push((
                ContextItemKind::Summary,
                "conversation summary".to_owned(),
                ModelMessage::new(
                    MessageRole::System,
                    format!("Conversation summary:\n{summary}"),
                ),
            ));
        }
        candidates.extend(input.conversation.iter().cloned().map(|message| {
            (
                ContextItemKind::Conversation,
                format!("{:?} message", message.role),
                message,
            )
        }));

        let mut total_tokens = candidates
            .iter()
            .map(|(_, _, message)| estimate_message_tokens(message))
            .sum::<u64>();
        let mut included_tokens: u64 = 0;
        let mut omitted_tokens: u64 = 0;
        let mut messages = Vec::new();
        let mut items = Vec::new();
        let limit = budget.effective_input_tokens;
        let mut omitted_conversation = Vec::new();
        for (kind, label, message) in candidates {
            let estimated_tokens = estimate_message_tokens(&message);
            let mandatory = matches!(
                kind,
                ContextItemKind::SystemInstructions
                    | ContextItemKind::RepositoryInstructions
                    | ContextItemKind::Task
            );
            let fits =
                limit.is_none_or(|limit| included_tokens.saturating_add(estimated_tokens) <= limit);
            if fits || mandatory && messages.is_empty() && limit.is_none() {
                included_tokens = included_tokens.saturating_add(estimated_tokens);
                messages.push(message);
                items.push(ContextItem {
                    kind,
                    label,
                    estimated_tokens,
                    included: true,
                    omission_reason: None,
                });
            } else if mandatory {
                return Err(LoomError::new(
                    ErrorCode::ContextLimitExceeded,
                    format!("required {label} does not fit in the context budget"),
                    false,
                ));
            } else {
                omitted_tokens = omitted_tokens.saturating_add(estimated_tokens);
                if kind == ContextItemKind::Conversation {
                    omitted_conversation.push(message.clone());
                }
                items.push(ContextItem {
                    kind,
                    label,
                    estimated_tokens,
                    included: false,
                    omission_reason: Some(
                        "exceeded input token budget; compact or raise the limit".to_owned(),
                    ),
                });
            }
        }
        let mut summary = None;
        let mut compacted = false;
        if !omitted_conversation.is_empty() {
            let generated = compact_messages(&omitted_conversation);
            summary = Some(generated.clone());
            compacted = true;
            let summary_message = ModelMessage::new(
                MessageRole::System,
                format!("Compacted conversation summary:\n{}", generated.text),
            );
            let summary_tokens = estimate_message_tokens(&summary_message);
            total_tokens = total_tokens.saturating_add(summary_tokens);
            if limit.is_none_or(|limit| included_tokens.saturating_add(summary_tokens) <= limit) {
                included_tokens = included_tokens.saturating_add(summary_tokens);
                messages.push(summary_message);
                items.push(ContextItem {
                    kind: ContextItemKind::Summary,
                    label: "generated conversation summary".to_owned(),
                    estimated_tokens: summary_tokens,
                    included: true,
                    omission_reason: None,
                });
            } else {
                omitted_tokens = omitted_tokens.saturating_add(summary_tokens);
                items.push(ContextItem {
                    kind: ContextItemKind::Summary,
                    label: "generated conversation summary".to_owned(),
                    estimated_tokens: summary_tokens,
                    included: false,
                    omission_reason: Some(
                        "generated summary did not fit; increase the context budget".to_owned(),
                    ),
                });
            }
        }
        if let Some(limit) = limit {
            if included_tokens > limit {
                return Err(LoomError::new(
                    ErrorCode::ContextLimitExceeded,
                    format!(
                        "assembled context uses {included_tokens} tokens but the budget is {limit}"
                    ),
                    false,
                ));
            }
        }
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

pub fn estimate_message_tokens(message: &ModelMessage) -> u64 {
    (message.content.chars().count() as u64).div_ceil(4)
}

pub fn compact_messages(messages: &[ModelMessage]) -> ContextSummary {
    let text = messages
        .iter()
        .map(|message| {
            let role = match message.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };
            format!("{role}: {}", message.content.replace('\n', " "))
        })
        .collect::<Vec<_>>()
        .join("\n");
    ContextSummary {
        text,
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
                ModelMessage::new(MessageRole::Assistant, "A".repeat(100)),
                ModelMessage::new(MessageRole::Tool, "B".repeat(100)),
            ],
            existing_summary: None,
        }
    }

    #[test]
    fn inspection_reports_compaction_and_budget_decisions() {
        let assembled = ContextAssembler::assemble(
            &input(),
            &ContextAssemblyOptions {
                max_input_tokens: Some(20),
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
}
