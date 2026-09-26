use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionLimits {
    pub max_duration_ms: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_tool_calls: Option<u64>,
    pub max_cost_micros: Option<u64>,
}

impl SessionLimits {
    pub fn is_unlimited(&self) -> bool {
        self.max_duration_ms.is_none()
            && self.max_input_tokens.is_none()
            && self.max_output_tokens.is_none()
            && self.max_tool_calls.is_none()
            && self.max_cost_micros.is_none()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub tool_calls: u64,
    pub cost_micros: u64,
    pub elapsed_ms: u64,
}

impl UsageSnapshot {
    pub fn add_tokens(&mut self, input_tokens: u64, output_tokens: u64, cached_input_tokens: u64) {
        self.input_tokens = self.input_tokens.saturating_add(input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(output_tokens);
        self.cached_input_tokens = self.cached_input_tokens.saturating_add(cached_input_tokens);
    }

    pub fn add_tool_call(&mut self) {
        self.tool_calls = self.tool_calls.saturating_add(1);
    }

    pub fn add_cost_micros(&mut self, cost_micros: u64) {
        self.cost_micros = self.cost_micros.saturating_add(cost_micros);
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Duration,
    ContextTokens,
    InputTokens,
    OutputTokens,
    ToolCalls,
    Cost,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LimitStatus {
    pub limits: SessionLimits,
    pub usage: UsageSnapshot,
    pub exceeded: Vec<LimitKind>,
}

impl LimitStatus {
    pub fn new(limits: SessionLimits, usage: UsageSnapshot) -> Self {
        let mut exceeded = Vec::new();
        if limits
            .max_duration_ms
            .is_some_and(|limit| usage.elapsed_ms >= limit)
        {
            exceeded.push(LimitKind::Duration);
        }
        if limits
            .max_input_tokens
            .is_some_and(|limit| usage.input_tokens >= limit)
        {
            exceeded.push(LimitKind::InputTokens);
        }
        if limits
            .max_output_tokens
            .is_some_and(|limit| usage.output_tokens >= limit)
        {
            exceeded.push(LimitKind::OutputTokens);
        }
        if limits
            .max_tool_calls
            .is_some_and(|limit| usage.tool_calls >= limit)
        {
            exceeded.push(LimitKind::ToolCalls);
        }
        if limits
            .max_cost_micros
            .is_some_and(|limit| usage.cost_micros >= limit)
        {
            exceeded.push(LimitKind::Cost);
        }
        Self {
            limits,
            usage,
            exceeded,
        }
    }

    pub fn is_exceeded(&self) -> bool {
        !self.exceeded.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_counts_saturate_and_unlimited_limits_are_detected() {
        assert!(SessionLimits::default().is_unlimited());
        assert!(
            !SessionLimits {
                max_cost_micros: Some(1),
                ..Default::default()
            }
            .is_unlimited()
        );
        let mut usage = UsageSnapshot {
            input_tokens: u64::MAX,
            tool_calls: u64::MAX,
            cost_micros: u64::MAX,
            ..Default::default()
        };
        usage.add_tokens(1, 4, 2);
        usage.add_tool_call();
        usage.add_cost_micros(1);
        assert_eq!(usage.input_tokens, u64::MAX);
        assert_eq!(usage.tool_calls, u64::MAX);
        assert_eq!(usage.cost_micros, u64::MAX);
        assert_eq!(usage.total_tokens(), u64::MAX);
        assert_eq!(usage.cached_input_tokens, 2);
    }

    #[test]
    fn limit_status_checks_duration_and_each_usage_budget() {
        let status = LimitStatus::new(
            SessionLimits {
                max_duration_ms: Some(5),
                max_input_tokens: Some(3),
                max_output_tokens: Some(4),
                max_tool_calls: Some(2),
                max_cost_micros: Some(9),
            },
            UsageSnapshot {
                elapsed_ms: 5,
                input_tokens: 3,
                output_tokens: 4,
                tool_calls: 2,
                cost_micros: 9,
                ..Default::default()
            },
        );
        assert_eq!(
            status.exceeded,
            vec![
                LimitKind::Duration,
                LimitKind::InputTokens,
                LimitKind::OutputTokens,
                LimitKind::ToolCalls,
                LimitKind::Cost,
            ]
        );
        assert!(status.is_exceeded());
        assert!(
            !LimitStatus::new(SessionLimits::default(), UsageSnapshot::default()).is_exceeded()
        );
    }

    #[test]
    fn limit_status_reports_each_explicitly_exceeded_budget() {
        let limits = SessionLimits {
            max_input_tokens: Some(2),
            max_tool_calls: Some(1),
            ..Default::default()
        };
        let usage = UsageSnapshot {
            input_tokens: 2,
            tool_calls: 1,
            ..Default::default()
        };

        let status = LimitStatus::new(limits, usage);

        assert_eq!(
            status.exceeded,
            vec![LimitKind::InputTokens, LimitKind::ToolCalls]
        );
    }
}
