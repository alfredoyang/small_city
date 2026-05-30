//! Integration tests for the single-threaded regional event runtime.

use small_city::core::regions::runtime::{
    ImportedResourceRequest, OutboundMessage, RegionEvent, RegionRuntime,
};
use small_city::core::regions::{
    ImportDecision, ImportedResource, RegionId, RegionState, ResourceId, ResourceKind,
};

#[test]
fn local_tick_is_processed_through_runtime() {
    let mut runtime = RegionRuntime::new(RegionState::new(RegionId(1), 3, 2));
    runtime.push_event(RegionEvent::Tick);

    let outbound = runtime.process_next_event();

    assert!(outbound.is_empty());
    assert_eq!(runtime.state().view().status.turn, 1);
    assert_eq!(runtime.pending_event_count(), 0);
}

#[test]
fn events_are_processed_in_insertion_order() {
    let mut runtime = RegionRuntime::new(RegionState::new(RegionId(2), 2, 2));
    runtime.push_event(RegionEvent::ProcessImportedResource(request(
        RegionId(9),
        resource(7, ResourceKind::Jobs, 2, 5, 0, 3, 0, 9),
    )));
    runtime.push_event(RegionEvent::ProcessImportedResource(request(
        RegionId(9),
        resource(7, ResourceKind::Jobs, 1, 5, 0, 3, 0, 9),
    )));

    let outbound = runtime.process_some_events(2);

    assert_eq!(
        decisions(&outbound),
        vec![ImportDecision::Accepted, ImportDecision::RejectedStale]
    );
    assert_eq!(
        runtime.state().imported_resources(),
        &[resource(7, ResourceKind::Jobs, 2, 5, 0, 3, 0, 9)]
    );
}

#[test]
fn process_some_events_respects_max_events() {
    let mut runtime = RegionRuntime::new(RegionState::new(RegionId(3), 3, 2));
    runtime.push_event(RegionEvent::Tick);
    runtime.push_event(RegionEvent::Tick);

    let outbound = runtime.process_some_events(1);

    assert!(outbound.is_empty());
    assert_eq!(runtime.state().view().status.turn, 1);
    assert_eq!(runtime.pending_event_count(), 1);
}

#[test]
fn neighbor_import_event_processes_only_target_payload() {
    let caller = RegionRuntime::new(RegionState::new(RegionId(4), 2, 2));
    let mut target = RegionRuntime::new(RegionState::new(RegionId(5), 2, 2));
    let imported_resource = resource(8, ResourceKind::ParkAccess, 1, 9, 0, 3, 2, 4);

    target.push_event(RegionEvent::ProcessImportedResource(
        ImportedResourceRequest {
            caller_region: caller.region_id(),
            resource: imported_resource,
            local_used_capacity: 3,
            border_crossing_cost: 2,
            target_neighbors: vec![RegionId(4), RegionId(6)],
        },
    ));

    let outbound = target.process_next_event();

    assert!(caller.state().imported_resources().is_empty());
    assert_eq!(target.state().imported_resources(), &[imported_resource]);
    assert_eq!(
        outbound,
        vec![OutboundMessage::ReturnImportedResourceResult {
            caller_region: RegionId(4),
            result: small_city::core::regions::ImportedResourceResult {
                decision: ImportDecision::Accepted,
                forwarded_resources: vec![ImportedResource {
                    remaining_capacity: 6,
                    hop_count: 1,
                    travel_cost: 4,
                    source_neighbor: RegionId(5),
                    ..imported_resource
                }],
            },
        }]
    );
}

#[test]
fn outbound_continuation_message_is_returned_before_caller_mutates() {
    let mut caller = RegionRuntime::new(RegionState::new(RegionId(10), 2, 2));
    let mut target = RegionRuntime::new(RegionState::new(RegionId(11), 2, 2));
    let imported_resource = resource(12, ResourceKind::ShoppingAccess, 1, 7, 0, 2, 0, 10);

    target.push_event(RegionEvent::ProcessImportedResource(request(
        caller.region_id(),
        imported_resource,
    )));
    let outbound = target.process_next_event();

    assert!(caller.state().neighbor_import_results().is_empty());

    let [
        OutboundMessage::ReturnImportedResourceResult {
            caller_region,
            result,
        },
    ] = outbound.as_slice()
    else {
        panic!("expected one returned imported-resource result");
    };

    assert_eq!(*caller_region, caller.region_id());

    caller.push_event(RegionEvent::RunImportedResourceContinuation {
        result: result.clone(),
    });
    assert!(caller.process_next_event().is_empty());

    assert_eq!(caller.state().neighbor_import_results(), &[result.clone()]);
}

fn decisions(outbound: &[OutboundMessage]) -> Vec<ImportDecision> {
    outbound
        .iter()
        .map(|message| match message {
            OutboundMessage::ReturnImportedResourceResult { result, .. } => result.decision,
        })
        .collect()
}

fn request(caller_region: RegionId, resource: ImportedResource) -> ImportedResourceRequest {
    ImportedResourceRequest {
        caller_region,
        resource,
        local_used_capacity: 0,
        border_crossing_cost: 1,
        target_neighbors: Vec::new(),
    }
}

fn resource(
    origin_region: u32,
    resource_kind: ResourceKind,
    generation: u64,
    remaining_capacity: u32,
    hop_count: u32,
    max_hops: u32,
    travel_cost: u32,
    source_neighbor: u32,
) -> ImportedResource {
    ImportedResource {
        id: ResourceId {
            origin_region: RegionId(origin_region),
            resource_kind,
            generation,
        },
        remaining_capacity,
        hop_count,
        max_hops,
        travel_cost,
        source_neighbor: RegionId(source_neighbor),
    }
}
