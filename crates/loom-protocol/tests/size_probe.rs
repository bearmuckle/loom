//! The message size budget for the protocol's top-level request and response
//! enums.
//!
//! Every request and response is moved, and often cloned, through the dispatcher
//! and the connection loops, so the size of these enums is paid per message.
//! Each enum is as large as its largest variant, so the variants carrying a
//! large snapshot or record payload are boxed — which keeps the JSON shape
//! unchanged, because `serde` serializes `Box<T>` exactly like `T` — rather than
//! hidden behind `#[allow(clippy::large_enum_variant)]`. This test fixes the
//! resulting budget: growing a variant past it has to be a deliberate edit of
//! the number below.

use std::mem::size_of;

use loom_protocol::{ClientRequest, ServerResponse};

/// The upper bound for one moved or cloned protocol message.
const MESSAGE_SIZE_BUDGET: usize = 256;

#[test]
fn top_level_protocol_messages_stay_within_their_size_budget() {
    // `ClientRequest` is bounded by
    // `RunRequest::StartSessionAgentRunWithOptions` and `ServerResponse` by
    // `TaskResponse::TaskStarted`/`Task`, so both sit at the budget. A new
    // unboxed snapshot, record, or option payload in any variant makes the
    // corresponding assertion below fail instead of quietly making every
    // message that much more expensive.
    assert!(
        size_of::<ClientRequest>() <= MESSAGE_SIZE_BUDGET,
        "ClientRequest grew to {} bytes",
        size_of::<ClientRequest>()
    );
    assert!(
        size_of::<ServerResponse>() <= MESSAGE_SIZE_BUDGET,
        "ServerResponse grew to {} bytes",
        size_of::<ServerResponse>()
    );
}
