use super::*;

impl InProcessConnection {
    /// Reads the child model policy of the project `session_id` belongs to.
    pub(crate) fn get_project_child_models(
        &self,
        session_id: AgentSessionId,
    ) -> Result<ServerResponse> {
        let persistence = self.child_model_persistence()?;
        let project = self.load_project_snapshot_for_session(session_id)?;
        let policy = persistence.load_project_child_model_policy(project.project_id)?;
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildModels {
                project_id: project.project_id,
                policy,
            },
        ))
    }

    /// Replaces the child model policy of the project rooted at `session_id`.
    ///
    /// Every model the policy names must resolve to a configured provider
    /// with pricing, exactly as a delegated child's model must, so a policy
    /// that would make delegation fail is refused when it is set rather than
    /// when an agent first delegates.
    pub(crate) fn set_project_child_models(
        &self,
        session_id: AgentSessionId,
        policy: loom_core::ProjectChildModelPolicy,
    ) -> Result<ServerResponse> {
        let persistence = self.child_model_persistence()?;
        let project = self.load_project_snapshot_for_session(session_id)?;
        if project.root_session_id != session_id {
            return Err(LoomError::invalid_request(format!(
                "session {session_id} is a member of project {}; set child models on the \
                 project root {}",
                project.project_id, project.root_session_id
            )));
        }
        let policy = policy.normalized()?;
        for model in policy.model_ids() {
            let model_id = ModelId::new(model);
            self.backend
                .provider(&model_id)
                .and_then(|_| self.backend.providers.pricing(&model_id).map(|_| ()))
                .map_err(|error| {
                    LoomError::invalid_request(format!(
                        "child model '{model}' is not usable on this worker: {}; \
                         `loom-cli models list` shows the configured models",
                        error.message
                    ))
                })?;
        }
        persistence.replace_project_child_model_policy(project.project_id, &policy)?;
        Ok(ServerResponse::Project(
            ProjectResponse::ProjectChildModels {
                project_id: project.project_id,
                policy,
            },
        ))
    }

    fn child_model_persistence(&self) -> Result<&Arc<dyn loom_persistence::Persistence>> {
        self.backend.persistence.as_ref().ok_or_else(|| {
            LoomError::new(
                ErrorCode::UnsupportedCapability,
                "project child models require durable storage",
                false,
            )
        })
    }
}
