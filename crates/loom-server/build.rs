//! Stamps the release version and the git revision into the binary so
//! `loom-server --version` reports the tag a release was built from instead of
//! the workspace version. The stamping itself lives in `loom-build`, shared with
//! the other binaries, and must never fail the build.

fn main() {
    loom_build::stamp();
}
