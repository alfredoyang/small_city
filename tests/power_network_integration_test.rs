use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use small_city::core::game::Game;
use small_city::interface::input::{BuildingKind, MapOverlayInput};
use small_city::interface::view::{CellView, GameView, InspectDetailsView};

#[test]
fn connected_road_network_powers_consumers() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..4 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);

    game.tick();

    assert_eq!(inspect_powered(&game, 1, 0), Some(true));
    assert_eq!(inspect_powered(&game, 2, 0), Some(true));

    let power_overlay = game.view_with_overlay(MapOverlayInput::Power);
    assert_eq!(cell(&power_overlay, 0, 1).symbol, '*');
    assert_eq!(cell(&power_overlay, 1, 0).symbol, '+');
    assert_eq!(cell(&power_overlay, 2, 0).symbol, '+');

    for _ in 0..3 {
        game.tick();
    }

    let residential = game.inspect(1, 0);
    assert_eq!(
        residential.cell.expect("residential cell").population,
        Some(2)
    );
    assert!(game.view().status.population > 0);
}

#[test]
fn nearby_building_without_road_network_is_not_powered() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);

    game.tick();

    let residential = game.inspect(1, 0).cell.expect("residential cell");
    assert_eq!(residential.powered, Some(false));
    assert_eq!(residential.population, Some(0));
    assert_eq!(game.view().status.power.total_supplied, 0);
}

#[test]
fn disconnected_road_network_does_not_receive_power() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(5, 5, BuildingKind::Road).success);
    assert!(game.build(5, 4, BuildingKind::Residential).success);
    assert!(game.build(6, 5, BuildingKind::Road).success);
    assert!(game.build(6, 4, BuildingKind::Commercial).success);

    game.tick();

    let residential = game.inspect(5, 4).cell.expect("residential cell");
    assert_eq!(residential.road_connected, Some(true));
    assert_eq!(residential.powered, Some(false));
    assert_eq!(residential.population, Some(0));
}

#[test]
fn power_capacity_limits_consumers() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    for x in 1..5 {
        assert!(game.build(x, 0, BuildingKind::Industrial).success);
    }
    assert!(game.build(5, 0, BuildingKind::Commercial).success);

    game.tick();

    let status = game.view().status.power;
    assert_eq!(status.total_capacity, 10);
    assert!(status.total_demand > status.total_capacity);
    assert!(status.total_shortage > 0);

    assert_eq!(inspect_powered(&game, 1, 0), Some(true));
    assert_eq!(inspect_powered(&game, 2, 0), Some(true));
    assert_eq!(inspect_powered(&game, 3, 0), Some(true));
    assert_eq!(inspect_powered(&game, 4, 0), Some(false));
    assert_eq!(inspect_powered(&game, 5, 0), Some(false));
    assert!(!all_consumers_powered(&game.view()));
}

#[test]
fn multiple_power_plants_on_same_network_combine_capacity() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 2, BuildingKind::PowerPlant).success);
    for x in 0..5 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    for x in 1..5 {
        assert!(game.build(x, 0, BuildingKind::Industrial).success);
    }

    game.tick();

    let status = game.view().status.power;
    assert_eq!(status.total_capacity, 20);
    assert_eq!(status.total_demand, 12);
    assert_eq!(status.total_shortage, 0);
    for x in 1..5 {
        assert_eq!(inspect_powered(&game, x, 0), Some(true));
    }
}

#[test]
fn save_load_preserves_power_network_behavior() {
    let path = save_path("power-network");
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..4 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);

    game.tick();
    game.save_to_file(&path).expect("save succeeds");

    let mut loaded = Game::load_from_file(&path).expect("load succeeds");
    loaded.tick();

    let view = loaded.view();
    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
    assert_eq!(view.status.power.total_capacity, 10);
    assert_eq!(view.status.power.total_demand, 3);
    assert_eq!(view.status.power.total_shortage, 0);
    assert_eq!(inspect_powered(&loaded, 1, 0), Some(true));
    assert_eq!(inspect_powered(&loaded, 2, 0), Some(true));
    assert_eq!(
        loaded
            .inspect(1, 0)
            .cell
            .expect("residential cell")
            .population,
        Some(2)
    );

    std::fs::remove_file(path).expect("remove save file");
}

fn cell(view: &GameView, x: usize, y: usize) -> &CellView {
    &view.map.cells[y * view.map.width + x]
}

fn inspect_powered(game: &Game, x: usize, y: usize) -> Option<bool> {
    match game.inspect(x, y).details {
        Some(InspectDetailsView::Residential { powered, .. })
        | Some(InspectDetailsView::Commercial { powered, .. })
        | Some(InspectDetailsView::Industrial { powered, .. }) => Some(powered),
        _ => None,
    }
}

fn all_consumers_powered(view: &GameView) -> bool {
    view.map
        .cells
        .iter()
        .filter_map(|cell| cell.powered)
        .all(|powered| powered)
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
