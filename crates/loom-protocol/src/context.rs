use loom_core::{ErrorCode, LoomError, Result, Timestamp};
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextSummary {
    pub text: String,
    pub source_message_count: usize,
    /// Version of the deterministic repaired-history projection covered by this summary.
    #[serde(default)]
    pub projection_version: u32,
    /// SHA-256 of the ordered projected messages before `source_message_count`.
    #[serde(default)]
    pub source_digest: String,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextAssemblyOptions {
    pub context_window: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub reserved_output_tokens: Option<u64>,
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

impl ContextInspection {
    pub fn within_budget(&self) -> bool {
        self.budget
            .effective_input_tokens
            .is_none_or(|budget| self.included_tokens <= budget)
    }
}

#[cfg(test)]
mod tests {
    use super::{ContextBudget, ContextInspection, ContextSummary};
    use loom_core::ErrorCode;

    #[test]
    fn context_budget_reserves_output_and_rejects_empty_input_space() {
        assert_eq!(
            ContextBudget::new(Some(100), Some(80), 20)
                .unwrap()
                .effective_input_tokens,
            Some(80)
        );
        assert_eq!(
            ContextBudget::new(Some(100), None, 20)
                .unwrap()
                .effective_input_tokens,
            Some(80)
        );
        assert_eq!(
            ContextBudget::new(None, Some(80), 20)
                .unwrap()
                .effective_input_tokens,
            Some(80)
        );
        assert_eq!(
            ContextBudget::new(None, None, 20)
                .unwrap()
                .effective_input_tokens,
            None
        );
        assert_eq!(
            ContextBudget::new(Some(10), None, 10).unwrap_err().code,
            ErrorCode::ContextLimitExceeded
        );
        assert_eq!(
            ContextBudget::new(None, None, 0).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn inspection_checks_included_tokens_when_a_budget_exists() {
        let mut inspection = serde_json::from_value::<ContextInspection>(serde_json::json!({
            "items": [], "total_tokens": 0, "included_tokens": 10,
            "omitted_tokens": 0, "budget": {
                "context_window": null, "requested_input_tokens": null,
                "reserved_output_tokens": 1, "effective_input_tokens": 10
            }, "compacted": false, "summary": null
        }))
        .unwrap();
        assert!(inspection.within_budget());
        inspection.included_tokens = 11;
        assert!(!inspection.within_budget());
        inspection.budget.effective_input_tokens = None;
        assert!(inspection.within_budget());
    }

    #[test]
    fn context_summary_projection_metadata_defaults_for_older_serialized_values() {
        let current = ContextSummary {
            text: "summary".to_owned(),
            source_message_count: 3,
            projection_version: 1,
            source_digest: "digest".to_owned(),
            created_at: loom_core::Timestamp::now(),
        };
        let mut value = serde_json::to_value(&current).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("projection_version");
        object.remove("source_digest");

        let legacy = serde_json::from_value::<ContextSummary>(value).unwrap();
        assert_eq!(legacy.text, current.text);
        assert_eq!(legacy.source_message_count, current.source_message_count);
        assert_eq!(legacy.projection_version, 0);
        assert!(legacy.source_digest.is_empty());
        assert_eq!(legacy.created_at, current.created_at);
    }
}
