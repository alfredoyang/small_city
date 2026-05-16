//! Integration tests for maintenance costs, income, and tick economy breakdowns.

use small_city::core::game::Game;
use small_city::interface::events::{EconomyBreakdownView, GameEventView, MetricChange};
use small_city::interface::input::BuildingKind;

#[test]
fn powered_industrial_income_is_reduced_by_maintenance() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert_eq!(game.view().status.money, 68);

    game.tick();

    assert_eq!(game.view().status.money, 69);
}

#[test]
fn unproductive_buildings_still_have_maintenance_costs() {
    let mut game = Game::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Park).success);
    assert_eq!(game.view().status.money, 74);

    game.tick();

    assert_eq!(game.view().status.money, 72);
}

#[test]
fn build_options_expose_maintenance_costs_to_ui() {
    let game = Game::new(2, 2);
    let view = game.view();

    let power_plant = view
        .build_options
        .iter()
        .find(|option| option.kind == BuildingKind::PowerPlant)
        .expect("power plant build option");
    let residential = view
        .build_options
        .iter()
        .find(|option| option.kind == BuildingKind::Residential)
        .expect("residential build option");

    assert_eq!(power_plant.maintenance_cost, 1);
    assert_eq!(residential.maintenance_cost, 0);
}

#[test]
fn tick_event_exposes_economy_breakdown() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);

    let result = game.tick();

    assert_eq!(
        result.event,
        GameEventView::TickSummary {
            turn: 1,
            population: MetricChange {
                before: 0,
                after: 0
            },
            money: MetricChange {
                before: 68,
                after: 69
            },
            happiness: MetricChange {
                before: 48,
                after: 48
            },
            pollution: MetricChange {
                before: 2,
                after: 2
            },
            unemployment: MetricChange {
                before: 0,
                after: 0
            },
            powered_buildings: MetricChange {
                before: 1,
                after: 1
            },
            economy: EconomyBreakdownView {
                population_income: 0,
                commercial_income: 0,
                industrial_income: 3,
                maintenance_cost: 2,
                net: 1
            },
        }
    );
}
