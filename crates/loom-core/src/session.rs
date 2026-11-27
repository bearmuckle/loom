use std::collections::BTreeMap;

use crate::{
    AgentSessionId, EventSequence, LoomError, Result, SessionTitleSource, Timestamp, WorkspaceId,
};
use serde::{Deserialize, Serialize};

/// Longest accepted session goal, in bytes of its trimmed UTF-8 encoding.
pub const MAX_AGENT_SESSION_GOAL_BYTES: usize = 8192;

/// Trims and validates a session goal before any state changes.
pub fn normalize_session_goal(raw: &str) -> Result<String> {
    let goal = raw.trim();
    if goal.is_empty() {
        return Err(LoomError::invalid_request(
            "agent session goal must not be empty",
        ));
    }
    if goal.len() > MAX_AGENT_SESSION_GOAL_BYTES {
        return Err(LoomError::invalid_request(format!(
            "agent session goal must be at most {MAX_AGENT_SESSION_GOAL_BYTES} bytes"
        )));
    }
    Ok(goal.to_owned())
}

/// Serializable snapshot of the session manager, owned by the neutral domain
/// layer so storage does not depend on the session crate.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionManagerState {
    pub sessions: BTreeMap<AgentSessionId, AgentSessionSnapshot>,
    pub next_sequence: EventSequence,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionState {
    Idle,
    Queued,
    Planning,
    AwaitingApproval,
    Paused,
    Executing,
    Evaluating,
    NeedsInput,
    Completed,
    Failed,
    Cancelled,
    Archived,
}

/// Canonical ACP harness identifier for Google's official Antigravity ACP agent.
pub const ANTIGRAVITY_HARNESS_ID: &str = "antigravity";

/// Default ACP authenticate method for Antigravity (official interactive sign-in).
pub const ANTIGRAVITY_AUTH_OAUTH_PERSONAL: &str = "oauth-personal";

/// Terms-safe ACP authenticate method for Antigravity using an operator-provided
/// `GEMINI_API_KEY` environment variable inherited by the harness process.
pub const ANTIGRAVITY_AUTH_GEMINI_API_KEY: &str = "gemini-api-key";

/// Disclosure shown in UI and status surfaces for the Antigravity ACP backend:
/// written Terms risk under Antigravity ToS §6 and FAQ.
pub const ANTIGRAVITY_TERMS_RISK_NOTICE: &str = "Terms risk: Antigravity Terms of Service section 6 and FAQ forbid third-party software using Antigravity OAuth on consumer plans and note account suspension risk. Use authenticate(\"gemini-api-key\") with GEMINI_API_KEY in the worker environment for the ToS-safe path.";

/// Disclosure shown in UI and status surfaces for the Antigravity ACP backend:
/// consumer Interactions / model-training note.
pub const ANTIGRAVITY_INTERACTIONS_NOTICE: &str = "Interactions / training: On consumer Antigravity plans, Google may use Interactions to evaluate and improve Google machine-learning technologies unless disabled in Google's in-product settings.";

/// Disclosure shown in UI and status surfaces for the Antigravity ACP backend:
/// unregistered ACP client identity and interactive-only scope.
pub const ANTIGRAVITY_UNREGISTERED_CLIENT_NOTICE: &str = "Unregistered client & interactive-only: Loom is not a Google-registered ACP client and identifies itself as \"loom\" (never impersonating T3 Code). Runs are strictly interactive-only (unattended, scheduled, CI, and auto-approve runs are refused).";

/// Capabilities advertised by an external ACP harness during `initialize`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpCapabilities {
    #[serde(default)]
    pub load_session: bool,
    #[serde(default)]
    pub resume_session: bool,
    #[serde(default)]
    pub list_sessions: bool,
    #[serde(default)]
    pub prompt_image: bool,
    #[serde(default)]
    pub prompt_audio: bool,
    #[serde(default)]
    pub prompt_embedded_context: bool,
    #[serde(default)]
    pub auth_methods: Vec<String>,
}

impl AcpCapabilities {
    /// Whether the harness genuinely advertised durable session resume.
    ///
    /// Loom only claims replay/resume when the harness reports `loadSession` or
    /// `sessionCapabilities.resume`; otherwise the journal is a projection only.
    pub fn supports_resume(&self) -> bool {
        self.load_session || self.resume_session
    }
}

/// Model entry reported by an ACP harness's own catalog (`session/new` models or
/// `configOptions`). Loom does not hard-code Antigravity models.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpModelCatalogEntry {
    pub id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Metadata recorded for a session owned by an external ACP harness.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpSessionBackend {
    /// Harness identifier, e.g. `"antigravity"`.
    pub harness: String,
    /// Harness command configured or discovered on the worker (never carries secrets).
    pub command: String,
    /// Negotiated ACP protocol version (e.g. `1`), populated once `initialize` succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
    /// Selected ACP `authenticate` method (e.g. `"oauth-personal"` or `"gemini-api-key"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    /// Agent name from `initialize` `agentInfo.name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Agent version from `initialize` `agentInfo.version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    /// Capabilities advertised by the ACP harness during `initialize`.
    #[serde(default)]
    pub capabilities: AcpCapabilities,
    /// Harness-assigned ACP session id from `session/new`, used for
    /// `session/load` or `session/resume` only when advertised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_session_id: Option<String>,
    /// Models reported by the harness catalog for the active sign-in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<AcpModelCatalogEntry>,
}

/// Describes which execution backend owns an agent session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionBackendDescriptor {
    Native,
    Acp(Box<AcpSessionBackend>),
}

impl SessionBackendDescriptor {
    pub fn acp(backend: AcpSessionBackend) -> Self {
        Self::Acp(Box::new(backend))
    }

    pub fn as_acp(&self) -> Option<&AcpSessionBackend> {
        match self {
            Self::Native => None,
            Self::Acp(backend) => Some(backend.as_ref()),
        }
    }

    pub fn is_acp(&self) -> bool {
        matches!(self, Self::Acp(_))
    }

    pub fn is_antigravity(&self) -> bool {
        matches!(
            self,
            Self::Acp(backend) if backend.harness == ANTIGRAVITY_HARNESS_ID
        )
    }

    /// Whether this session's backend supports durable replay/resume after a
    /// worker or harness restart.
    pub fn supports_durable_resume(&self) -> bool {
        match self {
            Self::Native => true,
            Self::Acp(backend) => backend.capabilities.supports_resume(),
        }
    }

    pub fn harness_name(&self) -> Option<&str> {
        match self {
            Self::Native => None,
            Self::Acp(backend) => Some(backend.harness.as_str()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentSessionSnapshot {
    pub id: AgentSessionId,
    pub workspace_id: WorkspaceId,
    /// The session's name. A project's row in the sidebar is rendered from its
    /// root session's name, so this doubles as the project title.
    pub name: String,
    /// The session's stated goal, trimmed and non-empty when set. A project's
    /// root goal is what the manager is told it owns. The field is additive, so
    /// a snapshot written before it existed reads as no goal.
    #[serde(default)]
    pub goal: Option<String>,
    pub state: AgentSessionState,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Who chose the current name. A manual rename pins it and stops automatic
    /// re-titling. A snapshot written before this field existed reads as
    /// `auto`.
    #[serde(default)]
    pub title_source: SessionTitleSource,
    /// Execution backend descriptor for this session. `None` (and absent in
    /// older snapshots) means Loom's native agent loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<SessionBackendDescriptor>,
}

impl AgentSessionSnapshot {
    /// Whether this session is backed by an external ACP harness.
    pub fn is_acp(&self) -> bool {
        self.backend
            .as_ref()
            .is_some_and(SessionBackendDescriptor::is_acp)
    }

    /// Whether this session is backed by the Antigravity ACP harness.
    pub fn is_antigravity(&self) -> bool {
        self.backend
            .as_ref()
            .is_some_and(SessionBackendDescriptor::is_antigravity)
    }

    /// Honest resume capability: native sessions always support durable journal
    /// replay; ACP sessions only when the harness advertised `loadSession` or
    /// `resume`.
    pub fn supports_durable_resume(&self) -> bool {
        self.backend
            .as_ref()
            .is_none_or(SessionBackendDescriptor::supports_durable_resume)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SessionEvent {
    AgentSessionCreated {
        snapshot: AgentSessionSnapshot,
    },
    AgentSessionStateChanged {
        session_id: AgentSessionId,
        previous: AgentSessionState,
        current: AgentSessionState,
    },
    AgentSessionForked {
        source_session_id: AgentSessionId,
        snapshot: AgentSessionSnapshot,
    },
    AgentSessionRenamed {
        session_id: AgentSessionId,
        name: String,
        /// Who chose the new name. The field is additive, so a record written
        /// before it existed reads as `auto`.
        #[serde(default)]
        source: SessionTitleSource,
    },
    AgentSessionGoalChanged {
        session_id: AgentSessionId,
        /// The new goal, or `None` when the goal was cleared. A record written
        /// before clearing existed always carries a goal.
        goal: Option<String>,
    },
    /// The session's title source changed without renaming it, so automation
    /// starts or stops for that session.
    AgentSessionTitleSourceChanged {
        session_id: AgentSessionId,
        source: SessionTitleSource,
    },
    AgentSessionArchived {
        session_id: AgentSessionId,
    },
    /// The session left the archive and returned to `Idle`.
    AgentSessionRestored {
        session_id: AgentSessionId,
    },
    /// The session's backend metadata was updated (for example after an ACP
    /// harness completed `initialize` and `session/new` and reported its
    /// protocol version, capabilities, and model catalog).
    AgentSessionBackendUpdated {
        session_id: AgentSessionId,
        backend: SessionBackendDescriptor,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionEventRecord {
    pub sequence: EventSequence,
    pub session_id: AgentSessionId,
    pub occurred_at: Timestamp,
    pub event: SessionEvent,
}
