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
    /// Last accepted sequence per run, kept apart from the gates so a state
    /// save never waits on a gate held by an action in progress (an action
    /// may itself save state while it holds its gate).
    sequences: Mutex<BTreeMap<RunId, u64>>,
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

    /// Durable form: the action sequence of every run that accepted at
    /// least one action. Saved with the rest of the state so a revision
    /// issued before a restart never matches after it.
    pub(crate) fn export(&self) -> BTreeMap<RunId, u64> {
        self.sequences
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Records that `run_id` accepted an action and is now at `sequence`.
    pub(crate) fn advanced(&self, run_id: RunId, sequence: u64) {
        self.sequences
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run_id, sequence);
    }

    /// Seeds the action sequences from the durable state.
    pub(crate) fn restore(&self, sequences: BTreeMap<RunId, u64>) {
        let mut gates = self.gates.lock().unwrap_or_else(PoisonError::into_inner);
        let mut durable = self
            .sequences
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (run_id, sequence) in sequences {
            let gate = gates.entry(run_id).or_default();
            let mut current = gate.lock().unwrap_or_else(PoisonError::into_inner);
            *current = (*current).max(sequence);
            durable.insert(run_id, *current);
        }
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
