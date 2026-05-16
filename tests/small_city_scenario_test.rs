//! Longer scenario tests combining multiple systems over many deterministic turns.

use small_city::core::game::Game;
use small_city::interface::events::{EconomyBreakdownView, GameEventView};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use small_city::interface::input::{BuildingKind, MapOverlayInput};

#[test]
fn powered_residential_and_commercial_city_grows_over_five_ticks() {
    let mut game = Game::new(10, 10);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);

    let starting_view = game.view();
    let starting_money = starting_view.status.money;
    let starting_population = starting_view.status.population;

    let mut economy_total = EconomyTotals::default();
    for _ in 0..5 {
        let result = game.tick();
        assert!(result.success);
        economy_total.add(tick_economy(&result.event));
    }

    let view = game.view();

    assert!(view.status.population > starting_population);
    assert_eq!(view.status.turn, 5);
    assert_eq!(view.status.money, starting_money + economy_total.net);
    assert!(economy_total.salaries_paid > 0);
    assert!(economy_total.workplace_tax > 0);
    assert!(economy_total.rent_income > 0);
    assert!(economy_total.commercial_sales_tax > 0);
    assert!(economy_total.shoppers_served > 0);
    assert_eq!(economy_total.rent_failures, 0);
    assert_eq!(economy_total.maintenance_cost, 10);
    assert!((0..=100).contains(&view.status.happiness));

    // The UI contract stays intact after a multi-system scenario.
    assert_eq!(view.map.width, 10);
    assert_eq!(view.map.height, 10);
    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
}

#[test]
fn upgraded_powered_city_remains_stable_over_twelve_ticks() {
    let mut game = Game::new(10, 10);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    for x in 1..=5 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::Park).success);

    assert!(game.upgrade(0, 0).success);
    assert!(game.upgrade(1, 0).success);
    assert!(game.upgrade(4, 0).success);

    let starting_money = game.view().status.money;
    let mut economy_total = EconomyTotals::default();
    for _ in 0..12 {
        let result = game.tick();
        assert!(result.success);
        economy_total.add(tick_economy(&result.event));
    }

    let view = game.view();
    let residential = game.inspect(1, 0).cell.expect("residential cell");
    let power_overlay = game.view_with_overlay(MapOverlayInput::Power);

    assert_eq!(view.status.turn, 12);
    assert_eq!(view.status.money, starting_money + economy_total.net);
    assert_eq!(view.status.power.total_capacity, 15);
    assert_eq!(view.status.power.total_shortage, 0);
    assert_eq!(residential.max_population, Some(8));
    assert!(residential.population.expect("population") > 0);
    assert!(economy_total.salaries_paid > 0);
    assert!(economy_total.workplace_tax > 0);
    assert!(economy_total.rent_income > 0);
    assert!(economy_total.commercial_sales_tax > 0);
    assert!(economy_total.shoppers_served > 0);
    assert!(economy_total.maintenance_cost > 0);
    assert_eq!(
        power_overlay.map.cells.len(),
        view.map.width * view.map.height
    );
    assert!((0..=100).contains(&view.status.happiness));
}

#[test]
fn replace_bulldoze_save_load_scenario_continues_for_twenty_ticks() {
    let path = save_path("long-scenario");
    let mut game = Game::new(12, 12);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..=6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::Park).success);

    for _ in 0..6 {
        assert!(game.tick().success);
    }

    assert!(game.replace(2, 0, BuildingKind::Residential).success);
    assert!(game.bulldoze(3, 0).success);
    assert!(game.build(5, 0, BuildingKind::Commercial).success);
    assert!(game.upgrade(1, 0).success);

    game.save_to_file(&path).expect("save long scenario");
    let mut loaded = Game::load_from_file(&path).expect("load long scenario");
    std::fs::remove_file(&path).expect("remove long scenario save");

    let loaded_starting_money = loaded.view().status.money;
    let mut loaded_economy_total = EconomyTotals::default();
    for _ in 0..14 {
        let result = loaded.tick();
        assert!(result.success);
        loaded_economy_total.add(tick_economy(&result.event));
    }

    let view = loaded.view();
    let first_residential = loaded.inspect(1, 0).cell.expect("first residential");
    let second_residential = loaded.inspect(2, 0).cell.expect("second residential");

    assert_eq!(view.status.turn, 20);
    assert_eq!(
        view.status.money,
        loaded_starting_money + loaded_economy_total.net
    );
    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
    assert_eq!(first_residential.building, Some(BuildingKind::Residential));
    assert_eq!(first_residential.upgrade_level, Some(2));
    assert_eq!(second_residential.building, Some(BuildingKind::Residential));
    assert_eq!(
        loaded.inspect(3, 0).cell.expect("bulldozed cell").building,
        None
    );
    assert!(view.status.population >= 1);
    assert!(loaded_economy_total.salaries_paid > 0);
    assert!(loaded_economy_total.workplace_tax > 0);
    assert!(loaded_economy_total.rent_income > 0);
    assert!(loaded_economy_total.commercial_sales_tax > 0);
    assert!(loaded_economy_total.shoppers_served > 0);
    assert!(loaded_economy_total.maintenance_cost > 0);
    assert!((0..=100).contains(&view.status.happiness));
}

#[derive(Default)]
struct EconomyTotals {
    salaries_paid: i32,
    workplace_tax: i32,
    rent_income: i32,
    commercial_sales_tax: i32,
    shoppers_served: i32,
    rent_failures: i32,
    maintenance_cost: i32,
    net: i32,
}

impl EconomyTotals {
    fn add(&mut self, breakdown: EconomyBreakdownView) {
        self.salaries_paid += breakdown.salaries_paid;
        self.workplace_tax += breakdown.workplace_tax;
        self.rent_income += breakdown.rent_income;
        self.commercial_sales_tax += breakdown.commercial_sales_tax;
        self.shoppers_served += breakdown.shoppers_served;
        self.rent_failures += breakdown.rent_failures;
        self.maintenance_cost += breakdown.maintenance_cost;
        self.net += breakdown.net;
    }
}

fn tick_economy(event: &GameEventView) -> EconomyBreakdownView {
    match event {
        GameEventView::TickSummary { economy, .. } => *economy,
        other => panic!("expected tick summary event, got {other:?}"),
    }
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
