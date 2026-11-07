//! The project's choice of model for the child agents it delegates to.
//!
//! A project root stores one optional default child model and an optional
//! model per kind of work, such as `cheap`, `coding`, `review`, or `vision`.
//! Kinds are short free-form labels rather than a fixed set: what counts as a
//! kind of work differs per project and per provider line-up, and the delegate
//! tool lists the kinds a project defines, so an agent never has to guess them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{LoomError, Result};

/// Most kinds one project maps to a model.
pub const MAX_CHILD_MODEL_KINDS: usize = 16;
/// Longest kind label, in bytes.
pub const MAX_CHILD_MODEL_KIND_BYTES: usize = 32;
/// Longest model ID a policy stores, in bytes; the bound durable model IDs use.
pub const MAX_CHILD_MODEL_ID_BYTES: usize = 4096;
/// The model ID alias that names the delegating agent's own model.
pub const CURRENT_MODEL_ALIAS: &str = "current";

/// The models a project's delegated children run on unless the delegating
/// agent names one explicitly.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectChildModelPolicy {
    /// The model a child runs on when its kind maps to none. `None` reuses the
    /// delegating agent's model.
    #[serde(default)]
    pub default_model_id: Option<String>,
    /// The model a child of one kind of work runs on, keyed by kind label.
    #[serde(default)]
    pub by_kind: BTreeMap<String, String>,
}

/// Where a delegated child's model came from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildModelSource {
    /// The delegating agent named the model.
    Explicit,
    /// The project maps the child's kind of work to the model.
    Kind,
    /// The project's default child model.
    ProjectDefault,
    /// The delegating agent's own model.
    Parent,
}

/// The model a delegated child runs on, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildModelChoice {
    pub model_id: String,
    pub source: ChildModelSource,
}

impl ProjectChildModelPolicy {
    /// Whether the policy chooses nothing, so every child reuses its parent's
    /// model.
    pub fn is_empty(&self) -> bool {
        self.default_model_id.is_none() && self.by_kind.is_empty()
    }

    /// Trims and validates every field, lower-casing kind labels.
    pub fn normalized(self) -> Result<Self> {
        let default_model_id = self
            .default_model_id
            .map(|model| normalize_child_model_id(&model))
            .transpose()?;
        if self.by_kind.len() > MAX_CHILD_MODEL_KINDS {
            return Err(LoomError::invalid_request(format!(
                "a project maps at most {MAX_CHILD_MODEL_KINDS} kinds of work to a child model"
            )));
        }
        let mut by_kind = BTreeMap::new();
        for (kind, model) in self.by_kind {
            let kind = normalize_child_model_kind(&kind)?;
            let model = normalize_child_model_id(&model)?;
            if by_kind.insert(kind.clone(), model).is_some() {
                return Err(LoomError::invalid_request(format!(
                    "child model kind '{kind}' is given more than once"
                )));
            }
        }
        Ok(Self {
            default_model_id,
            by_kind,
        })
    }

    /// Every model ID the policy names, each once.
    pub fn model_ids(&self) -> Vec<&str> {
        let mut ids = self
            .default_model_id
            .iter()
            .chain(self.by_kind.values())
            .map(String::as_str)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Chooses a delegated child's model.
    ///
    /// The order is: a model the delegating agent names explicitly, the model
    /// the child's kind maps to, the project's default child model, and last
    /// the delegating agent's own model. `current`, like an omitted model,
    /// leaves the choice to the project, so the user's policy is not bypassed
    /// by an agent that habitually passes the alias; an agent that wants its
    /// own model despite the policy names it by ID. An unknown kind falls
    /// through to the default.
    pub fn resolve(
        &self,
        requested_model_id: Option<&str>,
        kind: Option<&str>,
        parent_model_id: &str,
    ) -> ChildModelChoice {
        if let Some(requested) = requested_model_id
            .map(str::trim)
            .filter(|requested| !requested.is_empty())
            .filter(|requested| !requested.eq_ignore_ascii_case(CURRENT_MODEL_ALIAS))
        {
            return ChildModelChoice {
                model_id: requested.to_owned(),
                source: ChildModelSource::Explicit,
            };
        }
        if let Some(model) = kind
            .and_then(|kind| normalize_child_model_kind(kind).ok())
            .and_then(|kind| self.by_kind.get(&kind))
        {
            return ChildModelChoice {
                model_id: model.clone(),
                source: ChildModelSource::Kind,
            };
        }
        match &self.default_model_id {
            Some(model) => ChildModelChoice {
                model_id: model.clone(),
                source: ChildModelSource::ProjectDefault,
            },
            None => ChildModelChoice {
                model_id: parent_model_id.to_owned(),
                source: ChildModelSource::Parent,
            },
        }
    }
}

/// Trims and lower-cases a kind label, which must be 1 to 32 bytes of `a-z`,
/// `0-9`, `_`, or `-` and start with a letter or digit.
pub fn normalize_child_model_kind(raw: &str) -> Result<String> {
    let kind = raw.trim().to_ascii_lowercase();
    let valid = !kind.is_empty()
        && kind.len() <= MAX_CHILD_MODEL_KIND_BYTES
        && kind.starts_with(|first: char| first.is_ascii_alphanumeric())
        && kind
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        Ok(kind)
    } else {
        Err(LoomError::invalid_request(format!(
            "child model kind '{}' must be 1 to {MAX_CHILD_MODEL_KIND_BYTES} characters of \
             a-z, 0-9, '_' or '-', starting with a letter or digit",
            raw.trim()
        )))
    }
}

/// Trims a model ID a policy stores. `current` is refused: a policy entry that
/// reuses the parent's model is expressed by leaving the entry unset.
pub fn normalize_child_model_id(raw: &str) -> Result<String> {
    let model = raw.trim();
    if model.is_empty() || model.len() > MAX_CHILD_MODEL_ID_BYTES {
        return Err(LoomError::invalid_request(format!(
            "child model ID must contain 1 to {MAX_CHILD_MODEL_ID_BYTES} bytes"
        )));
    }
    if model.eq_ignore_ascii_case(CURRENT_MODEL_ALIAS) {
        return Err(LoomError::invalid_request(
            "child model ID 'current' is implicit; clear the entry to reuse the parent's model",
        ));
    }
    Ok(model.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ProjectChildModelPolicy {
        ProjectChildModelPolicy {
            default_model_id: Some("deepseek/deepseek-flash".to_owned()),
            by_kind: BTreeMap::from([
                ("vision".to_owned(), "openai/gpt-6-luna".to_owned()),
                ("cheap".to_owned(), "deepseek/deepseek-flash".to_owned()),
            ]),
        }
    }

    #[test]
    fn an_explicit_model_wins_over_kind_and_default() {
        let choice = policy().resolve(Some(" custom/model "), Some("vision"), "parent/model");
        assert_eq!(choice.model_id, "custom/model");
        assert_eq!(choice.source, ChildModelSource::Explicit);
    }

    #[test]
    fn a_mapped_kind_wins_over_the_default() {
        let choice = policy().resolve(None, Some(" Vision "), "parent/model");
        assert_eq!(choice.model_id, "openai/gpt-6-luna");
        assert_eq!(choice.source, ChildModelSource::Kind);
    }

    #[test]
    fn current_leaves_the_choice_to_the_project() {
        let choice = policy().resolve(Some("CURRENT"), Some("vision"), "parent/model");
        assert_eq!(choice.source, ChildModelSource::Kind);
        let choice = policy().resolve(Some("current"), None, "parent/model");
        assert_eq!(choice.model_id, "deepseek/deepseek-flash");
        assert_eq!(choice.source, ChildModelSource::ProjectDefault);
    }

    #[test]
    fn an_unknown_or_missing_kind_uses_the_default() {
        for kind in [None, Some("unmapped"), Some("not a kind!")] {
            let choice = policy().resolve(None, kind, "parent/model");
            assert_eq!(choice.model_id, "deepseek/deepseek-flash");
            assert_eq!(choice.source, ChildModelSource::ProjectDefault);
        }
    }

    #[test]
    fn without_a_policy_the_parent_model_is_reused() {
        let empty = ProjectChildModelPolicy::default();
        assert!(empty.is_empty());
        let choice = empty.resolve(None, Some("vision"), "parent/model");
        assert_eq!(choice.model_id, "parent/model");
        assert_eq!(choice.source, ChildModelSource::Parent);
        let mut kinds_only = policy();
        kinds_only.default_model_id = None;
        let choice = kinds_only.resolve(Some("current"), Some("other"), "parent/model");
        assert_eq!(choice.model_id, "parent/model");
        assert_eq!(choice.source, ChildModelSource::Parent);
    }

    #[test]
    fn normalization_trims_lowercases_and_validates() {
        let raw = ProjectChildModelPolicy {
            default_model_id: Some("  a/b ".to_owned()),
            by_kind: BTreeMap::from([(" Long-Reasoning ".to_owned(), " c/d ".to_owned())]),
        };
        let normalized = raw.normalized().unwrap();
        assert_eq!(normalized.default_model_id.as_deref(), Some("a/b"));
        assert_eq!(
            normalized.by_kind.get("long-reasoning").map(String::as_str),
            Some("c/d")
        );
        assert_eq!(normalized.model_ids(), vec!["a/b", "c/d"]);

        for bad_kind in ["", "-x", "has space", "ünicode", &"k".repeat(33)] {
            let policy = ProjectChildModelPolicy {
                default_model_id: None,
                by_kind: BTreeMap::from([(bad_kind.to_owned(), "a/b".to_owned())]),
            };
            assert!(policy.normalized().is_err(), "{bad_kind:?} must be refused");
        }
        for bad_model in [
            "",
            "  ",
            "current",
            &"m".repeat(MAX_CHILD_MODEL_ID_BYTES + 1),
        ] {
            let policy = ProjectChildModelPolicy {
                default_model_id: Some(bad_model.to_owned()),
                by_kind: BTreeMap::new(),
            };
            assert!(
                policy.normalized().is_err(),
                "{bad_model:?} must be refused"
            );
        }
        let duplicate = ProjectChildModelPolicy {
            default_model_id: None,
            by_kind: BTreeMap::from([
                ("Vision".to_owned(), "a/b".to_owned()),
                ("vision".to_owned(), "c/d".to_owned()),
            ]),
        };
        assert!(duplicate.normalized().is_err());
        let too_many = ProjectChildModelPolicy {
            default_model_id: None,
            by_kind: (0..=MAX_CHILD_MODEL_KINDS)
                .map(|index| (format!("k{index}"), "a/b".to_owned()))
                .collect(),
        };
        assert!(too_many.normalized().is_err());
    }

    #[test]
    fn a_policy_without_fields_decodes_as_empty() {
        let decoded: ProjectChildModelPolicy = serde_json::from_str("{}").unwrap();
        assert!(decoded.is_empty());
    }
}
