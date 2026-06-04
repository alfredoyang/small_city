//! Integration tests for core power-network capacity, demand, and allocation rules.

mod common;

use common::SingleRegionTestGame;
use small_city::interface::input::BuildingKind;

fn advance_one_week(game: &mut SingleRegionTestGame) {
    // Phase A moved population growth to weekly boundaries, so population tests
    // advance through one in-game week before asserting growth.
    for _ in 0..24 * 7 {
        game.tick();
    }
}

#[test]
fn residential_next_to_powered_road_network_becomes_powered() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);

    game.tick();

    assert_eq!(
        game.inspect(1, 0).cell.expect("residential cell").powered,
        Some(true)
    );
}

#[test]
fn residential_inside_old_radius_without_road_network_is_not_powered() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);

    game.tick();

    assert_eq!(
        game.inspect(1, 0).cell.expect("residential cell").powered,
        Some(false)
    );
}

#[test]
fn power_plant_not_adjacent_to_road_supplies_no_consumers() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(2, 0, BuildingKind::Residential).success);

    game.tick();

    let view = game.view();
    assert_eq!(view.status.power.total_capacity, 10);
    assert_eq!(view.status.power.total_demand, 1);
    assert_eq!(view.status.power.total_supplied, 0);
    assert_eq!(view.status.power.total_shortage, 1);
    assert_eq!(
        game.inspect(2, 0).cell.expect("residential cell").powered,
        Some(false)
    );
}

#[test]
fn disconnected_road_networks_do_not_share_power() {
    let mut game = SingleRegionTestGame::new(6, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(4, 1, BuildingKind::Road).success);
    assert!(game.build(4, 0, BuildingKind::Residential).success);

    game.tick();

    assert_eq!(
        game.inspect(4, 0).cell.expect("residential cell").powered,
        Some(false)
    );
}

#[test]
fn multiple_power_plants_on_same_network_combine_capacity() {
    let mut game = SingleRegionTestGame::new(8, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 2, BuildingKind::PowerPlant).success);
    for x in 0..5 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    for x in 1..5 {
        assert!(game.build(x, 0, BuildingKind::Industrial).success);
    }

    game.tick();

    let view = game.view();
    assert_eq!(view.status.power.total_capacity, 20);
    assert_eq!(view.status.power.total_demand, 12);
    assert_eq!(view.status.power.total_supplied, 12);
    assert_eq!(view.status.power.total_shortage, 0);
    for x in 1..5 {
        assert_eq!(
            game.inspect(x, 0).cell.expect("industrial cell").powered,
            Some(true)
        );
    }
}

#[test]
fn over_capacity_network_powers_consumers_by_position_order() {
    let mut game = SingleRegionTestGame::new(7, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    for x in 1..5 {
        assert!(game.build(x, 0, BuildingKind::Industrial).success);
    }
    assert!(game.build(5, 0, BuildingKind::Commercial).success);

    game.tick();

    for x in 1..4 {
        assert_eq!(
            game.inspect(x, 0).cell.expect("industrial cell").powered,
            Some(true)
        );
    }
    assert_eq!(
        game.inspect(4, 0).cell.expect("industrial cell").powered,
        Some(false)
    );
    assert_eq!(
        game.inspect(5, 0).cell.expect("commercial cell").powered,
        Some(false)
    );
    assert_eq!(game.view().status.power.total_supplied, 9);
    assert_eq!(game.view().status.power.total_shortage, 5);
}

#[test]
fn population_only_grows_when_powered_by_network() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);

    advance_one_week(&mut game);

    assert_eq!(
        game.inspect(1, 0)
            .cell
            .expect("residential cell")
            .population,
        Some(1)
    );
}
