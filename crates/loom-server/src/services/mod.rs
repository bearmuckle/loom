//! Owned backend service objects.
//!
//! Each service encapsulates one concern of the former `InProcessBackend` god
//! object: its state, its locks, and the operations that maintain its
//! invariants. The backend composes these and delegates to them.

use super::*;

pub(crate) mod admission;
pub(crate) mod credential;
pub(crate) mod idempotency;
