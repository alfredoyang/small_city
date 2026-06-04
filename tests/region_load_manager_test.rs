//! Integration tests for deterministic regional worker load decisions.

use small_city::core::regional_game::UiRequestId;
use small_city::core::regions::load_manager::{LoadManager, RegionMove, WorkerLoad};
use small_city::core::regions::runtime::{RegionEvent, RegionRuntime};
use small_city::core::regions::worker::{RegionWorker, WorkerId};
use small_city::core::regions::{RegionId, RegionState};

#[test]
fn no_move_is_chosen_below_threshold() {
    let manager = LoadManager::new(4);
    let loads = vec![
        load(WorkerId(1), &[RegionId(10), RegionId(11)], 2),
        load(WorkerId(2), &[RegionId(20)], 0),
    ];

    assert_eq!(manager.choose_move(&loads), None);
}

#[test]
fn no_move_is_chosen_with_one_worker() {
    let manager = LoadManager::new(1);
    let loads = vec![load(WorkerId(1), &[RegionId(10), RegionId(11)], 5)];

    assert_eq!(manager.choose_move(&loads), None);
}

#[test]
fn no_move_is_chosen_when_all_workers_have_one_region() {
    let manager = LoadManager::new(1);
    let loads = vec![
        load(WorkerId(1), &[RegionId(10)], 10),
        load(WorkerId(2), &[RegionId(20)], 0),
    ];

    assert_eq!(manager.choose_move(&loads), None);
}

#[test]
fn busiest_worker_moves_one_region_to_quietest_worker_above_threshold() {
    let manager = LoadManager::new(3);
    let loads = vec![
        load(WorkerId(1), &[RegionId(5), RegionId(3), RegionId(4)], 6),
        load(WorkerId(2), &[RegionId(20)], 0),
        load(WorkerId(3), &[RegionId(30), RegionId(31)], 2),
    ];

    assert_eq!(
        manager.choose_move(&loads),
        Some(RegionMove {
            region_id: RegionId(3),
            from_worker: WorkerId(1),
            to_worker: WorkerId(2),
        })
    );
}

#[test]
fn tie_handling_is_deterministic() {
    let manager = LoadManager::new(1);
    let loads = vec![
        load(WorkerId(3), &[RegionId(30), RegionId(31)], 4),
        load(WorkerId(1), &[RegionId(10), RegionId(11)], 4),
        load(WorkerId(2), &[RegionId(20)], 0),
        load(WorkerId(4), &[RegionId(40)], 0),
    ];

    assert_eq!(
        manager.choose_move(&loads),
        Some(RegionMove {
            region_id: RegionId(10),
            from_worker: WorkerId(1),
            to_worker: WorkerId(2),
        })
    );
}

#[test]
fn worker_load_counts_regions_and_queued_events_without_processing_state() {
    let mut worker = RegionWorker::new(WorkerId(7));
    worker
        .add_region(RegionRuntime::new(RegionState::new(RegionId(70), 2, 2)))
        .unwrap();
    worker
        .add_region(RegionRuntime::new(RegionState::new(RegionId(71), 2, 2)))
        .unwrap();

    worker.push_event(RegionId(70), tick(70)).unwrap();
    worker.push_event(RegionId(71), tick(71)).unwrap();
    worker.push_event(RegionId(71), tick(72)).unwrap();

    assert_eq!(
        worker.load(),
        WorkerLoad::new(WorkerId(7), vec![RegionId(70), RegionId(71)], 3)
    );
}

#[test]
fn optional_frame_time_can_drive_load_decision() {
    let manager = LoadManager::new(5);
    let loads = vec![
        load(WorkerId(1), &[RegionId(10), RegionId(11)], 0).with_frame_time_micros(9_000),
        load(WorkerId(2), &[RegionId(20)], 0),
    ];

    assert_eq!(
        manager.choose_move(&loads),
        Some(RegionMove {
            region_id: RegionId(10),
            from_worker: WorkerId(1),
            to_worker: WorkerId(2),
        })
    );
}

fn load(worker_id: WorkerId, region_ids: &[RegionId], queued_events: usize) -> WorkerLoad {
    WorkerLoad::new(worker_id, region_ids.to_vec(), queued_events)
}

fn tick(request_id: u64) -> RegionEvent {
    RegionEvent::Tick {
        request_id: UiRequestId(request_id),
    }
}
