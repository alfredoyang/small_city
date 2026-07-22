//! Longer scenario tests combining multiple systems over many deterministic turns.

mod common;

use common::SingleRegionTestGame;
use small_city::interface::events::{EconomyBreakdownView, GameEventView};
use small_city::interface::view::InspectDetailsView;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use small_city::interface::input::{BuildingKind, MapOverlayInput};

#[test]
fn powered_residential_and_commercial_city_grows_over_one_week() {
    let mut game = SingleRegionTestGame::new(10, 10);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);

    let starting_view = game.view();
    let starting_money = starting_view.status.money;
    let starting_population = starting_view.status.population;

    let economy_total = advance_one_week(&mut game);

    let view = game.view();

    assert!(view.status.population > starting_population);
    assert_eq!(view.status.turn, 24 * 7);
    assert_eq!(view.status.money, starting_money + economy_total.net);
    assert!(economy_total.salaries_paid > 0);
    assert!(economy_total.workplace_tax > 0);
    assert!(economy_total.rent_income > 0);
    // New residents can miss rent before their first completed commute makes
    // them eligible for payroll. The healthy starter city has only this bounded
    // startup stress, not sustained rent failure.
    assert!(economy_total.rent_failures <= 2);
    assert!(economy_total.maintenance_cost > 0);
    assert!((0..=100).contains(&view.status.happiness));

    // The UI contract stays intact after a multi-system scenario.
    assert_eq!(view.map.width, 10);
    assert_eq!(view.map.height, 10);
    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
}

#[test]
fn upgraded_powered_city_remains_stable_over_one_week() {
    let mut game = SingleRegionTestGame::new(10, 10);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..=5 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    // Gap at (2,0) lets the residential grow its footprint when it upgrades.
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(3, 0, BuildingKind::Commercial).success);
    assert!(game.build(4, 0, BuildingKind::Industrial).success);
    assert!(game.build(5, 0, BuildingKind::Park).success);

    assert!(game.upgrade(0, 0).success);
    assert!(game.upgrade(1, 0).success);
    assert!(game.upgrade(5, 0).success);

    let starting_money = game.view().status.money;
    let economy_total = advance_one_week(&mut game);

    let view = game.view();
    let residential = game.inspect(1, 0).cell.expect("residential cell");
    let power_overlay = game.view_with_overlay(MapOverlayInput::Power);

    assert_eq!(view.status.turn, 24 * 7);
    assert_eq!(view.status.money, starting_money + economy_total.net);
    assert_eq!(view.status.power.total_capacity, 15);
    assert_eq!(view.status.power.total_shortage, 0);
    // Upgrading grew the residential to a 2-cell footprint: capacity_for(Residential, 2) = 15.
    assert_eq!(residential.max_population, Some(15));
    assert!(residential.population.expect("population") > 0);
    assert!(economy_total.salaries_paid > 0);
    assert!(economy_total.workplace_tax > 0);
    assert!(economy_total.rent_income > 0);
    assert!(economy_total.maintenance_cost > 0);
    assert_eq!(
        power_overlay.map.cells.len(),
        view.map.width * view.map.height
    );
    assert!((0..=100).contains(&view.status.happiness));
}

#[test]
fn replace_bulldoze_save_load_scenario_continues_for_two_weeks() {
    let path = save_path("long-scenario");
    let mut game = SingleRegionTestGame::new(12, 12);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..=6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::Park).success);

    advance_one_week(&mut game);

    assert!(game.replace(2, 0, BuildingKind::Residential).success);
    assert!(game.bulldoze(3, 0).success);
    assert!(game.build(5, 0, BuildingKind::Commercial).success);
    assert!(game.upgrade(1, 0).success);

    game.save_to_file(&path).expect("save long scenario");
    let mut loaded = SingleRegionTestGame::load_from_file(&path).expect("load long scenario");
    std::fs::remove_file(&path).expect("remove long scenario save");

    let loaded_starting_money = loaded.view().status.money;
    let loaded_economy_total = advance_one_week(&mut loaded);

    let view = loaded.view();
    let first_residential = loaded.inspect(1, 0).cell.expect("first residential");
    let second_residential = loaded.inspect(2, 0).cell.expect("second residential");

    assert_eq!(view.status.turn, 24 * 7 * 2);
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
    assert!(loaded_economy_total.maintenance_cost > 0);
    assert!((0..=100).contains(&view.status.happiness));
}

#[test]
fn connected_economy_loop_runs_over_many_turns_after_upgrade_and_save_load() {
    let path = save_path("long-economy-loop");
    let mut game = SingleRegionTestGame::new(12, 12);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..=6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Residential).success);
    assert!(game.build(3, 0, BuildingKind::Commercial).success);
    assert!(game.build(4, 0, BuildingKind::Industrial).success);
    assert!(game.build(5, 0, BuildingKind::Park).success);

    let starting_rent = residential_rent(&game, 1, 0);
    let starting_power_maintenance = building_maintenance(&game, 0, 0);
    let starting_population = game.view().status.population;

    advance_one_week(&mut game);

    assert!(
        game.view().status.population > starting_population,
        "the connected city should grow before upgrades"
    );

    assert!(game.upgrade(0, 0).success);
    assert!(game.upgrade(1, 0).success);
    assert!(game.upgrade(5, 0).success);

    let upgraded_rent = residential_rent(&game, 1, 0);
    let upgraded_power_maintenance = building_maintenance(&game, 0, 0);
    assert!(
        upgraded_rent > starting_rent,
        "residential upgrade should increase rent from {starting_rent}, got {upgraded_rent}"
    );
    assert!(
        upgraded_power_maintenance > starting_power_maintenance,
        "power plant upgrade should increase maintenance"
    );

    let money_before_save = game.view().status.money;
    let pre_save_economy = advance_one_week(&mut game);
    assert_eq!(
        game.view().status.money,
        money_before_save + pre_save_economy.net
    );

    game.save_to_file(&path)
        .expect("save long economy scenario");
    let mut loaded =
        SingleRegionTestGame::load_from_file(&path).expect("load long economy scenario");
    std::fs::remove_file(&path).expect("remove long economy save");

    let loaded_starting_money = loaded.view().status.money;
    let post_load_economy = advance_one_week(&mut loaded);

    let view = loaded.view();
    let total_economy = pre_save_economy.plus(post_load_economy);
    let upgraded_home = loaded.inspect(1, 0).cell.expect("upgraded home cell");
    let commercial_tax = commercial_sales_tax(&loaded, 3, 0);

    assert_eq!(view.status.turn, 24 * 7 * 3);
    assert_eq!(
        view.status.money,
        loaded_starting_money + post_load_economy.net
    );
    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
    assert_eq!(upgraded_home.upgrade_level, Some(2));
    assert!(residential_rent(&loaded, 1, 0) >= upgraded_rent);
    assert!(commercial_tax > 1);
    assert!(total_economy.salaries_paid > 0);
    assert!(total_economy.workplace_tax > 0);
    assert!(total_economy.rent_income > 0);
    assert!(total_economy.commercial_sales_tax > 0);
    assert!(total_economy.shoppers_served > 0);
    // The long scenario proves the goods economy remains active across upgrades
    // and save/load: factories produce local goods, commercial buildings sell
    // delivered goods, and manufacturing/export tax contributes to the budget.
    assert!(total_economy.local_goods_produced > 0);
    assert!(total_economy.local_goods_stored > 0);
    assert!(total_economy.local_goods_sold > 0);
    assert!(total_economy.manufacturing_tax > 0);
    assert!(total_economy.maintenance_cost > 0);
    assert!(
        total_economy.rent_failures < total_economy.rent_income,
        "the long economy should mostly sustain rent payments"
    );
    assert!((0..=100).contains(&view.status.happiness));
}

#[test]
fn stable_starter_city_stays_in_sane_ranges_and_locks_reinvestment_pacing() {
    // Buildings sit on row 1 with the road along row 2, so the commercial and industrial each have
    // a clear 2x2 zone (rows 0-1) to grow into and can reach level 3 (their footprints anchor at
    // (4,0) and (7,0) after growing N then E).
    let mut game = SingleRegionTestGame::new(12, 12);

    assert!(game.build(0, 1, BuildingKind::PowerPlant).success);
    for x in 0..=8 {
        assert!(game.build(x, 2, BuildingKind::Road).success);
    }
    for x in 1..=3 {
        assert!(game.build(x, 1, BuildingKind::Residential).success);
    }
    assert!(game.build(4, 1, BuildingKind::Commercial).success);
    assert!(game.build(7, 1, BuildingKind::Industrial).success);

    let starting_money = game.view().status.money;
    let first_week = advance_one_week(&mut game);
    assert_eq!(
        building_upgrade_level(&game, 4, 1),
        2,
        "commercial should reach level 2 after the first week"
    );
    assert_eq!(
        building_upgrade_level(&game, 7, 1),
        2,
        "industrial should reach level 2 after one profitable starter-city week"
    );
    let second_week = advance_one_week(&mut game);
    assert_eq!(
        building_upgrade_level(&game, 4, 1),
        3,
        "commercial should reach level 3 after the second week"
    );
    assert_eq!(
        building_upgrade_level(&game, 7, 1),
        3,
        "industrial should reach level 3 after the second week"
    );
    let third_week = advance_one_week(&mut game);
    assert_eq!(
        building_upgrade_level(&game, 4, 1),
        3,
        "commercial should stay level 3 after the third week"
    );
    assert_eq!(
        building_upgrade_level(&game, 7, 1),
        3,
        "industrial should stay level 3 after the third week"
    );
    let economy = first_week.plus(second_week).plus(third_week);
    let view = game.view();

    assert_eq!(view.status.turn, 24 * 7 * 3);
    assert_eq!(view.status.money, starting_money + economy.net);
    // Goods export and manufacturing taxes make even a compact starter city strongly profitable,
    // and area-based capacity makes upgraded buildings job/pop-dense, so these use deliberately
    // broad caps (a 2x2 commercial + 2x2 industrial alone supply 12 + 24 = 36 jobs).
    assert_in_range("money", view.status.money, 20, 3_000);
    assert_in_range("population", view.status.population, 3, 40);
    assert_in_range("jobs", view.status.jobs, 5, 50);
    assert_in_range("unemployment", view.status.unemployment, 0, 10);
    assert_in_range("happiness", view.status.happiness, 35, 100);
    assert_in_range("pollution", view.status.pollution, 0, 10);
    assert_eq!(view.status.power.total_shortage, 0);
    assert_eq!(
        view.status.power.total_supplied,
        view.status.power.total_demand
    );
    assert!(economy.net > 0);
    assert!(economy.rent_failures < economy.rent_income);
    assert!(economy.local_goods_produced > 0);
}

#[test]
fn overbuilt_maintenance_pressure_city_blocks_reinvestment_without_demand() {
    let mut game = SingleRegionTestGame::new(12, 12);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..=7 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    for x in 1..=4 {
        assert!(game.build(x, 0, BuildingKind::Commercial).success);
    }
    assert!(game.build(5, 0, BuildingKind::Park).success);
    assert!(game.build(6, 0, BuildingKind::Park).success);

    let starting_money = game.view().status.money;
    let first_week = advance_one_week(&mut game);
    for x in 1..=4 {
        assert_eq!(
            building_upgrade_level(&game, x, 0),
            1,
            "commercial at ({x}, 0) should not auto-upgrade without population demand"
        );
    }
    let second_week = advance_one_week(&mut game);
    for x in 1..=4 {
        assert_eq!(
            building_upgrade_level(&game, x, 0),
            1,
            "commercial at ({x}, 0) should stay level 1 under overbuilt no-demand pressure"
        );
    }
    let economy = first_week.plus(second_week);
    let view = game.view();

    assert_eq!(view.status.turn, 24 * 7 * 2);
    assert_eq!(view.status.money, starting_money + economy.net);
    assert_eq!(view.status.population, 0);
    assert_eq!(view.status.unemployment, 0);
    assert_eq!(view.status.power.total_shortage, 0);
    assert!(economy.maintenance_cost > 0);
    assert_eq!(economy.rent_income, 0);
    assert_eq!(economy.commercial_sales_tax, 0);
    assert!(
        economy.net < 0,
        "overbuilt shops and parks without population should create budget pressure"
    );
    assert!(
        view.status.money < starting_money,
        "money should fall from {starting_money}, got {}",
        view.status.money
    );
    assert!(
        view.status.money <= 0,
        "current v0.4 allows money to go negative instead of blocking maintenance; got {}",
        view.status.money
    );
    assert!((0..=100).contains(&view.status.happiness));
}

#[test]
fn polluted_industrial_city_locks_fast_industrial_reinvestment_pacing() {
    let mut game = SingleRegionTestGame::new(12, 12);

    // Buildings sit on row 1 with the road along row 2 so each industrial has a clear 2x2 zone to
    // grow into; the two industrials are spaced apart (anchors end at (5,0) and (8,0)) so they grow
    // independently instead of merging into one.
    assert!(game.build(0, 1, BuildingKind::PowerPlant).success);
    // The road reaches both map edges (x=0 and x=11) so each industrial has a short export route
    // and stays profitable; otherwise the far one earns no export margin and never reinvests.
    for x in 0..=11 {
        assert!(game.build(x, 2, BuildingKind::Road).success);
    }
    for x in 1..=4 {
        assert!(game.build(x, 1, BuildingKind::Residential).success);
    }
    assert!(game.build(5, 1, BuildingKind::Industrial).success);
    assert!(game.build(8, 1, BuildingKind::Industrial).success);

    let first_week = advance_one_week(&mut game);
    // Both industrials grow to a 2x2 (footprints anchor at (5,0)/(8,0); their original build cells
    // (5,1)/(8,1) stay in the footprint at every level). Pollution paces reinvestment, so reaching
    // the level-3 cap takes several profitable weeks rather than happening at once.
    let mut economy = first_week;
    for _ in 0..11 {
        if building_upgrade_level(&game, 5, 1) >= 3 && building_upgrade_level(&game, 8, 1) >= 3 {
            break;
        }
        economy = economy.plus(advance_one_week(&mut game));
    }
    assert_eq!(
        building_upgrade_level(&game, 5, 1),
        3,
        "first industrial should reach the reinvestment cap"
    );
    assert_eq!(
        building_upgrade_level(&game, 8, 1),
        3,
        "second industrial should reach the reinvestment cap"
    );
    let view = game.view();
    let polluted_home = game.inspect(4, 1).cell.expect("polluted home cell");
    let edge_home = game.inspect(1, 1).cell.expect("edge home cell");

    // The loop runs a variable number of profitable weeks until both industrials hit the cap.
    assert!(view.status.turn >= 24 * 7 * 2);
    assert_in_range("population", view.status.population, 1, 30);
    assert!(
        view.status.pollution >= 3,
        "industrial-heavy layout should keep visible pollution pressure"
    );
    assert!(
        polluted_home.local_effects.pollution_pressure > edge_home.local_effects.pollution_pressure,
        "home next to industry should show stronger local pollution pressure"
    );
    assert!(
        polluted_home.local_effects.desirability <= edge_home.local_effects.desirability,
        "local pollution should not make the home next to industry more desirable"
    );
    assert!(
        residential_average_happiness(&game, 4, 1) <= residential_average_happiness(&game, 1, 1),
        "residents closest to industry should not be happier than residents farther away"
    );
    assert!((0..=100).contains(&view.status.happiness));
    assert_eq!(view.status.power.total_shortage, 0);
    assert!(economy.local_goods_produced > 0);
    assert!(economy.manufacturing_tax > 0);
}

fn advance_one_week(game: &mut SingleRegionTestGame) -> EconomyTotals {
    // Population and economy both run at daily boundaries; scenario tests still
    // collect full weeks to cover commuting, payroll, business reinvestment, and
    // longer budget pressure.
    advance_weeks(game, 1)
}

fn advance_weeks(game: &mut SingleRegionTestGame, weeks: u32) -> EconomyTotals {
    let mut economy_total = EconomyTotals::default();
    for _ in 0..24 * 7 * weeks {
        for _ in 0..6 {
            if let Some(result) = game.advance() {
                economy_total.add(tick_economy(&result.event));
            }
        }
    }
    economy_total
}

fn assert_in_range(name: &str, value: i32, min: i32, max: i32) {
    assert!(
        (min..=max).contains(&value),
        "{name} should be in {min}..={max}, got {value}"
    );
}

#[derive(Clone, Copy, Default)]
struct EconomyTotals {
    salaries_paid: i32,
    workplace_tax: i32,
    rent_income: i32,
    commercial_sales_tax: i32,
    shoppers_served: i32,
    local_goods_produced: i32,
    local_goods_stored: i32,
    local_goods_sold: i32,
    imported_goods_sold: i32,
    exported_goods: i32,
    manufacturing_tax: i32,
    export_tax: i32,
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
        self.local_goods_produced += breakdown.local_goods_produced;
        self.local_goods_stored += breakdown.local_goods_stored;
        self.local_goods_sold += breakdown.local_goods_sold;
        self.imported_goods_sold += breakdown.imported_goods_sold;
        self.exported_goods += breakdown.exported_goods;
        self.manufacturing_tax += breakdown.manufacturing_tax;
        self.export_tax += breakdown.export_tax;
        self.rent_failures += breakdown.rent_failures;
        self.maintenance_cost += breakdown.maintenance_cost;
        self.net += breakdown.net;
    }

    fn plus(self, other: Self) -> Self {
        Self {
            salaries_paid: self.salaries_paid + other.salaries_paid,
            workplace_tax: self.workplace_tax + other.workplace_tax,
            rent_income: self.rent_income + other.rent_income,
            commercial_sales_tax: self.commercial_sales_tax + other.commercial_sales_tax,
            shoppers_served: self.shoppers_served + other.shoppers_served,
            local_goods_produced: self.local_goods_produced + other.local_goods_produced,
            local_goods_stored: self.local_goods_stored + other.local_goods_stored,
            local_goods_sold: self.local_goods_sold + other.local_goods_sold,
            imported_goods_sold: self.imported_goods_sold + other.imported_goods_sold,
            exported_goods: self.exported_goods + other.exported_goods,
            manufacturing_tax: self.manufacturing_tax + other.manufacturing_tax,
            export_tax: self.export_tax + other.export_tax,
            rent_failures: self.rent_failures + other.rent_failures,
            maintenance_cost: self.maintenance_cost + other.maintenance_cost,
            net: self.net + other.net,
        }
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

fn residential_rent(game: &SingleRegionTestGame, x: usize, y: usize) -> i32 {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::Residential {
            rent_per_citizen, ..
        } => rent_per_citizen,
        other => panic!("expected residential details, got {other:?}"),
    }
}

fn commercial_sales_tax(game: &SingleRegionTestGame, x: usize, y: usize) -> i32 {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::Commercial {
            sales_tax_per_shopper,
            ..
        } => sales_tax_per_shopper,
        other => panic!("expected commercial details, got {other:?}"),
    }
}

fn residential_average_happiness(game: &SingleRegionTestGame, x: usize, y: usize) -> Option<i32> {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::Residential {
            average_happiness, ..
        } => average_happiness,
        other => panic!("expected residential details, got {other:?}"),
    }
}

fn building_upgrade_level(game: &SingleRegionTestGame, x: usize, y: usize) -> u8 {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::Residential { upgrade_level, .. }
        | InspectDetailsView::Commercial { upgrade_level, .. }
        | InspectDetailsView::Industrial { upgrade_level, .. }
        | InspectDetailsView::PowerPlant { upgrade_level, .. }
        | InspectDetailsView::Park { upgrade_level, .. } => upgrade_level,
        other => panic!("expected upgradeable building details, got {other:?}"),
    }
}

fn building_maintenance(game: &SingleRegionTestGame, x: usize, y: usize) -> i32 {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::Residential {
            maintenance_cost, ..
        }
        | InspectDetailsView::Commercial {
            maintenance_cost, ..
        }
        | InspectDetailsView::Industrial {
            maintenance_cost, ..
        }
        | InspectDetailsView::PowerPlant {
            maintenance_cost, ..
        }
        | InspectDetailsView::Park {
            maintenance_cost, ..
        } => maintenance_cost,
        other => panic!("expected building maintenance details, got {other:?}"),
    }
}
