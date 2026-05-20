//! Integration tests for local effects, desirability growth, overlays, and save/load refresh.

use std::path::PathBuf;

use small_city::core::game::Game;
use small_city::interface::input::{BuildingKind, MapOverlayInput};

#[test]
fn parks_improve_nearby_desirability() {
    let mut game = Game::new(10, 10);
    assert!(game.build(2, 2, BuildingKind::Park).success);

    let near = game.inspect(2, 1).local_effects.expect("near effects");
    let far = game.inspect(9, 9).local_effects.expect("far effects");

    assert!(near.land_value > far.land_value);
    assert!(near.desirability > far.desirability);
}

#[test]
fn industrial_lowers_nearby_desirability() {
    let mut game = Game::new(10, 10);
    assert!(game.build(2, 2, BuildingKind::Industrial).success);

    let near = game.inspect(2, 1).local_effects.expect("near effects");
    let far = game.inspect(9, 9).local_effects.expect("far effects");

    assert!(near.pollution_pressure > far.pollution_pressure);
    assert!(near.land_value < far.land_value);
    assert!(near.desirability < far.desirability);
}

#[test]
fn residential_near_park_grows_better_than_residential_near_industrial() {
    let mut park_city = Game::new(10, 10);
    build_powered_high_job_city(&mut park_city, 3, 0);
    assert!(park_city.build(3, 2, BuildingKind::Park).success);

    let mut industrial_city = Game::new(10, 10);
    build_powered_high_job_city(&mut industrial_city, 3, 0);
    assert!(
        industrial_city
            .build(2, 0, BuildingKind::Industrial)
            .success
    );

    advance_one_week(&mut park_city);
    advance_one_week(&mut industrial_city);

    let park_population = park_city
        .inspect(3, 0)
        .cell
        .expect("park city residential")
        .population
        .expect("population");
    let industrial_population = industrial_city
        .inspect(3, 0)
        .cell
        .expect("industrial city residential")
        .population
        .expect("population");

    assert!(park_population > industrial_population);
    assert_eq!(industrial_population, 0);
}

#[test]
fn land_value_overlay_returns_width_times_height_cells() {
    let game = Game::new(4, 3);

    let land_value = game.view_with_overlay(MapOverlayInput::LandValue);
    let desirability = game.view_with_overlay(MapOverlayInput::Desirability);

    assert_eq!(
        land_value.map.cells.len(),
        land_value.map.width * land_value.map.height
    );
    assert_eq!(
        desirability.map.cells.len(),
        desirability.map.width * desirability.map.height
    );
}

#[test]
fn save_load_preserves_behavior_after_derived_effects_are_refreshed() {
    let path = temp_save_path("small_city_v03_local_effects_roundtrip.json");
    let mut game = Game::new(10, 10);
    build_powered_high_job_city(&mut game, 3, 0);
    assert!(game.build(3, 2, BuildingKind::Park).success);
    advance_one_week(&mut game);
    game.save_to_file(&path).expect("save city");

    let mut loaded = Game::load_from_file(&path).expect("load city");
    let _ = std::fs::remove_file(&path);

    let before = loaded
        .inspect(3, 0)
        .local_effects
        .expect("loaded local effects");
    assert!(before.desirability > 0);
    assert!(loaded.tick().success);

    let view = loaded.view_with_overlay(MapOverlayInput::LandValue);
    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);

    let residential = loaded.inspect(3, 0).cell.expect("residential after load");
    assert_eq!(residential.powered, Some(true));
    assert!(residential.population.expect("population") > 0);
}

#[test]
fn long_city_growth_favors_high_desirability_residential_over_low_desirability() {
    let mut game = Game::new(12, 8);
    build_two_neighborhood_city(&mut game);

    let park_side = game.inspect(2, 0).local_effects.expect("park-side effects");
    let industrial_side = game
        .inspect(8, 0)
        .local_effects
        .expect("industrial-side effects");
    assert!(park_side.desirability > industrial_side.desirability);
    assert!(park_side.land_value > industrial_side.land_value);
    assert!(industrial_side.pollution_pressure > park_side.pollution_pressure);

    advance_weeks(&mut game, 2);

    let view = game.view();
    let land_overlay = game.view_with_overlay(MapOverlayInput::LandValue);
    let desirability_overlay = game.view_with_overlay(MapOverlayInput::Desirability);
    let park_population = game
        .inspect(2, 0)
        .cell
        .expect("park-side residential")
        .population
        .expect("park-side population");
    let industrial_population = game
        .inspect(8, 0)
        .cell
        .expect("industrial-side residential")
        .population
        .expect("industrial-side population");

    assert_eq!(view.status.turn, 24 * 7 * 2);
    assert!(park_population > industrial_population);
    assert!(park_population > 0);
    assert_eq!(industrial_population, 0);
    assert_eq!(
        land_overlay.map.cells.len(),
        land_overlay.map.width * land_overlay.map.height
    );
    assert_eq!(
        desirability_overlay.map.cells.len(),
        desirability_overlay.map.width * desirability_overlay.map.height
    );
}

#[test]
fn long_layout_mutations_refresh_local_effects_and_continue_after_save_load() {
    let path = temp_save_path("small_city_v03_long_local_effects_roundtrip.json");
    let mut game = Game::new(10, 8);
    build_industrial_blocked_city(&mut game);

    let blocked_effects = game
        .inspect(2, 0)
        .local_effects
        .expect("blocked local effects");
    advance_one_week(&mut game);
    assert_eq!(
        game.inspect(2, 0)
            .cell
            .expect("blocked residential")
            .population,
        Some(0)
    );

    assert!(game.bulldoze(1, 0).success);
    assert!(game.build(2, 2, BuildingKind::Park).success);
    let improved_effects = game
        .inspect(2, 0)
        .local_effects
        .expect("improved local effects");
    assert!(improved_effects.desirability > blocked_effects.desirability);
    assert!(improved_effects.land_value > blocked_effects.land_value);
    assert!(improved_effects.pollution_pressure < blocked_effects.pollution_pressure);

    game.save_to_file(&path)
        .expect("save mutated local effects city");
    let mut loaded = Game::load_from_file(&path).expect("load mutated local effects city");
    let _ = std::fs::remove_file(&path);

    let loaded_effects = loaded
        .inspect(2, 0)
        .local_effects
        .expect("loaded local effects");
    assert_eq!(loaded_effects, improved_effects);

    advance_one_week(&mut loaded);

    let residential = loaded.inspect(2, 0).cell.expect("residential after load");
    let desirability_overlay = loaded.view_with_overlay(MapOverlayInput::Desirability);

    assert!(residential.population.expect("population") > 0);
    assert_eq!(
        desirability_overlay.map.cells.len(),
        desirability_overlay.map.width * desirability_overlay.map.height
    );
}

fn build_powered_high_job_city(game: &mut Game, residential_x: usize, residential_y: usize) {
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(
        game.build(residential_x, residential_y, BuildingKind::Residential)
            .success
    );
    assert!(game.build(5, 0, BuildingKind::Commercial).success);
    assert!(game.build(6, 0, BuildingKind::Commercial).success);

    for x in 0..=6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
}

fn advance_one_week(game: &mut Game) {
    advance_weeks(game, 1);
}

fn advance_weeks(game: &mut Game, weeks: usize) {
    // Phase A time cadence moved population growth from every tick to the
    // weekly boundary, so growth assertions advance by explicit weeks.
    for _ in 0..24 * 7 * weeks {
        assert!(game.tick().success);
    }
}

fn build_two_neighborhood_city(game: &mut Game) {
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..=9 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    assert!(game.build(2, 0, BuildingKind::Residential).success);
    assert!(game.build(8, 0, BuildingKind::Residential).success);
    assert!(game.build(4, 0, BuildingKind::Commercial).success);
    assert!(game.build(5, 0, BuildingKind::Commercial).success);
    assert!(game.build(2, 2, BuildingKind::Park).success);
    assert!(game.build(7, 0, BuildingKind::Industrial).success);
}

fn build_industrial_blocked_city(game: &mut Game) {
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..=5 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    assert!(game.build(2, 0, BuildingKind::Residential).success);
    assert!(game.build(4, 0, BuildingKind::Commercial).success);
    assert!(game.build(5, 0, BuildingKind::Commercial).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
}

fn temp_save_path(file_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{}_{}", std::process::id(), file_name))
}
