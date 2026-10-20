//! The `--cursor-file` record that makes `events` resumable.
//!
//! The record is plain JSON so a person can read or delete it:
//! `{"version":1,"url":...,"session_id":...,"after":...,"stream_epoch":...}`.
//! A record written for another URL or session is not this run's position and
//! is ignored, as is one written by another version of this format. The file is
//! replaced by writing a temporary file in the same directory and renaming it
//! over the target, so an interrupted write cannot leave a torn record.

use std::path::Path;

use loom_core::AgentSessionId;

const VERSION: u64 = 1;

/// Where a session's event stream was read up to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Cursor {
    pub after: Option<u64>,
    pub stream_epoch: Option<String>,
}

/// Reads the cursor a previous run stored for this URL and session, or `None`
/// when the file is absent, belongs to another URL or session, or was written
/// in another version. A file that exists but is not a cursor record is an
/// error, because silently starting from the wrong position would reprint
/// history.
pub(crate) fn read(
    path: &Path,
    url: &str,
    session_id: AgentSessionId,
) -> Result<Option<Cursor>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not read the cursor file '{}': {error}",
                path.display()
            ));
        }
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("the cursor file '{}' is not JSON: {error}", path.display()))?;
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(VERSION) {
        return Ok(None);
    }
    if value.get("url").and_then(serde_json::Value::as_str) != Some(url) {
        return Ok(None);
    }
    let session_text = session_id.to_string();
    if value.get("session_id").and_then(serde_json::Value::as_str) != Some(session_text.as_str()) {
        return Ok(None);
    }
    Ok(Some(Cursor {
        after: value.get("after").and_then(serde_json::Value::as_u64),
        stream_epoch: value
            .get("stream_epoch")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    }))
}

/// Replaces the cursor file atomically.
pub(crate) fn write(
    path: &Path,
    url: &str,
    session_id: AgentSessionId,
    cursor: &Cursor,
) -> Result<(), String> {
    let record = serde_json::json!({
        "version": VERSION,
        "url": url,
        "session_id": session_id.to_string(),
        "after": cursor.after,
        "stream_epoch": cursor.stream_epoch,
    });
    let mut temporary = path.to_path_buf();
    temporary.set_extension("cursor.tmp");
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create the cursor directory '{}': {error}",
                parent.display()
            )
        })?;
    }
    std::fs::write(&temporary, format!("{record}\n")).map_err(|error| {
        format!(
            "could not write the cursor file '{}': {error}",
            temporary.display()
        )
    })?;
    std::fs::rename(&temporary, path).map_err(|error| {
        format!(
            "could not replace the cursor file '{}': {error}",
            path.display()
        )
    })
}
