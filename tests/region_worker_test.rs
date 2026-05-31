//! Integration tests for the shared single-threaded region worker.

use small_city::core::regions::continuation::{CallerContinuation, NeighborRequest};
use small_city::core::regions::runtime::{ImportedResourcePayload, RegionEvent, RegionRuntime};
use small_city::core::regions::worker::{RegionWorker, WorkerId, WorkerRoutingError};
use small_city::core::regions::{
    ImportedResource, ImportedResourceResult, RegionId, RegionState, ResourceId, ResourceKind,
};

#[test]
fn one_worker_processes_events_for_multiple_regions() {
    let mut worker = worker_with_regions(WorkerId(1), &[RegionId(1), RegionId(2)]);
    worker.push_event(RegionId(1), RegionEvent::Tick).unwrap();
    worker.push_event(RegionId(2), RegionEvent::Tick).unwrap();

    let summary = worker.process_region_events(1);

    assert!(summary.routing_errors.is_empty());
    assert_eq!(summary.processed_regions, 2);
    assert_eq!(turn(&worker, RegionId(1)), 1);
    assert_eq!(turn(&worker, RegionId(2)), 1);
}

#[test]
fn busy_region_cannot_starve_another_region_when_event_limit_is_set() {
    let mut worker = worker_with_regions(WorkerId(2), &[RegionId(3), RegionId(4)]);
    worker.push_event(RegionId(3), RegionEvent::Tick).unwrap();
    worker.push_event(RegionId(3), RegionEvent::Tick).unwrap();
    worker.push_event(RegionId(3), RegionEvent::Tick).unwrap();
    worker.push_event(RegionId(4), RegionEvent::Tick).unwrap();

    let summary = worker.process_region_events(1);

    assert!(summary.routing_errors.is_empty());
    assert_eq!(summary.processed_regions, 2);
    assert_eq!(turn(&worker, RegionId(3)), 1);
    assert_eq!(turn(&worker, RegionId(4)), 1);
    assert_eq!(pending_events(&worker, RegionId(3)), 2);
    assert_eq!(pending_events(&worker, RegionId(4)), 0);
}

#[test]
fn returned_continuation_is_routed_to_caller_region_inbox() {
    let caller = RegionId(5);
    let target = RegionId(6);
    let mut worker = worker_with_regions(WorkerId(3), &[caller, target]);

    worker
        .push_event(
            target,
            RegionEvent::ProcessImportedResource(request(
                caller,
                resource(20, ResourceKind::ShoppingAccess, 1),
            )),
        )
        .unwrap();

    let first_pass = worker.process_region_events(1);

    assert!(first_pass.routing_errors.is_empty());
    assert_eq!(pending_events(&worker, caller), 1);
    assert!(
        worker
            .region(caller)
            .expect("caller")
            .state()
            .neighbor_import_results()
            .is_empty()
    );

    let second_pass = worker.process_region_events(1);

    assert!(second_pass.routing_errors.is_empty());
    assert_eq!(pending_events(&worker, caller), 0);
    assert_eq!(
        worker
            .region(caller)
            .expect("caller")
            .state()
            .neighbor_import_results()
            .len(),
        1
    );
}

#[test]
fn missing_target_region_produces_deterministic_routing_error() {
    let missing_caller = RegionId(7);
    let target = RegionId(8);
    let mut worker = worker_with_regions(WorkerId(4), &[target]);

    worker
        .push_event(
            target,
            RegionEvent::ProcessImportedResource(request(
                missing_caller,
                resource(21, ResourceKind::ParkAccess, 1),
            )),
        )
        .unwrap();

    let summary = worker.process_region_events(1);

    assert_eq!(
        summary.routing_errors,
        vec![WorkerRoutingError::MissingTargetRegion {
            target_region: missing_caller,
        }]
    );
    assert_eq!(pending_events(&worker, target), 0);
}

#[test]
fn add_region_rejects_duplicate_region_id() {
    let mut worker = RegionWorker::new(WorkerId(5));

    assert!(
        worker
            .add_region(RegionRuntime::new(RegionState::new(RegionId(9), 2, 2)))
            .is_ok()
    );
    let error = worker
        .add_region(RegionRuntime::new(RegionState::new(RegionId(9), 3, 3)))
        .expect_err("duplicate region should be rejected");

    assert_eq!(
        error,
        WorkerRoutingError::DuplicateRegion {
            region_id: RegionId(9),
        }
    );
}

#[test]
fn process_region_events_with_zero_event_limit_reports_no_processed_regions() {
    let mut worker = worker_with_regions(WorkerId(6), &[RegionId(10), RegionId(11)]);
    worker.push_event(RegionId(10), RegionEvent::Tick).unwrap();
    worker.push_event(RegionId(11), RegionEvent::Tick).unwrap();

    let summary = worker.process_region_events(0);

    assert_eq!(summary.processed_regions, 0);
    assert!(summary.routing_errors.is_empty());
    assert_eq!(turn(&worker, RegionId(10)), 0);
    assert_eq!(turn(&worker, RegionId(11)), 0);
    assert_eq!(pending_events(&worker, RegionId(10)), 1);
    assert_eq!(pending_events(&worker, RegionId(11)), 1);
}

fn worker_with_regions(id: WorkerId, regions: &[RegionId]) -> RegionWorker {
    let mut worker = RegionWorker::new(id);
    for region_id in regions {
        worker
            .add_region(RegionRuntime::new(RegionState::new(*region_id, 2, 2)))
            .unwrap();
    }
    worker
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
            region.apply_neighbor_import_result(result);
        }),
    }
}

fn turn(worker: &RegionWorker, region_id: RegionId) -> u32 {
    worker
        .region(region_id)
        .expect("region")
        .state()
        .view()
        .status
        .turn
}

fn pending_events(worker: &RegionWorker, region_id: RegionId) -> usize {
    worker
        .region(region_id)
        .expect("region")
        .pending_event_count()
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
