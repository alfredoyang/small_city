//! Integration tests for the UI-facing regional game facade.

use std::sync::mpsc;
use std::thread;

use small_city::core::regional_game::{
    RegionViewSnapshot, RegionalGame, RegionalGameError, UiReply, UiRequest, UiRequestId,
};
use small_city::core::regions::{
    ImportDecision, ImportedResource, RegionId, RegionState, ResourceId, ResourceKind,
};
use small_city::interface::view::GameView;

#[test]
fn regional_facade_exposes_owned_view_data_without_world() {
    let game = RegionalGame::from_regions(vec![
        RegionState::new(RegionId(1), 2, 2),
        RegionState::new(RegionId(2), 3, 2),
    ])
    .unwrap();

    let view = game.view();

    assert_eq!(view.selected_region, Some(RegionId(1)));
    assert_eq!(view.regions.len(), 2);
    assert_eq!(view.regions[0].region_id, RegionId(1));
    assert_eq!(view.regions[0].view.map.width, 2);
    assert_eq!(view.regions[1].region_id, RegionId(2));
    assert_eq!(view.regions[1].view.map.width, 3);
    assert_snapshot_is_owned(view.regions[0].clone());
}

#[test]
fn regional_tick_advances_each_region_through_runtime() {
    let mut game = RegionalGame::from_regions(vec![
        RegionState::new(RegionId(3), 2, 2),
        RegionState::new(RegionId(4), 2, 2),
    ])
    .unwrap();

    game.tick_all_regions();
    game.tick_region(RegionId(3)).unwrap();

    let view = game.view();

    assert_eq!(turn(&view.regions, RegionId(3)), 2);
    assert_eq!(turn(&view.regions, RegionId(4)), 1);
}

#[test]
fn ui_snapshot_request_reaches_requested_region() {
    let mut game = RegionalGame::from_regions(vec![RegionState::new(RegionId(5), 4, 3)]).unwrap();

    game.tick_region(RegionId(5)).unwrap();
    let reply = game
        .handle_ui_request(UiRequest::GetRegionSnapshot {
            request_id: UiRequestId(42),
            region_id: RegionId(5),
        })
        .unwrap();

    let UiReply::RegionSnapshotReady {
        request_id,
        region_id,
        snapshot,
    } = reply;

    assert_eq!(request_id, UiRequestId(42));
    assert_eq!(region_id, RegionId(5));
    assert_eq!(snapshot.region_id, RegionId(5));
    assert_eq!(snapshot.revision, 1);
    assert_eq!(snapshot.view.map.width, 4);
    assert_eq!(snapshot.view.status.turn, 1);
}

#[test]
fn snapshot_request_for_unknown_region_returns_error() {
    let mut game = RegionalGame::from_regions(vec![RegionState::new(RegionId(6), 2, 2)]).unwrap();

    let error = game
        .handle_ui_request(UiRequest::GetRegionSnapshot {
            request_id: UiRequestId(7),
            region_id: RegionId(99),
        })
        .expect_err("unknown region should fail deterministically");

    assert_eq!(
        error,
        RegionalGameError::UnknownRegion {
            region_id: RegionId(99),
        }
    );
}

#[test]
fn inspect_region_returns_ui_safe_inspect_model() {
    let game = RegionalGame::from_regions(vec![RegionState::new(RegionId(7), 3, 2)]).unwrap();

    let inspect = game.inspect_region(RegionId(7), 1, 1).unwrap();

    assert!(inspect.in_bounds);
    assert!(inspect.cell.is_some());
}

#[test]
fn imported_resource_cache_can_be_rebuilt_from_authoritative_region_state() {
    let mut region = RegionState::new(RegionId(8), 2, 2);
    let result =
        region.process_imported_resource(resource(50, ResourceKind::Jobs, 1), 0, 1, &[RegionId(9)]);

    assert_eq!(result.decision, ImportDecision::Accepted);
    assert_eq!(region.imported_resources().len(), 1);

    region.rebuild_imported_resource_cache();

    assert_eq!(region.imported_resources().len(), 0);
}

#[test]
fn regional_game_can_run_on_worker_thread_and_process_events() {
    let region_id = RegionId(9);
    let (commands, command_receiver) = mpsc::channel();
    let worker_thread = thread::spawn(move || {
        let mut game = RegionalGame::from_regions(vec![RegionState::new(region_id, 2, 2)]).unwrap();

        while let Ok(command) = command_receiver.recv() {
            match command {
                RegionalGameThreadCommand::TickRegion(region_id) => {
                    game.tick_region(region_id).unwrap();
                }
                RegionalGameThreadCommand::Snapshot {
                    request_id,
                    region_id,
                    reply,
                } => {
                    let snapshot = game
                        .handle_ui_request(UiRequest::GetRegionSnapshot {
                            request_id,
                            region_id,
                        })
                        .unwrap();
                    reply.send(snapshot).unwrap();
                }
                RegionalGameThreadCommand::Stop => break,
            }
        }
    });

    commands
        .send(RegionalGameThreadCommand::TickRegion(region_id))
        .unwrap();
    let (reply, snapshot_receiver) = mpsc::channel();
    commands
        .send(RegionalGameThreadCommand::Snapshot {
            request_id: UiRequestId(77),
            region_id,
            reply,
        })
        .unwrap();
    let UiReply::RegionSnapshotReady {
        request_id,
        region_id: snapshot_region_id,
        snapshot,
    } = snapshot_receiver.recv().unwrap();

    commands.send(RegionalGameThreadCommand::Stop).unwrap();
    worker_thread.join().unwrap();

    assert_eq!(request_id, UiRequestId(77));
    assert_eq!(snapshot_region_id, region_id);
    assert_eq!(snapshot.region_id, region_id);
    assert_eq!(snapshot.view.status.turn, 1);
}

enum RegionalGameThreadCommand {
    TickRegion(RegionId),
    Snapshot {
        request_id: UiRequestId,
        region_id: RegionId,
        reply: mpsc::Sender<UiReply>,
    },
    Stop,
}

fn assert_snapshot_is_owned(snapshot: RegionViewSnapshot) -> GameView {
    snapshot.view
}

fn turn(snapshots: &[RegionViewSnapshot], region_id: RegionId) -> u32 {
    snapshots
        .iter()
        .find(|snapshot| snapshot.region_id == region_id)
        .expect("region snapshot")
        .view
        .status
        .turn
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
