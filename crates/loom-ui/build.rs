//! Stamps the release version and the client's git revision into the binary for
//! `--version` and the Settings → About pane. The stamping itself lives in
//! `loom-build`, shared with the other binaries, and must never fail the build.

fn main() {
    loom_build::stamp();
}
