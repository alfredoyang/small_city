//! Integration tests for player-visible multi-region regional gameplay.

use small_city::core::regional_game::RegionalGame;
use small_city::core::regions::RegionId;
use small_city::interface::input::BuildingKind;
use small_city::ui::city_driver::{CityDriver, CityLaunchMode};

fn has_generic_imported_resource_note(game: &RegionalGame, region_id: RegionId) -> bool {
    game.inspect_region(region_id, 0, 0)
        .unwrap()
        .explanations
        .iter()
        .any(|note| note.contains("Imported regional resources"))
}

#[test]
fn player_can_build_in_two_regions_through_ui_driver() {
    let mut driver =
        CityDriver::new(CityLaunchMode::RegionalMultiRegion).expect("regional UI driver");

    assert!(driver.build(1, 1, BuildingKind::Residential).success);
    assert!(driver.region_label().contains("1/9"));
    let initial_region_a = driver.view();
    assert!(
        initial_region_a.map.cells[1 + initial_region_a.map.width]
            .building
            .is_some()
    );

    let switched = driver.select_next_region();
    assert!(switched.contains("2/9"));
    assert!(driver.build(2, 1, BuildingKind::Park).success);
    let region_b = driver.view();

    assert_eq!(
        region_b.map.cells[2 + region_b.map.width].building,
        Some(BuildingKind::Park)
    );
    assert_eq!(region_b.map.cells[1 + region_b.map.width].building, None);

    let switched_back = driver.select_previous_region();
    assert!(switched_back.contains("1/9"));
    let region_a = driver.view();

    assert_eq!(
        region_a.map.cells[1 + region_a.map.width].building,
        Some(BuildingKind::Residential)
    );
    assert_eq!(region_a.map.cells[2 + region_a.map.width].building, None);
}

#[test]
fn selected_region_switching_changes_composed_view_deterministically() {
    let mut game = RegionalGame::two_region_default(3, 3).unwrap();

    assert_eq!(game.selected_region().unwrap(), RegionId(1));
    assert_eq!(game.selected_region_position().unwrap(), (1, 2));

    game.select_next_region().unwrap();
    assert_eq!(game.selected_region().unwrap(), RegionId(2));
    assert_eq!(game.selected_region_position().unwrap(), (2, 2));
    assert_eq!(game.view().unwrap().selected_region, Some(RegionId(2)));

    game.select_previous_region().unwrap();
    assert_eq!(game.selected_region().unwrap(), RegionId(1));
    assert_eq!(game.selected_region_position().unwrap(), (1, 2));
}

#[test]
fn two_region_default_wires_topology_for_cross_region_power_export() {
    let game = RegionalGame::two_region_default(3, 2).unwrap();

    assert!(
        game.build(RegionId(1), 2, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(1), 2, 1, BuildingKind::PowerPlant)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 0, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 1, 0, BuildingKind::Residential)
            .unwrap()
            .success
    );

    assert!(game.tick_region(RegionId(2)).unwrap().success);

    let view = game
        .view()
        .unwrap()
        .regions
        .into_iter()
        .find(|snapshot| snapshot.region_id == RegionId(2))
        .expect("region 2 snapshot")
        .view;
    let powered = view
        .map
        .cells
        .iter()
        .find(|cell| cell.x == 1 && cell.y == 0)
        .and_then(|cell| cell.powered)
        .unwrap_or(false);
    assert!(powered);
}

#[test]
fn old_generic_imported_resource_note_is_not_shown() {
    let game = RegionalGame::two_region_default(3, 3).unwrap();

    assert!(
        game.build(RegionId(1), 1, 1, BuildingKind::Park)
            .unwrap()
            .success
    );
    assert!(
        !has_generic_imported_resource_note(&game, RegionId(2)),
        "CR6 retires the visibility-only imported-resource cache and note"
    );
}

#[test]
fn road_builds_do_not_create_cross_region_imports() {
    let game = RegionalGame::two_region_default(3, 3).unwrap();

    assert!(
        game.build(RegionId(1), 1, 1, BuildingKind::Road)
            .unwrap()
            .success
    );

    assert!(
        !has_generic_imported_resource_note(&game, RegionId(2)),
        "common road placement should not fan out regional resource offers"
    );
}
