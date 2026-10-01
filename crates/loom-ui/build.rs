//! Stamps the client's git revision into the binary for the Settings → About
//! pane. The script must never fail the build: a packaged crate, a shallow
//! checkout, or a machine without git all fall back to the literal `unknown`.

use std::process::Command;

fn main() {
    // Precedence: an explicit `LOOM_GIT_REVISION` (CI or packaging override),
    // then the checked-out revision, then `unknown`.
    let revision = std::env::var("LOOM_GIT_REVISION")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(git_revision)
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=LOOM_GIT_REVISION={revision}");

    // Deliberately no `rerun-if-changed` directive for `.git`: when the path is
    // absent (packaged crate, worktree without a real `.git`) Cargo would rerun
    // this script — and rebuild the crate — on every invocation. Relying on
    // Cargo's default (rerun only when a file in this package changes) keeps
    // rebuilds incremental while still picking up a moved checkout.
}

/// The short revision of the current checkout, or `None` when git is missing,
/// the directory is not a repository, or the command fails for any reason.
fn git_revision() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let revision = String::from_utf8(output.stdout).ok()?;
    let revision = revision.trim();
    if revision.is_empty() {
        None
    } else {
        Some(revision.to_owned())
    }
}
