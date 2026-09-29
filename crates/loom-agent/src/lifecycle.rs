use super::*;

impl AgentRuntime {
    pub fn new(
        session_id: AgentSessionId,
        task: AgentTask,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
    ) -> Self {
        Self::new_with_options(
            session_id,
            task,
            provider,
            tools,
            ApprovalPolicy::default(),
            AgentRuntimeOptions::default(),
        )
    }

    pub fn new_with_options(
        session_id: AgentSessionId,
        task: AgentTask,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
        approval_policy: ApprovalPolicy,
        options: AgentRuntimeOptions,
    ) -> Self {
        let now = Timestamp::now();
        let run = AgentRunSnapshot {
            id: RunId::new(),
            attempt_id: loom_core::RunAttemptId::new(),
            control_revision: 0,
            session_id,
            task: task.task.clone(),
            model: task.model.clone(),
            state: AgentRunState::Planning,
            started_at: now,
            updated_at: now,
            completed_at: None,
            summary: None,
            evidence: Vec::new(),
        };
        let attempts = vec![loom_protocol::AgentRunAttemptRecord {
            run_id: run.id,
            session_id,
            id: run.attempt_id,
            number: 1,
            state: run.state,
            checkpoint_id: options.checkpoint_id,
            started_at: now,
            completed_at: None,
        }];
        let plan = AgentPlan { steps: Vec::new() };
        let messages = initial_messages(&task);
        let message_timeline_ordinals = (0..messages.len())
            .map(|ordinal| u64::try_from(ordinal).expect("initial message count fits in u64"))
            .collect::<Vec<_>>();
        let next_timeline_ordinal =
            u64::try_from(messages.len()).expect("initial message count fits in u64");
        Self {
            session_id,
            task,
            run,
            attempts,
            plan,
            provider: Some(provider),
            tools,
            messages,
            message_timeline_ordinals,
            next_timeline_ordinal,
            last_project_message_sequence: 0,
            pending_approval: None,
            pending_tool_execution: None,
            pending_project_join: None,
            pending_input: None,
            last_failed_call: None,
            next_message_id: 0,
            active_message_id: None,
            approval_policy,
            options,
            usage: UsageSnapshot::default(),
            context_inspection: None,
            context_checkpoint: None,
            provider_cursor: 0,
            step_id: None,
            step_index: 0,
            activities: Vec::new(),
            interactions: Vec::new(),
            denied_tool_calls: BTreeSet::new(),
            control: RunControl::new(),
            observer: None,
            flush_offset: 0,
        }
    }

    pub fn new_with_policy(
        session_id: AgentSessionId,
        task: AgentTask,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
        approval_policy: ApprovalPolicy,
    ) -> Self {
        Self::new_with_options(
            session_id,
            task,
            provider,
            tools,
            approval_policy,
            AgentRuntimeOptions::default(),
        )
    }

    pub fn new_with_policy_and_options(
        session_id: AgentSessionId,
        task: AgentTask,
        provider: Box<dyn ModelProvider>,
        tools: ToolExecutor,
        approval_policy: ApprovalPolicy,
        options: AgentRuntimeOptions,
    ) -> Self {
        Self::new_with_options(session_id, task, provider, tools, approval_policy, options)
    }
}
