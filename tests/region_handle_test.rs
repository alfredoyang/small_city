//! Integration tests for stable in-process region handles.

use small_city::core::regions::continuation::{CallerContinuation, NeighborRequest};
use small_city::core::regions::runtime::{
    ImportedResourcePayload, OutboundMessage, RegionEvent, RegionRuntime,
};
use small_city::core::regions::{
    ImportDecision, ImportedResource, ImportedResourceResult, RegionId, RegionState, ResourceId,
    ResourceKind,
};

#[test]
fn region_can_send_event_through_neighbor_handle() {
    let source = RegionRuntime::new(RegionState::new(RegionId(1), 2, 2));
    let mut target = RegionRuntime::new(RegionState::new(RegionId(2), 2, 2));
    let target_handle = target.handle();

    source.send_to_region(&target_handle, RegionEvent::Tick);

    assert_eq!(target.pending_event_count(), 1);
    assert!(target.process_next_event().is_empty());
    assert_eq!(target.state().view().status.turn, 1);
    assert_eq!(source.state().view().status.turn, 0);
}

#[test]
fn sender_handle_can_be_cloned_without_cloning_receiver() {
    let mut target = RegionRuntime::new(RegionState::new(RegionId(3), 2, 2));
    let first_sender = target.handle();
    let second_sender = first_sender.clone();

    first_sender.send(RegionEvent::Tick);
    second_sender.send(RegionEvent::Tick);

    assert_eq!(target.pending_event_count(), 2);
    assert!(target.process_some_events(2).is_empty());
    assert_eq!(target.state().view().status.turn, 2);
}

#[test]
fn receiver_remains_owned_by_target_runtime_after_runtime_moves() {
    let target = RegionRuntime::new(RegionState::new(RegionId(4), 2, 2));
    let handle = target.handle();

    let mut moved_target = target;
    handle.send(RegionEvent::Tick);

    assert_eq!(moved_target.pending_event_count(), 1);
    assert!(moved_target.process_next_event().is_empty());
    assert_eq!(moved_target.state().view().status.turn, 1);
}

#[test]
fn region_sends_event_and_caller_runs_returned_continuation() {
    let mut region_a = RegionRuntime::new(RegionState::new(RegionId(10), 2, 2));
    let mut region_b = RegionRuntime::new(RegionState::new(RegionId(11), 2, 2));
    let region_a_handle = region_a.handle();
    let region_b_handle = region_b.handle();

    region_a.send_to_region(
        &region_b_handle,
        RegionEvent::ProcessImportedResource(request(
            region_a.region_id(),
            resource(12, ResourceKind::ServiceAccess, 1),
        )),
    );

    let outbound = region_b.process_next_event();
    let [message] = outbound
        .try_into()
        .unwrap_or_else(|_| panic!("expected one outbound message"));
    let OutboundMessage::ReturnImportedResourceContinuation {
        caller_region,
        continuation,
        result,
    } = message
    else {
        panic!("expected returned imported-resource continuation");
    };

    assert_eq!(caller_region, region_a.region_id());
    assert_eq!(result.decision, ImportDecision::Accepted);
    assert_eq!(region_a.pending_event_count(), 0);
    assert_eq!(region_a.state().view().status.turn, 0);

    region_b.send_to_region(
        &region_a_handle,
        RegionEvent::RunImportedResourceContinuation {
            continuation,
            result,
        },
    );

    assert_eq!(region_a.pending_event_count(), 1);
    assert!(region_a.process_next_event().is_empty());
    assert_eq!(region_a.state().view().status.turn, 1);
    assert_eq!(region_a.state().neighbor_import_results().len(), 1);
    assert_eq!(region_b.state().view().status.turn, 0);
}

fn request(
    caller_region: RegionId,
    resource: ImportedResource,
) -> NeighborRequest<ImportedResourcePayload, ImportedResourceResult> {
    NeighborRequest {
        payload: ImportedResourcePayload {
            resource,
            local_used_capacity: 0,
            border_crossing_cost: 1,
            target_neighbors: Vec::new(),
        },
        continuation: CallerContinuation::new(caller_region, |region, result| {
            region.tick_local();
            region.apply_neighbor_import_result(result);
        }),
    }
}

fn resource(origin_region: u32, resource_kind: ResourceKind, generation: u64) -> ImportedResource {
    ImportedResource {
        id: ResourceId {
            origin_region: RegionId(origin_region),
            resource_kind,
            generation,
        },
        remaining_capacity: 5,
        hop_count: 0,
        max_hops: 2,
        travel_cost: 0,
        source_neighbor: RegionId(origin_region),
    }
}
