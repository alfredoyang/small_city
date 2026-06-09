//! Integration tests for the shared single-threaded region worker.

use small_city::core::regional_types::{RegionCommand, UiRequestId};
use small_city::core::regions::continuation::{CallerContinuation, NeighborRequest};
use small_city::core::regions::runtime::{ImportedResourcePayload, RegionEvent, RegionRuntime};
use small_city::core::regions::worker::{RegionWorker, WorkerId, WorkerRoutingError};
use small_city::core::regions::{
    BorderEdge, ImportedResource, ImportedResourceResult, RegionId, RegionNeighborLink,
    RegionRoadNetworkId, RegionState, ResourceId, ResourceKind,
};
use small_city::interface::input::BuildingKind;

#[test]
fn one_worker_processes_events_for_multiple_regions() {
    let mut worker = worker_with_regions(WorkerId(1), &[RegionId(1), RegionId(2)]);
    worker
        .push_event(
            RegionId(1),
            RegionEvent::Tick {
                request_id: UiRequestId(1),
            },
        )
        .unwrap();
    worker
        .push_event(
            RegionId(2),
            RegionEvent::Tick {
                request_id: UiRequestId(2),
            },
        )
        .unwrap();

    let summary = worker.process_region_events(1);

    assert!(summary.routing_errors.is_empty());
    assert_eq!(summary.processed_regions, 2);
    assert_eq!(summary.tick_replies.len(), 2);
    assert_eq!(turn(&worker, RegionId(1)), 1);
    assert_eq!(turn(&worker, RegionId(2)), 1);
}

#[test]
fn busy_region_cannot_starve_another_region_when_event_limit_is_set() {
    let mut worker = worker_with_regions(WorkerId(2), &[RegionId(3), RegionId(4)]);
    worker.push_event(RegionId(3), tick(3)).unwrap();
    worker.push_event(RegionId(3), tick(4)).unwrap();
    worker.push_event(RegionId(3), tick(5)).unwrap();
    worker.push_event(RegionId(4), tick(6)).unwrap();

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
        error.routing_error(),
        WorkerRoutingError::DuplicateRegion {
            region_id: RegionId(9),
        }
    );
    assert_eq!(error.into_runtime().region_id(), RegionId(9));
}

#[test]
fn process_region_events_with_zero_event_limit_reports_no_processed_regions() {
    let mut worker = worker_with_regions(WorkerId(6), &[RegionId(10), RegionId(11)]);
    worker.push_event(RegionId(10), tick(10)).unwrap();
    worker.push_event(RegionId(11), tick(11)).unwrap();

    let summary = worker.process_region_events(0);

    assert_eq!(summary.processed_regions, 0);
    assert!(summary.routing_errors.is_empty());
    assert_eq!(turn(&worker, RegionId(10)), 0);
    assert_eq!(turn(&worker, RegionId(11)), 0);
    assert_eq!(pending_events(&worker, RegionId(10)), 1);
    assert_eq!(pending_events(&worker, RegionId(11)), 1);
}

#[test]
fn worker_routes_export_change_to_neighbor_import_cache() {
    let source = RegionId(12);
    let target = RegionId(13);
    let mut worker = worker_with_regions(WorkerId(7), &[source, target]);

    worker
        .push_event(
            source,
            RegionEvent::RunCommand {
                request_id: UiRequestId(1),
                command: RegionCommand::Build {
                    x: 1,
                    y: 1,
                    kind: BuildingKind::Park,
                },
            },
        )
        .unwrap();

    drain_worker(&mut worker);

    let imported = worker
        .region(target)
        .expect("target")
        .state()
        .imported_resources();
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].id.origin_region, source);
    assert_eq!(imported[0].id.resource_kind, ResourceKind::ParkAccess);
    assert_eq!(imported[0].remaining_capacity, 1);
}

#[test]
fn worker_routes_export_removal_to_neighbor_import_cache() {
    let source = RegionId(14);
    let target = RegionId(15);
    let mut worker = worker_with_regions(WorkerId(8), &[source, target]);

    worker
        .push_event(
            source,
            RegionEvent::RunCommand {
                request_id: UiRequestId(1),
                command: RegionCommand::Build {
                    x: 1,
                    y: 1,
                    kind: BuildingKind::Park,
                },
            },
        )
        .unwrap();
    drain_worker(&mut worker);
    assert_eq!(
        worker
            .region(target)
            .expect("target")
            .state()
            .imported_resources()
            .len(),
        1
    );

    worker
        .push_event(
            source,
            RegionEvent::RunCommand {
                request_id: UiRequestId(2),
                command: RegionCommand::Bulldoze { x: 1, y: 1 },
            },
        )
        .unwrap();
    drain_worker(&mut worker);

    assert!(
        worker
            .region(target)
            .expect("target")
            .state()
            .imported_resources()
            .is_empty()
    );
}

#[test]
fn discovery_joins_complementary_border_road_networks() {
    let left = region_with_roads(RegionId(16), 2, 1, &[(1, 0)]);
    let right = region_with_roads(RegionId(17), 2, 1, &[(0, 0)]);
    let worker = worker_with_region_states(WorkerId(9), vec![left, right]);

    let discovery = worker.cross_region_discovery(&[neighbor(16, BorderEdge::East, 17)]);

    assert_component(
        &discovery,
        network(16, 0),
        &[network(16, 0), network(17, 0)],
    );
}

#[test]
fn discovery_does_not_join_mismatched_border_offsets() {
    let left = region_with_roads(RegionId(18), 2, 2, &[(1, 0)]);
    let right = region_with_roads(RegionId(19), 2, 2, &[(0, 1)]);
    let worker = worker_with_region_states(WorkerId(10), vec![left, right]);

    let discovery = worker.cross_region_discovery(&[neighbor(18, BorderEdge::East, 19)]);

    assert_component(&discovery, network(18, 0), &[network(18, 0)]);
    assert_component(&discovery, network(19, 0), &[network(19, 0)]);
}

#[test]
fn discovery_keeps_one_regions_disconnected_networks_in_separate_components() {
    let left = region_with_roads(RegionId(20), 2, 5, &[(1, 1)]);
    let middle = region_with_roads(RegionId(21), 3, 5, &[(0, 1), (2, 3)]);
    let right = region_with_roads(RegionId(22), 2, 5, &[(0, 3)]);
    let worker = worker_with_region_states(WorkerId(11), vec![left, middle, right]);

    let discovery = worker.cross_region_discovery(&[
        neighbor(20, BorderEdge::East, 21),
        neighbor(21, BorderEdge::East, 22),
    ]);

    assert_component(
        &discovery,
        network(20, 0),
        &[network(20, 0), network(21, 0)],
    );
    assert_component(
        &discovery,
        network(21, 1),
        &[network(21, 1), network(22, 0)],
    );
}

#[test]
fn discovery_publishes_owned_availability_hints() {
    let mut source = RegionState::new(RegionId(23), 3, 2);
    assert!(source.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(source.build(0, 1, BuildingKind::Road).success);
    let worker = worker_with_region_states(WorkerId(12), vec![source]);

    let discovery = worker.cross_region_discovery(&[]);

    assert_eq!(discovery.availability_hints.len(), 1);
    assert_eq!(discovery.availability_hints[0].network, network(23, 0));
    assert!(discovery.availability_hints[0].has_spare_power);
    assert!(!discovery.availability_hints[0].has_spare_jobs);
}

#[test]
fn discovery_does_not_join_unrelated_regions_with_matching_border_links() {
    let left = region_with_roads(RegionId(24), 2, 1, &[(1, 0)]);
    let right = region_with_roads(RegionId(25), 2, 1, &[(0, 0)]);
    let worker = worker_with_region_states(WorkerId(13), vec![left, right]);

    let discovery = worker.cross_region_discovery(&[]);

    assert_component(&discovery, network(24, 0), &[network(24, 0)]);
    assert_component(&discovery, network(25, 0), &[network(25, 0)]);
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

fn worker_with_region_states(id: WorkerId, regions: Vec<RegionState>) -> RegionWorker {
    let mut worker = RegionWorker::new(id);
    for region in regions {
        worker.add_region(RegionRuntime::new(region)).unwrap();
    }
    worker
}

fn assert_component(
    discovery: &small_city::core::regions::worker::CrossRegionDiscovery,
    member: RegionRoadNetworkId,
    expected: &[RegionRoadNetworkId],
) {
    assert_eq!(discovery.component_of(member), Some(expected));
}

fn region_with_roads(
    region_id: RegionId,
    width: usize,
    height: usize,
    roads: &[(usize, usize)],
) -> RegionState {
    let mut region = RegionState::new(region_id, width, height);
    for (x, y) in roads {
        assert!(region.build(*x, *y, BuildingKind::Road).success);
    }
    region
}

fn network(region: u32, road_network: u32) -> RegionRoadNetworkId {
    RegionRoadNetworkId {
        region: RegionId(region),
        road_network,
    }
}

fn neighbor(region: u32, edge: BorderEdge, neighbor: u32) -> RegionNeighborLink {
    RegionNeighborLink::new(RegionId(region), edge, RegionId(neighbor))
}

fn tick(request_id: u64) -> RegionEvent {
    RegionEvent::Tick {
        request_id: UiRequestId(request_id),
    }
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

fn drain_worker(worker: &mut RegionWorker) {
    for _ in 0..16 {
        if worker.process_region_events(1).processed_regions == 0 {
            return;
        }
    }

    panic!("worker did not drain");
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
