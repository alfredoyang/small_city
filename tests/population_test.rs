//! Integration tests for residential growth rates driven by demand and desirability.

mod common;

use common::SingleRegionTestGame;
use small_city::interface::input::BuildingKind;

#[test]
fn residential_population_grows_faster_when_residential_demand_is_high() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(3, 0, BuildingKind::Commercial).success);
    assert!(game.build(4, 0, BuildingKind::Commercial).success);
    assert!(game.build(1, 2, BuildingKind::Park).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);
    assert!(game.build(4, 1, BuildingKind::Road).success);

    advance_one_day(&mut game);
    let cell = game.inspect(1, 0).cell.expect("residential cell");

    assert_eq!(cell.population, Some(3));
}

#[test]
fn residential_population_grows_normally_when_residential_demand_is_medium() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);

    advance_one_day(&mut game);
    let cell = game.inspect(1, 0).cell.expect("residential cell");

    assert_eq!(cell.population, Some(1));
}

fn advance_one_day(game: &mut SingleRegionTestGame) {
    // Population growth runs at the daily boundary, not every hourly tick.
    for _ in 0..24 {
        assert!(game.tick().success);
    }
}
