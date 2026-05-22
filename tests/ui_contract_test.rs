//! UI boundary contract tests ensuring the ASCII UI uses public view models and Game APIs.

use small_city::core::game::Game;
use small_city::interface::input::{BuildingKind, MapOverlayInput, UiCommand, parse_command};
use small_city::interface::view::GameView;

#[test]
fn game_view_contains_width_times_height_cells() {
    let game = Game::new(4, 3);
    let view = game.view();

    assert_eq!(view.map.cells.len(), 12);
}

#[test]
fn map_overlays_return_width_times_height_cells() {
    let game = Game::new(4, 3);

    for overlay in [
        MapOverlayInput::Normal,
        MapOverlayInput::Power,
        MapOverlayInput::Pollution,
        MapOverlayInput::Population,
        MapOverlayInput::LandValue,
        MapOverlayInput::Desirability,
    ] {
        let view = game.view_with_overlay(overlay);

        assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
        assert_eq!(view.map.cells.len(), 12);
    }
}

#[test]
fn power_overlay_shows_powered_road_network() {
    let mut game = Game::new(7, 7);
    assert!(game.build(3, 3, BuildingKind::PowerPlant).success);
    assert!(game.build(3, 2, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);
    assert!(game.build(3, 0, BuildingKind::Residential).success);

    let view = game.view_with_overlay(MapOverlayInput::Power);
    let symbol_at = |x: usize, y: usize| view.map.cells[y * view.map.width + x].symbol;

    assert_eq!(symbol_at(3, 3), 'P');
    assert_eq!(symbol_at(3, 2), '*');
    assert_eq!(symbol_at(3, 1), '*');
    assert_eq!(symbol_at(3, 0), '+');
    assert_eq!(symbol_at(0, 0), '.');
}

#[test]
fn empty_cells_are_buildable() {
    let game = Game::new(2, 2);
    let cell = game.inspect(1, 1).cell.expect("cell");

    assert!(cell.buildable);
}

#[test]
fn occupied_cells_are_not_buildable() {
    let mut game = Game::new(2, 2);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    let cell = game.inspect(1, 1).cell.expect("cell");

    assert!(!cell.buildable);
}

#[test]
fn residential_cell_view_includes_population_data() {
    let mut game = Game::new(2, 2);
    assert!(game.build(1, 1, BuildingKind::Residential).success);
    let cell = game.inspect(1, 1).cell.expect("cell");

    assert_eq!(cell.population, Some(0));
    assert_eq!(cell.max_population, Some(5));
}

#[test]
fn cell_view_includes_road_connected_status_for_non_road_buildings() {
    let mut game = Game::new(3, 3);
    assert!(game.build(1, 1, BuildingKind::Residential).success);
    assert!(game.build(1, 2, BuildingKind::Road).success);

    let cell = game.inspect(1, 1).cell.expect("cell");

    assert_eq!(cell.road_connected, Some(true));
}

#[test]
fn city_status_view_includes_demand_data() {
    let game = Game::new(2, 2);
    let demand = game.view().status.demand;

    assert_eq!(
        demand,
        game.view_with_overlay(MapOverlayInput::Normal)
            .status
            .demand
    );
}

#[test]
fn ui_contract_returns_game_view_not_world() {
    let game = Game::new(2, 2);
    let _: GameView = game.view();
}

#[test]
fn parse_build_command() {
    let command = parse_command("build residential 1 2").expect("valid command");

    assert_eq!(
        command,
        UiCommand::Build {
            kind: BuildingKind::Residential,
            x: 1,
            y: 2
        }
    );
}

#[test]
fn parse_view_overlay_command() {
    let command = parse_command("view power").expect("valid command");

    assert_eq!(
        command,
        UiCommand::View {
            overlay: MapOverlayInput::Power
        }
    );

    let command = parse_command("view desirability").expect("valid command");

    assert_eq!(
        command,
        UiCommand::View {
            overlay: MapOverlayInput::Desirability
        }
    );
}

#[test]
fn ascii_ui_does_not_import_ecs_internals() {
    let source = std::fs::read_to_string("src/ui/ascii.rs").expect("ascii ui source");

    for forbidden_import in [
        "crate::core::world",
        "crate::core::components",
        "crate::core::systems",
        "crate::core::resources",
        "crate::core::grid",
        "crate::core::region",
        "crate::core::region_actor",
    ] {
        assert!(
            !source.contains(forbidden_import),
            "ASCII UI must not import ECS or actor internals via {forbidden_import}"
        );
    }
}

#[test]
fn tui_does_not_import_ecs_internals() {
    let source = std::fs::read_to_string("src/ui/tui.rs").expect("tui source");

    for forbidden_import in [
        "crate::core::world",
        "crate::core::components",
        "crate::core::systems",
        "crate::core::resources",
        "crate::core::grid",
        "crate::core::region",
        "crate::core::region_actor",
    ] {
        assert!(
            !source.contains(forbidden_import),
            "TUI must not import ECS or actor internals via {forbidden_import}"
        );
    }
}

#[test]
fn ascii_ui_save_load_uses_game_api_only() {
    let source = std::fs::read_to_string("src/ui/ascii.rs").expect("ascii ui source");

    assert!(source.contains("game.save_to_file"));
    assert!(source.contains("Game::load_from_file"));
    for forbidden in ["serde", "serde_json", "std::fs", "File::"] {
        assert!(
            !source.contains(forbidden),
            "ASCII UI save/load must not use {forbidden} directly"
        );
    }
}

#[test]
fn ascii_ui_replace_and_upgrade_use_game_api_only() {
    let source = std::fs::read_to_string("src/ui/ascii.rs").expect("ascii ui source");

    assert!(source.contains(".replace("));
    assert!(source.contains("game.upgrade"));
    for forbidden in ["crate::core::world", "crate::core::components", "world."] {
        assert!(
            !source.contains(forbidden),
            "ASCII UI replace/upgrade must not use {forbidden} directly"
        );
    }
}

#[test]
fn ascii_ui_renders_command_result_events() {
    let source = std::fs::read_to_string("src/ui/ascii.rs").expect("ascii ui source");

    assert!(source.contains(".message()"));
}

#[test]
fn ascii_ui_renders_demand_from_status_view() {
    let source = std::fs::read_to_string("src/ui/ascii.rs").expect("ascii ui source");

    assert!(source.contains("status.demand"));
}

#[test]
fn ascii_ui_renders_inspect_explanations_from_inspect_view() {
    let source = std::fs::read_to_string("src/ui/ascii.rs").expect("ascii ui source");

    assert!(source.contains("inspect.explanations"));
    assert!(source.contains("Inspect Notes:"));
}
