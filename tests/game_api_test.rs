//! Integration tests for public Game API tick events and basic simulation effects.

use small_city::core::game::Game;
use small_city::interface::events::{EconomyBreakdownView, GameEventView, MetricChange};
use small_city::interface::input::BuildingKind;

#[test]
fn industrial_creates_pollution() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::Industrial).success);

    game.tick();

    assert_eq!(game.view().status.pollution, 2);
}

#[test]
fn park_reduces_pollution_effect() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::Industrial).success);
    assert!(game.build(1, 0, BuildingKind::Park).success);

    game.tick();

    assert_eq!(game.view().status.pollution, 1);
}

#[test]
fn happiness_includes_park_bonus() {
    let mut high_happiness = Game::new(10, 10);
    for x in 0..10 {
        assert!(high_happiness.build(x, 0, BuildingKind::Park).success);
    }
    high_happiness.tick();
    assert_eq!(high_happiness.view().status.happiness, 80);
}

#[test]
fn tick_advances_turn_deterministically() {
    let mut game = Game::new(10, 10);

    game.tick();
    game.tick();

    assert_eq!(game.view().status.turn, 2);
}

#[test]
fn tick_returns_structured_summary_events() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::Park).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);

    let result = game.tick();

    assert!(result.success);
    assert_eq!(result.events.len(), 1);
    assert_eq!(result.event, result.events[0]);
    assert_eq!(
        result.events[0],
        GameEventView::TickSummary {
            turn: 1,
            population: MetricChange {
                before: 0,
                after: 2
            },
            money: MetricChange {
                before: 47,
                after: 53
            },
            happiness: MetricChange {
                before: 52,
                after: 55
            },
            pollution: MetricChange {
                before: 1,
                after: 1
            },
            unemployment: MetricChange {
                before: 0,
                after: 0
            },
            powered_buildings: MetricChange {
                before: 3,
                after: 3
            },
            economy: EconomyBreakdownView {
                salaries_paid: 6,
                workplace_tax: 2,
                rent_income: 4,
                commercial_sales_tax: 4,
                shoppers_served: 2,
                rent_failures: 0,
                maintenance_cost: 4,
                net: 6
            },
        }
    );
}

#[test]
fn tick_summary_message_includes_metric_changes() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);

    let message = game.tick().message();

    assert!(message.contains("population 1 (+1)"));
    assert!(message.contains("money 54 (+1)"));
    assert!(message.contains("powered buildings 3 (+0)"));
    assert!(
        message.contains(
            "Economy: salaries paid 3, workplace tax +1, rent +2, sales tax +1, shoppers 1, rent failures 0, maintenance -3, net +1"
        )
    );
}
