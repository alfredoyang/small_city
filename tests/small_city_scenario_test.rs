use small_city::core::game::Game;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use small_city::interface::input::{BuildingKind, MapOverlayInput};

#[test]
fn powered_residential_and_commercial_city_grows_over_five_ticks() {
    let mut game = Game::new(10, 10);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);

    let starting_view = game.view();
    let starting_money = starting_view.status.money;
    let starting_population = starting_view.status.population;

    for _ in 0..5 {
        assert!(game.tick().success);
    }

    let view = game.view();

    assert!(view.status.population > starting_population);
    assert_eq!(view.status.turn, 5);
    assert_ne!(view.status.money, starting_money);
    assert!((0..=100).contains(&view.status.happiness));

    // The UI contract stays intact after a multi-system scenario.
    assert_eq!(view.map.width, 10);
    assert_eq!(view.map.height, 10);
    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
}

#[test]
fn upgraded_powered_city_remains_stable_over_twelve_ticks() {
    let mut game = Game::new(10, 10);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    for x in 1..=5 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::Park).success);

    assert!(game.upgrade(0, 0).success);
    assert!(game.upgrade(1, 0).success);
    assert!(game.upgrade(4, 0).success);

    for _ in 0..12 {
        assert!(game.tick().success);
    }

    let view = game.view();
    let residential = game.inspect(1, 0).cell.expect("residential cell");
    let power_overlay = game.view_with_overlay(MapOverlayInput::Power);

    assert_eq!(view.status.turn, 12);
    assert_eq!(view.status.power.total_capacity, 15);
    assert_eq!(view.status.power.total_shortage, 0);
    assert_eq!(residential.max_population, Some(8));
    assert!(residential.population.expect("population") > 0);
    assert_eq!(
        power_overlay.map.cells.len(),
        view.map.width * view.map.height
    );
    assert!((0..=100).contains(&view.status.happiness));
}

#[test]
fn replace_bulldoze_save_load_scenario_continues_for_twenty_ticks() {
    let path = save_path("long-scenario");
    let mut game = Game::new(12, 12);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..=6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::Park).success);

    for _ in 0..6 {
        assert!(game.tick().success);
    }

    assert!(game.replace(2, 0, BuildingKind::Residential).success);
    assert!(game.bulldoze(3, 0).success);
    assert!(game.build(5, 0, BuildingKind::Commercial).success);
    assert!(game.upgrade(1, 0).success);

    game.save_to_file(&path).expect("save long scenario");
    let mut loaded = Game::load_from_file(&path).expect("load long scenario");
    std::fs::remove_file(&path).expect("remove long scenario save");

    for _ in 0..14 {
        assert!(loaded.tick().success);
    }

    let view = loaded.view();
    let first_residential = loaded.inspect(1, 0).cell.expect("first residential");
    let second_residential = loaded.inspect(2, 0).cell.expect("second residential");

    assert_eq!(view.status.turn, 20);
    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
    assert_eq!(first_residential.building, Some(BuildingKind::Residential));
    assert_eq!(first_residential.upgrade_level, Some(2));
    assert_eq!(second_residential.building, Some(BuildingKind::Residential));
    assert_eq!(
        loaded.inspect(3, 0).cell.expect("bulldozed cell").building,
        None
    );
    assert!(view.status.population >= 1);
    assert!((0..=100).contains(&view.status.happiness));
}

fn save_path(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "small_city_{name}_{}_{}.json",
        std::process::id(),
        unique
    ))
}
