//! Stamps the release version and git revision into the binary so
//! `loom-ui --version` reports the tag and revision a release was built from
//! instead of the workspace version. The stamping itself lives in `loom-build`,
//! shared with the other binaries, and must never fail the build.

fn main() {
    loom_build::stamp();
}
