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

/// DT3 moves local job matching into the derived pass. A paused workplace build
/// should therefore update the citizen's local assignment in the view without
/// settling salary, taxes, rent, or maintenance.
#[test]
fn paused_workplace_build_updates_local_job_assignment_without_a_tick() {
    let mut game = SingleRegionTestGame::new(5, 4);
    build_growth_city(&mut game);
    advance_one_day(&mut game);
    assert_eq!(assignment_count(&game.view(), 1, 0), 1);

    assert!(game.bulldoze(2, 0).success);
    assert_eq!(assignment_count(&game.view(), 1, 0), 0);

    let money_before = game.view().status.money;
    assert!(game.build(2, 0, BuildingKind::Commercial).success);

    let view = game.view();
    assert_eq!(view.status.turn, 24);
    assert_eq!(assignment_count(&view, 1, 0), 1);
    assert!(
        view.status.money < money_before,
        "building cost is the only paused money change"
    );
}

/// A mid-day workplace config change may update derived assignments immediately,
/// but salary/tax/rent/maintenance stay frozen until the next daily boundary.
#[test]
fn midday_workplace_change_does_not_settle_money_until_daily_boundary() {
    let mut game = SingleRegionTestGame::new(5, 4);
    build_growth_city(&mut game);
    advance_one_day(&mut game);

    assert!(game.bulldoze(2, 0).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    let after_build_money = game.view().status.money;

    assert!(game.tick().success);
    let after_midday_tick = game.view();
    assert_eq!(after_midday_tick.status.turn, 25);
    assert_eq!(after_midday_tick.status.money, after_build_money);

    for _ in 0..23 {
        assert!(game.tick().success);
    }
    assert_ne!(
        game.view().status.money,
        after_build_money,
        "daily economy should settle at the next day boundary"
    );
}

/// DT3 should keep the running simulation stable: the assignment visible before
/// economy settlement is the one salary/tax uses at the next daily boundary.
#[test]
fn money_and_assignments_match_scripted_tick_values_after_dt3_split() {
    let mut game = SingleRegionTestGame::new(5, 4);
    build_growth_city(&mut game);

    advance_one_day(&mut game);
    assert_eq!(assignment_count(&game.view(), 1, 0), 1);
    assert_eq!(game.view().status.money, 65);

    advance_one_day(&mut game);
    assert_eq!(assignment_count(&game.view(), 1, 0), 2);
    assert_eq!(game.view().status.money, 71);
}

/// DT2 splits derived happiness target from actual happiness. Paused config
/// changes can move the target immediately, but actual citizen happiness remains
/// a time-pass value and changes only when a tick runs.
#[test]
fn paused_amenity_change_moves_happiness_target_without_actual_happiness() {
    let mut game = SingleRegionTestGame::new(5, 4);
    build_growth_city(&mut game);
    advance_one_day(&mut game);

    let before = game.view();
    assert!(
        before.status.citizens > 0,
        "the fixture must have a citizen before testing happiness"
    );
    let before_actual = before
        .status
        .average_citizen_happiness
        .expect("actual happiness");
    let before_target = before
        .status
        .average_citizen_happiness_target
        .expect("target happiness");

    assert!(game.build(1, 2, BuildingKind::Park).success);

    let after = game.view();
    assert_eq!(after.status.turn, before.status.turn);
    assert_eq!(
        after.status.average_citizen_happiness,
        Some(before_actual),
        "actual happiness must not move while paused"
    );
    let after_target = after
        .status
        .average_citizen_happiness_target
        .expect("target after");
    assert!(
        after_target > before_target,
        "adding an amenity should raise the derived happiness target, before {before_target}, after {after_target}"
    );
}

/// DT2 should not change actual happiness during normal ticking. This pins a
/// scripted run where a citizen grows, a paused amenity changes only the target,
/// then the next daily tick applies the same actual-happiness formula as before:
/// target minus accumulated daily decay.
#[test]
fn actual_happiness_matches_scripted_tick_values_after_h2_split() {
    let mut game = SingleRegionTestGame::new(5, 4);
    build_growth_city(&mut game);

    advance_one_day(&mut game);
    assert_happiness(&game.view(), Some(75), Some(76));

    assert!(game.build(1, 2, BuildingKind::Park).success);
    assert_happiness(&game.view(), Some(75), Some(92));

    advance_one_day(&mut game);
    assert_happiness(&game.view(), Some(85), Some(87));
}

fn build_growth_city(game: &mut SingleRegionTestGame) {
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
}

fn assert_happiness(view: &GameView, actual: Option<i32>, target: Option<i32>) {
    assert_eq!(
        view.status.average_citizen_happiness, actual,
        "actual happiness mismatch at turn {}",
        view.status.turn
    );
    assert_eq!(
        view.status.average_citizen_happiness_target, target,
        "target happiness mismatch at turn {}",
        view.status.turn
    );
}

fn assignment_count(view: &GameView, x: usize, y: usize) -> usize {
    view.map
        .cells
        .iter()
        .find(|cell| cell.x == x && cell.y == y)
        .map(|cell| cell.job_assignments.len())
        .unwrap_or(0)
}

fn advance_one_day(game: &mut SingleRegionTestGame) {
    for _ in 0..24 {
        assert!(game.tick().success);
    }
}
