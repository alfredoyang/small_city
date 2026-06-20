//! Integration tests for regional facade tick events and basic simulation effects.

mod common;

use common::SingleRegionTestGame;
use small_city::core::resources::GameTime;
use small_city::interface::events::{EconomyBreakdownView, GameEventView, MetricChange};
use small_city::interface::input::BuildingKind;
use small_city::interface::view::{GameTimeView, InspectDetailsView};

#[test]
fn default_single_region_test_game_uses_larger_distance_friendly_map() {
    let view = SingleRegionTestGame::default().view();

    assert_eq!(view.map.width, 20);
    assert_eq!(view.map.height, 15);
    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
}

#[test]
fn industrial_creates_pollution() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::Industrial).success);

    game.tick();

    assert_eq!(game.view().status.pollution, 2);
}

#[test]
fn park_reduces_pollution_effect() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::Industrial).success);
    assert!(game.build(1, 0, BuildingKind::Park).success);

    game.tick();

    assert_eq!(game.view().status.pollution, 1);
}

#[test]
fn happiness_includes_park_bonus() {
    let mut high_happiness = SingleRegionTestGame::new(10, 10);
    for x in 0..10 {
        assert!(high_happiness.build(x, 0, BuildingKind::Park).success);
    }
    high_happiness.tick();
    assert_eq!(high_happiness.view().status.happiness, 80);
}

#[test]
fn tick_advances_turn_deterministically() {
    let mut game = SingleRegionTestGame::new(10, 10);

    game.tick();
    game.tick();

    assert_eq!(game.view().status.turn, 2);
    assert_eq!(game.view().status.time.total_hours, 2);
    assert_eq!(
        game.view().status.time.label,
        "Year 1, Month 1, Week 1, Day 1, 02:00"
    );
}

#[test]
fn tick_returns_structured_summary_events() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    // Gaps at (3,0) and (5,0) give the commercial and industrial room to grow when they upgrade.
    assert!(game.build(4, 0, BuildingKind::Industrial).success);
    assert!(game.build(6, 0, BuildingKind::Park).success);
    for x in 0..=6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let result = advance_one_week(&mut game);

    assert!(result.success);
    assert_eq!(result.events.len(), 3);
    assert_eq!(result.event, result.events[0]);
    assert_eq!(
        result.events[0],
        GameEventView::TickSummary {
            turn: 168,
            time: expected_time(168),
            population: MetricChange {
                before: 5,
                after: 5
            },
            money: MetricChange {
                before: 216,
                after: 249
            },
            happiness: MetricChange {
                before: 90,
                after: 91
            },
            // The profitable industrial can auto-upgrade at the weekly boundary,
            // increasing its pollution source after the economy event is applied.
            pollution: MetricChange {
                before: 1,
                after: 2
            },
            unemployment: MetricChange {
                before: 0,
                after: 0
            },
            powered_buildings: MetricChange {
                before: 3,
                after: 3
            },
            // Tick summaries now expose the goods supply chain. These values
            // verify that local industrial output reaches commercial storage and
            // contributes manufacturing tax to the same public event the UI uses.
            economy: EconomyBreakdownView {
                salaries_paid: 18,
                workplace_tax: 8,
                rent_income: 15,
                commercial_sales_tax: 9,
                shoppers_served: 3,
                local_goods_produced: 4,
                local_goods_stored: 3,
                local_goods_sold: 3,
                imported_goods_sold: 0,
                exported_goods: 1,
                manufacturing_tax: 4,
                export_tax: 1,
                rent_failures: 0,
                maintenance_cost: 4,
                net: 33
            },
        }
    );
    assert_eq!(
        result.events[1],
        GameEventView::BusinessAutoUpgraded {
            x: 2,
            y: 0,
            kind: BuildingKind::Commercial,
            level: 2
        }
    );
    assert_eq!(
        result.events[2],
        GameEventView::BusinessAutoUpgraded {
            x: 4,
            y: 0,
            kind: BuildingKind::Industrial,
            level: 2
        }
    );
}

#[test]
fn tick_summary_message_includes_metric_changes() {
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    // Gap at (3,0) gives the commercial room to grow; industrial sits past it.
    assert!(game.build(4, 0, BuildingKind::Industrial).success);
    for x in 0..=4 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let message = advance_one_week(&mut game).message();

    assert!(message.contains("population 5 (+0)"));
    assert!(message.contains("Year 1, Month 1, Week 2, Day 1, 00:00"));
    assert!(message.contains("money 260 (+34)"));
    assert!(message.contains("powered buildings 3 (+0)"));
    // The message expectation changed because tick feedback now explains goods
    // production, local/imported sales, export flow, and related taxes.
    assert!(
        message.contains(
            "Economy: salaries paid 18, workplace tax +8, rent +15, sales tax +9, shoppers 3, local goods produced 4, stored 3, sold 3, imported 0, exported 1, manufacturing tax +4, export tax +1, rent failures 0, maintenance -3, net +34"
        )
    );
    assert!(message.contains("Commercial at (2, 0) upgraded to level 2 from reinvestment"));
    assert!(message.contains("Industrial at (4, 0) upgraded to level 2 from reinvestment"));
}

#[test]
fn business_reinvestment_can_raise_industrial_to_level_three_and_emit_event() {
    // The industrial sits at (1,1) with an empty 2x2 zone above/right, and the road runs along
    // y=2 so it never blocks the square. Reinvestment grows it N then E to a 2x2 anchored at (1,0)
    // (level 2 after week one, level 3 after week two).
    let mut game = SingleRegionTestGame::new(10, 10);
    assert!(game.build(0, 1, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 1, BuildingKind::Industrial).success);
    for x in 3..=6 {
        assert!(game.build(x, 1, BuildingKind::Residential).success);
    }
    for x in 0..=6 {
        assert!(game.build(x, 2, BuildingKind::Road).success);
    }

    // Advance week by week, collecting the level-2 and level-3 reinvestment events. Each
    // reinvestment costs flat business cash, so reaching the cap can take a few profitable weeks.
    let mut level_two_event = None;
    let mut level_three_event = None;
    for _ in 0..8 {
        let week = advance_one_week(&mut game);
        for event in &week.events {
            if let GameEventView::BusinessAutoUpgraded {
                kind: BuildingKind::Industrial,
                level,
                ..
            } = event
            {
                match level {
                    2 => level_two_event = Some(event.clone()),
                    3 => level_three_event = Some(event.clone()),
                    _ => {}
                }
            }
        }
        if level_three_event.is_some() {
            break;
        }
    }

    // The growth anchors the 2x2 footprint at (1,0), so both events report that corner.
    assert_eq!(
        level_two_event,
        Some(GameEventView::BusinessAutoUpgraded {
            x: 1,
            y: 0,
            kind: BuildingKind::Industrial,
            level: 2
        })
    );
    assert_eq!(
        level_three_event,
        Some(GameEventView::BusinessAutoUpgraded {
            x: 1,
            y: 0,
            kind: BuildingKind::Industrial,
            level: 3
        })
    );
    match game.inspect(1, 0).details.expect("industrial details") {
        InspectDetailsView::Industrial {
            upgrade_level,
            maintenance_cost,
            goods_production,
            jobs,
            ..
        } => {
            assert_eq!(upgrade_level, 3);
            assert_eq!(maintenance_cost, 3);
            assert_eq!(goods_production, 8);
            // 2x2 footprint: jobs are area-based capacity_for(Industrial, 4) = 24.
            assert_eq!(jobs, 24);
        }
        other => panic!("expected industrial details, got {other:?}"),
    }
    assert_eq!(game.view().status.pollution, 4);
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
