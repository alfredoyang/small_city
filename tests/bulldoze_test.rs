use small_city::core::game::Game;
use small_city::interface::input::BuildingKind;

#[test]
fn bulldoze_occupied_cell_succeeds() {
    let mut game = Game::new(3, 3);
    assert!(game.build(1, 1, BuildingKind::Residential).success);

    let result = game.bulldoze(1, 1);

    assert!(result.success);
    assert_eq!(result.message(), "Bulldozed building at (1, 1)");
}

#[test]
fn bulldoze_empty_cell_fails() {
    let mut game = Game::new(3, 3);

    let result = game.bulldoze(1, 1);

    assert!(!result.success);
    assert_eq!(result.message(), "Cell is already empty");
}

#[test]
fn bulldoze_removes_the_building_from_game_view() {
    let mut game = Game::new(3, 3);
    assert!(game.build(1, 1, BuildingKind::Residential).success);

    assert!(game.bulldoze(1, 1).success);

    let cell = game.view().map.cells[4].clone();
    assert_eq!(cell.building, None);
    assert!(cell.buildable);
}

#[test]
fn bulldoze_deducts_money() {
    let mut game = Game::new(3, 3);
    assert!(game.build(1, 1, BuildingKind::Residential).success);
    let before = game.view().status.money;

    assert!(game.bulldoze(1, 1).success);

    assert_eq!(game.view().status.money, before - 1);
}

#[test]
fn bulldozing_a_road_can_affect_road_connectivity() {
    let mut game = Game::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    game.tick();
    assert_eq!(game.view().status.jobs, 3);

    assert!(game.bulldoze(1, 1).success);

    let industrial = game.inspect(1, 0).cell.expect("industrial cell");
    assert_eq!(industrial.road_connected, Some(false));
    assert_eq!(game.view().status.jobs, 0);
}

#[test]
fn simulation_can_continue_after_bulldoze() {
    let mut game = Game::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);

    assert!(game.bulldoze(1, 1).success);
    let result = game.tick();

    assert!(result.success);
    assert_eq!(game.view().status.turn, 1);
}
