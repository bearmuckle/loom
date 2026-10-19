//! Automatic project titles.
//!
//! A project title is the name of a project's root agent session, which is what
//! the sidebar renders as the project row. The policy for choosing one lives
//! here so it is unit-testable without a provider: the sanitising rules, the
//! deterministic fallback derived from a run's goal, and the divergence test
//! that decides whether a new candidate is worth re-titling the project.
//!
//! The backend applies this policy (`InProcessBackend::maybe_auto_title`);
//! nothing here reaches a provider.

use serde::{Deserialize, Serialize};

/// Who chose a session's current name.
///
/// A project is titled automatically only while its title source is [`Auto`].
/// A manual rename pins the title, so the backend stops re-choosing it.
///
/// [`Auto`]: SessionTitleSource::Auto
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTitleSource {
    /// Chosen by the backend from the work being planned or the run's goal.
    #[default]
    Auto,
    /// Chosen by a person, which stops automatic re-titling.
    Manual,
}

/// Longest title the policy keeps, in characters.
pub const MAX_SESSION_TITLE_CHARS: usize = 48;

/// Most words a title keeps.
pub const MAX_SESSION_TITLE_WORDS: usize = 6;

/// Leading request phrasing that names the asker rather than the work.
const FILLER_PREFIXES: [&str; 11] = [
    "please ",
    "can you ",
    "could you ",
    "would you ",
    "i want to ",
    "i'd like to ",
    "i would like to ",
    "i need to ",
    "we need to ",
    "let's ",
    "help me ",
];

/// Trims a candidate into a usable title, or rejects it.
///
/// A candidate that contains a path or a URL is not a title: a project row must
/// not read as a file path or a link.
pub fn sanitize_title(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // Quotes, backticks, and markdown emphasis around the whole candidate are
    // decoration, not part of the name.
    let candidate = trimmed.trim_matches(['"', '\'', '`', '*', '_']).trim();
    if candidate.is_empty()
        || candidate.contains('/')
        || candidate.contains('\\')
        || candidate.contains("://")
        || candidate.contains('\n')
    {
        return None;
    }
    let collapsed = candidate.split_whitespace().collect::<Vec<_>>().join(" ");
    let collapsed = normalize_tail(&collapsed);
    if collapsed.is_empty() {
        return None;
    }
    let mut title = collapsed
        .split(' ')
        .take(MAX_SESSION_TITLE_WORDS)
        .collect::<Vec<_>>()
        .join(" ");
    if title.chars().count() > MAX_SESSION_TITLE_CHARS {
        let mut bounded = String::new();
        for word in title.split(' ') {
            let candidate = if bounded.is_empty() {
                word.to_owned()
            } else {
                format!("{bounded} {word}")
            };
            if candidate.chars().count() > MAX_SESSION_TITLE_CHARS {
                break;
            }
            bounded = candidate;
        }
        title = normalize_tail(&bounded);
    }
    if title.is_empty() {
        return None;
    }
    let title = uppercase_first_alphabetic(&title);
    (title.chars().count() >= 2).then_some(title)
}

/// Drops a trailing sentence separator, so a title does not read as a fragment
/// of a sentence.
fn normalize_tail(text: &str) -> String {
    text.trim_end_matches(['.', ':', ';', ','])
        .trim_end()
        .to_owned()
}

/// Upper-cases the first alphabetic character, leaving the rest as written.
fn uppercase_first_alphabetic(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut uppercased = false;
    for character in text.chars() {
        if !uppercased && character.is_alphabetic() {
            result.extend(character.to_uppercase());
            uppercased = true;
        } else {
            result.push(character);
        }
    }
    result
}

/// A deterministic title derived from a run's goal.
///
/// Only the first sentence is considered, and a leading filler phrase such as
/// "please" or "can you" is dropped, so the project row names the work instead
/// of the request.
pub fn title_from_goal(goal: &str) -> Option<String> {
    let first = goal.split(['.', '!', '?', '\n']).next().unwrap_or_default();
    let without_code = strip_backticked_spans(first);
    let without_urls = without_code
        .split_whitespace()
        .filter(|word| !word.contains("://"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut candidate = without_urls.trim().to_owned();
    let lowered = candidate.to_lowercase();
    if let Some(prefix) = FILLER_PREFIXES
        .iter()
        .copied()
        .find(|prefix| lowered.starts_with(prefix))
    {
        candidate = candidate[prefix.len()..].trim().to_owned();
    }
    sanitize_title(&candidate)
}

/// Removes backticked spans from `text`, keeping the text around them.
fn strip_backticked_spans(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_span = false;
    for character in text.chars() {
        match character {
            '`' => in_span = !in_span,
            _ if !in_span => result.push(character),
            _ => {}
        }
    }
    result
}

/// Whether a name is a placeholder a project should never keep.
pub fn is_placeholder_title(name: &str) -> bool {
    matches!(
        name.trim().to_lowercase().as_str(),
        "new session" | "new project" | "untitled"
    )
}

/// Whether `candidate` differs enough from `current` to replace it.
///
/// The words are compared as lowercase alphanumeric tokens, so a candidate that
/// mostly repeats the current title is not worth a rename while one that names
/// different work is. A placeholder current title always loses.
pub fn titles_diverge(current: &str, candidate: &str) -> bool {
    let candidate_words = title_words(candidate);
    if candidate_words.is_empty() {
        return false;
    }
    if is_placeholder_title(current) {
        return true;
    }
    let current_words = title_words(current);
    // Equal word lists are equal normalized strings, so an unchanged title never
    // renames.
    if current_words == candidate_words {
        return false;
    }
    let shared = candidate_words
        .iter()
        .filter(|word| current_words.contains(word))
        .count();
    // `shared / candidate_words.len() < 0.5`, compared as integers so the
    // boundary is exact.
    shared * 2 < candidate_words.len()
}

/// The lowercase alphanumeric words of a title.
fn title_words(title: &str) -> Vec<String> {
    title
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_trims_decoration_and_collapses_whitespace() {
        assert_eq!(
            sanitize_title("  **implement   automatic titles**  ").as_deref(),
            Some("Implement automatic titles")
        );
        assert_eq!(
            sanitize_title("`automatic titles`").as_deref(),
            Some("Automatic titles")
        );
        assert_eq!(
            sanitize_title("\"quoted title\"").as_deref(),
            Some("Quoted title")
        );
        assert_eq!(sanitize_title("   "), None);
        assert_eq!(sanitize_title("` `"), None);
    }

    #[test]
    fn sanitize_drops_a_trailing_separator() {
        for raw in [
            "Implement titles.",
            "Implement titles:",
            "Implement titles;",
            "Implement titles,",
        ] {
            assert_eq!(
                sanitize_title(raw).as_deref(),
                Some("Implement titles"),
                "{raw}"
            );
        }
    }

    #[test]
    fn sanitize_rejects_paths_urls_and_newlines() {
        assert_eq!(sanitize_title("crates/loom-core"), None);
        assert_eq!(sanitize_title("crates\\loom-core"), None);
        assert_eq!(sanitize_title("https://example.test/x"), None);
        assert_eq!(sanitize_title("two\nlines"), None);
    }

    #[test]
    fn sanitize_caps_words_and_characters_on_a_word_boundary() {
        assert_eq!(
            sanitize_title("one two three four five six seven eight").as_deref(),
            Some("One two three four five six")
        );
        let long = sanitize_title(
            "exceptionallylongword anotherverylongword thirdverylongword fourthverylongword",
        )
        .expect("a long candidate still yields a title");
        assert!(long.chars().count() <= MAX_SESSION_TITLE_CHARS);
        assert!(!long.ends_with(' '));
        assert!(long.starts_with("Exceptionallylongword"));
        // A single word longer than the cap cannot be kept on a boundary.
        assert_eq!(sanitize_title(&"x".repeat(60)), None);
    }

    #[test]
    fn sanitize_uppercases_the_first_alphabetic_character() {
        assert_eq!(
            sanitize_title("implement titles").as_deref(),
            Some("Implement titles")
        );
        assert_eq!(sanitize_title("12 turtles").as_deref(), Some("12 Turtles"));
        assert_eq!(
            sanitize_title("3d viewer for projects").as_deref(),
            Some("3D viewer for projects")
        );
        assert_eq!(
            sanitize_title("all lower case stays lower").as_deref(),
            Some("All lower case stays lower")
        );
    }

    #[test]
    fn sanitize_rejects_titles_shorter_than_two_characters() {
        assert_eq!(sanitize_title("a"), None);
        assert_eq!(sanitize_title("ab").as_deref(), Some("Ab"));
    }

    #[test]
    fn goal_titles_use_the_first_sentence_only() {
        assert_eq!(
            title_from_goal("Implement automatic titles. Then update the sidebar.").as_deref(),
            Some("Implement automatic titles")
        );
        assert_eq!(
            title_from_goal("Implement automatic titles!\nMore detail").as_deref(),
            Some("Implement automatic titles")
        );
        assert_eq!(
            title_from_goal("Does it work? Yes").as_deref(),
            Some("Does it work")
        );
    }

    #[test]
    fn goal_titles_drop_code_spans_and_urls() {
        assert_eq!(
            title_from_goal("Fix `send_project_agent_message` delivery").as_deref(),
            Some("Fix delivery")
        );
        assert_eq!(
            title_from_goal("Review https://example for details").as_deref(),
            Some("Review for details")
        );
    }

    #[test]
    fn goal_titles_strip_a_leading_filler_phrase() {
        for (goal, expected) in [
            ("Please implement titles", "Implement titles"),
            ("Can you implement titles", "Implement titles"),
            ("Could you implement titles", "Implement titles"),
            ("Would you implement titles", "Implement titles"),
            ("I want to implement titles", "Implement titles"),
            ("I'd like to implement titles", "Implement titles"),
            ("I would like to implement titles", "Implement titles"),
            ("I need to implement titles", "Implement titles"),
            ("We need to implement titles", "Implement titles"),
            ("Let's implement titles", "Implement titles"),
            ("Help me implement titles", "Implement titles"),
            ("PLEASE implement titles", "Implement titles"),
        ] {
            assert_eq!(title_from_goal(goal).as_deref(), Some(expected), "{goal}");
        }
    }

    #[test]
    fn goal_titles_reject_goals_without_a_usable_first_sentence() {
        assert_eq!(title_from_goal(""), None);
        assert_eq!(title_from_goal("   "), None);
        assert_eq!(title_from_goal("Fix crates/loom-core/src/title.rs"), None);
    }

    #[test]
    fn placeholder_titles_are_recognised() {
        assert!(is_placeholder_title("new session"));
        assert!(is_placeholder_title("  New Project "));
        assert!(is_placeholder_title("UNTITLED"));
        assert!(!is_placeholder_title("Implement automatic titles"));
        assert!(!is_placeholder_title("Untitled work"));
    }

    #[test]
    fn divergence_accepts_a_candidate_that_names_different_work() {
        assert!(titles_diverge("New session", "Implement automatic titles"));
        assert!(titles_diverge(
            "Implement automatic titles",
            "Fix the sidebar"
        ));
    }

    #[test]
    fn divergence_rejects_an_identical_case_or_empty_candidate() {
        assert!(!titles_diverge(
            "Implement automatic titles",
            "implement  AUTOMATIC titles"
        ));
        assert!(!titles_diverge("Implement automatic titles", ""));
        assert!(!titles_diverge("Implement automatic titles", "   "));
        assert!(!titles_diverge("Implement automatic titles", "!!!"));
    }

    #[test]
    fn divergence_boundary_is_below_half_of_the_candidate_words() {
        // Two of five candidate words are shared, which is below half.
        assert!(titles_diverge(
            "Implement automatic titles for project rows",
            "Implement automatic sidebar rendering now"
        ));
        // Three of five is above half, so the candidate does not win.
        assert!(!titles_diverge(
            "Implement automatic titles for project rows",
            "Implement automatic titles now done"
        ));
        // Four of five.
        assert!(!titles_diverge(
            "Implement automatic title policy",
            "Implement automatic title policy quickly"
        ));
    }

    #[test]
    fn a_placeholder_current_title_always_diverges() {
        assert!(titles_diverge("New project", "Fix the sidebar"));
        assert!(titles_diverge("untitled", "anything at all"));
        assert!(!titles_diverge("New project", ""));
    }

    #[test]
    fn title_source_serialises_as_snake_case_and_defaults_to_auto() {
        assert_eq!(SessionTitleSource::default(), SessionTitleSource::Auto);
        assert_eq!(
            serde_json::to_string(&SessionTitleSource::Manual).unwrap(),
            "\"manual\""
        );
        assert_eq!(
            serde_json::from_str::<SessionTitleSource>("\"auto\"").unwrap(),
            SessionTitleSource::Auto
        );
    }
}
