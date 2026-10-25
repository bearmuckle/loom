//! The GitHub issue or pull request a session works on.
//!
//! The reference is derived on the client from the GitHub tool calls the
//! session makes, both from live events and from the durable run projection.
//! There is no backend field for it, so no protocol request carries it and no
//! network call is made to find it out. A title is deliberately not looked up:
//! a title only exists in the tool payload, and a truncated payload must
//! produce no reference rather than a wrong one.
//!
//! The URL is built from the repository and the number, never taken from a
//! tool payload, so no token or query string can reach the header chip.

use std::collections::BTreeMap;

use loom_core::ToolCallId;
use loom_model::{MessageRole, ModelMessage, ToolCall};
use serde_json::Value;

/// Whether a session reference points at an issue or a pull request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionReferenceKind {
    Issue,
    PullRequest,
}

impl SessionReferenceKind {
    /// The kind as the header tooltip and the settings rows name it.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pull request",
        }
    }

    /// The URL path segment GitHub addresses this kind under.
    const fn path(self) -> &'static str {
        match self {
            Self::Issue => "issues",
            Self::PullRequest => "pull",
        }
    }

    /// Whether the header may show this kind under the two client-local
    /// preferences. The filter is applied when rendering, not when deriving,
    /// so flipping a preference takes effect without rebuilding the reference.
    pub(crate) const fn enabled(
        self,
        show_issue_references: bool,
        show_pull_request_references: bool,
    ) -> bool {
        match self {
            Self::Issue => show_issue_references,
            Self::PullRequest => show_pull_request_references,
        }
    }
}

/// The one issue or pull request a session's header links to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionReference {
    pub(crate) kind: SessionReferenceKind,
    /// `owner/name`, already checked against [`valid_repository`].
    pub(crate) repository: String,
    /// A positive GitHub number.
    pub(crate) number: u64,
}

impl SessionReference {
    /// The header label: `owner/name#123`.
    pub(crate) fn label(&self) -> String {
        format!("{}#{}", self.repository, self.number)
    }

    /// The link target, built from the parts rather than read from a payload.
    pub(crate) fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/{}",
            self.repository,
            self.kind.path(),
            self.number
        )
    }
}

/// Whether a call reads a GitHub object or writes one.
///
/// A create is a write too, but its number only arrives with the result, so it
/// is its own case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReferenceAccess {
    Read,
    Write,
    Create,
}

/// What one GitHub tool call contributes to the session's reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReferenceCandidate {
    /// The call named the whole reference in its arguments.
    Resolved {
        access: ReferenceAccess,
        reference: SessionReference,
    },
    /// A create call, whose number arrives with the result.
    Pending {
        kind: SessionReferenceKind,
        repository: String,
    },
}

impl ReferenceCandidate {
    /// The candidate a call contributes when it is first seen.
    ///
    /// Only a call that names one object contributes. The comment, label,
    /// list, search, and refs tools are left out: the first three are ambiguous
    /// between an issue and a pull request, and the others identify no single
    /// object.
    pub(crate) fn from_call(call: &ToolCall) -> Option<Self> {
        let (kind, access) = call_kind(&call.name)?;
        let repository = repository_from_arguments(&call.arguments)?;
        if access == ReferenceAccess::Create {
            return Some(Self::Pending { kind, repository });
        }
        let number = number_from_arguments(&call.arguments)?;
        Some(Self::Resolved {
            access,
            reference: SessionReference {
                kind,
                repository,
                number,
            },
        })
    }

    /// Whether the call wrote to GitHub, so it outranks a read.
    pub(crate) const fn is_write(&self) -> bool {
        !matches!(
            self,
            Self::Resolved {
                access: ReferenceAccess::Read,
                ..
            }
        )
    }

    /// The reference the call resolved to, given the tool result payload.
    ///
    /// A resolved call ignores `output`; a create reads its number from it. A
    /// truncated or unparseable payload yields `None`, which is intended: the
    /// call then contributes nothing.
    pub(crate) fn resolved(&self, output: &str) -> Option<SessionReference> {
        match self {
            Self::Resolved { reference, .. } => Some(reference.clone()),
            Self::Pending { kind, repository } => {
                number_from_output(output).map(|number| SessionReference {
                    kind: *kind,
                    repository: repository.clone(),
                    number,
                })
            }
        }
    }
}

/// The two reference slots a session derives from its GitHub calls.
///
/// A write outranks a read because it is what the session produced: a manager
/// session that read twenty issues still shows the pull request it opened.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SessionReferences {
    pub(crate) write: Option<SessionReference>,
    pub(crate) read: Option<SessionReference>,
}

impl SessionReferences {
    /// Records a completed candidate in the slot its access belongs to.
    pub(crate) fn record(&mut self, candidate: &ReferenceCandidate, reference: SessionReference) {
        if candidate.is_write() {
            self.write = Some(reference);
        } else {
            self.read = Some(reference);
        }
    }
}

/// The reference two slots resolve to: the write, else the read.
pub(crate) fn preferred_reference<'a>(
    write: Option<&'a SessionReference>,
    read: Option<&'a SessionReference>,
) -> Option<&'a SessionReference> {
    write.or(read)
}

/// Derives both slots from a durable transcript page.
///
/// Messages are in order, so a tool call is captured from the assistant message
/// that carries it and promoted by the `role: Tool` message with the matching
/// `tool_call_id` that follows. A call the page does not complete contributes
/// nothing.
pub(crate) fn references_from_messages(messages: &[ModelMessage]) -> SessionReferences {
    let mut pending: BTreeMap<ToolCallId, ReferenceCandidate> = BTreeMap::new();
    let mut references = SessionReferences::default();
    for message in messages {
        if message.role == MessageRole::Assistant {
            for call in &message.tool_calls {
                if let Some(candidate) = ReferenceCandidate::from_call(call) {
                    pending.insert(call.id, candidate);
                }
            }
        }
        if message.role == MessageRole::Tool
            && let Some(id) = message.tool_call_id
            && let Some(candidate) = pending.remove(&id)
            && let Some(reference) = candidate.resolved(&message.content)
        {
            references.record(&candidate, reference);
        }
    }
    references
}

/// The kind and access a GitHub tool name reports, if it names one object.
fn call_kind(name: &str) -> Option<(SessionReferenceKind, ReferenceAccess)> {
    use ReferenceAccess::{Create, Read, Write};
    use SessionReferenceKind::{Issue, PullRequest};
    match name {
        "github_get_issue" => Some((Issue, Read)),
        "github_update_issue" => Some((Issue, Write)),
        "github_get_pull_request" => Some((PullRequest, Read)),
        "github_update_pull_request" => Some((PullRequest, Write)),
        "github_create_issue" => Some((Issue, Create)),
        "github_create_pull_request" => Some((PullRequest, Create)),
        _ => None,
    }
}

/// The `repository` argument, when it is a repository GitHub can address.
fn repository_from_arguments(arguments: &Value) -> Option<String> {
    let repository = arguments.get("repository")?.as_str()?;
    valid_repository(repository).then(|| repository.to_owned())
}

/// Whether a string is exactly `owner/name`.
///
/// Both segments must be non-empty and may only hold the characters GitHub
/// allows. Refusing anything else keeps a malformed or hostile value from
/// changing the URL's shape.
fn valid_repository(repository: &str) -> bool {
    let mut segments = repository.split('/');
    let (Some(owner), Some(name), None) = (segments.next(), segments.next(), segments.next())
    else {
        return false;
    };
    [owner, name].into_iter().all(|segment| {
        !segment.is_empty()
            && segment.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
    })
}

/// The `number` argument as a positive integer.
fn number_from_arguments(arguments: &Value) -> Option<u64> {
    positive_integer(arguments.get("number")?)
}

/// A JSON value that is a positive integer, written as a number or a string.
fn positive_integer(value: &Value) -> Option<u64> {
    let number = match value {
        Value::Number(number) => number.as_u64()?,
        Value::String(text) => text.trim().parse::<u64>().ok()?,
        _ => return None,
    };
    (number > 0).then_some(number)
}

/// The number a create result reports.
///
/// The whole payload is tried first; a tool that appends a line after the JSON
/// body (`github_create_pull_request` adds a base-freshness line) is retried on
/// the text before the first newline. A payload truncated mid-way parses as
/// neither and yields nothing.
fn number_from_output(output: &str) -> Option<u64> {
    let value = serde_json::from_str::<Value>(output).ok().or_else(|| {
        let first_line = output.split('\n').next().unwrap_or_default();
        serde_json::from_str::<Value>(first_line).ok()
    })?;
    value.get("number").and_then(positive_integer).or_else(|| {
        value
            .get("html_url")
            .and_then(Value::as_str)
            .and_then(number_from_url)
    })
}

/// The trailing segment of a GitHub object URL, as a positive integer.
fn number_from_url(url: &str) -> Option<u64> {
    let number = url.trim_end_matches('/').rsplit('/').next()?;
    number.parse::<u64>().ok().filter(|number| *number > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new(),
            name: name.to_owned(),
            arguments,
        }
    }

    fn issue_call(name: &str, repository: &str, number: Value) -> ToolCall {
        call(
            name,
            serde_json::json!({"repository": repository, "number": number}),
        )
    }

    fn resolved(candidate: Option<ReferenceCandidate>) -> SessionReference {
        let candidate = candidate.expect("the call names one object");
        candidate
            .resolved("")
            .expect("a resolved candidate needs no result payload")
    }

    fn preferred(references: &SessionReferences) -> Option<&SessionReference> {
        preferred_reference(references.write.as_ref(), references.read.as_ref())
    }

    #[test]
    fn the_url_and_label_come_from_the_parts() {
        let reference = SessionReference {
            kind: SessionReferenceKind::PullRequest,
            repository: "bearmuckle/loom".to_owned(),
            number: 340,
        };
        assert_eq!(reference.label(), "bearmuckle/loom#340");
        assert_eq!(
            reference.url(),
            "https://github.com/bearmuckle/loom/pull/340"
        );

        let issue = SessionReference {
            kind: SessionReferenceKind::Issue,
            ..reference
        };
        assert_eq!(issue.url(), "https://github.com/bearmuckle/loom/issues/340");
    }

    #[test]
    fn get_calls_read_and_update_calls_write() {
        for (name, kind) in [
            ("github_get_issue", SessionReferenceKind::Issue),
            ("github_get_pull_request", SessionReferenceKind::PullRequest),
        ] {
            let candidate =
                ReferenceCandidate::from_call(&issue_call(name, "owner/name", 12.into()))
                    .expect("a get call names one object");
            assert!(!candidate.is_write(), "{name} is a read");
            let reference = resolved(Some(candidate));
            assert_eq!(reference.kind, kind);
            assert_eq!(reference.number, 12);
        }

        for (name, kind) in [
            ("github_update_issue", SessionReferenceKind::Issue),
            (
                "github_update_pull_request",
                SessionReferenceKind::PullRequest,
            ),
        ] {
            let candidate =
                ReferenceCandidate::from_call(&issue_call(name, "owner/name", 12.into()))
                    .expect("an update call names one object");
            assert!(candidate.is_write(), "{name} is a write");
            let reference = resolved(Some(candidate));
            assert_eq!(reference.kind, kind);
        }
    }

    #[test]
    fn a_number_written_as_a_string_is_still_a_number() {
        let reference = resolved(ReferenceCandidate::from_call(&issue_call(
            "github_get_issue",
            "owner/name",
            "34".into(),
        )));
        assert_eq!(reference.number, 34);
    }

    #[test]
    fn ambiguous_and_anonymous_tools_contribute_nothing() {
        for name in [
            "github_comment",
            "github_add_labels",
            "github_remove_labels",
            "github_list_issues",
            "github_search_issues",
            "github_get_ref",
            "read_file",
        ] {
            assert_eq!(
                ReferenceCandidate::from_call(&issue_call(name, "owner/name", 1.into())),
                None,
                "{name} names no single issue or pull request"
            );
        }
    }

    #[test]
    fn malformed_arguments_yield_nothing() {
        for arguments in [
            Value::Null,
            serde_json::json!({}),
            serde_json::json!({"repository": "owner/name"}),
            serde_json::json!({"number": 4}),
            serde_json::json!({"repository": 7, "number": 4}),
            serde_json::json!({"repository": "owner/name", "number": null}),
            serde_json::json!({"repository": "owner/name", "number": -4}),
            serde_json::json!({"repository": "owner/name", "number": "four"}),
            serde_json::json!({"repository": "owner/name", "number": 4.5}),
        ] {
            assert_eq!(
                ReferenceCandidate::from_call(&call("github_get_issue", arguments.clone())),
                None,
                "arguments {arguments} identify no reference"
            );
        }
    }

    #[test]
    fn a_repository_must_be_exactly_owner_and_name() {
        for repository in [
            "",
            "owner",
            "owner/",
            "/name",
            "owner/name/extra",
            "owner/na me",
            "owner/name?token=secret",
            "owner/name#fragment",
        ] {
            assert_eq!(
                ReferenceCandidate::from_call(&issue_call(
                    "github_get_issue",
                    repository,
                    4.into()
                )),
                None,
                "{repository:?} is not a repository"
            );
        }
        // The characters GitHub allows in each segment do not reject a real one.
        assert!(
            ReferenceCandidate::from_call(&issue_call(
                "github_get_issue",
                "bjorn-harrtell/loom.rs_2",
                4.into()
            ))
            .is_some()
        );
    }

    #[test]
    fn a_zero_number_yields_nothing() {
        assert_eq!(
            ReferenceCandidate::from_call(&issue_call("github_get_issue", "owner/name", 0.into())),
            None
        );
        assert_eq!(
            ReferenceCandidate::from_call(&issue_call(
                "github_get_pull_request",
                "owner/name",
                "0".into()
            )),
            None
        );
    }

    #[test]
    fn a_create_call_waits_for_its_result() {
        let candidate = ReferenceCandidate::from_call(&call(
            "github_create_pull_request",
            serde_json::json!({"repository": "owner/name", "title": "Ship it"}),
        ))
        .expect("a create call names the repository");
        assert!(candidate.is_write());
        assert_eq!(
            candidate.resolved(r#"{"number": 41}"#).map(|r| r.number),
            Some(41)
        );
        assert_eq!(
            candidate.resolved(r#"{"html_url": "https://github.com/owner/name/pull/42"}"#),
            Some(SessionReference {
                kind: SessionReferenceKind::PullRequest,
                repository: "owner/name".to_owned(),
                number: 42,
            })
        );
    }

    #[test]
    fn a_create_result_may_carry_a_trailing_line() {
        let candidate = ReferenceCandidate::from_call(&call(
            "github_create_pull_request",
            serde_json::json!({"repository": "owner/name"}),
        ))
        .expect("a create call names the repository");
        let output = "{\"number\": 43}\nBase freshness: origin/main is 3 commits ahead";
        assert_eq!(candidate.resolved(output).map(|r| r.number), Some(43));
    }

    #[test]
    fn a_truncated_create_result_yields_nothing() {
        let candidate = ReferenceCandidate::from_call(&call(
            "github_create_issue",
            serde_json::json!({"repository": "owner/name"}),
        ))
        .expect("a create call names the repository");
        for output in [
            "{\"number\": 4",
            "{\"html_url\": \"https://github.com/owner/name/issues/4",
            "no json here",
            "",
            "{\"number\": 0}",
            "{\"html_url\": \"https://github.com/owner/name/issues/last\"}",
        ] {
            assert_eq!(candidate.resolved(output), None, "{output:?}");
        }
    }

    #[test]
    fn writing_outranks_reading() {
        let read = call(
            "github_get_issue",
            serde_json::json!({"repository": "owner/name", "number": 7}),
        );
        let write = call(
            "github_update_pull_request",
            serde_json::json!({"repository": "owner/name", "number": 9}),
        );
        let mut references = SessionReferences::default();
        let candidate = ReferenceCandidate::from_call(&read).unwrap();
        references.record(&candidate, candidate.resolved("").unwrap());
        let candidate = ReferenceCandidate::from_call(&write).unwrap();
        references.record(&candidate, candidate.resolved("").unwrap());
        assert_eq!(
            preferred(&references).map(SessionReference::label),
            Some("owner/name#9".to_owned())
        );

        // A later read does not displace the write.
        let candidate = ReferenceCandidate::from_call(&read).unwrap();
        references.record(&candidate, candidate.resolved("").unwrap());
        assert_eq!(
            preferred(&references).map(SessionReference::label),
            Some("owner/name#9".to_owned())
        );
        assert_eq!(references.read.as_ref().map(|r| r.number), Some(7));
    }

    #[test]
    fn the_kind_filter_runs_at_render_time() {
        assert!(SessionReferenceKind::Issue.enabled(true, false));
        assert!(SessionReferenceKind::PullRequest.enabled(false, true));
        assert!(!SessionReferenceKind::Issue.enabled(false, true));
        assert!(!SessionReferenceKind::PullRequest.enabled(true, false));
        assert!(!SessionReferenceKind::Issue.enabled(false, false));
    }

    #[test]
    fn a_durable_page_derives_the_call_and_its_result() {
        let get = issue_call("github_get_issue", "owner/name", 5.into());
        let create = call(
            "github_create_pull_request",
            serde_json::json!({"repository": "owner/name"}),
        );
        let mut assistant = ModelMessage::new(MessageRole::Assistant, "working");
        assistant.tool_calls = vec![get.clone(), create.clone()];
        let mut completed_get = ModelMessage::new(MessageRole::Tool, "{\"number\": 5}");
        completed_get.tool_call_id = Some(get.id);
        let mut completed_create = ModelMessage::new(MessageRole::Tool, "{\"number\": 6}");
        completed_create.tool_call_id = Some(create.id);

        let references = references_from_messages(&[
            ModelMessage::new(MessageRole::User, "task"),
            assistant,
            completed_get,
            completed_create,
        ]);
        assert_eq!(references.read.as_ref().map(|r| r.number), Some(5));
        assert_eq!(
            preferred(&references).map(SessionReference::label),
            Some("owner/name#6".to_owned())
        );
    }

    #[test]
    fn a_call_the_page_never_completes_contributes_nothing() {
        let get = issue_call("github_get_issue", "owner/name", 5.into());
        let mut assistant = ModelMessage::new(MessageRole::Assistant, "working");
        assistant.tool_calls = vec![get];

        assert_eq!(
            references_from_messages(&[assistant]),
            SessionReferences::default()
        );
    }

    #[test]
    fn a_result_without_its_call_contributes_nothing() {
        let mut result = ModelMessage::new(MessageRole::Tool, "{\"number\": 5}");
        result.tool_call_id = Some(ToolCallId::new());
        assert_eq!(
            references_from_messages(&[result]),
            SessionReferences::default()
        );
    }
}
