use super::*;

impl InProcessBackend {
    /// The Jev switches in force for a session's next run.
    ///
    /// The workspace holds the switches, project and session overrides inherit
    /// until their storage lands, and the decider credential is the gate: no key
    /// on this worker resolves to `None`, so every Jev surface is absent.
    pub(crate) fn session_jev_policy(
        &self,
        session_id: AgentSessionId,
    ) -> Result<Option<JevPolicy>> {
        let workspace_id = self.sessions()?.get(session_id)?.workspace_id;
        let settings = self
            .workspace_configs()?
            .get(&workspace_id)
            .map(|config| config.jev)
            .unwrap_or_default();
        Ok(settings.resolve(
            &SessionJevPolicy::default(),
            &ProjectJevPolicy::default(),
            self.providers.decider_available()?,
        ))
    }

    /// Attaches the read-only `decide` tool to a run's executor when Jev allows
    /// it, and returns the switches the run pins.
    ///
    /// A workspace with the tool switched off, the master switch off, or no
    /// decider credential gets an executor without the tool, so the tool is
    /// absent rather than advertised and refused. The decision client resolves
    /// its key here, once per run, so a replaced key reaches the next run.
    pub(crate) fn with_jev_tools(
        &self,
        tools: ToolExecutor,
        session_id: AgentSessionId,
    ) -> Result<(ToolExecutor, Option<JevPolicy>)> {
        let policy = self.session_jev_policy(session_id)?;
        if policy.is_some_and(|policy| policy.decide_tool)
            && let Some(decider) = self.providers.decider()?
        {
            return Ok((tools.with_decision_provider(decider), policy));
        }
        Ok((tools, policy))
    }
}
