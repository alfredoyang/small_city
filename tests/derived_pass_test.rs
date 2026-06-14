//! DT1 integration tests: the derived pass is visible while paused.
//!
//! A config command (build/bulldoze/upgrade) only marks the derived state dirty;
//! the next view read recomputes the derived pass, so power, job counts, and
//! stats update with no tick. The time pass (money, population, turn) stays frozen
//! until a tick advances it. The running-sim parity is covered by the existing
//! scenario/registry suites, which pass unchanged.

mod common;

use common::SingleRegionTestGame;
use small_city::interface::input::BuildingKind;
use small_city::interface::view::GameView;

fn cell_powered(view: &GameView, x: usize, y: usize) -> Option<bool> {
    view.map
        .cells
        .iter()
        .find(|cell| cell.x == x && cell.y == y)
        .and_then(|cell| cell.powered)
}

/// Build a plant + road + commercial, then read the view with no tick: DT1 makes
/// the commercial show as powered immediately, where the old model needed a tick.
#[test]
fn paused_build_powers_a_building_in_the_view_without_a_tick() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);

    // No tick has run.
    let view = game.view();
    assert_eq!(view.status.turn, 0, "the time pass must not have advanced");
    assert_eq!(
        cell_powered(&view, 1, 0),
        Some(true),
        "a paused build must power the commercial in the view"
    );
}

/// The job count is part of the derived pass (effective workplaces gate on power),
/// so it too updates in a paused view once the workplace is powered.
#[test]
fn paused_build_updates_job_count_in_the_view_without_a_tick() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert_eq!(game.view().status.jobs, 0);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);

    let view = game.view();
    assert_eq!(view.status.turn, 0, "the time pass must not have advanced");
    assert!(
        view.status.jobs > 0,
        "a paused build of a powered workplace must surface its job slots, got {}",
        view.status.jobs
    );
}

/// Bulldozing the plant while paused must drop the commercial back to unpowered in
/// the view, proving the derived pass recomputes on every config change, not once.
#[test]
fn paused_bulldoze_unpowers_a_building_in_the_view_without_a_tick() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);
    assert_eq!(cell_powered(&game.view(), 1, 0), Some(true));

    assert!(game.bulldoze(0, 0).success);

    let view = game.view();
    assert_eq!(view.status.turn, 0, "the time pass must not have advanced");
    assert_eq!(
        cell_powered(&view, 1, 0),
        Some(false),
        "removing the only plant must unpower the commercial in the paused view"
    );
}

/// The time pass stays frozen while paused: repeated views and paused builds never
/// advance the turn or grow population. Only a tick does.
#[test]
fn paused_commands_do_not_advance_the_time_pass() {
    let mut game = SingleRegionTestGame::new(5, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);

    // Reading the derived view repeatedly must not advance time or grow citizens.
    for _ in 0..5 {
        let view = game.view();
        assert_eq!(view.status.turn, 0);
        assert_eq!(view.status.population, 0);
    }

    // A tick advances the time pass.
    assert!(game.tick().success);
    assert_eq!(game.view().status.turn, 1);
}
