use small_city::core::game::Game;
use small_city::interface::input::BuildingKind;

#[test]
fn building_cost_is_deducted_correctly() {
    let mut game = Game::new(10, 10);

    let result = game.build(1, 1, BuildingKind::Residential);

    assert!(result.success);
    assert_eq!(game.view().status.money, 95);
}

#[test]
fn cannot_build_outside_the_map() {
    let mut game = Game::new(2, 2);

    let result = game.build(2, 0, BuildingKind::Road);

    assert!(!result.success);
    assert_eq!(game.view().status.money, 100);
}

#[test]
fn cannot_build_on_occupied_cell() {
    let mut game = Game::new(2, 2);
    assert!(game.build(0, 0, BuildingKind::Road).success);

    let result = game.build(0, 0, BuildingKind::Residential);

    assert!(!result.success);
    assert_eq!(game.view().status.money, 99);
}

#[test]
fn cannot_build_without_enough_money() {
    let mut game = Game::new(10, 10);
    for x in 0..5 {
        assert!(game.build(x, 0, BuildingKind::PowerPlant).success);
    }

    let result = game.build(5, 0, BuildingKind::Road);

    assert!(!result.success);
    assert_eq!(game.view().status.money, 0);
}
