//! Integration tests for regional game save/load at explicit runner safe points.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

mod common;

use common::write_legacy_single_city_save;
use serde_json::Value;
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
fn multi_worker_regional_game_save_restart_and_load_are_safe_points() {
    let path = save_path("regional-multi-worker-roundtrip");
    let game = RegionalGame::from_regions_with_worker_assignments(
        vec![
            RegionState::new(RegionId(1), 3, 2),
            RegionState::new(RegionId(2), 3, 2),
        ],
        2,
        vec![0, 1],
    )
    .unwrap();
    build_cross_region_power_fixture(&game);

    assert!(game.tick_region(RegionId(2)).unwrap().success);
    assert!(region_cell_powered(&game, RegionId(2), 1, 0));
    let before = game.view().unwrap();

    let restarted = game.save_to_file(&path).unwrap();
    let save_text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !save_text.contains("worker"),
        "worker setup is transient runtime configuration, not save data"
    );
    assert_eq!(
        turn(&restarted, RegionId(2)),
        turn_from_view(&before, RegionId(2))
    );
    assert!(restarted.tick_region(RegionId(2)).unwrap().success);
    assert!(region_cell_powered(&restarted, RegionId(2), 1, 0));

    let loaded = RegionalGame::load_from_file(&path).unwrap();
    assert!(loaded.tick_region(RegionId(2)).unwrap().success);
    assert!(region_cell_powered(&loaded, RegionId(2), 1, 0));

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
fn saved_regional_game_stores_layout_not_explicit_topology() {
    let path = save_path("regional-layout-save-shape");
    let game = RegionalGame::two_region_default(3, 2).unwrap();

    let restarted = game.save_to_file(&path).unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();

    assert_eq!(saved["layout"]["rows"], 1);
    assert_eq!(saved["layout"]["columns"], 2);
    assert!(saved.get("topology").is_none());

    assert!(restarted.shutdown().is_ok());
    remove_save_file(path);
}

#[test]
fn old_regional_save_without_layout_infers_row_major_topology() {
    let path = save_path("regional-layout-migration");
    let game = RegionalGame::two_region_default(3, 2).unwrap();
    build_cross_region_power_fixture(&game);
    game.save_to_file(&path).unwrap();

    let mut saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    saved.as_object_mut().unwrap().remove("layout");
    std::fs::write(&path, serde_json::to_vec_pretty(&saved).unwrap()).unwrap();

    let loaded = RegionalGame::load_from_file(&path).unwrap();
    assert!(loaded.tick_region(RegionId(2)).unwrap().success);
    assert!(region_cell_powered(&loaded, RegionId(2), 1, 0));

    remove_save_file(path);
}

#[test]
fn save_load_rebuilds_cross_region_power_export_after_tick_without_saving_grant() {
    let path = save_path("regional-power-export-rebuild");
    let game = RegionalGame::two_region_default(3, 2).unwrap();
    build_cross_region_power_fixture(&game);

    assert!(game.tick_region(RegionId(2)).unwrap().success);
    assert!(region_cell_powered(&game, RegionId(2), 1, 0));

    game.save_to_file(&path).unwrap();
    let save_text = std::fs::read_to_string(&path).unwrap();
    assert!(!save_text.contains("source_region"));
    assert!(!save_text.contains("\"Imported\""));
    assert!(
        !save_text.contains("\"powered\": true"),
        "powered flags are derived from local/imported allocation and should be rebuilt"
    );

    let loaded = RegionalGame::load_from_file(&path).unwrap();
    assert!(loaded.tick_region(RegionId(2)).unwrap().success);
    assert!(
        region_cell_powered(&loaded, RegionId(2), 1, 0),
        "loaded topology and hints should allow the normal tick flow to re-request exported power"
    );

    remove_save_file(path);
}

#[test]
fn save_load_rebuilds_cross_region_remote_jobs_after_daily_tick() {
    let path = save_path("regional-remote-job-rebuild");
    let game = RegionalGame::two_region_default(6, 3).unwrap();
    build_cross_region_remote_job_fixture(&game);
    run_regional_days(&game, 10);

    let before_view = game.view().unwrap();
    let before_assignment = region_view(&before_view.regions, RegionId(1))
        .map
        .cells
        .iter()
        .find(|cell| cell.x == 0 && cell.y == 0)
        .and_then(|cell| cell.job_assignments.first().copied())
        .expect("pre-save remote assignment");
    assert!(before_assignment.is_remote);

    game.save_to_file(&path).unwrap();
    let loaded = RegionalGame::load_from_file(&path).unwrap();
    run_regional_days(&loaded, 1);

    let loaded_view = loaded.view().unwrap();
    let loaded_region = region_view(&loaded_view.regions, RegionId(1));
    assert!(
        loaded_region.status.population > 0,
        "fixture should have residents that need jobs"
    );
    let loaded_assignment = loaded_region
        .map
        .cells
        .iter()
        .find(|cell| cell.x == 0 && cell.y == 0)
        .and_then(|cell| cell.job_assignments.first().copied())
        .expect("loaded remote assignment");
    assert!(
        loaded_assignment.is_remote,
        "loaded game should rebuild remote jobs through normal export allocation"
    );

    remove_save_file(path);
}

#[test]
fn save_load_rebuilds_local_job_assignment_visibility_immediately() {
    let path = save_path("regional-local-job-visibility-rebuild");
    let game = RegionalGame::single_region(4, 3).unwrap();
    build_local_job_visibility_fixture(&game);

    let before_view = game.view().unwrap();
    let before_assignment = region_view(&before_view.regions, RegionId(1))
        .map
        .cells
        .iter()
        .find(|cell| cell.x == 1 && cell.y == 0)
        .and_then(|cell| cell.job_assignments.first().copied())
        .expect("pre-save local assignment");

    game.save_to_file(&path).unwrap();
    let loaded = RegionalGame::load_from_file(&path).unwrap();
    let loaded_view = loaded.view().unwrap();
    let loaded_assignment = region_view(&loaded_view.regions, RegionId(1))
        .map
        .cells
        .iter()
        .find(|cell| cell.x == 1 && cell.y == 0)
        .and_then(|cell| cell.job_assignments.first().copied())
        .expect("loaded local assignment");

    assert_eq!(loaded_assignment, before_assignment);
    assert_eq!(loaded_assignment.region, RegionId(1));
    assert_eq!((loaded_assignment.x, loaded_assignment.y), (2, 0));
    assert!(!loaded_assignment.is_remote);

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
fn regional_loader_accepts_existing_single_city_save() {
    let path = save_path("single-city-compatible");
    write_legacy_single_city_save(
        &path,
        3,
        3,
        &[(0, 0, BuildingKind::PowerPlant), (1, 0, BuildingKind::Road)],
    )
    .unwrap();

    let loaded = RegionalGame::load_from_file(&path).unwrap();
    let converted_view = loaded.selected_region_view().unwrap();

    assert_eq!(loaded.selected_region().unwrap(), RegionId(1));
    assert_eq!(
        cell_building(&converted_view, 0, 0),
        Some(BuildingKind::PowerPlant)
    );
    assert_eq!(
        cell_building(&converted_view, 1, 0),
        Some(BuildingKind::Road)
    );
    remove_save_file(path);
}

#[test]
fn converted_single_city_save_can_continue_and_roundtrip_as_regional_save() {
    let legacy_path = save_path("single-city-continues");
    let regional_path = save_path("converted-regional-roundtrip");
    write_legacy_single_city_save(
        &legacy_path,
        4,
        3,
        &[
            (0, 0, BuildingKind::PowerPlant),
            (1, 0, BuildingKind::Road),
            (1, 1, BuildingKind::Residential),
        ],
    )
    .unwrap();

    let converted = RegionalGame::load_from_file(&legacy_path).unwrap();
    let before_turn = converted.selected_region_view().unwrap().status.turn;

    assert!(
        converted
            .build(RegionId(1), 2, 1, BuildingKind::Commercial)
            .unwrap()
            .success
    );
    converted.tick_region(RegionId(1)).unwrap();
    let after_continue = converted.selected_region_view().unwrap();

    assert_eq!(after_continue.status.turn, before_turn + 1);
    assert_eq!(
        cell_building(&after_continue, 2, 1),
        Some(BuildingKind::Commercial)
    );

    converted.save_to_file(&regional_path).unwrap();
    let reloaded = RegionalGame::load_from_file(&regional_path).unwrap();

    assert_eq!(reloaded.selected_region_view().unwrap(), after_continue);
    remove_save_file(legacy_path);
    remove_save_file(regional_path);
}

#[test]
fn regional_loader_reports_invalid_save_format_deterministically() {
    let path = save_path("regional-invalid-json");
    std::fs::write(&path, "not valid json").expect("write invalid save file");

    let result = RegionalGame::load_from_file(&path);

    assert!(matches!(result, Err(RegionalGameSaveError::SaveFormat(_))));
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

fn build_cross_region_power_fixture(game: &RegionalGame) {
    assert!(
        game.build(RegionId(1), 2, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(1), 2, 1, BuildingKind::PowerPlant)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 0, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 1, 0, BuildingKind::Residential)
            .unwrap()
            .success
    );
}

fn build_cross_region_remote_job_fixture(game: &RegionalGame) {
    assert!(
        game.build(RegionId(1), 0, 0, BuildingKind::Residential)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(1), 0, 1, BuildingKind::Park)
            .unwrap()
            .success
    );
    for x in 1..=5 {
        assert!(
            game.build(RegionId(1), x, 0, BuildingKind::Road)
                .unwrap()
                .success
        );
    }
    assert!(
        game.build(RegionId(1), 4, 1, BuildingKind::PowerPlant)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(1), 3, 2, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(1), 4, 2, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(1), 5, 2, BuildingKind::Industrial)
            .unwrap()
            .success
    );

    assert!(
        game.build(RegionId(2), 0, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 1, 0, BuildingKind::Road)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 0, 1, BuildingKind::Industrial)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(2), 1, 1, BuildingKind::PowerPlant)
            .unwrap()
            .success
    );
}

fn build_local_job_visibility_fixture(game: &RegionalGame) {
    assert!(
        game.build(RegionId(1), 0, 0, BuildingKind::PowerPlant)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(1), 1, 0, BuildingKind::Residential)
            .unwrap()
            .success
    );
    assert!(
        game.build(RegionId(1), 2, 0, BuildingKind::Commercial)
            .unwrap()
            .success
    );
    for x in 0..=2 {
        assert!(
            game.build(RegionId(1), x, 1, BuildingKind::Road)
                .unwrap()
                .success
        );
    }

    for _ in 0..24 {
        assert!(game.tick_region(RegionId(1)).unwrap().success);
    }
}

fn run_regional_days(game: &RegionalGame, days: u64) {
    for _ in 0..(days * 24) {
        game.tick_region(RegionId(1)).unwrap();
        game.tick_region(RegionId(2)).unwrap();
    }
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

fn region_cell_powered(game: &RegionalGame, region_id: RegionId, x: usize, y: usize) -> bool {
    let view = game.view().unwrap();
    region_view(&view.regions, region_id)
        .map
        .cells
        .iter()
        .find(|cell| cell.x == x && cell.y == y)
        .and_then(|cell| cell.powered)
        .unwrap_or(false)
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
