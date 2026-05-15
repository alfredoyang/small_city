use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use small_city::core::game::{Game, GameError};
use small_city::interface::input::BuildingKind;

#[test]
fn saving_a_game_creates_a_file() {
    let path = save_path("creates-file");
    let game = Game::new(4, 3);

    game.save_to_file(&path).expect("save succeeds");

    assert!(path.exists());
    std::fs::remove_file(path).expect("remove save file");
}

#[test]
fn save_load_roundtrip_restores_city_state_visible_through_game_view() {
    let path = save_path("roundtrip");
    let game = full_city_game();
    game.save_to_file(&path).expect("save succeeds");

    let loaded = Game::load_from_file(&path).expect("load succeeds");
    let original_view = game.view();
    let loaded_view = loaded.view();

    assert_eq!(loaded_view.status.money, original_view.status.money);
    assert_eq!(loaded_view.status.turn, original_view.status.turn);
    assert_eq!(loaded_view.map.width, 7);
    assert_eq!(loaded_view.map.height, 5);
    assert_eq!(
        loaded_view.map.cells.len(),
        loaded_view.map.width * loaded_view.map.height
    );
    assert_eq!(building_count(&loaded), 10);
    assert_eq!(loaded_view.status.population, 3);
    assert_eq!(loaded_view.status.pollution, 1);
    assert_eq!(loaded_view.status.happiness, 52);

    assert_eq!(
        loaded.inspect(0, 0).cell.expect("power plant").building,
        Some(BuildingKind::PowerPlant)
    );
    let residential = loaded.inspect(1, 0).cell.expect("residential cell");
    let commercial = loaded.inspect(2, 0).cell.expect("commercial cell");
    let industrial = loaded.inspect(3, 0).cell.expect("industrial cell");
    let park = loaded.inspect(4, 0).cell.expect("park cell");

    assert_eq!(residential.building, Some(BuildingKind::Residential));
    assert_eq!(residential.population, Some(3));
    assert_eq!(residential.powered, Some(true));
    assert_eq!(residential.road_connected, Some(true));
    assert_eq!(commercial.building, Some(BuildingKind::Commercial));
    assert_eq!(commercial.powered, Some(true));
    assert_eq!(commercial.road_connected, Some(true));
    assert_eq!(industrial.building, Some(BuildingKind::Industrial));
    assert_eq!(industrial.powered, Some(true));
    assert_eq!(industrial.road_connected, Some(true));
    assert_eq!(park.building, Some(BuildingKind::Park));

    remove_save_file(path);
}

#[test]
fn loaded_game_can_tick_again() {
    let path = save_path("continues");
    let game = full_city_game();
    game.save_to_file(&path).expect("save succeeds");

    let mut loaded = Game::load_from_file(&path).expect("load succeeds");
    let before = loaded.view();

    loaded.tick();
    let after = loaded.view();

    assert_eq!(after.status.turn, before.status.turn + 1);
    assert!(after.status.money > before.status.money);
    assert!(after.status.population >= before.status.population);
    assert_eq!(after.map.cells.len(), after.map.width * after.map.height);
    remove_save_file(path);
}

#[test]
fn loaded_game_view_has_width_times_height_cells() {
    let path = save_path("cell-count");
    let game = full_city_game();
    game.save_to_file(&path).expect("save succeeds");

    let loaded = Game::load_from_file(&path).expect("load succeeds");
    let view = loaded.view();

    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
    remove_save_file(path);
}

#[test]
fn invalid_file_path_returns_error() {
    let path = save_path("missing-directory").join("save.json");
    let game = Game::new(2, 2);

    let result = game.save_to_file(path);

    assert!(matches!(result, Err(GameError::Io(_))));
}

#[test]
fn missing_save_file_returns_error() {
    let path = save_path("missing-file");

    let result = Game::load_from_file(path);

    assert!(matches!(result, Err(GameError::Io(_))));
}

#[test]
fn invalid_json_returns_error() {
    let path = save_path("invalid-json");
    std::fs::write(&path, "not valid json").expect("write invalid save file");

    let result = Game::load_from_file(&path);

    assert!(matches!(result, Err(GameError::SaveFormat(_))));
    remove_save_file(path);
}

fn full_city_game() -> Game {
    let mut game = Game::new(7, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::Park).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);
    assert!(game.build(4, 1, BuildingKind::Road).success);
    for _ in 0..3 {
        game.tick();
    }
    game
}

fn building_count(game: &Game) -> usize {
    game.view()
        .map
        .cells
        .iter()
        .filter(|cell| cell.building.is_some())
        .count()
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

fn remove_save_file(path: PathBuf) {
    std::fs::remove_file(path).expect("remove save file");
}
