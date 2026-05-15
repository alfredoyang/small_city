use small_city::core::game::Game;
use small_city::interface::input::BuildingKind;

#[test]
fn replace_occupied_cell_succeeds_and_deducts_new_building_cost() {
    let mut game = Game::new(4, 4);
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
    let mut game = Game::new(4, 4);

    let result = game.replace(1, 1, BuildingKind::Commercial);

    assert!(!result.success);
    assert!(result.message().contains("Cannot replace an empty cell"));
}

#[test]
fn replace_same_type_fails() {
    let mut game = Game::new(4, 4);
    assert!(game.build(1, 1, BuildingKind::Residential).success);

    let result = game.replace(1, 1, BuildingKind::Residential);

    assert!(!result.success);
    assert!(result.message().contains("already has that building type"));
}

#[test]
fn replace_refreshes_derived_state() {
    let mut game = Game::new(5, 5);
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
    let mut game = Game::new(4, 4);
    assert!(game.build(1, 1, BuildingKind::Residential).success);

    let result = game.upgrade(1, 1);

    assert!(result.success);
    let cell = game.inspect(1, 1).cell.expect("residential cell");
    assert_eq!(cell.max_population, Some(8));
    assert_eq!(cell.upgrade_level, Some(2));
}

#[test]
fn upgrade_power_plant_increases_capacity() {
    let mut game = Game::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);

    let result = game.upgrade(0, 0);

    assert!(result.success);
    assert_eq!(game.view().status.power.total_capacity, 15);
}

#[test]
fn upgrade_park_increases_happiness_effect() {
    let mut game = Game::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::Park).success);

    let result = game.upgrade(0, 0);

    assert!(result.success);
    assert_eq!(game.view().status.happiness, 55);
}

#[test]
fn unsupported_or_max_upgrade_fails() {
    let mut game = Game::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::Road).success);
    assert!(!game.upgrade(0, 0).success);

    assert!(game.replace(0, 0, BuildingKind::Park).success);
    assert!(game.upgrade(0, 0).success);
    let result = game.upgrade(0, 0);

    assert!(!result.success);
    assert!(result.message().contains("fully upgraded"));
}
