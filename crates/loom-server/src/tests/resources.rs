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
    assert_eq!(
        refreshed.resources.memory_total_bytes,
        status.resources.memory_total_bytes
    );
    assert!(status.capabilities.contains(Capability::ReadAgentSession));
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

#[test]
fn worker_resource_monitor_measures_cpu_utilization_after_a_baseline_sample() {
    let mut monitor = ResourceMonitor::default();

    assert_eq!(monitor.sample(None, None).cpu_usage_percent, None);
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        monitor
            .sample(None, None)
            .cpu_usage_percent
            .is_some_and(|value| value <= 100)
    );
}
