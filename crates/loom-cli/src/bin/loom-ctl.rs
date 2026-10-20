//! `loom-ctl`: a read-only client of the versioned Loom protocol.
//!
//! The binary owns no state and accepts no mutation verb: every request it
//! sends is one of the read-only requests documented on the `ctl` module, so a
//! worker is never changed by running it. The module tree is shared verbatim
//! with its unit tests, leaving this file argument collection and process
//! exit.

#[path = "../ctl/mod.rs"]
mod ctl;

fn main() {
    let arguments = std::env::args().skip(1).collect();
    std::process::exit(ctl::main(arguments));
}
