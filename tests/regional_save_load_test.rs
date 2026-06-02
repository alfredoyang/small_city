//! Integration tests for regional game save/load at explicit runner safe points.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use small_city::core::game::Game;
use small_city::core::regional_game::{
    RegionalGame, RegionalGameSaveError, RegionalGameSaveFailure,
};
use small_city::core::regions::{RegionId, RegionState};
use small_city::interface::input::BuildingKind;
use small_city::interface::view::GameView;

#[test]
fn multi_region_game_round_trips_with_identical_views() {
    let path = save_path("regional-roundtrip");
    let game = regional_game_with_distinct_regions();
    let before = game.view().unwrap();

    let saved_game = game.save_to_file(&path).unwrap();
    let loaded = RegionalGame::load_from_file(&path).unwrap();

    assert_eq!(saved_game.view().unwrap(), before);
    assert_eq!(loaded.view().unwrap(), before);
    remove_save_file(path);
}

#[test]
fn roundtrip_preserves_non_sorted_region_order() {
    let path = save_path("regional-order");
    let game = regional_game_with_regions_in_order([RegionId(2), RegionId(1)]);
    let before = game.view().unwrap();

    game.save_to_file(&path).unwrap();
    let loaded = RegionalGame::load_from_file(&path).unwrap();
    let loaded_view = loaded.view().unwrap();

    assert_eq!(
        region_order(&loaded_view.regions),
        vec![RegionId(2), RegionId(1)]
    );
    assert_eq!(loaded_view, before);
    remove_save_file(path);
}

#[test]
fn save_failure_returns_restarted_game_with_progress() {
    let path = save_path("regional-missing-directory").join("save.json");
    let game = regional_game_with_distinct_regions();
    let before = game.view().unwrap();

    let failure = game
        .save_to_file(path)
        .expect_err("save should fail when parent path is not a directory");

    let RegionalGameSaveFailure::Recoverable { game, error } = failure else {
        panic!("file save failure should return the restarted game");
    };
    assert!(matches!(error, RegionalGameSaveError::Io(_)));
    assert_eq!(game.view().unwrap(), before);

    game.tick_region(RegionId(1)).unwrap();
    assert_eq!(
        turn(&game, RegionId(1)),
        turn_from_view(&before, RegionId(1)) + 1
    );
}

#[test]
fn loading_preserves_each_regions_authoritative_state_independently() {
    let path = save_path("regional-independent-state");
    let game = regional_game_with_distinct_regions();
    game.save_to_file(&path).unwrap();

    let loaded = RegionalGame::load_from_file(&path).unwrap();
    let view = loaded.view().unwrap();
    let region_a = region_view(&view.regions, RegionId(1));
    let region_b = region_view(&view.regions, RegionId(2));

    assert_eq!(
        cell_building(region_a, 0, 0),
        Some(BuildingKind::PowerPlant)
    );
    assert_eq!(cell_building(region_a, 1, 0), Some(BuildingKind::Road));
    assert_eq!(
        cell_building(region_b, 0, 0),
        Some(BuildingKind::Residential)
    );
    assert_eq!(cell_building(region_b, 1, 0), Some(BuildingKind::Park));
    assert_eq!(cell_building(region_a, 1, 0), Some(BuildingKind::Road));
    assert_ne!(region_a, region_b);
    remove_save_file(path);
}

#[test]
fn saved_regional_game_can_continue_after_safe_point_restart() {
    let path = save_path("regional-continues");
    let game = regional_game_with_distinct_regions();
    let saved_game = game.save_to_file(&path).unwrap();
    let before_turn = turn(&saved_game, RegionId(1));

    saved_game.tick_region(RegionId(1)).unwrap();

    assert_eq!(turn(&saved_game, RegionId(1)), before_turn + 1);
    remove_save_file(path);
}

#[test]
fn existing_single_city_saves_remain_loadable() {
    let path = save_path("single-city-compatible");
    let mut game = Game::new(3, 3);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Road).success);
    game.save_to_file(&path).unwrap();

    let loaded = Game::load_from_file(&path).unwrap();

    assert_eq!(loaded.view(), game.view());
    remove_save_file(path);
}

fn regional_game_with_distinct_regions() -> RegionalGame {
    regional_game_with_regions_in_order([RegionId(1), RegionId(2)])
}

fn regional_game_with_regions_in_order(region_ids: [RegionId; 2]) -> RegionalGame {
    let game = RegionalGame::from_regions(vec![
        RegionState::new(region_ids[0], 4, 3),
        RegionState::new(region_ids[1], 4, 3),
    ])
    .unwrap();

    assert!(
        game.build(region_ids[0], 0, 0, BuildingKind::PowerPlant)
            .unwrap()
            .success
    );
    assert!(
        game.build(region_ids[0], 1, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(region_ids[1], 0, 0, BuildingKind::Residential)
            .unwrap()
            .success
    );
    assert!(
        game.build(region_ids[1], 1, 0, BuildingKind::Park)
            .unwrap()
            .success
    );
    game.tick_region(region_ids[0]).unwrap();
    game.tick_region(region_ids[1]).unwrap();
    game.tick_region(region_ids[1]).unwrap();

    game
}

fn region_view(
    regions: &[small_city::core::regional_game::RegionViewSnapshot],
    region_id: RegionId,
) -> &GameView {
    &regions
        .iter()
        .find(|snapshot| snapshot.region_id == region_id)
        .expect("region snapshot")
        .view
}

fn cell_building(view: &GameView, x: usize, y: usize) -> Option<BuildingKind> {
    view.map
        .cells
        .iter()
        .find(|cell| cell.x == x && cell.y == y)
        .and_then(|cell| cell.building)
}

fn turn(game: &RegionalGame, region_id: RegionId) -> u32 {
    let view = game.view().unwrap();
    turn_from_view(&view, region_id)
}

fn turn_from_view(
    view: &small_city::core::regional_game::RegionalGameView,
    region_id: RegionId,
) -> u32 {
    view.regions
        .iter()
        .find(|snapshot| snapshot.region_id == region_id)
        .expect("region snapshot")
        .view
        .status
        .turn
}

fn region_order(regions: &[small_city::core::regional_game::RegionViewSnapshot]) -> Vec<RegionId> {
    regions.iter().map(|snapshot| snapshot.region_id).collect()
}

fn save_path(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("small_city_{name}_{unique}.json"))
}

fn remove_save_file(path: PathBuf) {
    std::fs::remove_file(path).expect("remove save file");
}
