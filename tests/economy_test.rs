//! Integration tests for maintenance costs, income, and tick economy breakdowns.

mod common;

use common::SingleRegionTestGame;
use small_city::core::resources::GameTime;
use small_city::interface::events::{EconomyBreakdownView, GameEventView, MetricChange};
use small_city::interface::input::BuildingKind;
use small_city::interface::view::{GameTimeView, InspectDetailsView};

#[test]
fn workplace_without_citizen_workers_pays_no_tax_but_still_has_maintenance() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert_eq!(game.view().status.money, 68);

    advance_one_day(&mut game);

    assert_eq!(game.view().status.money, 74);
}

#[test]
fn unproductive_buildings_still_have_maintenance_costs() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Park).success);
    assert_eq!(game.view().status.money, 74);

    advance_one_day(&mut game);

    assert_eq!(game.view().status.money, 72);
}

#[test]
fn build_options_expose_maintenance_costs_to_ui() {
    let game = SingleRegionTestGame::new(2, 2);
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
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);

    let result = advance_one_day(&mut game);

    assert_eq!(
        result.event,
        GameEventView::TickSummary {
            turn: 24,
            time: expected_time(24),
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
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);

    let result = advance_to_first_payday(&mut game);

    assert_eq!(
        tick_economy(&result.event),
        // Payroll begins only after residents have travelled to work. At the
        // first paid settlement, one resident has arrived and can earn, rent,
        // and shop while the other new residents still have no attendance.
        EconomyBreakdownView {
            salaries_paid: 3,
            workplace_tax: 4,
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
            rent_failures: 2,
            maintenance_cost: 3,
            net: 8,
        }
    );
    assert_eq!(game.view().status.citizens, 3);
    assert_eq!(game.view().status.money, 63);
}

#[test]
fn commercial_without_shoppers_pays_no_sales_tax() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);

    let result = advance_one_day(&mut game);

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
fn profitable_industrial_auto_upgrades_from_business_cash() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    // Leave (2,0) empty so the industrial has room to grow its footprint when it upgrades.
    for x in 3..=6 {
        assert!(game.build(x, 0, BuildingKind::Residential).success);
    }
    for x in 0..=6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    advance_one_week(&mut game);

    let inspect = game.inspect(1, 0);
    match inspect.details.expect("industrial details") {
        InspectDetailsView::Industrial {
            upgrade_level,
            maintenance_cost,
            goods_production,
            business_cash,
            recent_profit,
            upgrade_ready,
            jobs,
            ..
        } => {
            assert_eq!(upgrade_level, 2);
            assert_eq!(maintenance_cost, 2);
            assert_eq!(goods_production, 6);
            assert!(business_cash >= 14);
            assert!(recent_profit > 0);
            assert!(upgrade_ready);
            // Grew to a 2-cell footprint: jobs are area-based capacity_for(Industrial, 2) = 12.
            assert_eq!(jobs, 12);
        }
        other => panic!("expected industrial details, got {other:?}"),
    }
    assert!(
        inspect
            .explanations
            .iter()
            .any(|note| note.contains("upgrade ready from reinvestment"))
    );
    assert!(
        inspect
            .explanations
            .iter()
            .all(|note| !note.contains("already fully upgraded"))
    );
}

#[test]
fn profitable_commercial_auto_upgrades_from_shopping_profit() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 1..=5 {
        assert!(game.build(x, 0, BuildingKind::Residential).success);
    }
    assert!(game.build(6, 0, BuildingKind::Commercial).success);
    // Leave (7,0) empty so the commercial has room to grow when it upgrades.
    assert!(game.build(8, 0, BuildingKind::Industrial).success);
    for x in 0..=8 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    advance_one_working_week(&mut game);

    match game.inspect(6, 0).details.expect("commercial details") {
        InspectDetailsView::Commercial {
            upgrade_level,
            maintenance_cost,
            goods_capacity,
            business_cash,
            upgrade_threshold,
            recent_profit,
            upgrade_ready,
            jobs,
            ..
        } => {
            assert_eq!(upgrade_level, 2);
            assert_eq!(maintenance_cost, 2);
            assert_eq!(goods_capacity, 12);
            assert!(business_cash >= 0);
            assert_eq!(upgrade_threshold, Some(8));
            assert!(recent_profit > 0);
            assert!(upgrade_ready);
            // Grew to a 2-cell footprint: jobs are area-based capacity_for(Commercial, 2) = 6.
            assert_eq!(jobs, 6);
        }
        other => panic!("expected commercial details, got {other:?}"),
    }
}

#[test]
fn unprofitable_commercial_tracks_blocked_business_progress() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);

    advance_one_week(&mut game);

    let inspect = game.inspect(1, 0);
    match inspect.details.expect("commercial details") {
        InspectDetailsView::Commercial {
            upgrade_level,
            business_cash,
            upgrade_threshold,
            recent_profit,
            upgrade_ready,
            ..
        } => {
            assert_eq!(upgrade_level, 1);
            assert_eq!(business_cash, 0);
            assert_eq!(upgrade_threshold, Some(8));
            assert_eq!(recent_profit, -1);
            assert!(!upgrade_ready);
        }
        other => panic!("expected commercial details, got {other:?}"),
    }
    assert!(
        inspect
            .explanations
            .iter()
            .any(|note| note.contains("blocked by low demand"))
    );
}

#[test]
fn profitable_industrial_waits_when_demand_is_low() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);

    advance_one_week(&mut game);

    let inspect = game.inspect(1, 0);
    match inspect.details.expect("industrial details") {
        InspectDetailsView::Industrial {
            upgrade_level,
            business_cash,
            recent_profit,
            upgrade_ready,
            ..
        } => {
            assert_eq!(upgrade_level, 1);
            assert!(business_cash >= 14);
            assert!(recent_profit > 0);
            assert!(!upgrade_ready);
        }
        other => panic!("expected industrial details, got {other:?}"),
    }
    assert!(
        inspect
            .explanations
            .iter()
            .any(|note| note.contains("blocked by low demand"))
    );
}

#[test]
fn disconnected_commercial_does_not_receive_shoppers_or_pay_sales_tax() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(5, 0, BuildingKind::Industrial).success);
    assert!(game.build(8, 0, BuildingKind::Commercial).success);
    for x in 0..=5 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let result = advance_one_working_week(&mut game);

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
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    assert!(game.build(3, 1, BuildingKind::Road).success);

    let first_tick = advance_to_first_payday(&mut game);
    assert!(matches!(
        first_tick.event,
        GameEventView::TickSummary {
            // The pre-bulldoze tick uses local goods because industrial and
            // commercial are both connected. This locks in the intended baseline
            // before removing the commercial road connection.
            economy: EconomyBreakdownView {
                salaries_paid: 3,
                workplace_tax: 4,
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
                rent_failures: 2,
                maintenance_cost: 3,
                net: 8,
            },
            ..
        }
    ));

    assert!(game.bulldoze(2, 1).success);
    // One day lets the assigned workers attempt the disconnected commute. The
    // following settlement proves that no workplace arrival recorded attendance.
    let _ = advance_one_working_day(&mut game);
    let second_tick = advance_one_working_day(&mut game);

    assert_eq!(
        tick_economy(&second_tick.event),
        // After the road is bulldozed, commercial shopping and industrial goods
        // flow stop. Maintenance still applies because the buildings remain.
        // Population now arrives daily, so this road-removal check uses the first
        // daily economy pass before weekly auto-upgrades can change maintenance.
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
            rent_failures: 3,
            maintenance_cost: 3,
            net: -3,
        }
    );
}

#[test]
fn unreachable_workplace_stops_salary_but_not_productive_workplace_tax() {
    let mut game = SingleRegionTestGame::new(6, 3);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::PowerPlant).success);
    for x in 0..=4 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let first_payday = advance_to_first_payday(&mut game);
    assert!(tick_economy(&first_payday.event).salaries_paid > 0);
    assert!(tick_economy(&first_payday.event).workplace_tax > 0);

    // The industrial keeps its own power connection through the second plant.
    // Break the bridge for the complete work window, then restore it before the
    // next daily job resolution so the assignment remains productive but the
    // citizen has no attendance to settle.
    assert!(game.bulldoze(2, 1).success);
    let _ = advance_hours(&mut game, 15);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    let after_failed_commute = advance_hours(&mut game, 9).expect("daily settlement");
    let economy = tick_economy(&after_failed_commute.event);

    assert_eq!(economy.salaries_paid, 0);
    assert!(economy.workplace_tax > 0);
}

#[test]
fn save_load_pays_recorded_attendance_once() {
    let path = std::env::temp_dir().join(format!(
        "small_city_arrival_pay_roundtrip_{}.json",
        std::process::id()
    ));
    let mut game = SingleRegionTestGame::new(6, 3);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::PowerPlant).success);
    for x in 0..=4 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let _ = advance_to_first_payday(&mut game);
    let _ = advance_hours(&mut game, 15);
    game.save_to_file(&path).expect("save recorded attendance");

    let mut loaded = SingleRegionTestGame::load_from_file(&path).expect("load recorded attendance");
    std::fs::remove_file(&path).expect("remove attendance save");

    let saved_attendance_payday = advance_hours(&mut loaded, 9).expect("daily settlement");
    assert!(tick_economy(&saved_attendance_payday.event).salaries_paid > 0);

    assert!(loaded.bulldoze(2, 1).success);
    let _ = advance_hours(&mut loaded, 15);
    assert!(loaded.build(2, 1, BuildingKind::Road).success);
    let after_saved_attendance = advance_hours(&mut loaded, 9).expect("daily settlement");
    assert_eq!(tick_economy(&after_saved_attendance.event).salaries_paid, 0);
}

#[test]
fn mid_commute_save_can_miss_the_next_payroll() {
    let path = std::env::temp_dir().join(format!(
        "small_city_arrival_pay_mid_commute_{}.json",
        std::process::id()
    ));
    let mut game = SingleRegionTestGame::new(45, 3);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(35, 0, BuildingKind::Industrial).success);
    assert!(game.build(36, 0, BuildingKind::PowerPlant).success);
    for x in 0..=36 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let _ = advance_to_first_payday(&mut game);
    let _ = advance_hours(&mut game, 10);
    game.save_to_file(&path).expect("save mid-commute");

    let mut loaded = SingleRegionTestGame::load_from_file(&path).expect("load mid-commute");
    std::fs::remove_file(&path).expect("remove mid-commute save");

    let next_settlement = advance_hours(&mut loaded, 14).expect("daily settlement");
    assert_eq!(tick_economy(&next_settlement.event).salaries_paid, 0);
    assert!(tick_economy(&next_settlement.event).workplace_tax > 0);
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

    advance_one_working_week(&mut game);
    let before = residential_average_happiness(&game, 1, 0).expect("resident happiness");

    assert!(game.bulldoze(3, 1).success);
    let failed_rent_tick = advance_one_day(&mut game);
    advance_one_week(&mut game);
    let after = residential_average_happiness(&game, 1, 0).expect("resident happiness after rent");

    assert!(
        after < before,
        "expected happiness to drop from {before} to below it, got {after}"
    );
    assert!(tick_economy(&failed_rent_tick.event).rent_failures > 0);
}

#[test]
fn level_two_building_has_higher_maintenance_than_level_one() {
    let mut game = SingleRegionTestGame::new(5, 5);
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

    assert!(plain.shoppers_served > 0);
    assert!(premium.shoppers_served > 0);
    assert!(
        premium.commercial_sales_tax / premium.shoppers_served
            > plain.commercial_sales_tax / plain.shoppers_served
    );
}

#[test]
fn industrial_goods_fill_commercial_storage_and_surplus_exports() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(5, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Industrial).success);
    assert!(game.build(2, 0, BuildingKind::Industrial).success);
    assert!(game.build(3, 0, BuildingKind::Industrial).success);
    assert!(game.build(4, 0, BuildingKind::Commercial).success);
    for x in 0..=5 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let economy = tick_economy(&advance_one_day(&mut game).event);

    assert_eq!(economy.local_goods_produced, 12);
    assert_eq!(economy.local_goods_stored, 8);
    assert_eq!(economy.exported_goods, 4);
    assert_eq!(economy.manufacturing_tax, 12);
    assert_eq!(economy.export_tax, 4);
    assert_eq!(commercial_goods(&game, 4, 0), (8, 8));
}

#[test]
fn commercial_imports_goods_when_local_storage_is_empty() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);

    assert_eq!(
        tick_economy(&advance_one_day(&mut game).event).imported_goods_sold,
        0
    );
    let economy = tick_economy(&advance_one_working_week(&mut game).event);

    assert_eq!(economy.local_goods_produced, 0);
    assert_eq!(economy.local_goods_sold, 0);
    assert_eq!(economy.imported_goods_sold, 1);
    assert_eq!(economy.manufacturing_tax, 0);
    assert_eq!(economy.export_tax, 0);
}

#[test]
fn citizens_prefer_nearby_reachable_jobs() {
    let mut game = SingleRegionTestGame::new(8, 4);
    assert!(game.build(7, 1, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 0, BuildingKind::Industrial).success);
    assert!(game.build(5, 0, BuildingKind::Commercial).success);
    assert!(game.build(6, 0, BuildingKind::Residential).success);
    for x in 0..=6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let economy = tick_economy(&advance_to_first_payday(&mut game).event);

    assert_eq!(economy.salaries_paid, 3);
    assert_eq!(economy.workplace_tax, 8);
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
    let mut game = SingleRegionTestGame::new(14, 4);
    assert!(game.build(11, 2, BuildingKind::PowerPlant).success);
    assert!(game.build(10, 1, BuildingKind::Industrial).success);
    for x in 0..=10 {
        assert!(game.build(x, 2, BuildingKind::Road).success);
    }

    let economy = tick_economy(&advance_one_day(&mut game).event);

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

    let mut loaded = SingleRegionTestGame::load_from_file(&path).expect("load city");
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

fn expected_time(total_hours: u64) -> GameTimeView {
    let time = GameTime { total_hours };
    GameTimeView {
        total_hours,
        year: time.year(),
        month: time.month(),
        week: time.week_of_month(),
        day: time.day_of_week(),
        hour: time.hour_of_day(),
        label: time.label(),
    }
}

fn advance_one_day(
    game: &mut SingleRegionTestGame,
) -> small_city::interface::events::CommandResult {
    // Phase A time cadence moved economy from every tick to the daily boundary.
    let mut result = game.tick();
    for _ in 1..24 {
        result = game.tick();
    }
    result
}

fn advance_one_working_day(
    game: &mut SingleRegionTestGame,
) -> small_city::interface::events::CommandResult {
    let mut result = None;
    for _ in 0..24 * 6 {
        if let Some(tick) = game.advance() {
            result = Some(tick);
        }
    }
    result.expect("one day includes daily economy")
}

fn advance_hours(
    game: &mut SingleRegionTestGame,
    hours: usize,
) -> Option<small_city::interface::events::CommandResult> {
    let mut result = None;
    for _ in 0..hours * 6 {
        if let Some(tick) = game.advance() {
            result = Some(tick);
        }
    }
    result
}

fn advance_to_first_payday(
    game: &mut SingleRegionTestGame,
) -> small_city::interface::events::CommandResult {
    let first_day = advance_one_working_day(game);
    assert_eq!(tick_economy(&first_day.event).salaries_paid, 0);

    let first_payday = advance_one_working_day(game);
    assert!(tick_economy(&first_payday.event).salaries_paid > 0);
    first_payday
}

fn advance_one_working_week(
    game: &mut SingleRegionTestGame,
) -> small_city::interface::events::CommandResult {
    let mut result = advance_one_working_day(game);
    for _ in 1..7 {
        result = advance_one_working_day(game);
    }
    result
}

fn advance_one_week(
    game: &mut SingleRegionTestGame,
) -> small_city::interface::events::CommandResult {
    // Population grows daily while business reinvestment still runs at the weekly boundary.
    let mut result = game.tick();
    for _ in 1..24 * 7 {
        result = game.tick();
    }
    result
}

fn powered_residential_city(with_park: bool) -> SingleRegionTestGame {
    let mut game = SingleRegionTestGame::new(10, 10);
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

    tick_economy(&advance_to_first_payday(&mut game).event)
}

fn shopping_happiness_city(commercial_x: usize) -> i32 {
    let mut game = SingleRegionTestGame::new(20, 4);
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

    advance_one_working_week(&mut game);
    residential_average_happiness(&game, 1, 0).expect("resident happiness")
}

fn imported_goods_sold_after_two_ticks(far_from_edge: bool) -> i32 {
    let mut game = SingleRegionTestGame::new(20, 4);
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

    let _ = advance_to_first_payday(&mut game);
    tick_economy(&advance_one_working_day(&mut game).event).imported_goods_sold
}

fn residential_rent(game: &SingleRegionTestGame, x: usize, y: usize) -> i32 {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::Residential {
            rent_per_citizen, ..
        } => rent_per_citizen,
        other => panic!("expected residential details, got {other:?}"),
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

fn power_plant_maintenance(game: &SingleRegionTestGame, x: usize, y: usize) -> i32 {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::PowerPlant {
            maintenance_cost, ..
        } => maintenance_cost,
        other => panic!("expected power plant details, got {other:?}"),
    }
}

fn commercial_goods(game: &SingleRegionTestGame, x: usize, y: usize) -> (i32, i32) {
    match game.inspect(x, y).details.expect("inspect details") {
        InspectDetailsView::Commercial {
            goods_stored,
            goods_capacity,
            ..
        } => (goods_stored, goods_capacity),
        other => panic!("expected commercial details, got {other:?}"),
    }
}
