//! Integration tests for the region-owned state wrapper.

mod common;

use common::SingleRegionTestGame;
use small_city::core::regions::{
    BorderEdge, BorderLinkId, NetworkBorderLink, RegionId, RegionRoadNetworkId, RegionState,
    RegionalSpareCapacity,
};
use small_city::interface::input::BuildingKind;

#[test]
fn region_tick_local_matches_game_tick_for_same_empty_city() {
    let mut game = SingleRegionTestGame::new(4, 3);
    let mut region = RegionState::new(RegionId(10), 4, 3);

    let game_result = game.tick();
    let region_result = region.tick_local();

    assert_eq!(region_result, game_result);
    assert_eq!(region.view(), game.view());
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
fn regional_spare_capacity_matches_local_registry_remaining_capacity() {
    let mut region = RegionState::new(RegionId(5), 5, 3);
    assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(region.build(0, 1, BuildingKind::Road).success);
    assert!(region.build(1, 1, BuildingKind::Road).success);
    assert!(region.build(2, 1, BuildingKind::Road).success);
    assert!(region.build(1, 0, BuildingKind::Commercial).success);
    assert!(region.build(2, 0, BuildingKind::Industrial).success);

    assert_eq!(
        region.regional_spare_capacity(),
        RegionalSpareCapacity {
            power_capacity: 5,
            job_slots: 5,
        }
    );
}

#[test]
fn regional_spare_capacity_keeps_unreachable_jobs_spare() {
    let mut region = RegionState::new(RegionId(7), 6, 3);
    assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(region.build(0, 1, BuildingKind::Road).success);
    assert!(region.build(1, 1, BuildingKind::Road).success);
    assert!(region.build(1, 0, BuildingKind::Residential).success);

    assert!(region.build(5, 0, BuildingKind::PowerPlant).success);
    assert!(region.build(5, 1, BuildingKind::Road).success);
    assert!(region.build(4, 1, BuildingKind::Road).success);
    assert!(region.build(4, 0, BuildingKind::Commercial).success);

    for _ in 0..24 {
        assert!(region.tick_local().success);
    }

    assert_eq!(region.view().status.population, 1);
    assert_eq!(region.regional_spare_capacity().job_slots, 2);
}

#[test]
fn regional_spare_capacity_is_owned_summary_without_ecs_identity() {
    let region = RegionState::new(RegionId(6), 3, 3);
    let summary = region.regional_spare_capacity();

    let copied = summary;
    assert_eq!(summary, copied);
    assert_eq!(summary.power_capacity, 0);
    assert_eq!(summary.job_slots, 0);
}

#[test]
fn edge_road_cells_report_network_border_links() {
    let mut region = RegionState::new(RegionId(8), 4, 3);
    assert!(region.build(0, 0, BuildingKind::Road).success);
    assert!(region.build(1, 1, BuildingKind::Road).success);
    assert!(region.build(3, 2, BuildingKind::Road).success);
    assert!(region.build(2, 0, BuildingKind::Residential).success);

    assert_eq!(
        region.network_border_links(),
        vec![
            network_link(8, 0, BorderEdge::North, 0),
            network_link(8, 0, BorderEdge::West, 0),
            network_link(8, 2, BorderEdge::South, 3),
            network_link(8, 2, BorderEdge::East, 2),
        ]
    );
}

#[test]
fn border_link_matches_complementary_neighbor_link() {
    assert_eq!(
        link(BorderEdge::North, 2).matching_neighbor_link(),
        link(BorderEdge::South, 2)
    );
    assert_eq!(
        link(BorderEdge::East, 1).matching_neighbor_link(),
        link(BorderEdge::West, 1)
    );
}

#[test]
fn availability_hints_report_spare_registry_capacity_without_ecs_identity() {
    let mut region = RegionState::new(RegionId(9), 5, 3);
    assert!(region.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(region.build(0, 1, BuildingKind::Road).success);
    assert!(region.build(1, 1, BuildingKind::Road).success);
    assert!(region.build(1, 0, BuildingKind::Commercial).success);
    assert!(region.build(4, 2, BuildingKind::Road).success);

    let hints = region.availability_hints();
    let copied = hints.clone();

    assert_eq!(hints, copied);
    assert_eq!(hints.len(), 2);
    assert_eq!(hints[0].network, network(9, 0));
    assert!(hints[0].has_spare_power);
    assert!(hints[0].has_spare_jobs);
    assert_eq!(hints[1].network, network(9, 1));
    assert!(!hints[1].has_spare_power);
    assert!(hints[1].has_spare_jobs);
}

fn network(region: u32, road_network: u32) -> RegionRoadNetworkId {
    RegionRoadNetworkId {
        region: RegionId(region),
        road_network,
    }
}

fn link(edge: BorderEdge, offset: usize) -> BorderLinkId {
    BorderLinkId { edge, offset }
}

fn network_link(
    region: u32,
    road_network: u32,
    edge: BorderEdge,
    offset: usize,
) -> NetworkBorderLink {
    NetworkBorderLink {
        network: network(region, road_network),
        link: link(edge, offset),
    }
}
