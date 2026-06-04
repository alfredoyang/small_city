//! Integration tests for replace and upgrade commands plus their derived effects.

mod common;

use common::SingleRegionTestGame;
use small_city::interface::input::BuildingKind;

#[test]
fn replace_occupied_cell_succeeds_and_deducts_new_building_cost() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(1, 1, BuildingKind::Residential).success);
    let before = game.view().status.money;

    let result = game.replace(1, 1, BuildingKind::Commercial);

    assert!(result.success);
    assert_eq!(
        game.inspect(1, 1).cell.expect("replaced cell").building,
        Some(BuildingKind::Commercial)
    );
    assert_eq!(
        game.view().status.money,
        before - BuildingKind::Commercial.cost()
    );
}

#[test]
fn replace_empty_cell_fails() {
    let mut game = SingleRegionTestGame::new(4, 4);

    let result = game.replace(1, 1, BuildingKind::Commercial);

    assert!(!result.success);
    assert!(result.message().contains("Cannot replace an empty cell"));
}

#[test]
fn replace_same_type_fails() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(1, 1, BuildingKind::Residential).success);

    let result = game.replace(1, 1, BuildingKind::Residential);

    assert!(!result.success);
    assert!(result.message().contains("already has that building type"));
}

#[test]
fn replace_refreshes_derived_state() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);
    assert_eq!(game.view().status.jobs, 2);

    assert!(game.replace(1, 0, BuildingKind::Residential).success);

    assert_eq!(game.view().status.jobs, 0);
    assert_eq!(
        game.inspect(1, 0).cell.expect("replaced cell").building,
        Some(BuildingKind::Residential)
    );
}

#[test]
fn upgrade_residential_increases_capacity() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(1, 1, BuildingKind::Residential).success);
    let before_money = game.view().status.money;

    let result = game.upgrade(1, 1);

    assert!(result.success);
    assert_eq!(game.view().status.money, before_money - 10);
    let cell = game.inspect(1, 1).cell.expect("residential cell");
    assert_eq!(cell.max_population, Some(8));
    assert_eq!(cell.upgrade_level, Some(2));
}

#[test]
fn manual_upgrade_fails_without_enough_money_and_keeps_state() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::Residential).success);
    for (x, y) in [(1, 0), (2, 0), (3, 0), (0, 1)] {
        assert!(game.build(x, y, BuildingKind::PowerPlant).success);
    }
    assert!(game.build(1, 1, BuildingKind::Park).success);
    let before_money = game.view().status.money;
    let before_cell = game.inspect(0, 0).cell.expect("residential cell");
    let before_max_population = before_cell.max_population;

    let result = game.upgrade(0, 0);

    assert!(!result.success);
    assert!(result.message().contains("Not enough money"));
    assert_eq!(game.view().status.money, before_money);
    let cell = game.inspect(0, 0).cell.expect("residential cell");
    assert_eq!(cell.upgrade_level, Some(1));
    assert_eq!(cell.max_population, before_max_population);
}

#[test]
fn upgrade_power_plant_increases_capacity() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);

    let result = game.upgrade(0, 0);

    assert!(result.success);
    assert_eq!(game.view().status.power.total_capacity, 15);
}

#[test]
fn upgrade_park_increases_happiness_effect() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::Park).success);

    let result = game.upgrade(0, 0);

    assert!(result.success);
    assert_eq!(game.view().status.happiness, 55);
}

#[test]
fn unsupported_or_max_upgrade_fails() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::Road).success);
    assert!(!game.upgrade(0, 0).success);

    assert!(game.replace(0, 0, BuildingKind::Park).success);
    assert!(game.upgrade(0, 0).success);
    let result = game.upgrade(0, 0);

    assert!(!result.success);
    assert!(result.message().contains("fully upgraded"));
}
