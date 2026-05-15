use small_city::core::game::Game;
use small_city::interface::input::BuildingKind;

#[test]
fn residential_population_grows_faster_when_residential_demand_is_high() {
    let mut game = Game::new(10, 10);
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

    game.tick();
    let cell = game.inspect(1, 0).cell.expect("residential cell");

    assert_eq!(cell.population, Some(3));
}

#[test]
fn residential_population_grows_normally_when_residential_demand_is_medium() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);

    game.tick();
    let cell = game.inspect(1, 0).cell.expect("residential cell");

    assert_eq!(cell.population, Some(1));
}
