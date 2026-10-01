//! Embedded persistence tests, split by domain.

use super::*;
use loom_session::{SessionManager, WorkspaceManager};
use std::process::Command;
use uuid::Uuid;

#[allow(unused_imports)]
use support::*;

mod catalog;
mod content;
mod feed;
mod misc;
mod project;
mod runs;
mod schema;
mod support;
mod writer;
