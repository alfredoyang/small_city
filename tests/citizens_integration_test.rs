//! Integration tests for off-grid citizen entities, home aggregates, and save/load behavior.

use std::path::PathBuf;

mod common;

use common::Game;
use small_city::interface::input::BuildingKind;
use small_city::interface::view::InspectDetailsView;

#[test]
fn residential_growth_spawns_citizens_visible_through_views() {
    let mut game = Game::new(10, 10);
    build_growth_city(&mut game, 1, 0);

    advance_one_week(&mut game);

    let view = game.view();
    let inspect = game.inspect(1, 0);

    assert_eq!(view.status.population, view.status.citizens);
    assert!(view.status.citizens > 0);
    match inspect.details.expect("residential details") {
        InspectDetailsView::Residential {
            population,
            citizens,
            average_happiness,
            ..
        } => {
            assert_eq!(population, citizens);
            assert_eq!(population, view.status.citizens);
            assert!(average_happiness.expect("citizen happiness") > 0);
        }
        other => panic!("expected residential details, got {other:?}"),
    }
}

#[test]
fn bulldozing_residential_removes_its_citizens() {
    let mut game = Game::new(10, 10);
    build_growth_city(&mut game, 1, 0);
    advance_one_week(&mut game);
    assert!(game.view().status.citizens > 0);

    assert!(game.bulldoze(1, 0).success);

    assert_eq!(game.view().status.population, 0);
    assert_eq!(game.view().status.citizens, 0);
}

#[test]
fn save_load_preserves_citizens_and_home_aggregates() {
    let path = temp_save_path("small_city_citizens_roundtrip.json");
    let mut game = Game::new(10, 10);
    build_growth_city(&mut game, 1, 0);
    advance_one_week(&mut game);
    game.save_to_file(&path).expect("save citizens");

    let loaded = Game::load_from_file(&path).expect("load citizens");
    let _ = std::fs::remove_file(&path);

    let view = loaded.view();
    let inspect = loaded.inspect(1, 0);

    assert_eq!(view.status.population, view.status.citizens);
    assert!(view.status.citizens > 0);
    match inspect.details.expect("residential details") {
        InspectDetailsView::Residential {
            population,
            citizens,
            average_happiness,
            ..
        } => {
            assert_eq!(population, citizens);
            assert_eq!(population, view.status.citizens);
            assert!(average_happiness.is_some());
        }
        other => panic!("expected residential details, got {other:?}"),
    }
}

#[test]
fn citizens_can_affect_nearby_local_effects_after_growth() {
    let mut game = Game::new(10, 10);
    build_growth_city(&mut game, 1, 0);
    assert!(game.build(1, 2, BuildingKind::Park).success);

    let before = game.inspect(1, 0).local_effects.expect("before effects");
    advance_one_week(&mut game);
    let after = game.inspect(1, 0).local_effects.expect("after effects");

    assert!(game.view().status.citizens > 0);
    assert!(after.land_value >= before.land_value);
    assert!(after.desirability >= before.desirability);
}

fn build_growth_city(game: &mut Game, residential_x: usize, residential_y: usize) {
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(
        game.build(residential_x, residential_y, BuildingKind::Residential)
            .success
    );
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);

    for x in 0..=3 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
}

fn advance_one_week(game: &mut Game) {
    // Phase A time cadence moved population growth from every tick to the
    // weekly boundary, so citizen growth tests advance through one full week.
    for _ in 0..24 * 7 {
        assert!(game.tick().success);
    }
}

fn temp_save_path(file_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{}_{}", std::process::id(), file_name))
}
