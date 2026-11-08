//! The decision-model contract: a factual state plus a few closed questions.
//!
//! A decision model answers one bounded request with one choice per question and
//! a probability distribution over the options. It is not a chat provider: it
//! has no transcript, no tools, and no streaming, so it gets a sibling trait to
//! [`crate::ModelProvider`] rather than a stream implementation. A chat path
//! cannot reach it and a misuse is a compile error.
//!
//! Everything here is advisory. A decision is an input to a judgement the caller
//! still makes, never an action: the tool built on it is read-only, and nothing
//! in this module approves, executes, or writes.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{CancellationToken, ModelId, ProviderId, TokenUsage};

/// The most questions one request may carry. The design bound is 1-6 closed
/// questions, so batching is expected and looping is not.
pub const MAX_DECISION_QUESTIONS: usize = 6;
/// A question with one option has no decision in it.
pub const MIN_DECISION_OPTIONS: usize = 2;
/// The service schema caps a choice question at 255 labels.
pub const MAX_DECISION_OPTIONS: usize = 255;
/// A probe accepted a 128 KiB state and fully attended it; a larger state hits
/// the model's token ceiling, so the request is refused before it is sent.
pub const MAX_DECISION_STATE_BYTES: usize = 128 * 1024;
pub const MAX_DECISION_QUESTION_ID_BYTES: usize = 64;
pub const MAX_DECISION_INSTRUCTIONS_BYTES: usize = 2 * 1024;
pub const MAX_DECISION_OPTION_LABEL_BYTES: usize = 128;
pub const MAX_DECISION_OPTION_WHEN_BYTES: usize = 512;

/// One closed option: the label the model may choose, and optionally when it
/// applies.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionOption {
    pub label: String,
    /// The criterion the option is chosen by. An empty value tells the model to
    /// interpret the label, which is what the service does with an empty
    /// criterion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

impl DecisionOption {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            when: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionQuestion {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub options: Vec<DecisionOption>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionRequest {
    /// The facts the answer is grounded in. Names and counts, never keys or
    /// credential ids.
    pub state: String,
    pub questions: Vec<DecisionQuestion>,
}

/// One answered question: the chosen label, the model's confidence in it, and
/// the full distribution the choice was taken from.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DecisionAnswer {
    pub choice: String,
    /// As reported by the service. It is read, never derived from the
    /// distribution: the two disagree in practice.
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
}

/// The answer to every question plus the usage the service reported.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DecisionResponse {
    pub answers: BTreeMap<String, DecisionAnswer>,
    /// The model the service reported serving, which may name a version instead
    /// of the requested alias.
    pub model: String,
    #[serde(default)]
    pub usage: TokenUsage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionDescriptor {
    pub provider: ProviderId,
    pub model: ModelId,
}

/// A sibling of [`crate::ModelProvider`] for decision requests.
///
/// It takes no messages and returns one document, so it cannot be mistaken for
/// a chat provider. A caller must call it from a context where a failed call can
/// fall back instead of failing the work that asked.
pub trait DecisionProvider: Send + Sync {
    fn descriptor(&self) -> DecisionDescriptor;

    fn decide(
        &self,
        request: &DecisionRequest,
        cancel: &CancellationToken,
    ) -> loom_core::Result<DecisionResponse>;
}

impl DecisionRequest {
    /// Refuses a malformed request before it is sent.
    ///
    /// The tool validates too, because a model can ignore a JSON schema. The
    /// service refuses a wrong container with a 422, so checking here keeps a
    /// malformed call cheap, bounded, and reported in the caller's own words.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.state.trim().is_empty() {
            return Err("state must not be empty".to_owned());
        }
        if self.state.len() > MAX_DECISION_STATE_BYTES {
            return Err(format!(
                "state must be at most {MAX_DECISION_STATE_BYTES} bytes"
            ));
        }
        if self.questions.is_empty() {
            return Err("questions must not be empty".to_owned());
        }
        if self.questions.len() > MAX_DECISION_QUESTIONS {
            return Err(format!(
                "questions must be at most {MAX_DECISION_QUESTIONS}"
            ));
        }
        let mut ids = BTreeSet::new();
        for question in &self.questions {
            if question.id.trim().is_empty() {
                return Err("every question needs an id".to_owned());
            }
            if question.id.len() > MAX_DECISION_QUESTION_ID_BYTES {
                return Err(format!(
                    "question id '{}' must be at most {MAX_DECISION_QUESTION_ID_BYTES} bytes",
                    question.id
                ));
            }
            if question.id.chars().any(char::is_control) {
                return Err(format!(
                    "question id '{}' must not contain control characters",
                    question.id
                ));
            }
            if !ids.insert(question.id.as_str()) {
                return Err(format!("question id '{}' is repeated", question.id));
            }
            if let Some(instructions) = &question.instructions
                && instructions.len() > MAX_DECISION_INSTRUCTIONS_BYTES
            {
                return Err(format!(
                    "question '{}' instructions must be at most \
                     {MAX_DECISION_INSTRUCTIONS_BYTES} bytes",
                    question.id
                ));
            }
            if question.options.len() < MIN_DECISION_OPTIONS {
                return Err(format!(
                    "question '{}' needs at least {MIN_DECISION_OPTIONS} options",
                    question.id
                ));
            }
            if question.options.len() > MAX_DECISION_OPTIONS {
                return Err(format!(
                    "question '{}' has more than {MAX_DECISION_OPTIONS} options",
                    question.id
                ));
            }
            let mut labels = BTreeSet::new();
            for option in &question.options {
                if option.label.trim().is_empty() {
                    return Err(format!(
                        "question '{}' has an option with an empty label",
                        question.id
                    ));
                }
                if option.label.len() > MAX_DECISION_OPTION_LABEL_BYTES {
                    return Err(format!(
                        "question '{}' has an option label longer than \
                         {MAX_DECISION_OPTION_LABEL_BYTES} bytes",
                        question.id
                    ));
                }
                if option.label.chars().any(char::is_control) {
                    return Err(format!(
                        "question '{}' has an option label with control characters",
                        question.id
                    ));
                }
                if !labels.insert(option.label.as_str()) {
                    return Err(format!(
                        "question '{}' repeats the option '{}'",
                        question.id, option.label
                    ));
                }
                if option
                    .when
                    .as_ref()
                    .is_some_and(|when| when.len() > MAX_DECISION_OPTION_WHEN_BYTES)
                {
                    return Err(format!(
                        "question '{}' has a criterion longer than \
                         {MAX_DECISION_OPTION_WHEN_BYTES} bytes",
                        question.id
                    ));
                }
            }
        }
        Ok(())
    }
}

impl DecisionResponse {
    /// Refuses an answer that does not belong to the request it answered.
    ///
    /// A choice outside the offered labels, a missing answer, or a probability
    /// outside 0-1 is discarded rather than clamped: a silently repaired answer
    /// reads as a fact the caller can act on.
    pub fn validate_against(&self, request: &DecisionRequest) -> std::result::Result<(), String> {
        let expected = request
            .questions
            .iter()
            .map(|question| question.id.as_str())
            .collect::<BTreeSet<_>>();
        let received = self
            .answers
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if expected != received {
            return Err("answers do not match the questions that were asked".to_owned());
        }
        for question in &request.questions {
            let answer = self
                .answers
                .get(&question.id)
                .ok_or_else(|| format!("question '{}' has no answer", question.id))?;
            let labels = question
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<BTreeSet<_>>();
            if !labels.contains(answer.choice.as_str()) {
                return Err(format!(
                    "question '{}' answered with an option that was not offered",
                    question.id
                ));
            }
            if !answer.confidence.is_finite() || !(0.0..=1.0).contains(&answer.confidence) {
                return Err(format!(
                    "question '{}' reported a confidence outside 0-1",
                    question.id
                ));
            }
            if !answer.probabilities.contains_key(&answer.choice) {
                return Err(format!(
                    "question '{}' omitted the chosen option from its probabilities",
                    question.id
                ));
            }
            for (label, probability) in &answer.probabilities {
                if !labels.contains(label.as_str()) {
                    return Err(format!(
                        "question '{}' reported a probability for an option that was not offered",
                        question.id
                    ));
                }
                if !probability.is_finite() || !(0.0..=1.0).contains(probability) {
                    return Err(format!(
                        "question '{}' reported a probability outside 0-1",
                        question.id
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DecisionAnswer, DecisionOption, DecisionQuestion, DecisionRequest, DecisionResponse,
        MAX_DECISION_OPTIONS, MAX_DECISION_QUESTIONS,
    };

    fn question(id: &str) -> DecisionQuestion {
        DecisionQuestion {
            id: id.to_owned(),
            instructions: Some("Pick the option that matches the state.".to_owned()),
            options: vec![
                DecisionOption {
                    label: "red".to_owned(),
                    when: Some("the state names red".to_owned()),
                },
                DecisionOption::new("blue"),
            ],
        }
    }

    fn sample_request() -> DecisionRequest {
        DecisionRequest {
            state: "the user names blue".to_owned(),
            questions: vec![question("colour")],
        }
    }

    fn sample_answer() -> DecisionResponse {
        DecisionResponse {
            answers: [(
                "colour".to_owned(),
                DecisionAnswer {
                    choice: "blue".to_owned(),
                    confidence: 0.91,
                    probabilities: [("blue".to_owned(), 0.91), ("red".to_owned(), 0.09)].into(),
                },
            )]
            .into(),
            model: "jev-1.13".to_owned(),
            usage: Default::default(),
        }
    }

    #[test]
    fn a_bounded_request_is_accepted() {
        assert_eq!(sample_request().validate(), Ok(()));
    }

    #[test]
    fn the_state_must_be_present_and_bounded() {
        let mut req = sample_request();
        req.state = "  ".to_owned();
        assert!(req.validate().unwrap_err().contains("state"));

        let mut req = sample_request();
        req.state = "x".repeat(super::MAX_DECISION_STATE_BYTES + 1);
        assert!(req.validate().unwrap_err().contains("at most"));
    }

    #[test]
    fn questions_are_bounded_and_distinct() {
        let mut req = sample_request();
        req.questions.clear();
        assert!(req.validate().unwrap_err().contains("must not be empty"));

        let mut req = sample_request();
        req.questions = (0..=MAX_DECISION_QUESTIONS)
            .map(|index| question(&format!("q{index}")))
            .collect();
        assert!(req.validate().unwrap_err().contains("at most"));

        let mut req = sample_request();
        req.questions = vec![question("same"), question("same")];
        assert!(req.validate().unwrap_err().contains("repeated"));

        let mut req = sample_request();
        req.questions[0].id = "  ".to_owned();
        assert!(req.validate().unwrap_err().contains("needs an id"));
    }

    #[test]
    fn every_question_needs_two_to_two_hundred_and_five_labels() {
        let mut req = sample_request();
        req.questions[0].options.truncate(1);
        assert!(req.validate().unwrap_err().contains("at least 2"));

        let mut req = sample_request();
        req.questions[0].options = (0..=MAX_DECISION_OPTIONS)
            .map(|index| DecisionOption::new(format!("option {index}")))
            .collect();
        assert!(req.validate().unwrap_err().contains("more than"));
    }

    #[test]
    fn options_cannot_be_empty_or_repeated() {
        let mut req = sample_request();
        req.questions[0].options[1].label = "  ".to_owned();
        assert!(req.validate().unwrap_err().contains("empty label"));

        let mut req = sample_request();
        req.questions[0].options[1].label = "red".to_owned();
        assert!(req.validate().unwrap_err().contains("repeats"));
    }

    #[test]
    fn a_valid_answer_matches_the_request() {
        assert_eq!(sample_answer().validate_against(&sample_request()), Ok(()));
    }

    #[test]
    fn an_invented_choice_is_discarded() {
        let mut resp = sample_answer();
        resp.answers.get_mut("colour").unwrap().choice = "green".to_owned();
        assert!(
            resp.validate_against(&sample_request())
                .unwrap_err()
                .contains("not offered")
        );
    }

    #[test]
    fn a_missing_or_extra_answer_is_discarded() {
        let mut resp = sample_answer();
        resp.answers.clear();
        assert!(
            resp.validate_against(&sample_request())
                .unwrap_err()
                .contains("do not match")
        );

        let mut resp = sample_answer();
        resp.answers.insert(
            "extra".to_owned(),
            DecisionAnswer {
                choice: "red".to_owned(),
                confidence: 1.0,
                probabilities: Default::default(),
            },
        );
        assert!(
            resp.validate_against(&sample_request())
                .unwrap_err()
                .contains("do not match")
        );
    }

    #[test]
    fn confidence_and_probabilities_must_be_probabilities() {
        let mut resp = sample_answer();
        resp.answers.get_mut("colour").unwrap().confidence = 1.5;
        assert!(
            resp.validate_against(&sample_request())
                .unwrap_err()
                .contains("confidence")
        );

        let mut resp = sample_answer();
        resp.answers
            .get_mut("colour")
            .unwrap()
            .probabilities
            .insert("blue".to_owned(), f64::NAN);
        assert!(
            resp.validate_against(&sample_request())
                .unwrap_err()
                .contains("probability")
        );

        let mut resp = sample_answer();
        resp.answers
            .get_mut("colour")
            .unwrap()
            .probabilities
            .remove("blue");
        assert!(
            resp.validate_against(&sample_request())
                .unwrap_err()
                .contains("chosen option")
        );
    }
}
