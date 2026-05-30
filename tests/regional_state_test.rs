//! Integration tests for the region-owned state wrapper.

use small_city::core::game::Game;
use small_city::core::regions::{
    ImportDecision, ImportedResource, ImportedResourceResult, RegionId, RegionState, ResourceId,
    ResourceKind,
};

#[test]
fn region_tick_local_matches_game_tick_for_same_empty_city() {
    let mut game = Game::new(4, 3);
    let mut region = RegionState::new(RegionId(10), 4, 3);

    let game_result = game.tick();
    let region_result = region.tick_local();

    assert_eq!(region_result, game_result);
    assert_eq!(region.view(), game.view());
}

#[test]
fn imported_resource_processing_stays_in_target_region_cache() {
    let mut caller = RegionState::new(RegionId(1), 2, 2);
    let mut target = RegionState::new(RegionId(2), 2, 2);
    let imported_resource = resource(9, ResourceKind::Jobs, 3, 8, 0, 3, 1, 1);

    let result =
        target.process_imported_resource(imported_resource, 2, 4, &[RegionId(1), RegionId(3)]);

    assert_eq!(result.decision, ImportDecision::Accepted);
    assert_eq!(target.imported_resources(), &[imported_resource]);
    assert!(caller.imported_resources().is_empty());
    assert_eq!(
        result.forwarded_resources,
        vec![ImportedResource {
            remaining_capacity: 6,
            hop_count: 1,
            travel_cost: 5,
            source_neighbor: RegionId(2),
            ..imported_resource
        }]
    );

    caller.apply_neighbor_import_result(result.clone());

    assert_eq!(caller.neighbor_import_results(), &[result]);
    assert_eq!(target.imported_resources(), &[imported_resource]);
}

#[test]
fn region_view_and_inspect_return_ui_safe_models() {
    let region = RegionState::new(RegionId(4), 3, 2);
    let view = region.view();
    let inspect = region.inspect(1, 1);

    assert_eq!(view.map.width, 3);
    assert_eq!(view.map.height, 2);
    assert_eq!(view.status.turn, 0);
    assert!(inspect.in_bounds);
    assert!(inspect.cell.is_some());
}

#[test]
fn applying_neighbor_import_result_records_owned_reply_only() {
    let mut region = RegionState::new(RegionId(3), 2, 2);
    let result = ImportedResourceResult {
        decision: ImportDecision::RejectedDuplicate,
        forwarded_resources: Vec::new(),
    };

    region.apply_neighbor_import_result(result.clone());

    assert_eq!(region.neighbor_import_results(), &[result]);
    assert!(region.imported_resources().is_empty());
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
