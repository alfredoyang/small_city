//! Integration tests for road connectivity requirements on growth and effective jobs.

mod common;

use common::SingleRegionTestGame;
use small_city::interface::input::BuildingKind;

fn advance_one_week(game: &mut SingleRegionTestGame) {
    // Phase A moved population growth to weekly boundaries, so growth tests
    // advance through one in-game week before asserting population changes.
    for _ in 0..24 * 7 {
        game.tick();
    }
}

#[test]
fn residential_without_adjacent_road_does_not_grow() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Residential).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(2, 0, BuildingKind::Road).success);

    game.tick();

    let cell = game.inspect(0, 1).cell.expect("residential cell");
    assert_eq!(cell.population, Some(0));
    assert_eq!(cell.road_connected, Some(false));
}

#[test]
fn residential_with_adjacent_road_grows() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 1, BuildingKind::Commercial).success);

    advance_one_week(&mut game);

    let cell = game.inspect(2, 0).cell.expect("residential cell");
    assert_eq!(cell.population, Some(1));
    assert_eq!(cell.road_connected, Some(true));
}

#[test]
fn commercial_without_road_does_not_provide_effective_jobs() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);

    game.tick();

    assert_eq!(game.view().status.jobs, 0);
    assert_eq!(
        game.inspect(1, 0)
            .cell
            .expect("commercial cell")
            .road_connected,
        Some(false)
    );
}

#[test]
fn industrial_with_road_provides_effective_jobs() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 0, BuildingKind::Road).success);

    game.tick();

    assert_eq!(game.view().status.jobs, 3);
    assert_eq!(
        game.inspect(1, 0)
            .cell
            .expect("industrial cell")
            .road_connected,
        Some(true)
    );
}
