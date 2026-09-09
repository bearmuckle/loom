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
