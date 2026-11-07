//! Jev activation: one master switch, two sub-toggles, three scopes.
//!
//! A workspace stores the switches once and a project or a session overrides
//! any of them, so a project can withhold a switch without editing the
//! workspace and one run can turn Jev off without touching either. Every
//! override is three-state (`inherit | on | off`) and defaults to inheriting, so
//! an older client round-tripping a policy clears nothing.
//!
//! The switches alone never turn Jev on. A run resolves them and then ANDs the
//! answer with whether the decider's credential resolves on this worker, so Jev
//! is absent, not disabled, on a worker without a key: [`JevSettings::resolve`]
//! returns `None` and every caller renders nothing.

use serde::{Deserialize, Serialize};

/// The Jev switches a workspace stores.
///
/// `auto_model` is off because choosing a model through Jev changes what a run
/// costs, so it is an explicit opt-in. `decide_tool` is on because the tool is
/// read-only: it runs under the default approval policy without a prompt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct JevSettings {
    /// The master switch. Meaningful only once the decider credential resolves.
    pub enabled: bool,
    /// Resolve `--model auto` through Jev instead of the caller's default.
    pub auto_model: bool,
    /// Let agents ask Jev through the `decide` tool.
    pub decide_tool: bool,
}

impl Default for JevSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_model: false,
            decide_tool: true,
        }
    }
}

/// A project root's overrides of the workspace switches.
///
/// `None` inherits the workspace value. The master switch is not project-scoped:
/// turning Jev off for one project is a session decision, so a project cannot
/// hide Jev from the work that is already running against it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ProjectJevPolicy {
    pub auto_model: Option<bool>,
    pub decide_tool: Option<bool>,
}

/// A session's overrides of the project and workspace switches.
///
/// `None` inherits. The master switch is here too, so one run or one session
/// can opt out of Jev without editing the project or the workspace.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct SessionJevPolicy {
    pub enabled: Option<bool>,
    pub auto_model: Option<bool>,
    pub decide_tool: Option<bool>,
}

/// The Jev switches a run evaluates against, pinned when it is created.
///
/// A run reads its switches once, so a later session or project change applies
/// from the next run instead of mutating a run that is already spending money.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JevPolicy {
    pub enabled: bool,
    pub auto_model: bool,
    pub decide_tool: bool,
}

impl JevSettings {
    /// The switches in force for a run, after the session and project overrides
    /// and the credential gate.
    ///
    /// The first explicit scope wins per switch: session, then project, then the
    /// workspace. `credential_available` is whether the decider's credential
    /// resolves on this worker, so a worker without one resolves to `None` no
    /// matter what any scope says.
    pub fn resolve(
        &self,
        session: &SessionJevPolicy,
        project: &ProjectJevPolicy,
        credential_available: bool,
    ) -> Option<JevPolicy> {
        if !credential_available {
            return None;
        }
        let policy = JevPolicy {
            enabled: session.enabled.unwrap_or(self.enabled),
            auto_model: session
                .auto_model
                .or(project.auto_model)
                .unwrap_or(self.auto_model),
            decide_tool: session
                .decide_tool
                .or(project.decide_tool)
                .unwrap_or(self.decide_tool),
        };
        policy.enabled.then_some(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::{JevSettings, ProjectJevPolicy, SessionJevPolicy};

    #[test]
    fn workspace_defaults_opt_into_the_master_switch_and_the_tool() {
        let settings = JevSettings::default();

        assert!(settings.enabled);
        // Selecting a model through Jev changes what a run costs.
        assert!(!settings.auto_model);
        assert!(settings.decide_tool);
    }

    #[test]
    fn every_scope_starts_by_inheriting() {
        assert_eq!(ProjectJevPolicy::default().auto_model, None);
        assert_eq!(ProjectJevPolicy::default().decide_tool, None);
        assert_eq!(SessionJevPolicy::default().enabled, None);
        assert_eq!(SessionJevPolicy::default().auto_model, None);
        assert_eq!(SessionJevPolicy::default().decide_tool, None);
    }

    #[test]
    fn an_absent_scope_keeps_the_workspace_switch() {
        let settings = JevSettings {
            enabled: true,
            auto_model: true,
            decide_tool: false,
        };

        let policy = settings
            .resolve(
                &SessionJevPolicy::default(),
                &ProjectJevPolicy::default(),
                true,
            )
            .unwrap();

        assert!(policy.enabled);
        assert!(policy.auto_model);
        assert!(!policy.decide_tool);
    }

    #[test]
    fn the_project_override_beats_the_workspace_and_the_session_beats_the_project() {
        let settings = JevSettings {
            enabled: true,
            auto_model: false,
            decide_tool: true,
        };
        let project = ProjectJevPolicy {
            auto_model: Some(true),
            decide_tool: Some(false),
        };

        let inherited = settings
            .resolve(&SessionJevPolicy::default(), &project, true)
            .unwrap();
        assert!(inherited.auto_model);
        assert!(!inherited.decide_tool);

        let session = SessionJevPolicy {
            enabled: None,
            auto_model: Some(false),
            decide_tool: Some(true),
        };
        let overridden = settings.resolve(&session, &project, true).unwrap();
        assert!(!overridden.auto_model);
        assert!(overridden.decide_tool);
    }

    #[test]
    fn a_session_can_turn_the_master_switch_off_over_a_workspace_that_has_it_on() {
        let settings = JevSettings::default();
        let session = SessionJevPolicy {
            enabled: Some(false),
            ..SessionJevPolicy::default()
        };

        assert_eq!(
            settings.resolve(&session, &ProjectJevPolicy::default(), true),
            None
        );
    }

    #[test]
    fn a_master_switch_that_is_off_resolves_to_no_policy() {
        let settings = JevSettings {
            enabled: false,
            auto_model: true,
            decide_tool: true,
        };

        assert_eq!(
            settings.resolve(
                &SessionJevPolicy::default(),
                &ProjectJevPolicy::default(),
                true
            ),
            None
        );
    }

    #[test]
    fn a_session_can_turn_the_master_switch_on_over_a_workspace_that_has_it_off() {
        let settings = JevSettings {
            enabled: false,
            auto_model: false,
            decide_tool: false,
        };
        let session = SessionJevPolicy {
            enabled: Some(true),
            decide_tool: Some(true),
            ..SessionJevPolicy::default()
        };

        let policy = settings
            .resolve(&session, &ProjectJevPolicy::default(), true)
            .unwrap();
        assert!(policy.enabled);
        assert!(policy.decide_tool);
    }

    #[test]
    fn a_worker_without_a_decider_credential_resolves_to_no_policy() {
        // Nothing a scope says matters: without a key no call can succeed, so
        // every surface is absent rather than disabled.
        let settings = JevSettings {
            enabled: true,
            auto_model: true,
            decide_tool: true,
        };
        let session = SessionJevPolicy {
            enabled: Some(true),
            auto_model: Some(true),
            decide_tool: Some(true),
        };
        let project = ProjectJevPolicy {
            auto_model: Some(true),
            decide_tool: Some(true),
        };

        assert_eq!(settings.resolve(&session, &project, false), None);
    }
}
