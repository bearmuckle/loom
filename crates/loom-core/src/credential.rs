//! Non-secret metadata of provider credentials.
//!
//! A provider credential is one person's account with one model provider (for
//! example a GitHub Copilot sign-in). Every new sign-in creates a new
//! *version* of that person's credential instead of overwriting the secret a
//! running agent uses. A run pins one credential id and version for its whole
//! lifetime.
//!
//! These types carry metadata only. Secrets stay on the worker that owns the
//! credential and are never part of a response, an event, a snapshot, or a
//! log line.

use crate::{CredentialId, ParticipantId, Timestamp};
use serde::{Deserialize, Serialize};

/// What clients may know about one provider credential.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderCredentialMetadata {
    pub id: CredentialId,
    /// Provider the credential authenticates with, for example
    /// `github-copilot`.
    pub provider: String,
    /// The person the credential belongs to; `None` for the worker-wide
    /// credential configured by the operator.
    pub owner: Option<ParticipantId>,
    /// Display label of the owner, shown before a run starts.
    pub owner_label: String,
    /// Latest version. A new sign-in increments it; older versions stay
    /// resolvable for runs that pinned them.
    pub version: u32,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// A revoked credential never authenticates again, in any version.
    pub revoked: bool,
}

/// The credential a session owner selected for later runs of the session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionCredentialSelection {
    pub credential_id: CredentialId,
    /// Who selected it; `None` for an operator connection.
    pub selected_by: Option<ParticipantId>,
    pub selected_at: Timestamp,
}

/// The immutable credential and model a run uses for its lifetime.
///
/// It is fixed when the run starts and does not change when another
/// participant signs in, the owner disconnects, someone sends a follow-up, or
/// the owner selects a different credential for later runs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunCredentialPin {
    /// The pinned credential; `None` when the session had no selected
    /// credential and the run uses the worker-wide provider configuration.
    pub credential_id: Option<CredentialId>,
    /// The pinned version of `credential_id`.
    pub version: Option<u32>,
    pub provider: String,
    pub model: String,
    pub owner: Option<ParticipantId>,
    pub owner_label: String,
}
