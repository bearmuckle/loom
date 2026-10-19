//! In-process tests: resources.

#[allow(unused_imports)]
use super::support::*;
use super::*;

#[test]
fn worker_node_status_reports_capabilities_and_resources() {
    let backend = InProcessBackend::new();
    let connection = backend.connect();
    negotiate_m5(&connection);
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::GetWorkerNodeStatus,
    )));
    let response =
        loom_protocol::decode_response(&loom_protocol::encode_response(&response).unwrap())
            .unwrap();
    let ServerResponse::Control(ControlResponse::WorkerNodeStatus(status)) =
        response.result.unwrap()
    else {
        panic!("expected worker node status");
    };
    assert!(status.online);
    assert!(status.resources.cpu_count > 0);
    assert_eq!(status.resources.cpu_usage_percent, None);
    assert!(
        status
            .resources
            .memory_total_bytes
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(status.resources.memory_available_bytes.is_some());
    assert!(
        status
            .resources
            .memory_usage_percent
            .is_some_and(|value| value <= 100)
    );

    std::thread::sleep(Duration::from_millis(250));
    let refreshed = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::GetWorkerNodeStatus,
    )));
    let refreshed =
        loom_protocol::decode_response(&loom_protocol::encode_response(&refreshed).unwrap())
            .unwrap();
    let ServerResponse::Control(ControlResponse::WorkerNodeStatus(refreshed)) =
        refreshed.result.unwrap()
    else {
        panic!("expected refreshed worker node status");
    };
    assert!(
        refreshed
            .resources
            .cpu_usage_percent
            .is_some_and(|value| value <= 100)
    );
    assert!(
        refreshed
            .resources
            .memory_total_bytes
            .is_some_and(|bytes| bytes > 0)
    );
    assert!(
        refreshed
            .resources
            .memory_usage_percent
            .is_some_and(|value| value <= 100)
    );
    assert_eq!(refreshed.resources.cpu_count, status.resources.cpu_count);
    assert_eq!(refreshed.node_id, status.node_id);
    assert_eq!(refreshed.name, status.name);
    assert_eq!(status.name, backend.node_name());
    assert_eq!(
        refreshed.resources.memory_total_bytes,
        status.resources.memory_total_bytes
    );
    assert!(status.capabilities.contains(Capability::ReadAgentSession));
}

#[test]
fn worker_node_status_reports_an_operator_set_node_name() {
    let backend = InProcessBackend::with_provider_registry_and_persistence_named(
        loom_providers::ProviderRegistry::demo(),
        None,
        Some("prod-builder"),
    )
    .unwrap();
    assert_eq!(backend.node_name(), "prod-builder");

    let connection = backend.connect();
    negotiate_m5(&connection);
    let response = connection.request(RequestEnvelope::new(ClientRequest::Control(
        ControlRequest::GetWorkerNodeStatus,
    )));
    let ServerResponse::Control(ControlResponse::WorkerNodeStatus(status)) =
        response.result.unwrap()
    else {
        panic!("expected worker node status");
    };
    assert_eq!(status.name, "prod-builder");
    // The name is display-only, so the node id is unaffected.
    assert!(!status.node_id.is_empty());
    assert_ne!(status.node_id, status.name);

    // A backend without a configured name keeps the derived shape, and the
    // status reports exactly what the backend says.
    let derived = InProcessBackend::new();
    let name = derived.node_name().to_owned();
    assert!(
        name.rsplit_once(" · ")
            .is_some_and(|(hostname, short_id)| !hostname.is_empty() && short_id.len() == 8),
        "{name:?} should be `hostname · short-id`"
    );
}

#[test]
fn a_named_durable_backend_reports_the_configured_name() {
    let path = std::env::temp_dir().join(format!("loom-node-name-{}.db", loom_core::RunId::new()));
    let backend = InProcessBackend::with_provider_registry_persistent_credentials_named(
        loom_providers::ProviderRegistry::demo(),
        Some(path.clone()),
        Some("prod-builder"),
    )
    .unwrap();
    assert_eq!(backend.node_name(), "prod-builder");
    backend.shutdown().unwrap();

    // The name is display-only, so reopening the same database without one
    // derives the display name again instead of replaying the old one.
    let reopened = InProcessBackend::with_provider_registry_persistent_credentials_named(
        loom_providers::ProviderRegistry::demo(),
        Some(path.clone()),
        None,
    )
    .unwrap();
    assert_ne!(reopened.node_name(), "prod-builder");
    reopened.shutdown().unwrap();

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(path.with_extension("credentials.json")).ok();
    std::fs::remove_dir_all(path.with_extension("session-roots")).ok();
    std::fs::remove_dir_all(path.with_extension("clone-cache")).ok();
}

#[test]
fn worker_resource_percentages_handle_unavailable_and_out_of_range_samples() {
    assert_eq!(cpu_usage_percent(f32::NAN), None);
    assert_eq!(cpu_usage_percent(-1.0), Some(0));
    assert_eq!(cpu_usage_percent(47.6), Some(48));
    assert_eq!(cpu_usage_percent(120.0), Some(100));
    assert_eq!(memory_usage_percent(None, Some(5)), None);
    assert_eq!(memory_usage_percent(Some(0), Some(0)), None);
    assert_eq!(memory_usage_percent(Some(100), Some(25)), Some(75));
    assert_eq!(memory_usage_percent(Some(100), Some(150)), Some(0));
}
