//! Runs one prompt through the local `claude` and prints what it streams.
//!
//! `cargo run -p loom-harness --example claude_chat -- "prompt" [--interrupt]`
//! Tool requests are denied. `--interrupt` stops the turn after its first text.

use std::time::Duration;

use loom_harness::{ClaudeCodeOptions, ClaudeCodeSession, ClaudeEvent, PermissionDecision};

fn main() {
    let mut prompt = None;
    let mut interrupt = false;
    for argument in std::env::args().skip(1) {
        if argument == "--interrupt" {
            interrupt = true;
        } else {
            prompt = Some(argument);
        }
    }
    let prompt = prompt.expect("usage: claude_chat <prompt> [--interrupt]");
    let mut options = ClaudeCodeOptions::new(std::env::current_dir().unwrap());
    options.tools = Some(vec![]);
    options.setting_sources = Some("project".to_owned());
    let mut session = ClaudeCodeSession::spawn(&options).expect("spawn claude");
    session.send_user_message(&prompt).expect("send prompt");
    let mut interrupted = false;
    loop {
        match session.next_event(Duration::from_secs(120)) {
            loom_harness::claude_code::Next::Event(event) => {
                println!("{event:?}");
                match event {
                    ClaudeEvent::PermissionRequest { request_id, .. } => {
                        let deny = PermissionDecision::Deny {
                            message: "denied by the example".to_owned(),
                        };
                        session.respond_permission(&request_id, &deny).unwrap();
                    }
                    ClaudeEvent::TextDelta { .. } if interrupt && !interrupted => {
                        interrupted = true;
                        session.interrupt().unwrap();
                    }
                    ClaudeEvent::Result(_) => break,
                    _ => {}
                }
            }
            other => {
                println!("{other:?}\n{}", session.stderr_tail());
                break;
            }
        }
    }
}
