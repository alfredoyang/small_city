use small_city::core::game::Game;
use small_city::interface::input::BuildingKind;

#[test]
fn power_plant_powers_nearby_residential() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(3, 0, BuildingKind::Residential).success);

    game.tick();
    let inspect = game.inspect(3, 0);

    assert_eq!(inspect.cell.expect("cell").powered, Some(true));
}
