//! Integration tests for maintenance costs, income, and tick economy breakdowns.

use small_city::core::game::Game;
use small_city::interface::events::{EconomyBreakdownView, GameEventView, MetricChange};
use small_city::interface::input::BuildingKind;
use small_city::interface::view::InspectDetailsView;

#[test]
fn workplace_without_citizen_workers_pays_no_tax_but_still_has_maintenance() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert_eq!(game.view().status.money, 68);

    game.tick();

    assert_eq!(game.view().status.money, 74);
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
                after: 74
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
            // Expected values changed after goods flow: productive industrial
            // buildings now manufacture local goods even without shoppers. With
            // no connected commercial storage, those goods are exported and add
            // manufacturing/export tax to the city budget.
            economy: EconomyBreakdownView {
                salaries_paid: 0,
                workplace_tax: 0,
                rent_income: 0,
                commercial_sales_tax: 0,
                shoppers_served: 0,
                local_goods_produced: 4,
                local_goods_stored: 0,
                local_goods_sold: 0,
                imported_goods_sold: 0,
                exported_goods: 4,
                manufacturing_tax: 4,
                export_tax: 4,
                rent_failures: 0,
                maintenance_cost: 2,
                net: 6
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
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);

    let result = game.tick();

    assert_eq!(
        tick_economy(&result.event),
        // This expected breakdown includes the local-goods path. Industrial
        // production fills commercial storage, the citizen buys one local good,
        // and city net now includes manufacturing tax in addition to rent,
        // workplace tax, sales tax, and maintenance.
        EconomyBreakdownView {
            salaries_paid: 3,
            workplace_tax: 1,
            rent_income: 2,
            commercial_sales_tax: 1,
            shoppers_served: 1,
            local_goods_produced: 4,
            local_goods_stored: 4,
            local_goods_sold: 1,
            imported_goods_sold: 0,
            exported_goods: 0,
            manufacturing_tax: 4,
            export_tax: 0,
            rent_failures: 0,
            maintenance_cost: 3,
            net: 5,
        }
    );
    assert_eq!(game.view().status.citizens, 1);
    assert_eq!(game.view().status.money, 58);
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
                ..
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
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);

    let first_tick = game.tick();
    assert!(matches!(
        first_tick.event,
        GameEventView::TickSummary {
            // The pre-bulldoze tick uses local goods because industrial and
            // commercial are both connected. This locks in the intended baseline
            // before removing the commercial road connection.
            economy: EconomyBreakdownView {
                salaries_paid: 3,
                workplace_tax: 1,
                rent_income: 2,
                commercial_sales_tax: 1,
                shoppers_served: 1,
                local_goods_produced: 4,
                local_goods_stored: 4,
                local_goods_sold: 1,
                imported_goods_sold: 0,
                exported_goods: 0,
                manufacturing_tax: 4,
                export_tax: 0,
                rent_failures: 0,
                maintenance_cost: 3,
                net: 5,
            },
            ..
        }
    ));

    assert!(game.bulldoze(2, 1).success);
    let second_tick = game.tick();

    assert_eq!(
        tick_economy(&second_tick.event),
        // After the road is bulldozed, commercial shopping and industrial goods
        // flow stop. Maintenance still applies because the buildings remain.
        EconomyBreakdownView {
            salaries_paid: 0,
            workplace_tax: 0,
            rent_income: 0,
            commercial_sales_tax: 0,
            shoppers_served: 0,
            local_goods_produced: 0,
            local_goods_stored: 0,
            local_goods_sold: 0,
            imported_goods_sold: 0,
            exported_goods: 0,
            manufacturing_tax: 0,
            export_tax: 0,
            rent_failures: 1,
            maintenance_cost: 3,
            net: -3,
        }
    );
}

#[test]
fn residential_in_higher_land_value_area_charges_higher_rent() {
    let plain = powered_residential_city(false);
    let premium = powered_residential_city(true);

    assert!(
        residential_rent(&premium, 1, 0) > residential_rent(&plain, 1, 0),
        "park-driven land value should increase rent"
    );
}

#[test]
fn citizen_unable_to_pay_rent_gets_lower_happiness() {
    let mut game = powered_residential_city(true);
    assert!(game.build(3, 0, BuildingKind::Commercial).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);

    assert!(game.tick().success);
    let before = residential_average_happiness(&game, 1, 0).expect("resident happiness");

    assert!(game.bulldoze(3, 1).success);
    let failed_rent_tick = game.tick();
    assert!(game.tick().success);
    let after = residential_average_happiness(&game, 1, 0).expect("resident happiness after rent");

    assert!(
        after < before,
        "expected happiness to drop from {before} to below it, got {after}"
    );
    assert!(tick_economy(&failed_rent_tick.event).rent_failures > 0);
}

#[test]
fn level_two_building_has_higher_maintenance_than_level_one() {
    let mut game = Game::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);

    let level_one = power_plant_maintenance(&game, 0, 0);
    assert!(game.upgrade(0, 0).success);
    let level_two = power_plant_maintenance(&game, 0, 0);

    assert_eq!(level_one, 1);
    assert_eq!(level_two, 2);
    assert!(level_two > level_one);
}

#[test]
fn commercial_in_higher_land_value_area_pays_more_sales_tax() {
    let plain = commercial_tax_city(false);
    let premium = commercial_tax_city(true);

    assert!(premium.commercial_sales_tax > plain.commercial_sales_tax);
    assert!(premium.shoppers_served >= plain.shoppers_served);
}

#[test]
fn industrial_goods_fill_commercial_storage_and_surplus_exports() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(5, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(2, 0, BuildingKind::Industrial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::Commercial).success);
    for x in 0..=5 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let economy = tick_economy(&game.tick().event);

    assert_eq!(economy.local_goods_produced, 12);
    assert_eq!(economy.local_goods_stored, 8);
    assert_eq!(economy.exported_goods, 4);
    assert_eq!(economy.manufacturing_tax, 12);
    assert_eq!(economy.export_tax, 4);
    assert_eq!(commercial_goods(&game, 4, 0), (8, 8));
}

#[test]
fn commercial_imports_goods_when_local_storage_is_empty() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);

    assert_eq!(tick_economy(&game.tick().event).imported_goods_sold, 0);
    let economy = tick_economy(&game.tick().event);

    assert_eq!(economy.local_goods_produced, 0);
    assert_eq!(economy.local_goods_sold, 0);
    assert_eq!(economy.imported_goods_sold, 1);
    assert_eq!(economy.manufacturing_tax, 0);
    assert_eq!(economy.export_tax, 0);
}

#[test]
fn citizens_prefer_nearby_reachable_jobs() {
    let mut game = Game::new(8, 4);
    assert!(game.build(7, 1, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 0, BuildingKind::Industrial).success);
    assert!(game.build(5, 0, BuildingKind::Commercial).success);
    assert!(game.build(6, 0, BuildingKind::Residential).success);
    for x in 0..=6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let economy = tick_economy(&game.tick().event);

    assert_eq!(economy.salaries_paid, 3);
    assert_eq!(economy.workplace_tax, 1);
}

#[test]
fn nearby_commercial_gives_better_shopping_happiness_than_far_commercial() {
    let nearby = shopping_happiness_city(3);
    let far = shopping_happiness_city(14);

    assert!(
        nearby > far,
        "nearby shop happiness {nearby} should beat far shop happiness {far}"
    );
}

#[test]
fn far_export_access_lowers_export_and_manufacturing_margin() {
    let mut game = Game::new(14, 4);
    assert!(game.build(11, 2, BuildingKind::PowerPlant).success);
    assert!(game.build(10, 1, BuildingKind::Industrial).success);
    for x in 0..=10 {
        assert!(game.build(x, 2, BuildingKind::Road).success);
    }

    let economy = tick_economy(&game.tick().event);

    assert_eq!(economy.local_goods_produced, 4);
    assert_eq!(economy.exported_goods, 4);
    assert_eq!(economy.manufacturing_tax, 0);
    assert_eq!(economy.export_tax, 0);
}

#[test]
fn far_import_access_raises_import_cost_for_shoppers() {
    let near = imported_goods_sold_after_two_ticks(false);
    let far = imported_goods_sold_after_two_ticks(true);

    assert_eq!(near, 1);
    assert_eq!(far, 0);
}

#[test]
fn save_load_preserves_land_value_rent_behavior() {
    let path = std::env::temp_dir().join("small_city_v04_economy_roundtrip.json");
    let game = powered_residential_city(true);
    let before = residential_rent(&game, 1, 0);
    game.save_to_file(&path).expect("save city");

    let mut loaded = Game::load_from_file(&path).expect("load city");
    let _ = std::fs::remove_file(&path);
    let after = residential_rent(&loaded, 1, 0);

    assert_eq!(after, before);
    assert!(loaded.tick().success);
}

fn tick_economy(event: &GameEventView) -> EconomyBreakdownView {
    match event {
        GameEventView::TickSummary { economy, .. } => *economy,
        other => panic!("expected tick summary event, got {other:?}"),
    }
}

fn powered_residential_city(with_park: bool) -> Game {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    if with_park {
        assert!(game.build(2, 0, BuildingKind::Park).success);
    }
    game
}

fn commercial_tax_city(with_park: bool) -> EconomyBreakdownView {
    let mut game = powered_residential_city(false);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);
    if with_park {
        assert!(game.build(2, 2, BuildingKind::Park).success);
    }

    tick_economy(&game.tick().event)
}

fn shopping_happiness_city(commercial_x: usize) -> i32 {
    let mut game = Game::new(20, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(
        game.build(commercial_x, 0, BuildingKind::Commercial)
            .success
    );
    assert!(game.build(18, 0, BuildingKind::Industrial).success);
    for x in 0..=18 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    assert!(game.tick().success);
    residential_average_happiness(&game, 1, 0).expect("resident happiness")
}

fn imported_goods_sold_after_two_ticks(far_from_edge: bool) -> i32 {
    let mut game = Game::new(20, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    if far_from_edge {
        assert!(game.build(10, 0, BuildingKind::Commercial).success);
        assert!(game.build(11, 0, BuildingKind::Residential).success);
        for x in 0..=11 {
            assert!(game.build(x, 1, BuildingKind::Road).success);
        }
    } else {
        assert!(game.build(1, 0, BuildingKind::Commercial).success);
        assert!(game.build(2, 0, BuildingKind::Residential).success);
        for x in 0..=2 {
            assert!(game.build(x, 1, BuildingKind::Road).success);
        }
    }

    assert!(game.tick().success);
    tick_economy(&game.tick().event).imported_goods_sold
}

fn residential_rent(game: &Game, x: usize, y: usize) -> i32 {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::Residential {
            rent_per_citizen, ..
        } => rent_per_citizen,
        other => panic!("expected residential details, got {other:?}"),
    }
}

fn residential_average_happiness(game: &Game, x: usize, y: usize) -> Option<i32> {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::Residential {
            average_happiness, ..
        } => average_happiness,
        other => panic!("expected residential details, got {other:?}"),
    }
}

fn power_plant_maintenance(game: &Game, x: usize, y: usize) -> i32 {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::PowerPlant {
            maintenance_cost, ..
        } => maintenance_cost,
        other => panic!("expected power plant details, got {other:?}"),
    }
}

fn commercial_goods(game: &Game, x: usize, y: usize) -> (i32, i32) {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::Commercial {
            goods_stored,
            goods_capacity,
            ..
        } => (goods_stored, goods_capacity),
        other => panic!("expected commercial details, got {other:?}"),
    }
}
