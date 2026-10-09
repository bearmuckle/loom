//! Stamps the release version and git revision into the binary so
//! `loom-ui --version` reports the tag and revision a release was built from
//! instead of the workspace version. The script must never fail the build: a
//! packaged crate or a machine without git falls back to the crate version and
//! the literal `unknown`.

use std::process::Command;

fn main() {
    // Precedence: an explicit `LOOM_BUILD_VERSION` (CI or packaging override),
    // then the crate version. Releases are tagged `v0.8.x` while the workspace
    // version stays `0.1.0`, so the stamped value is what a released binary
    // must report.
    let version = std::env::var("LOOM_BUILD_VERSION")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION")
        });
    println!("cargo:rustc-env=LOOM_BUILD_VERSION={version}");

    // Precedence: an explicit `LOOM_GIT_REVISION` (CI or packaging override),
    // then the checked-out revision, then `unknown`.
    let revision = std::env::var("LOOM_GIT_REVISION")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(git_revision)
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=LOOM_GIT_REVISION={revision}");

    // Re-stamp when an override changes. Without this Cargo reuses the cached
    // result of this script, so restoring a cached `target/` directory (the
    // release workflow does) would leave the binary stamped for an earlier
    // build and a release would report the workspace version.
    println!("cargo:rerun-if-env-changed=LOOM_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=LOOM_GIT_REVISION");

    // Still no `rerun-if-changed` directive for `.git`: when the path is absent
    // (packaged crate, worktree without a real `.git`) Cargo would rerun this
    // script — and rebuild the crate — on every invocation.
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
