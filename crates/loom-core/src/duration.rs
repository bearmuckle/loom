//! The archive-retention input grammar, shared by the server flags and the CLI.
//!
//! One owner for the syntax keeps `--archive-retention` and `loom-cli retention
//! set --window` from drifting into two grammars. The parser is neutral: it
//! names the input it was given in every error, so each caller reports its own
//! flag or environment variable.

use crate::{LoomError, Result};

/// Parses a retention window.
///
/// The accepted syntax is `<n><unit>` with unit `ms`, `s`, `m`, `h`, `d`, or
/// `w`; a bare integer means seconds; `off` and `never` mean retention stays
/// disabled. `source` names the flag or environment variable for the error, so
/// an operator is told which input to fix.
pub fn parse_retention_window(value: &str, source: &str) -> Result<Option<u64>> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("never")
    {
        return Ok(None);
    }
    let normalized = value.to_ascii_lowercase();
    let (number, multiplier_ms) = if let Some(number) = normalized.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = normalized.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = normalized.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = normalized.strip_suffix('h') {
        (number, 3_600_000)
    } else if let Some(number) = normalized.strip_suffix('d') {
        (number, 86_400_000)
    } else if let Some(number) = normalized.strip_suffix('w') {
        (number, 604_800_000)
    } else {
        (normalized.as_str(), 1_000)
    };
    let number = number.parse::<u64>().map_err(|_| {
        LoomError::invalid_request(format!(
            "{source} must be a duration such as 30s, 5m, 12h, 14d, or 2w, a plain number of \
             seconds, or off; got '{value}'"
        ))
    })?;
    Ok(Some(number.saturating_mul(multiplier_ms)))
}

/// Parses the force-discard switch: `1`/`true`/`yes` and `0`/`false`/`no`,
/// case-insensitively. `source` names the flag or environment variable for the
/// error.
pub fn parse_force_discard(value: &str, source: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => Err(LoomError::invalid_request(format!(
            "{source} must be 1/true/yes or 0/false/no; got '{value}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_force_discard, parse_retention_window};
    use crate::ErrorCode;

    #[test]
    fn retention_durations_parse_every_unit_and_reject_junk() {
        for (value, expected) in [
            ("250ms", 250),
            ("30s", 30_000),
            ("5m", 300_000),
            ("2h", 7_200_000),
            ("3d", 259_200_000),
            ("1w", 604_800_000),
            (" 14d ", 1_209_600_000),
            ("45", 45_000),
            ("0", 0),
        ] {
            assert_eq!(
                parse_retention_window(value, "--window").unwrap(),
                Some(expected),
                "{value}"
            );
        }
        for disabled in ["off", "OFF", "never", "Never", ""] {
            assert_eq!(
                parse_retention_window(disabled, "--window").unwrap(),
                None,
                "{disabled}"
            );
        }
        for rejected in ["h", "ms", "1.5h", "-3s", "5x", "later", "1_000s"] {
            let error = parse_retention_window(rejected, "--window").unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{rejected}");
            assert!(error.message.contains("--window"), "{rejected}: {error}");
        }
        assert_eq!(
            parse_retention_window("99999999999999999999w", "--window")
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn force_discard_accepts_both_spellings_and_names_its_source() {
        for (accepted, expected) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("0", false),
            ("false", false),
            (" No ", false),
        ] {
            assert_eq!(
                parse_force_discard(accepted, "--force-discard").unwrap(),
                expected,
                "{accepted}"
            );
        }
        let error = parse_force_discard("maybe", "--force-discard").unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains("--force-discard"));
    }
}
