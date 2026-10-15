use super::{git_revision, stamp};

#[test]
fn stamping_never_fails_a_build_and_reports_the_checkout_when_there_is_one() {
    // `stamp` writes the `cargo:` directives the build scripts used to print
    // themselves; emitting them is the contract, and it must not panic.
    stamp();

    // Outside a checkout — a packaged crate or a machine without git — there is
    // simply no revision to report, and `stamp` falls back to `unknown` instead
    // of failing the build.
    if let Some(revision) = git_revision() {
        assert!(!revision.is_empty());
        assert!(!revision.trim().is_empty(), "{revision:?}");
        assert!(!revision.contains('\n'), "{revision:?}");
    }
}
