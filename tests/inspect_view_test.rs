//! Integration tests for InspectView data and ASCII inspect formatting.

mod common;

use common::SingleRegionTestGame;
use small_city::interface::input::BuildingKind;
use small_city::interface::view::{
    CitizenRelation, InspectDetailsView, InspectFlag, InspectView, LocalEffectsView,
};
use small_city::ui::ascii::format_inspect;

#[test]
fn inspect_empty_cell_shows_buildable_status() {
    let game = SingleRegionTestGame::new(2, 2);
    let inspect = game.inspect(1, 1);

    assert_eq!(
        inspect.details,
        Some(InspectDetailsView::Empty { buildable: true })
    );
    let formatted = format_inspect(&inspect);
    assert!(formatted.contains("EMPTY LAND"));
    assert!(formatted.contains("Buildable Yes"));
}

#[test]
fn inspect_residential_shows_powered_state_and_population() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    game.tick();

    let inspect = game.inspect(1, 0);

    assert_eq!(
        inspect.details,
        Some(InspectDetailsView::Residential {
            powered: true,
            power_demand: 1,
            road_connected: true,
            upgrade_level: 1,
            maintenance_cost: 0,
            rent_per_citizen: 2,
            population: 0,
            max_population: 5,
            citizens: 0,
            average_happiness: None,
            average_happiness_target: None,
            average_money: None,
            unpaid_citizens: 0,
            job_assignments: Vec::new(),
        })
    );
    let formatted = format_inspect(&inspect);
    assert!(formatted.contains("RESIDENTIAL"));
    assert!(formatted.contains("Pwr on d1"));
    assert!(formatted.contains("People  [..........] 0/5"));
    assert!(formatted.contains("Work    none"));
}

#[test]
fn inspect_and_cell_view_show_local_citizen_workplace_tile() {
    let mut game = SingleRegionTestGame::new(4, 3);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    for x in 0..=2 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    for _ in 0..24 {
        assert!(game.tick().success);
    }

    let inspect = game.inspect(1, 0);
    let (assignment, unpaid_citizens) = match &inspect.details {
        Some(InspectDetailsView::Residential {
            job_assignments,
            unpaid_citizens,
            ..
        }) => (
            job_assignments.first().copied().expect("local assignment"),
            *unpaid_citizens,
        ),
        details => panic!("expected residential inspect, got {details:?}"),
    };
    let cell_assignment = game
        .view()
        .map
        .cells
        .iter()
        .find(|cell| cell.x == 1 && cell.y == 0)
        .and_then(|cell| cell.job_assignments.first().copied())
        .expect("cell assignment");

    assert_eq!(assignment.cell.region.0, 1);
    assert_eq!((assignment.cell.x, assignment.cell.y), (2, 0));
    assert_eq!(assignment.salary, 3);
    assert!(!assignment.is_remote);
    assert_eq!(unpaid_citizens, 1);
    assert_eq!(cell_assignment, assignment);
    assert!(format_inspect(&inspect).contains("local R1 (2, 0) salary 3"));
    assert!(format_inspect(&inspect).contains("Unpaid  1 not arrived"));

    // Advance through 09:00 (the work-phase start) and enough movement steps
    // for this two-road local commute to arrive.
    for _ in 0..60 {
        let _ = game.advance();
    }

    let arrived_inspect = game.inspect(1, 0);
    let arrived_unpaid_citizens = match &arrived_inspect.details {
        Some(InspectDetailsView::Residential {
            unpaid_citizens, ..
        }) => *unpaid_citizens,
        details => panic!("expected residential inspect, got {details:?}"),
    };
    assert_eq!(arrived_unpaid_citizens, 0);
    assert!(format_inspect(&arrived_inspect).contains("Unpaid  0 not arrived"));
}

#[test]
fn roster_lists_residents_with_their_workplace_and_workers_with_their_home() {
    // Same fixture as the local-workplace test: the resident at (1,0) takes the
    // local commercial job at (2,0) after a day of ticks.
    let mut game = SingleRegionTestGame::new(4, 3);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);
    for x in 0..=2 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    for _ in 0..24 {
        assert!(game.tick().success);
    }

    // Residential roster: one entry per resident, each tagged with where they work.
    let residential = game.inspect(1, 0);
    let citizens = match residential.details {
        Some(InspectDetailsView::Residential { citizens, .. }) => citizens,
        details => panic!("expected residential inspect, got {details:?}"),
    };
    assert_eq!(residential.roster.len() as i32, citizens);
    assert!(!residential.roster.is_empty());
    assert!(matches!(
        residential.roster[0].relation,
        CitizenRelation::WorksAt {
            cell,
            salary: 3,
            is_remote: false,
        } if cell.x == 2 && cell.y == 0
    ));
    assert!(residential.roster[0].unpaid_since_daily_settlement);

    // Workplace roster: the local worker, tagged with where they live.
    let commercial = game.inspect(2, 0);
    assert_eq!(commercial.roster.len(), residential.roster.len());
    assert!(matches!(
        commercial.roster[0].relation,
        CitizenRelation::LivesAt {
            region: None,
            x: 1,
            y: 0
        }
    ));

    // Road and empty cells carry no roster.
    assert!(game.inspect(0, 1).roster.is_empty());
    assert!(game.inspect(3, 2).roster.is_empty());

    // Deterministic: the same state inspected twice yields the identical roster.
    assert_eq!(game.inspect(1, 0).roster, residential.roster);

    for _ in 0..60 {
        let _ = game.advance();
    }
    assert!(!game.inspect(1, 0).roster[0].unpaid_since_daily_settlement);
}

#[test]
fn inspect_commercial_and_industrial_show_powered_state_and_jobs() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);
    assert!(game.build(2, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    // P2 goods trucks: the daily goods phase plans supply, then travel subticks
    // deliver the level-1 factory's single 3-unit cargo.
    for _ in 0..24 * 6 {
        let _ = game.advance();
    }

    let commercial = game.inspect(1, 0);
    let industrial = game.inspect(2, 0);

    assert_eq!(
        commercial.details,
        // Commercial inspect includes city goods inventory through the
        // view model, because storage lives on the commercial building and must
        // be visible without exposing ECS internals.
        Some(InspectDetailsView::Commercial {
            powered: true,
            power_demand: 2,
            road_connected: true,
            upgrade_level: 1,
            maintenance_cost: 1,
            sales_tax_per_shopper: 1,
            goods_stored: 3,
            goods_capacity: 8,
            business_cash: 0,
            upgrade_threshold: Some(8),
            recent_profit: -1,
            upgrade_ready: false,
            jobs: 2,
            goods_sold_from_city: 0,
            goods_sold_from_outside: 0,
        })
    );
    assert_eq!(
        industrial.details,
        // Industrial inspect includes goods production so players can see
        // how much city supply the factory contributes to nearby commercial.
        Some(InspectDetailsView::Industrial {
            powered: true,
            power_demand: 3,
            road_connected: true,
            upgrade_level: 1,
            maintenance_cost: 1,
            goods_production: 4,
            business_cash: 3,
            upgrade_threshold: Some(14),
            recent_profit: 3,
            upgrade_ready: false,
            jobs: 3
        })
    );
    let commercial_format = format_inspect(&commercial);
    let industrial_format = format_inspect(&industrial);
    assert!(commercial_format.contains("COMMERCIAL"));
    assert!(commercial_format.contains("3/8"));
    assert!(commercial_format.contains("Sales   1 per shopper  Jobs 2"));
    assert!(industrial_format.contains("INDUSTRIAL"));
    assert!(industrial_format.contains("Output  4 goods/turn"));
    assert!(industrial_format.contains("Jobs    3"));
    assert!(
        commercial
            .explanations
            .iter()
            .any(|note| note.contains("city goods stored"))
    );
    assert!(
        !commercial
            .explanations
            .iter()
            .any(|note| note.contains("local goods stored"))
    );
    // An industrial sells to commercial storage or exports off the edge, so it must not warn about a
    // "nearest commercial route" (it read "unreachable by road" even when the factory was fine).
    assert!(
        !industrial
            .explanations
            .iter()
            .any(|note| note.contains("nearest commercial route")),
        "industrial should not show a commercial-route note: {:?}",
        industrial.explanations
    );
}

#[test]
fn inspect_road_shows_building_type() {
    let mut game = SingleRegionTestGame::new(2, 2);
    assert!(game.build(0, 0, BuildingKind::Road).success);

    let inspect = game.inspect(0, 0);

    assert_eq!(inspect.details, Some(InspectDetailsView::Road));
    assert!(format_inspect(&inspect).contains("ROAD"));
}

#[test]
fn inspect_power_plant_and_park_show_special_effects() {
    let mut game = SingleRegionTestGame::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Park).success);

    let power_plant = game.inspect(0, 0);
    let park = game.inspect(1, 0);

    assert_eq!(
        power_plant.details,
        Some(InspectDetailsView::PowerPlant {
            road_connected: false,
            connected_to_road_network: false,
            upgrade_level: 1,
            maintenance_cost: 1,
            power_capacity: 10
        })
    );
    assert_eq!(
        park.details,
        Some(InspectDetailsView::Park {
            road_connected: false,
            upgrade_level: 1,
            maintenance_cost: 1,
            happiness_effect: 3
        })
    );
    let power_plant_format = format_inspect(&power_plant);
    let park_format = format_inspect(&park);
    assert!(power_plant_format.contains("POWER PLANT"));
    assert!(power_plant_format.contains("Output  10 capacity"));
    assert!(park_format.contains("PARK"));
    assert!(park_format.contains("Happy   +3"));
}

#[test]
fn inspect_out_of_bounds_formats_without_cell_data() {
    let game = SingleRegionTestGame::new(2, 2);
    let inspect = game.inspect(5, 5);

    assert_eq!(inspect.details, None);
    assert_eq!(format_inspect(&inspect), "(5, 5) outside map");
}

#[test]
fn inspect_explains_missing_adjacent_road() {
    let mut game = SingleRegionTestGame::new(3, 3);
    assert!(game.build(1, 1, BuildingKind::Residential).success);

    let inspect = game.inspect(1, 1);

    assert!(
        inspect
            .explanations
            .contains(&"Blocked: no orthogonally adjacent road.".to_string())
    );
}

#[test]
fn inspect_explains_unpowered_road_network() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);

    game.tick();
    let inspect = game.inspect(1, 0);

    assert!(
        inspect
            .explanations
            .contains(&"Blocked: adjacent road network is not powered.".to_string())
    );
}

#[test]
fn inspect_explains_insufficient_power_capacity() {
    let mut game = SingleRegionTestGame::new(8, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    for x in 0..6 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }
    for x in 1..5 {
        assert!(game.build(x, 0, BuildingKind::Industrial).success);
    }
    assert!(game.build(5, 0, BuildingKind::Commercial).success);

    game.tick();
    let inspect = game.inspect(4, 0);

    assert!(
        inspect
            .explanations
            .contains(&"Blocked: connected power network lacks enough capacity.".to_string())
    );
}

#[test]
fn inspect_explains_no_available_jobs_for_residential_growth() {
    let mut game = SingleRegionTestGame::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);

    game.tick();
    let inspect = game.inspect(1, 0);

    assert!(inspect.flags.contains(&InspectFlag::GrowthBlockedNoJobs));
    assert!(
        !inspect
            .explanations
            .iter()
            .any(|note| note.contains("no jobs are available"))
    );
}

#[test]
fn inspect_exposes_road_network_distance_notes() {
    let mut game = SingleRegionTestGame::new(6, 4);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(4, 0, BuildingKind::Commercial).success);
    for x in 1..=4 {
        assert!(game.build(x, 1, BuildingKind::Road).success);
    }

    let inspect = game.inspect(1, 0);

    assert!(
        inspect
            .explanations
            .contains(&"Commute: nearest workplace is 3 road tiles away.".to_string())
    );
    assert!(
        inspect
            .explanations
            .contains(&"Shopping: nearest commercial is 3 road tiles away.".to_string())
    );
}

#[test]
fn inspect_explains_local_pollution_and_happiness_effects() {
    let mut game = SingleRegionTestGame::new(3, 3);
    assert!(game.build(0, 0, BuildingKind::Industrial).success);
    assert!(game.build(1, 0, BuildingKind::Park).success);

    let industrial = game.inspect(0, 0);
    let park = game.inspect(1, 0);

    assert!(
        industrial
            .explanations
            .contains(&"Local effect: adds 2 pollution.".to_string())
    );
    assert!(
        park.explanations
            .contains(&"Local effect: adds +3 happiness.".to_string())
    );
}

#[test]
fn inspect_card_layout_is_fixed_slot_aligned() {
    // Sim-independent golden: locks the header + status column layout and the
    // per-row bar/label positions so the fixed-slot card cannot silently drift.
    let inspect = InspectView {
        x: 12,
        y: 4,
        in_bounds: true,
        cell: None,
        details: Some(InspectDetailsView::Commercial {
            powered: true,
            power_demand: 2,
            road_connected: true,
            upgrade_level: 2,
            maintenance_cost: 3,
            sales_tax_per_shopper: 1,
            goods_stored: 4,
            goods_capacity: 8,
            business_cash: 30,
            upgrade_threshold: Some(50),
            recent_profit: 5,
            upgrade_ready: false,
            jobs: 2,
            goods_sold_from_city: 6,
            goods_sold_from_outside: 2,
        }),
        local_effects: Some(LocalEffectsView {
            land_value: 6,
            pollution_pressure: 1,
            accessibility: 5,
            desirability: 5,
        }),
        flags: Vec::new(),
        explanations: Vec::new(),
        roster: Vec::new(),
        road_traveler_count: 0,
    };

    assert_eq!(
        format_inspect(&inspect),
        "(12,  4) COMMERCIAL   Lvl [##.] 2/3\n\
         Pwr on d2    Road Y   Maint 3\n\
         Goods   [#####.....] 4/8\n\
         Cash    [######....] 30/50  ready No\n\
         Sales   1 per shopper  Jobs 2  Recent 5\n\
         Source  [########::]  🏭 6 city-made · 🌍 2 from outside\n\
         Local   Land 4  Poll .  Access 3  Desir 3"
    );
}
