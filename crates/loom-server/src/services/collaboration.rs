//! Per-run ordering of people's directions, answers, and control actions.
//!
//! Each run has a gate: a lock held while an action is checked against the
//! run's current [`RunRevision`] and applied, plus a counter of accepted
//! actions. Two actions decided against the same revision therefore apply at
//! most once between them; the other sees the advanced revision and is
//! answered with a conflict. See [`RunRevision`] for the client-facing rule.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use loom_core::{RunId, RunRevision};
use loom_model::AgentRunSnapshot;

#[derive(Default)]
pub(crate) struct CollaborationService {
    gates: Mutex<BTreeMap<RunId, Arc<Mutex<u64>>>>,
}

/// The held gate of one run; the counter is the run's action sequence.
pub(crate) struct RunActionGate {
    sequence: Arc<Mutex<u64>>,
}

impl RunActionGate {
    pub(crate) fn lock(&self) -> MutexGuard<'_, u64> {
        self.sequence.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CollaborationService {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn gate(&self, run_id: RunId) -> RunActionGate {
        let sequence = Arc::clone(
            self.gates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(run_id)
                .or_default(),
        );
        RunActionGate { sequence }
    }
}

/// The revision of `run` with the gate's action sequence.
pub(crate) fn run_revision(run: &AgentRunSnapshot, action_sequence: u64) -> RunRevision {
    RunRevision {
        attempt_id: run.attempt_id,
        control_revision: run.control_revision,
        action_sequence,
    }
}
