use small_city::core::game::Game;
use small_city::interface::input::BuildingKind;

#[test]
fn residential_population_grows_when_powered_and_jobs_are_available() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);

    game.tick();
    let cell = game.inspect(1, 0).cell.expect("residential cell");

    assert_eq!(cell.population, Some(1));
}
