//! `loom-ctl`: a client of the versioned Loom protocol.
//!
//! The binary owns no state. Every request it sends is one of the requests
//! documented on the `ctl` module: the read-only verbs inspect a worker without
//! changing it, and `run` is the one mutation, starting a single agent run and
//! returning as soon as the worker accepts it. The module tree is shared
//! verbatim with its unit tests, leaving this file argument collection and
//! process exit.

#[path = "../ctl/mod.rs"]
mod ctl;

fn main() {
    let arguments = std::env::args().skip(1).collect();
    std::process::exit(ctl::main(arguments));
}
