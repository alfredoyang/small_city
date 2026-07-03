//! UI boundary contract tests ensuring the ASCII UI uses public view models and facades.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

mod common;

use common::{SingleRegionTestGame, write_legacy_single_city_save};
use small_city::core::regional_game::RegionalGame;
use small_city::core::regions::{RegionId, RegionState};
use small_city::interface::input::{BuildingKind, MapOverlayInput, UiCommand, parse_command};
use small_city::interface::view::GameView;
use small_city::ui::city_driver::CityDriver;

#[test]
fn game_view_contains_width_times_height_cells() {
    let game = SingleRegionTestGame::new(4, 3);
    let view = game.view();

    assert_eq!(view.map.cells.len(), 12);
}

#[test]
fn map_overlays_return_width_times_height_cells() {
    let game = SingleRegionTestGame::new(4, 3);

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
    let mut game = SingleRegionTestGame::new(7, 7);
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
    let game = SingleRegionTestGame::new(2, 2);
    let cell = game.inspect(1, 1).cell.expect("cell");

    assert!(cell.buildable);
}

#[test]
fn occupied_cells_are_not_buildable() {
    let mut game = SingleRegionTestGame::new(2, 2);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    let cell = game.inspect(1, 1).cell.expect("cell");

    assert!(!cell.buildable);
}

#[test]
fn residential_cell_view_includes_population_data() {
    let mut game = SingleRegionTestGame::new(2, 2);
    assert!(game.build(1, 1, BuildingKind::Residential).success);
    let cell = game.inspect(1, 1).cell.expect("cell");

    assert_eq!(cell.population, Some(0));
    assert_eq!(cell.max_population, Some(5));
}

#[test]
fn cell_view_includes_road_connected_status_for_non_road_buildings() {
    let mut game = SingleRegionTestGame::new(3, 3);
    assert!(game.build(1, 1, BuildingKind::Residential).success);
    assert!(game.build(1, 2, BuildingKind::Road).success);

    let cell = game.inspect(1, 1).cell.expect("cell");

    assert_eq!(cell.road_connected, Some(true));
}

#[test]
fn horizontal_roads_expose_east_west_road_links() {
    let mut game = SingleRegionTestGame::new(4, 2);
    assert!(game.build(0, 0, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Road).success);
    assert!(game.build(2, 0, BuildingKind::Road).success);
    let view = game.view();

    assert_eq!(
        cell(&view, 1, 0).road_links,
        road_links(false, true, false, true)
    );
    assert_eq!(
        cell(&view, 0, 0).road_links,
        road_links(false, true, false, false)
    );
    assert_eq!(
        cell(&view, 2, 0).road_links,
        road_links(false, false, false, true)
    );
}

#[test]
fn corner_road_exposes_perpendicular_links() {
    let mut game = SingleRegionTestGame::new(3, 3);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(1, 2, BuildingKind::Road).success);
    let view = game.view();

    assert_eq!(
        cell(&view, 1, 1).road_links,
        road_links(false, true, true, false)
    );
}

#[test]
fn intersection_road_exposes_all_four_links() {
    let mut game = SingleRegionTestGame::new(3, 3);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(1, 2, BuildingKind::Road).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    let view = game.view();

    assert_eq!(
        cell(&view, 1, 1).road_links,
        road_links(true, true, true, true)
    );
}

#[test]
fn non_road_cells_and_map_edges_do_not_report_invalid_road_links() {
    let mut game = SingleRegionTestGame::new(3, 3);
    assert!(game.build(0, 0, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 2, BuildingKind::Road).success);
    let view = game.view();

    assert_eq!(
        cell(&view, 0, 0).road_links,
        road_links(false, false, false, false)
    );
    assert_eq!(
        cell(&view, 1, 0).road_links,
        road_links(false, false, false, false)
    );
    assert_eq!(
        cell(&view, 0, 1).road_links,
        road_links(false, false, false, false)
    );
    assert_eq!(
        cell(&view, 2, 2).road_links,
        road_links(false, false, false, false)
    );
}

#[test]
fn city_status_view_includes_demand_data() {
    let game = SingleRegionTestGame::new(2, 2);
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
    let game = SingleRegionTestGame::new(2, 2);
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
    ] {
        assert!(
            !source.contains(forbidden_import),
            "ASCII UI must not import ECS internals via {forbidden_import}"
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
    ] {
        assert!(
            !source.contains(forbidden_import),
            "TUI must not import ECS internals via {forbidden_import}"
        );
    }
}

#[test]
fn ascii_ui_save_load_uses_facade_only() {
    let source = std::fs::read_to_string("src/ui/ascii.rs").expect("ascii ui source");

    assert!(source.contains("game.save_to_file"));
    assert!(source.contains("game.load_from_file"));
    for forbidden in ["serde", "serde_json", "std::fs", "File::"] {
        assert!(
            !source.contains(forbidden),
            "ASCII UI save/load must not use {forbidden} directly"
        );
    }
}

#[test]
fn ascii_ui_replace_and_upgrade_use_facade_only() {
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

#[test]
fn regional_ui_driver_uses_facade_commands_and_snapshots() {
    let mut driver = CityDriver::regional_multi_region().expect("regional UI driver");

    let preview = driver.preview_build(1, 1, BuildingKind::Residential);
    let build = driver.build(1, 1, BuildingKind::Residential);
    let inspect = driver.inspect(1, 1);
    let view = driver.view_with_overlay(MapOverlayInput::Normal);

    assert!(preview.can_build);
    assert!(build.success);
    assert_eq!(
        inspect.cell.expect("regional inspected cell").building,
        Some(BuildingKind::Residential)
    );
    assert_eq!(
        view.map.cells[1 + view.map.width].building,
        Some(BuildingKind::Residential)
    );
}

#[test]
fn regional_ui_driver_load_uses_loaded_selected_region() {
    let path = save_path("regional-ui-selected-region");
    let game = RegionalGame::from_regions(vec![RegionState::new(RegionId(2), 3, 3)]).unwrap();
    let saved_game = game.save_to_file(&path).unwrap();
    let mut driver = CityDriver::regional_multi_region().expect("regional UI driver");

    driver.load_from_file(&path).unwrap();
    let build = driver.build(1, 1, BuildingKind::Residential);
    let view = driver.view();

    assert!(build.success);
    assert_eq!(
        view.map.cells[1 + view.map.width].building,
        Some(BuildingKind::Residential)
    );
    drop(saved_game);
    remove_save_file(path);
}

#[test]
fn regional_ui_driver_load_accepts_legacy_single_city_save() {
    let path = save_path("regional-ui-legacy-save");
    write_legacy_single_city_save(&path, 3, 3, &[(1, 1, BuildingKind::Residential)]).unwrap();
    let mut driver = CityDriver::regional_multi_region().expect("regional UI driver");

    driver.load_from_file(&path).unwrap();
    let view = driver.view();

    assert_eq!(
        view.map.cells[1 + view.map.width].building,
        Some(BuildingKind::Residential)
    );
    assert!(driver.region_label().contains("Region: 1/1"));
    remove_save_file(path);
}

#[test]
fn default_launch_uses_regional_mode_without_legacy_escape_hatch() {
    let source = std::fs::read_to_string("src/main.rs").expect("main source");
    let tui_source = std::fs::read_to_string("src/ui/tui.rs").expect("tui source");
    let ascii_source = std::fs::read_to_string("src/ui/ascii.rs").expect("ascii source");

    assert!(source.contains("Some(\"tui\") | None => small_city::ui::tui::run()"));
    assert!(source.contains("Some(\"ascii\") => small_city::ui::ascii::run()"));
    assert!(!source.contains("legacy-single"));
    assert!(!source.contains("legacy-ascii"));
    assert!(!source.contains("\"regional\""));
    assert!(tui_source.contains("CityDriver::regional_multi_region()"));
    assert!(!tui_source.contains("run_legacy_single"));
    assert!(!tui_source.contains("run_regional"));
    assert!(ascii_source.contains("CityDriver::regional_multi_region()"));
    assert!(!ascii_source.contains("run_legacy_single"));
    assert!(!ascii_source.contains("run_regional"));
}

#[test]
fn city_driver_has_only_regional_backend_in_production_code() {
    let source = std::fs::read_to_string("src/ui/city_driver.rs").expect("city driver source");

    assert!(!source.contains("CityBackend::SingleCity"));
    assert!(!source.contains("SingleCity"));
    assert!(!source.contains("crate::core::game"));
    assert!(source.contains("RegionalMultiRegion(Box<RegionalGame>)"));
}

#[test]
fn production_core_no_longer_exports_old_game_facade() {
    let source = std::fs::read_to_string("src/core/mod.rs").expect("core module source");

    assert!(!source.contains("pub mod game"));
    assert!(!std::path::Path::new("src/core/game.rs").exists());
}

#[test]
fn ui_regional_path_does_not_import_worker_runtime_or_ecs_internals() {
    for path in ["src/ui/ascii.rs", "src/ui/tui.rs", "src/ui/city_driver.rs"] {
        let source = std::fs::read_to_string(path).expect("ui source");

        for forbidden in [
            "crate::core::world",
            "crate::core::components",
            "crate::core::systems",
            "crate::core::resources",
            "crate::core::grid",
            "crate::core::regions::runtime",
            "crate::core::regions::worker",
            "crate::core::regions::threaded",
            "RegionState",
            "RegionRuntime",
            "RegionWorker",
            "ThreadedRegionWorker",
        ] {
            assert!(
                !source.contains(forbidden),
                "{path} must not import or name {forbidden}"
            );
        }
    }
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

fn cell(view: &GameView, x: usize, y: usize) -> &small_city::interface::view::CellView {
    &view.map.cells[y * view.map.width + x]
}

fn road_links(
    north: bool,
    east: bool,
    south: bool,
    west: bool,
) -> small_city::interface::view::RoadLinks {
    small_city::interface::view::RoadLinks {
        north,
        east,
        south,
        west,
    }
}
