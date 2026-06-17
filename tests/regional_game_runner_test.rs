//! Integration tests for the threaded regional game runner boundary.

use small_city::core::regional_game::{RegionCommand, UiReply, UiRequestId};
use small_city::core::regional_game_runner::{RegionalGameRunner, RegionalGameRunnerError};
use small_city::core::regions::{BorderEdge, RegionId, RegionNeighborLink, RegionState};
use small_city::interface::input::BuildingKind;

#[test]
fn runner_starts_one_threaded_worker_and_processes_regional_tick() {
    let runner = RegionalGameRunner::start(vec![RegionState::new(RegionId(1), 2, 2)]).unwrap();

    let tick_result = runner.tick_region(UiRequestId(9), RegionId(1)).unwrap();
    let reply = runner
        .request_region_snapshot(UiRequestId(10), RegionId(1))
        .unwrap();

    let UiReply::RegionSnapshotReady {
        request_id,
        region_id,
        snapshot,
    } = reply;

    assert!(tick_result.success);
    assert_eq!(tick_result.events.len(), 1);
    assert_eq!(request_id, UiRequestId(10));
    assert_eq!(region_id, RegionId(1));
    assert_eq!(snapshot.view.status.turn, 1);
    runner.shutdown().unwrap();
}

#[test]
fn runner_rejects_invalid_worker_count() {
    let error =
        RegionalGameRunner::start_with_worker_count(vec![RegionState::new(RegionId(10), 2, 2)], 0)
            .expect_err("zero workers cannot own regions");

    assert_eq!(
        error,
        RegionalGameRunnerError::InvalidWorkerCount { worker_count: 0 }
    );
}

#[test]
fn runner_can_start_two_workers_and_recover_each_region() {
    let runner = RegionalGameRunner::start_with_worker_count(
        vec![
            RegionState::new(RegionId(11), 2, 2),
            RegionState::new(RegionId(12), 3, 2),
            RegionState::new(RegionId(13), 4, 2),
        ],
        2,
    )
    .unwrap();

    runner.tick_region(UiRequestId(11), RegionId(11)).unwrap();
    runner.tick_region(UiRequestId(12), RegionId(12)).unwrap();
    runner.tick_region(UiRequestId(13), RegionId(13)).unwrap();

    let mut recovered = runner.shutdown().unwrap();
    assert_eq!(
        recovered
            .region_snapshot(RegionId(11))
            .unwrap()
            .view
            .status
            .turn,
        1
    );
    assert_eq!(
        recovered
            .region_snapshot(RegionId(12))
            .unwrap()
            .view
            .status
            .turn,
        1
    );
    assert_eq!(
        recovered
            .region_snapshot(RegionId(13))
            .unwrap()
            .view
            .status
            .turn,
        1
    );
}

#[test]
fn two_worker_runner_routes_commands_by_region_owner() {
    let runner = RegionalGameRunner::start_with_worker_count(
        vec![
            RegionState::new(RegionId(31), 3, 3),
            RegionState::new(RegionId(32), 3, 3),
        ],
        2,
    )
    .unwrap();

    runner
        .run_region_command(
            UiRequestId(31),
            RegionId(31),
            RegionCommand::Build {
                x: 1,
                y: 1,
                kind: BuildingKind::Road,
            },
        )
        .unwrap();
    runner
        .run_region_command(
            UiRequestId(32),
            RegionId(32),
            RegionCommand::Build {
                x: 2,
                y: 1,
                kind: BuildingKind::Road,
            },
        )
        .unwrap();

    let left = runner
        .request_region_snapshot(UiRequestId(33), RegionId(31))
        .unwrap();
    let right = runner
        .request_region_snapshot(UiRequestId(34), RegionId(32))
        .unwrap();
    let UiReply::RegionSnapshotReady { snapshot: left, .. } = left;
    let UiReply::RegionSnapshotReady {
        snapshot: right, ..
    } = right;

    assert_eq!(left.view.map.cells[4].building, Some(BuildingKind::Road));
    assert_eq!(right.view.map.cells[5].building, Some(BuildingKind::Road));
    runner.shutdown().unwrap();
}

#[test]
fn two_worker_runner_batches_ticks_and_returns_one_result_per_region() {
    let runner = RegionalGameRunner::start_with_worker_count(
        vec![
            RegionState::new(RegionId(41), 2, 2),
            RegionState::new(RegionId(42), 2, 2),
        ],
        2,
    )
    .unwrap();

    let results = runner
        .tick_regions(&[
            (UiRequestId(41), RegionId(41)),
            (UiRequestId(42), RegionId(42)),
        ])
        .unwrap();

    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| result.success));
    let left = runner
        .request_region_snapshot(UiRequestId(43), RegionId(41))
        .unwrap();
    let right = runner
        .request_region_snapshot(UiRequestId(44), RegionId(42))
        .unwrap();
    let UiReply::RegionSnapshotReady { snapshot: left, .. } = left;
    let UiReply::RegionSnapshotReady {
        snapshot: right, ..
    } = right;

    assert_eq!(left.view.status.turn, 1);
    assert_eq!(right.view.status.turn, 1);
    runner.shutdown().unwrap();
}

#[test]
fn tick_batch_validates_all_regions_before_enqueueing_any_tick() {
    let runner =
        RegionalGameRunner::start_with_worker_count(vec![RegionState::new(RegionId(51), 2, 2)], 1)
            .unwrap();

    let error = runner
        .tick_regions(&[
            (UiRequestId(51), RegionId(51)),
            (UiRequestId(52), RegionId(99)),
        ])
        .expect_err("unknown region should reject the whole batch");
    assert_eq!(
        error,
        RegionalGameRunnerError::UnknownRegion {
            region_id: RegionId(99)
        }
    );

    let reply = runner
        .request_region_snapshot(UiRequestId(53), RegionId(51))
        .unwrap();
    let UiReply::RegionSnapshotReady { snapshot, .. } = reply;

    assert_eq!(snapshot.view.status.turn, 0);
    runner.shutdown().unwrap();
}

#[test]
fn two_worker_runner_rejects_topology_until_cross_worker_import_routing_exists() {
    let error = RegionalGameRunner::start_with_topology_and_worker_count(
        vec![
            RegionState::new(RegionId(21), 2, 2),
            RegionState::new(RegionId(22), 2, 2),
        ],
        vec![RegionNeighborLink::new(
            RegionId(21),
            BorderEdge::East,
            RegionId(22),
        )],
        2,
    )
    .expect_err("MW2 should not drop cross-worker import events");

    assert_eq!(
        error,
        RegionalGameRunnerError::CrossWorkerTopologyUnsupported { worker_count: 2 }
    );
}

#[test]
fn runner_returns_owned_snapshot_for_requested_region() {
    let runner = RegionalGameRunner::start(vec![
        RegionState::new(RegionId(2), 2, 2),
        RegionState::new(RegionId(3), 4, 3),
    ])
    .unwrap();

    let reply = runner
        .request_region_snapshot(UiRequestId(20), RegionId(3))
        .unwrap();

    let UiReply::RegionSnapshotReady { snapshot, .. } = reply;

    assert_eq!(snapshot.region_id, RegionId(3));
    assert_eq!(snapshot.view.map.width, 4);
    assert_eq!(snapshot.view.map.height, 3);
    assert_eq!(snapshot.revision, 0);
    runner.shutdown().unwrap();
}

#[test]
fn runner_shutdown_recovers_authoritative_region_state() {
    let runner = RegionalGameRunner::start(vec![RegionState::new(RegionId(4), 2, 2)]).unwrap();

    runner.tick_region(UiRequestId(21), RegionId(4)).unwrap();
    let mut recovered = runner.shutdown().unwrap();
    let snapshot = recovered.region_snapshot(RegionId(4)).unwrap();

    assert_eq!(snapshot.region_id, RegionId(4));
    assert_eq!(snapshot.view.status.turn, 1);
}

#[test]
fn unknown_region_requests_return_deterministic_errors() {
    let runner = RegionalGameRunner::start(vec![RegionState::new(RegionId(5), 2, 2)]).unwrap();

    let tick_error = runner
        .tick_region(UiRequestId(31), RegionId(99))
        .expect_err("unknown tick region should fail");
    let snapshot_error = runner
        .request_region_snapshot(UiRequestId(30), RegionId(99))
        .expect_err("unknown snapshot region should fail");

    assert_eq!(
        tick_error,
        RegionalGameRunnerError::UnknownRegion {
            region_id: RegionId(99),
        }
    );
    assert_eq!(
        snapshot_error,
        RegionalGameRunnerError::UnknownRegion {
            region_id: RegionId(99),
        }
    );
    runner.shutdown().unwrap();
}

#[test]
fn duplicate_regions_are_rejected_before_thread_start() {
    let error = RegionalGameRunner::start(vec![
        RegionState::new(RegionId(6), 2, 2),
        RegionState::new(RegionId(6), 3, 3),
    ])
    .expect_err("duplicate region should fail");

    assert_eq!(
        error,
        RegionalGameRunnerError::DuplicateRegion {
            region_id: RegionId(6),
        }
    );
}

#[test]
fn ui_facing_code_can_use_runner_without_worker_or_runtime_types() {
    fn request_turn_snapshot(
        runner: &RegionalGameRunner,
        region_id: RegionId,
    ) -> Result<u32, RegionalGameRunnerError> {
        let reply = runner.request_region_snapshot(UiRequestId(40), region_id)?;
        let UiReply::RegionSnapshotReady { snapshot, .. } = reply;

        Ok(snapshot.view.status.turn)
    }

    let runner = RegionalGameRunner::start(vec![RegionState::new(RegionId(7), 2, 2)]).unwrap();

    runner.tick_region(UiRequestId(41), RegionId(7)).unwrap();

    assert_eq!(request_turn_snapshot(&runner, RegionId(7)).unwrap(), 1);
    runner.shutdown().unwrap();
}
