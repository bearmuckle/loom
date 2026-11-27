//! Antigravity (`agy acp` / `antigravity-acp`) discovery, auth selection,
//! interactive-only policy guard, and disclosure notices.
//!
//! Loom launches the operator's own unmodified `agy` / `antigravity-acp` binary
//! and delegates authentication solely to ACP `authenticate`. Loom never reads
//! `GEMINI_API_KEY` or any Google/Antigravity/Gemini credential or token file.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use loom_core::{
    ANTIGRAVITY_AUTH_GEMINI_API_KEY, ANTIGRAVITY_AUTH_OAUTH_PERSONAL, ANTIGRAVITY_HARNESS_ID,
    ANTIGRAVITY_INTERACTIONS_NOTICE, ANTIGRAVITY_TERMS_RISK_NOTICE,
    ANTIGRAVITY_UNREGISTERED_CLIENT_NOTICE, ErrorCode, LoomError, Result,
};

use super::{AcpAuthMethod, AcpCommandSpec};

/// Environment variable allowing the operator to override the Antigravity ACP
/// command on the worker (for example a custom path to `agy` or
/// `antigravity-acp`).
pub const ANTIGRAVITY_COMMAND_ENV: &str = "LOOM_ANTIGRAVITY_ACP_COMMAND";

/// Environment variables that mark an unattended or non-interactive worker run.
const UNATTENDED_ENV_VARS: &[&str] = &[
    "LOOM_UNATTENDED",
    "LOOM_HEADLESS",
    "LOOM_SCHEDULED",
    "CI",
    "GITHUB_ACTIONS",
];

/// Returns the user-facing policy notices for the Antigravity ACP backend.
pub fn antigravity_notices() -> Vec<String> {
    vec![
        ANTIGRAVITY_TERMS_RISK_NOTICE.to_owned(),
        ANTIGRAVITY_INTERACTIONS_NOTICE.to_owned(),
        ANTIGRAVITY_UNREGISTERED_CLIENT_NOTICE.to_owned(),
    ]
}

/// Discovers the official `agy` / `antigravity-acp` binary on `PATH`, or
/// resolves an explicit operator override.
///
/// Resolution order:
/// 1. Explicit `override_cmd` argument (session/request config).
/// 2. `LOOM_ANTIGRAVITY_ACP_COMMAND` environment variable (via `env_override`).
/// 3. `agy` on `path_env` → invoked as `agy acp --stdio`.
/// 4. `antigravity-acp` on `path_env` → invoked as `antigravity-acp --stdio`.
pub fn discover_antigravity_command(
    override_cmd: Option<&str>,
    env_override: Option<&str>,
    path_env: Option<&OsStr>,
) -> Result<AcpCommandSpec> {
    if let Some(raw) = override_cmd
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            env_override
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
    {
        return parse_command_override(raw);
    }

    if let Some(path_env) = path_env {
        if let Some(agy) = find_executable_in_path("agy", path_env) {
            return Ok(AcpCommandSpec {
                program: agy,
                args: vec!["acp".to_owned(), "--stdio".to_owned()],
                display: "agy acp".to_owned(),
                supports_agy_profile_flags: true,
            });
        }
        if let Some(acp_bin) = find_executable_in_path("antigravity-acp", path_env) {
            return Ok(AcpCommandSpec {
                program: acp_bin,
                args: vec!["--stdio".to_owned()],
                display: "antigravity-acp".to_owned(),
                supports_agy_profile_flags: false,
            });
        }
    }

    Err(LoomError::new(
        ErrorCode::ProviderUnavailable,
        format!(
            "Antigravity ACP binary not found: neither `agy` nor `antigravity-acp` was found on PATH. \
             Install Google's official Antigravity CLI, or set {ANTIGRAVITY_COMMAND_ENV} / a manual \
             command override. For a ToS-safe key-based path once installed, select auth method \
             '{ANTIGRAVITY_AUTH_GEMINI_API_KEY}' with GEMINI_API_KEY in the worker environment."
        ),
        false,
    ))
}

fn parse_command_override(raw: &str) -> Result<AcpCommandSpec> {
    let tokens = split_command_tokens(raw)?;
    let Some((first, rest)) = tokens.split_first() else {
        return Err(LoomError::invalid_request(
            "Antigravity ACP command override must not be empty",
        ));
    };
    let program = PathBuf::from(first);
    let stem = program
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if rest.is_empty() && stem == "agy" {
        return Ok(AcpCommandSpec {
            program,
            args: vec!["acp".to_owned(), "--stdio".to_owned()],
            display: "agy acp".to_owned(),
            supports_agy_profile_flags: true,
        });
    }
    let is_agy_acp = stem == "agy" && rest.first().map(String::as_str) == Some("acp");
    let display = sanitize_command_display(&program, is_agy_acp);
    Ok(AcpCommandSpec {
        program,
        args: rest.to_vec(),
        display,
        supports_agy_profile_flags: is_agy_acp,
    })
}

/// Returns a sanitized display label for a resolved ACP harness command.
///
/// Never includes raw command arguments or inline environment assignments, so
/// persisted session metadata and events cannot leak flags or secrets.
fn sanitize_command_display(program: &Path, is_agy_acp: bool) -> String {
    if is_agy_acp {
        return "agy acp".to_owned();
    }
    let stem = program
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if stem == "antigravity-acp" {
        return "antigravity-acp".to_owned();
    }
    program
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("acp-harness")
        .to_owned()
}

fn split_command_tokens(raw: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in raw.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if quote.is_some() {
        return Err(LoomError::invalid_request(
            "unterminated quote in Antigravity ACP command override",
        ));
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn find_executable_in_path(name: &str, path_env: &OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_env) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            for ext in ["exe", "cmd", "bat"] {
                let with_ext = dir.join(format!("{name}.{ext}"));
                if is_executable_file(&with_ext) {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Validates and selects the ACP `authenticate` method against the methods
/// advertised by the agent in `initialize`.
///
/// Defaults to `oauth-personal`. If the requested method is not advertised by
/// the agent, returns a clear error naming the advertised methods and hinting
/// at `gemini-api-key`.
pub fn validate_auth_method(
    requested: Option<&str>,
    advertised: &[AcpAuthMethod],
) -> Result<String> {
    let chosen = requested
        .map(str::trim)
        .filter(|method| !method.is_empty())
        .unwrap_or(ANTIGRAVITY_AUTH_OAUTH_PERSONAL);
    if advertised.iter().any(|method| method.id == chosen) {
        return Ok(chosen.to_owned());
    }
    let available = if advertised.is_empty() {
        "none".to_owned()
    } else {
        advertised
            .iter()
            .map(|method| method.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let hint = if chosen != ANTIGRAVITY_AUTH_GEMINI_API_KEY
        && advertised
            .iter()
            .any(|method| method.id == ANTIGRAVITY_AUTH_GEMINI_API_KEY)
    {
        format!(
            " Try auth method '{ANTIGRAVITY_AUTH_GEMINI_API_KEY}' with GEMINI_API_KEY set in \
             the worker environment."
        )
    } else if chosen != ANTIGRAVITY_AUTH_GEMINI_API_KEY {
        format!(
            " Consider switching to auth method '{ANTIGRAVITY_AUTH_GEMINI_API_KEY}' when \
             supported by the harness."
        )
    } else {
        String::new()
    };
    Err(LoomError::new(
        ErrorCode::ProviderAuthentication,
        format!(
            "ACP harness does not advertise auth method '{chosen}' (advertised: {available}).{hint}"
        ),
        false,
    ))
}

/// Execution context checked by the interactive-only guard for Antigravity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AntigravityRunContext {
    pub interactive: bool,
    pub auto_approve_actions: bool,
    pub delegated_or_scheduled: bool,
}

/// Enforces that Antigravity ACP runs are strictly interactive (person-present).
///
/// Refuses headless, scheduled, CI, overnight, delegated, and auto-approve runs
/// with a clear error.
pub fn check_interactive_only(harness: &str, ctx: AntigravityRunContext) -> Result<()> {
    check_interactive_only_with_ci_policy(harness, ctx, true)
}

/// Enforces that Antigravity ACP runs are strictly interactive, with an
/// explicit toggle for whether ambient `CI` / `GITHUB_ACTIONS` environment
/// variables are checked (always `true` in production; tests running under CI
/// can opt out explicitly on their test backend).
pub fn check_interactive_only_with_ci_policy(
    harness: &str,
    ctx: AntigravityRunContext,
    enforce_ci_env: bool,
) -> Result<()> {
    check_interactive_only_with_env(harness, ctx, |name| {
        if !enforce_ci_env && matches!(name, "CI" | "GITHUB_ACTIONS") {
            return None;
        }
        std::env::var(name).ok()
    })
}

/// Pure version of [`check_interactive_only`] with injectable environment lookup.
pub fn check_interactive_only_with_env(
    harness: &str,
    ctx: AntigravityRunContext,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<()> {
    if harness != ANTIGRAVITY_HARNESS_ID {
        return Ok(());
    }
    if !ctx.interactive {
        return Err(LoomError::new(
            ErrorCode::InvalidRequest,
            "Antigravity ACP backend is interactive-only: headless or unattended sessions are \
             refused. Start an interactive session with a person present, or use a standard \
             API-key provider for automated/unattended runs.",
            false,
        ));
    }
    if ctx.delegated_or_scheduled {
        return Err(LoomError::new(
            ErrorCode::InvalidRequest,
            "Antigravity ACP backend is interactive-only: delegated, background, and scheduled \
             runs are refused.",
            false,
        ));
    }
    if ctx.auto_approve_actions {
        return Err(LoomError::new(
            ErrorCode::InvalidRequest,
            "Antigravity ACP backend is interactive-only: auto-approve mode is refused. Require \
             interactive approvals before starting an Antigravity run.",
            false,
        ));
    }
    for var in UNATTENDED_ENV_VARS {
        if let Some(value) = lookup(var)
            && is_truthy_env(&value)
        {
            return Err(LoomError::new(
                ErrorCode::InvalidRequest,
                format!(
                    "Antigravity ACP backend is interactive-only: refused because {var} is set \
                     in the worker environment (CI/unattended/scheduled runs are not allowed)."
                ),
                false,
            ));
        }
    }
    Ok(())
}

fn is_truthy_env(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && !matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
}

/// Redacts common credential, OAuth token, API key, and authorization-code
/// patterns from ACP harness stderr or diagnostic strings before Loom stores
/// them in the durable journal or run summaries.
///
/// This filter operates purely on token/parameter patterns and never reads
/// `GEMINI_API_KEY` or any credential store.
pub fn redact_acp_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Google API keys: `AIza[0-9A-Za-z_-]{10,}`
        if bytes[i..].starts_with(b"AIza") {
            let mut end = i + 4;
            while end < len && is_token_char(bytes[end]) {
                end += 1;
            }
            if end - i >= 14 {
                out.push_str("[REDACTED_GOOGLE_KEY]");
                i = end;
                continue;
            }
        }

        // Google OAuth access tokens: `ya29.[0-9A-Za-z._-]+`
        if bytes[i..].starts_with(b"ya29.") {
            let mut end = i + 5;
            while end < len && (is_token_char(bytes[end]) || bytes[end] == b'.') {
                end += 1;
            }
            if end > i + 5 {
                out.push_str("[REDACTED_OAUTH_TOKEN]");
                i = end;
                continue;
            }
        }

        // `Bearer <token>` (case-insensitive)
        if i + 7 <= len
            && bytes[i..i + 6].eq_ignore_ascii_case(b"bearer")
            && bytes[i + 6].is_ascii_whitespace()
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
            let mut val_start = i + 6;
            while val_start < len && bytes[val_start].is_ascii_whitespace() {
                val_start += 1;
            }
            let mut val_end = val_start;
            while val_end < len && is_bearer_token_char(bytes[val_end]) {
                val_end += 1;
            }
            if val_end > val_start {
                out.push_str(&input[i..val_start]);
                out.push_str("[REDACTED]");
                i = val_end;
                continue;
            }
        }

        // Sensitive query parameters or key=value pairs (`access_token=`,
        // `refresh_token=`, `id_token=`, `client_secret=`, `api_key=`,
        // `apikey=`, `gemini_api_key=`, `authorization=`, `code=`, `key=`).
        if let Some((key_len, val_end)) = match_sensitive_kv(input, i) {
            out.push_str(&input[i..i + key_len + 1]);
            out.push_str("[REDACTED]");
            i = val_end;
            continue;
        }

        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')
}

fn is_bearer_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~' | b'+' | b'/' | b'=')
}

fn is_kv_value_char(b: u8) -> bool {
    !b.is_ascii_whitespace()
        && !matches!(
            b,
            b'&' | b'"' | b'\'' | b'`' | b',' | b';' | b')' | b']' | b'}' | b'>' | b'#'
        )
}

const SENSITIVE_KV_KEYS: &[&str] = &[
    "gemini_api_key",
    "refresh_token",
    "client_secret",
    "authorization",
    "access_token",
    "id_token",
    "api_key",
    "apikey",
    "token",
    "code",
    "key",
];

fn match_sensitive_kv(input: &str, start: usize) -> Option<(usize, usize)> {
    let bytes = input.as_bytes();
    // Ensure key starts at a token boundary (`?`, `&`, whitespace, punctuation, or start).
    if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        return None;
    }
    let rest = &bytes[start..];
    for key in SENSITIVE_KV_KEYS {
        let klen = key.len();
        if rest.len() > klen
            && rest[..klen].eq_ignore_ascii_case(key.as_bytes())
            && rest[klen] == b'='
        {
            let val_start = start + klen + 1;
            let mut val_end = val_start;
            while val_end < bytes.len() && is_kv_value_char(bytes[val_end]) {
                val_end += 1;
            }
            if val_end > val_start {
                return Some((klen, val_end));
            }
        }
    }
    None
}
