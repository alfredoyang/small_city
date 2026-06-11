//! Integration tests for the optional threaded region worker runner.

use small_city::core::regional_game::UiRequestId;
use small_city::core::regions::runtime::{RegionEvent, RegionRuntime};
use small_city::core::regions::threaded::{ThreadedRegionWorker, ThreadedWorkerShutdown};
use small_city::core::regions::worker::{RegionWorker, WorkerId};
use small_city::core::regions::{RegionId, RegionState};

#[test]
fn threaded_worker_processes_tick_request_and_returns_summary() {
    let region_id = RegionId(1);
    let worker = worker_with_regions(WorkerId(1), &[region_id]);
    let handle = worker.handle_for(region_id).expect("region handle");
    let threaded = ThreadedRegionWorker::start(worker);

    handle.send(tick(1));
    let summary = threaded.process_region_events(1).unwrap();
    let shutdown = threaded
        .shutdown(ThreadedWorkerShutdown::RejectPending)
        .unwrap();

    assert_eq!(summary.processed_regions, 1);
    assert!(summary.routing_errors.is_empty());
    assert_eq!(summary.tick_replies.len(), 1);
    assert_eq!(turn(&shutdown.worker, region_id), 1);
}

#[test]
fn region_handle_can_deliver_event_to_worker_thread() {
    let region_a = RegionId(2);
    let region_b = RegionId(3);
    let worker_a = worker_with_regions(WorkerId(2), &[region_a]);
    let worker_b = worker_with_regions(WorkerId(3), &[region_b]);
    let region_b_handle = worker_b.handle_for(region_b).expect("region B handle");
    let threaded_a = ThreadedRegionWorker::start(worker_a);
    let threaded_b = ThreadedRegionWorker::start(worker_b);

    region_b_handle.send(tick(2));
    let summary_b = threaded_b.process_region_events(1).unwrap();
    let shutdown_a = threaded_a
        .shutdown(ThreadedWorkerShutdown::RejectPending)
        .unwrap();
    let shutdown_b = threaded_b
        .shutdown(ThreadedWorkerShutdown::RejectPending)
        .unwrap();

    assert_eq!(summary_b.processed_regions, 1);
    assert!(summary_b.routing_errors.is_empty());
    assert_eq!(turn(&shutdown_a.worker, region_a), 0);
    assert_eq!(turn(&shutdown_b.worker, region_b), 1);
}

#[test]
fn shutdown_can_reject_pending_work_deterministically() {
    let region_id = RegionId(4);
    let worker = worker_with_regions(WorkerId(4), &[region_id]);
    let handle = worker.handle_for(region_id).expect("region handle");
    let threaded = ThreadedRegionWorker::start(worker);

    handle.send(tick(3));
    let shutdown = threaded
        .shutdown(ThreadedWorkerShutdown::RejectPending)
        .unwrap();

    assert_eq!(shutdown.final_pass.processed_regions, 0);
    assert!(shutdown.final_pass.routing_errors.is_empty());
    assert_eq!(pending_events(&shutdown.worker, region_id), 1);
    assert_eq!(turn(&shutdown.worker, region_id), 0);
}

#[test]
fn shutdown_can_drain_one_bounded_pass_deterministically() {
    let region_id = RegionId(5);
    let worker = worker_with_regions(WorkerId(5), &[region_id]);
    let handle = worker.handle_for(region_id).expect("region handle");
    let threaded = ThreadedRegionWorker::start(worker);

    handle.send(tick(4));
    handle.send(tick(5));
    let shutdown = threaded
        .shutdown(ThreadedWorkerShutdown::DrainOnce {
            max_events_per_region: 1,
        })
        .unwrap();

    assert_eq!(shutdown.final_pass.processed_regions, 1);
    assert!(shutdown.final_pass.routing_errors.is_empty());
    assert_eq!(pending_events(&shutdown.worker, region_id), 1);
    assert_eq!(turn(&shutdown.worker, region_id), 1);
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

fn pending_events(worker: &RegionWorker, region_id: RegionId) -> usize {
    worker
        .region(region_id)
        .expect("region")
        .pending_event_count()
}
