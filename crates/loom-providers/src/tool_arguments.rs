//! Decoding of the raw argument payload of a provider tool call.
//!
//! A tool call the model sent with arguments that cannot be decoded must not
//! abort the whole completion: the runtime answers that one call with a tool
//! error and the model can resend it, while every other call in the same
//! completion is still processed. This module therefore returns the reason as
//! a value instead of a provider error.

use serde_json::Value;

/// Upper bound on the argument payload of a single tool call.
///
/// The limit is a guard against a pathological payload, such as a garbled or
/// endless argument stream that would otherwise be parsed and held in memory.
/// Exceeding it is reported to the model as a failed call rather than
/// surfacing as a parse failure, so the model can resend the call.
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;

/// Decodes the raw argument string of one tool call.
///
/// Returns the decoded argument object, or a human-readable reason that names
/// the tool when the payload cannot be used as arguments. The reason is
/// written for the model, so it states what was wrong rather than a parser
/// code; a payload that looks cut off says so explicitly.
pub fn decode_tool_arguments(name: &str, raw: &str) -> std::result::Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    if raw.len() > MAX_TOOL_ARGUMENT_BYTES {
        return Err(format!(
            "tool '{name}' arguments are {} bytes, which exceeds the {MAX_TOOL_ARGUMENT_BYTES} byte limit",
            raw.len()
        ));
    }
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => {
            let mut reason = format!(
                "tool '{name}' arguments are {} bytes and were not valid JSON: {error}",
                raw.len()
            );
            if error.classify() == serde_json::error::Category::Eof {
                reason.push_str(" (the payload looks truncated and was cut off)");
            }
            return Err(reason);
        }
    };
    if !value.is_object() {
        return Err(format!(
            "tool '{name}' arguments must be a JSON object, but a JSON {} was received",
            json_kind(&value)
        ));
    }
    Ok(value)
}

/// Names the JSON kind of `value` for a reason aimed at the model.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_TOOL_ARGUMENT_BYTES, decode_tool_arguments};

    #[test]
    fn object_arguments_are_decoded() {
        assert_eq!(
            decode_tool_arguments("read_file", r#"{"path":"README.md"}"#).unwrap(),
            serde_json::json!({"path": "README.md"})
        );
    }

    #[test]
    fn empty_arguments_decode_to_an_empty_object() {
        for raw in ["", " ", "\n\t "] {
            assert_eq!(
                decode_tool_arguments("list_files", raw).unwrap(),
                serde_json::json!({})
            );
        }
    }

    #[test]
    fn an_invalid_escape_names_the_tool_and_the_byte_length() {
        let raw = r#"{"path":"C:\Looms"}"#;
        let reason = decode_tool_arguments("read_file", raw).unwrap_err();
        assert!(reason.contains("read_file"), "{reason}");
        assert!(reason.contains(&format!("{} bytes", raw.len())), "{reason}");
        assert!(!reason.contains("truncated"), "{reason}");
    }

    #[test]
    fn a_truncated_payload_is_reported_as_cut_off() {
        let raw = r#"{"path""#;
        let reason = decode_tool_arguments("read_file", raw).unwrap_err();
        assert!(reason.contains("read_file"), "{reason}");
        assert!(reason.contains(&format!("{} bytes", raw.len())), "{reason}");
        assert!(
            reason.contains("looks truncated and was cut off"),
            "{reason}"
        );
    }

    #[test]
    fn an_oversized_payload_reports_the_limit_without_parsing_it() {
        let raw = format!("\"{}\"", "x".repeat(MAX_TOOL_ARGUMENT_BYTES));
        let reason = decode_tool_arguments("read_file", &raw).unwrap_err();
        assert!(reason.contains("read_file"), "{reason}");
        assert!(reason.contains(&format!("{} bytes", raw.len())), "{reason}");
        assert!(
            reason.contains(&MAX_TOOL_ARGUMENT_BYTES.to_string()),
            "{reason}"
        );
    }

    #[test]
    fn a_non_object_payload_reports_the_kind_received() {
        for (raw, kind) in [
            ("[1]", "array"),
            ("42", "number"),
            ("\"text\"", "string"),
            ("null", "null"),
            ("true", "boolean"),
        ] {
            let reason = decode_tool_arguments("read_file", raw).unwrap_err();
            assert!(reason.contains("read_file"), "{reason}");
            assert!(reason.contains(&format!("JSON {kind}")), "{reason}");
        }
    }
}
