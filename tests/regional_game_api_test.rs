//! Integration tests for the UI-facing regional game facade.

use small_city::core::regional_game::{
    RegionViewSnapshot, RegionalGame, RegionalGameError, UiReply, UiRequest, UiRequestId,
};
use small_city::core::regions::{
    ImportDecision, ImportedResource, RegionId, RegionState, ResourceId, ResourceKind,
};
use small_city::interface::input::BuildingKind;
use small_city::interface::view::GameView;

#[test]
fn single_region_constructor_keeps_region_state_construction_inside_facade() {
    let game = RegionalGame::single_region(3, 2).unwrap();

    let view = game.selected_region_view().unwrap();

    assert_eq!(view.map.width, 3);
    assert_eq!(view.map.height, 2);
}

#[test]
fn regional_facade_exposes_owned_view_data_without_world() {
    let game = RegionalGame::from_regions(vec![
        RegionState::new(RegionId(1), 2, 2),
        RegionState::new(RegionId(2), 3, 2),
    ])
    .unwrap();

    let view = game.view().unwrap();

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
    let game = RegionalGame::from_regions(vec![
        RegionState::new(RegionId(3), 2, 2),
        RegionState::new(RegionId(4), 2, 2),
    ])
    .unwrap();

    game.tick_all_regions().unwrap();
    game.tick_region(RegionId(3)).unwrap();

    let view = game.view().unwrap();

    assert_eq!(turn(&view.regions, RegionId(3)), 2);
    assert_eq!(turn(&view.regions, RegionId(4)), 1);
}

#[test]
fn ui_snapshot_request_reaches_requested_region() {
    let game = RegionalGame::from_regions(vec![RegionState::new(RegionId(5), 4, 3)]).unwrap();

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
    let game = RegionalGame::from_regions(vec![RegionState::new(RegionId(6), 2, 2)]).unwrap();

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
fn regional_facade_rejects_duplicate_regions() {
    let error = RegionalGame::from_regions(vec![
        RegionState::new(RegionId(7), 2, 2),
        RegionState::new(RegionId(7), 3, 3),
    ])
    .expect_err("duplicate region should fail deterministically");

    assert_eq!(
        error,
        RegionalGameError::DuplicateRegion {
            region_id: RegionId(7),
        }
    );
}

#[test]
fn inspect_region_returns_ui_safe_inspect_model() {
    let game = RegionalGame::from_regions(vec![RegionState::new(RegionId(8), 3, 2)]).unwrap();

    let inspect = game.inspect_region(RegionId(8), 1, 1).unwrap();

    assert!(inspect.in_bounds);
    assert!(inspect.cell.is_some());
}

#[test]
fn selected_region_commands_target_first_region_without_exposing_region_id_to_ui() {
    let game = RegionalGame::from_regions(vec![
        RegionState::new(RegionId(12), 3, 3),
        RegionState::new(RegionId(13), 3, 3),
    ])
    .unwrap();

    assert!(
        game.build_selected_region(1, 1, BuildingKind::Residential)
            .unwrap()
            .success
    );
    let selected_view = game.selected_region_view().unwrap();
    let all_regions = game.view().unwrap();

    assert_eq!(
        cell_building(&selected_view, 1, 1),
        Some(BuildingKind::Residential)
    );
    assert_eq!(
        region_view(&all_regions.regions, RegionId(12)).map.cells[4].building,
        Some(BuildingKind::Residential)
    );
    assert_eq!(
        region_view(&all_regions.regions, RegionId(13)).map.cells[4].building,
        None
    );
}

#[test]
fn imported_resource_cache_can_be_rebuilt_from_authoritative_region_state() {
    let mut region = RegionState::new(RegionId(9), 2, 2);
    let result = region.process_imported_resource(
        resource(50, ResourceKind::Jobs, 1),
        0,
        1,
        &[RegionId(10)],
    );

    assert_eq!(result.decision, ImportDecision::Accepted);
    assert_eq!(region.imported_resources().len(), 1);

    region.rebuild_imported_resource_cache();

    assert_eq!(region.imported_resources().len(), 0);
}

#[test]
fn regional_game_uses_threaded_runner_and_processes_events() {
    let region_id = RegionId(11);
    let game = RegionalGame::from_regions(vec![RegionState::new(region_id, 2, 2)]).unwrap();

    game.tick_region(region_id).unwrap();
    let UiReply::RegionSnapshotReady {
        request_id,
        region_id: snapshot_region_id,
        snapshot,
    } = game
        .handle_ui_request(UiRequest::GetRegionSnapshot {
            request_id: UiRequestId(77),
            region_id,
        })
        .unwrap();

    assert_eq!(request_id, UiRequestId(77));
    assert_eq!(snapshot_region_id, region_id);
    assert_eq!(snapshot.region_id, region_id);
    assert_eq!(snapshot.view.status.turn, 1);
}

fn assert_snapshot_is_owned(snapshot: RegionViewSnapshot) -> GameView {
    snapshot.view
}

fn region_view(
    regions: &[small_city::core::regional_game::RegionViewSnapshot],
    region_id: RegionId,
) -> &GameView {
    &regions
        .iter()
        .find(|snapshot| snapshot.region_id == region_id)
        .expect("region snapshot")
        .view
}

fn cell_building(view: &GameView, x: usize, y: usize) -> Option<BuildingKind> {
    view.map
        .cells
        .iter()
        .find(|cell| cell.x == x && cell.y == y)
        .and_then(|cell| cell.building)
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
