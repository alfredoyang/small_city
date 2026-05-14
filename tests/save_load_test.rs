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
fn loading_restores_city_state_visible_through_game_view() {
    let path = save_path("restore-state");
    let game = populated_game();
    game.save_to_file(&path).expect("save succeeds");

    let loaded = Game::load_from_file(&path).expect("load succeeds");
    let original_view = game.view();
    let loaded_view = loaded.view();

    assert_eq!(loaded_view.status.money, original_view.status.money);
    assert_eq!(loaded_view.status.turn, original_view.status.turn);
    assert_eq!(loaded_view.map.width, 6);
    assert_eq!(loaded_view.map.height, 5);
    assert_eq!(building_count(&loaded), 3);

    let residential = loaded.inspect(1, 0).cell.expect("residential cell");
    assert_eq!(residential.population, Some(1));
    assert_eq!(residential.powered, Some(true));

    std::fs::remove_file(path).expect("remove save file");
}

#[test]
fn loaded_game_view_has_width_times_height_cells() {
    let path = save_path("cell-count");
    let game = populated_game();
    game.save_to_file(&path).expect("save succeeds");

    let loaded = Game::load_from_file(&path).expect("load succeeds");
    let view = loaded.view();

    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
    std::fs::remove_file(path).expect("remove save file");
}

#[test]
fn simulation_can_continue_after_loading() {
    let path = save_path("continues");
    let game = populated_game();
    game.save_to_file(&path).expect("save succeeds");

    let mut loaded = Game::load_from_file(&path).expect("load succeeds");
    let before = loaded.view().status.turn;

    loaded.tick();
    assert!(loaded.build(3, 0, BuildingKind::Park).success);

    assert_eq!(loaded.view().status.turn, before + 1);
    assert_eq!(building_count(&loaded), 4);
    std::fs::remove_file(path).expect("remove save file");
}

#[test]
fn invalid_file_path_returns_error() {
    let path = save_path("missing-directory").join("save.json");
    let game = Game::new(2, 2);

    let result = game.save_to_file(path);

    assert!(matches!(result, Err(GameError::Io(_))));
}

fn populated_game() -> Game {
    let mut game = Game::new(6, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Industrial).success);
    game.tick();
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
