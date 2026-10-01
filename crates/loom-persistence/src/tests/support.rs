//! Shared setup and helpers for the persistence tests.

use super::*;

// The cross-process owner-lock test spawns a child while holding a lock
// file. Serialize it with the drop/reacquire tests so a forked child cannot
// transiently retain another test's flock descriptor.
pub(super) static EXCLUSIVE_WRITER_TEST_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn stored_fragment_count(path: &std::path::Path, run_id: RunId) -> usize {
    let connection = Connection::open(path).unwrap();
    let mut statement = connection
        .prepare("SELECT fragments FROM run_messages WHERE run_id=?1")
        .unwrap();
    let counts = statement
        .query_map([run_id.as_uuid().as_bytes().as_slice()], |row| {
            row.get::<_, String>(0)
        })
        .unwrap()
        .map(|row| decode_stored_fragments(&row.unwrap()).unwrap().len())
        .collect::<Vec<_>>();
    counts.into_iter().sum()
}

pub(super) fn invalid_feed_for_rollback() -> DurableFeedState {
    DurableFeedState {
        next_sequence: EventSequence::new(u64::MAX),
        retention_limit: 250,
        events: Vec::new(),
        workspace_events: Vec::new(),
    }
}

pub(super) fn run_exclusive_writer_probe(path: &Path, should_be_owned: bool) {
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("tests::exclusive_writer_subprocess_probe")
        .arg("--nocapture")
        .env("LOOM_TEST_OWNER_PROBE_PATH", path)
        .env("LOOM_TEST_OWNER_EXPECTED", should_be_owned.to_string())
        .status()
        .unwrap();
    assert!(status.success(), "subprocess ownership probe failed");
}
