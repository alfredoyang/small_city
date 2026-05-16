//! Integration tests for maintenance costs, income, and tick economy breakdowns.

use small_city::core::game::Game;
use small_city::interface::events::{EconomyBreakdownView, GameEventView, MetricChange};
use small_city::interface::input::BuildingKind;

#[test]
fn workplace_without_citizen_workers_pays_no_tax_but_still_has_maintenance() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert_eq!(game.view().status.money, 68);

    game.tick();

    assert_eq!(game.view().status.money, 66);
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
                after: 66
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
                salaries_paid: 0,
                workplace_tax: 0,
                rent_income: 0,
                commercial_sales_tax: 0,
                shoppers_served: 0,
                rent_failures: 0,
                maintenance_cost: 2,
                net: -2
            },
        }
    );
}

#[test]
fn citizen_salary_rent_and_shopping_create_city_tax_income() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);

    let result = game.tick();

    assert!(matches!(
        result.event,
        GameEventView::TickSummary {
            economy: EconomyBreakdownView {
                salaries_paid: 3,
                workplace_tax: 1,
                rent_income: 1,
                commercial_sales_tax: 1,
                shoppers_served: 1,
                rent_failures: 0,
                maintenance_cost: 2,
                net: 1,
            },
            ..
        }
    ));
    assert_eq!(game.view().status.citizens, 1);
    assert_eq!(game.view().status.money, 65);
}

#[test]
fn commercial_without_shoppers_pays_no_sales_tax() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);

    let result = game.tick();

    assert!(matches!(
        result.event,
        GameEventView::TickSummary {
            economy: EconomyBreakdownView {
                salaries_paid: 0,
                workplace_tax: 0,
                rent_income: 0,
                commercial_sales_tax: 0,
                shoppers_served: 0,
                rent_failures: 0,
                maintenance_cost: 2,
                net: -2,
            },
            ..
        }
    ));
}

#[test]
fn disconnected_commercial_does_not_receive_shoppers_or_pay_sales_tax() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(5, 0, BuildingKind::Industrial).success);
    assert!(game.build(8, 0, BuildingKind::Commercial).success);
    for x in 0..=5 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let result = game.tick();

    let economy = tick_economy(&result.event);
    assert!(economy.salaries_paid > 0);
    assert!(economy.workplace_tax > 0);
    assert!(economy.rent_income > 0);
    assert_eq!(economy.commercial_sales_tax, 0);
    assert_eq!(economy.shoppers_served, 0);
    assert_eq!(economy.rent_failures, 0);
    assert_eq!(economy.maintenance_cost, 3);
}

#[test]
fn bulldozing_workplace_road_stops_future_salary_and_shopping() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);

    let first_tick = game.tick();
    assert!(matches!(
        first_tick.event,
        GameEventView::TickSummary {
            economy: EconomyBreakdownView {
                salaries_paid: 3,
                workplace_tax: 1,
                rent_income: 1,
                commercial_sales_tax: 1,
                shoppers_served: 1,
                rent_failures: 0,
                maintenance_cost: 2,
                net: 1,
            },
            ..
        }
    ));

    assert!(game.bulldoze(2, 1).success);
    let second_tick = game.tick();

    assert!(matches!(
        second_tick.event,
        GameEventView::TickSummary {
            economy: EconomyBreakdownView {
                salaries_paid: 0,
                workplace_tax: 0,
                rent_income: 1,
                commercial_sales_tax: 0,
                shoppers_served: 0,
                rent_failures: 0,
                maintenance_cost: 2,
                net: -1,
            },
            ..
        }
    ));
}

fn tick_economy(event: &GameEventView) -> EconomyBreakdownView {
    match event {
        GameEventView::TickSummary { economy, .. } => *economy,
        other => panic!("expected tick summary event, got {other:?}"),
    }
}
