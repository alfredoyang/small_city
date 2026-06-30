//! Integration tests for moving region runtimes between workers at safe points.

use small_city::core::regional_game::UiRequestId;
use small_city::core::regions::directory::RegionDirectory;
use small_city::core::regions::runtime::{RegionEvent, RegionRuntime};
use small_city::core::regions::worker::{RegionOwnerDirectory, RegionWorker, WorkerId};
use small_city::core::regions::{RegionId, RegionState};
use std::sync::Arc;

#[test]
fn runtime_can_be_removed_from_one_worker_and_added_to_another() {
    let region_id = RegionId(1);
    let mut source_worker = worker_with_regions(WorkerId(1), &[region_id]);
    let mut target_worker = test_worker(WorkerId(2));

    let runtime = source_worker
        .remove_region(region_id)
        .expect("runtime should be removable");
    target_worker.add_region(runtime).unwrap();

    assert!(source_worker.region(region_id).is_none());
    assert!(target_worker.region(region_id).is_some());
}

#[test]
fn only_new_owner_processes_moved_runtime_events() {
    let region_id = RegionId(2);
    let mut source_worker = worker_with_regions(WorkerId(3), &[region_id]);
    let mut target_worker = test_worker(WorkerId(4));

    source_worker
        .push_event(region_id, tick(1))
        .expect("source owns region before move");
    let runtime = source_worker
        .remove_region(region_id)
        .expect("runtime should be removable");
    target_worker.add_region(runtime).unwrap();

    let source_summary = source_worker.process_region_events(1);
    let target_summary = target_worker.process_region_events(1);

    assert_eq!(source_summary.processed_regions, 0);
    assert!(source_summary.routing_errors.is_empty());
    assert_eq!(target_summary.processed_regions, 1);
    assert!(target_summary.routing_errors.is_empty());
    assert_eq!(turn(&target_worker, region_id), 1);
}

#[test]
fn existing_send_handle_still_delivers_to_moved_runtime() {
    let region_id = RegionId(3);
    let mut source_worker = worker_with_regions(WorkerId(5), &[region_id]);
    let mut target_worker = test_worker(WorkerId(6));
    let handle = source_worker
        .handle_for(region_id)
        .expect("handle should exist before move");

    let runtime = source_worker
        .remove_region(region_id)
        .expect("runtime should be removable");
    target_worker.add_region(runtime).unwrap();

    handle.send(tick(2));
    let summary = target_worker.process_region_events(1);

    assert!(source_worker.region(region_id).is_none());
    assert_eq!(summary.processed_regions, 1);
    assert!(summary.routing_errors.is_empty());
    assert_eq!(turn(&target_worker, region_id), 1);
}

#[test]
fn failed_reassignment_returns_runtime_with_queued_events() {
    let region_id = RegionId(4);
    let mut source_worker = worker_with_regions(WorkerId(7), &[region_id]);
    let mut target_worker = worker_with_regions(WorkerId(8), &[region_id]);

    source_worker
        .push_event(region_id, tick(3))
        .expect("source owns region before move");
    let moved_runtime = source_worker
        .remove_region(region_id)
        .expect("runtime should be removable");

    let add_error = target_worker
        .add_region(moved_runtime)
        .expect_err("target already owns this region id");
    let recovered_runtime = add_error.into_runtime();
    source_worker.add_region(recovered_runtime).unwrap();

    let summary = source_worker.process_region_events(1);

    assert_eq!(summary.processed_regions, 1);
    assert!(summary.routing_errors.is_empty());
    assert_eq!(turn(&source_worker, region_id), 1);
    assert_eq!(turn(&target_worker, region_id), 0);
}

fn worker_with_regions(id: WorkerId, regions: &[RegionId]) -> RegionWorker {
    let mut worker = test_worker(id);
    for region_id in regions {
        worker
            .add_region(RegionRuntime::new(RegionState::new(*region_id, 2, 2)))
            .unwrap();
    }
    worker
}

fn test_worker(id: WorkerId) -> RegionWorker {
    let owners = Arc::new(RegionOwnerDirectory::new());
    let directory = Arc::new(RegionDirectory::with_owners(
        Vec::new(),
        Arc::clone(&owners),
    ));
    RegionWorker::with_directory_and_owners(id, directory, owners)
}

fn tick(request_id: u64) -> RegionEvent {
    RegionEvent::Tick {
        request_id: UiRequestId(request_id),
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
