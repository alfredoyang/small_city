use small_city::core::game::Game;
use small_city::interface::input::BuildingKind;

#[test]
fn powered_industrial_adds_income() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert_eq!(game.view().status.money, 70);

    game.tick();

    assert_eq!(game.view().status.money, 73);
}
