//! Integration tests for InspectView data and ASCII inspect formatting.

use small_city::core::game::Game;
use small_city::interface::input::BuildingKind;
use small_city::interface::view::InspectDetailsView;
use small_city::ui::ascii::format_inspect;

#[test]
fn inspect_empty_cell_shows_buildable_status() {
    let game = Game::new(2, 2);
    let inspect = game.inspect(1, 1);

    assert_eq!(
        inspect.details,
        Some(InspectDetailsView::Empty { buildable: true })
    );
    assert_eq!(
        format_inspect(&inspect),
        "(1, 1) Empty Land | Buildable: Yes"
    );
}

#[test]
fn inspect_residential_shows_powered_state_and_population() {
    let mut game = Game::new(4, 4);
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
            average_money: None,
        })
    );
    assert_eq!(
        format_inspect(&inspect),
        "(1, 0) Residential | Powered: Yes | Demand: 1 | Road: Yes | Level: 1 | Maintenance: 0 | Rent: 2 | Population: 0/5 | Citizens: 0 | Avg Happiness: None | Avg Money: None"
    );
}

#[test]
fn inspect_commercial_and_industrial_show_powered_state_and_jobs() {
    let mut game = Game::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);
    assert!(game.build(2, 0, BuildingKind::Industrial).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(2, 1, BuildingKind::Road).success);
    // Phase A time cadence moved goods flow from every tick to the daily boundary.
    for _ in 0..24 {
        assert!(game.tick().success);
    }

    let commercial = game.inspect(1, 0);
    let industrial = game.inspect(2, 0);

    assert_eq!(
        commercial.details,
        // Commercial inspect now includes local goods inventory through the
        // view model, because storage lives on the commercial building and must
        // be visible without exposing ECS internals.
        Some(InspectDetailsView::Commercial {
            powered: true,
            power_demand: 2,
            road_connected: true,
            upgrade_level: 1,
            maintenance_cost: 1,
            sales_tax_per_shopper: 1,
            goods_stored: 4,
            goods_capacity: 8,
            business_cash: 0,
            upgrade_threshold: Some(8),
            recent_profit: -1,
            upgrade_ready: false,
            jobs: 2
        })
    );
    assert_eq!(
        industrial.details,
        // Industrial inspect now includes goods production so players can see
        // how much local supply the factory contributes to nearby commercial.
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
    assert_eq!(
        format_inspect(&commercial),
        // ASCII formatting changed only because it renders the new InspectView
        // fields; the UI still does not read core storage directly.
        "(1, 0) Commercial | Powered: Yes | Demand: 2 | Road: Yes | Level: 1 | Maintenance: 1 | Sales Tax: 1 | Goods: 4/8 | Business: 0/8 recent -1 ready No | Jobs: 2"
    );
    assert_eq!(
        format_inspect(&industrial),
        "(2, 0) Industrial | Powered: Yes | Demand: 3 | Road: Yes | Level: 1 | Maintenance: 1 | Goods: 4 | Business: 3/14 recent 3 ready No | Jobs: 3"
    );
}

#[test]
fn inspect_road_shows_building_type() {
    let mut game = Game::new(2, 2);
    assert!(game.build(0, 0, BuildingKind::Road).success);

    let inspect = game.inspect(0, 0);

    assert_eq!(inspect.details, Some(InspectDetailsView::Road));
    assert_eq!(format_inspect(&inspect), "(0, 0) Road");
}

#[test]
fn inspect_power_plant_and_park_show_special_effects() {
    let mut game = Game::new(5, 5);
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
    assert_eq!(
        format_inspect(&power_plant),
        "(0, 0) Power Plant | Road: No | Network: No | Level: 1 | Maintenance: 1 | Capacity: 10"
    );
    assert_eq!(
        format_inspect(&park),
        "(1, 0) Park | Road: No | Level: 1 | Maintenance: 1 | Happiness: +3"
    );
}

#[test]
fn inspect_out_of_bounds_formats_without_cell_data() {
    let game = Game::new(2, 2);
    let inspect = game.inspect(5, 5);

    assert_eq!(inspect.details, None);
    assert_eq!(format_inspect(&inspect), "(5, 5) is outside the map");
}

#[test]
fn inspect_explains_missing_adjacent_road() {
    let mut game = Game::new(3, 3);
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
    let mut game = Game::new(4, 4);
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
    let mut game = Game::new(8, 4);
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
    let mut game = Game::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(0, 1, BuildingKind::Road).success);
    assert!(game.build(1, 1, BuildingKind::Road).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);

    game.tick();
    let inspect = game.inspect(1, 0);

    assert!(
        inspect
            .explanations
            .contains(&"Population growth is blocked because no jobs are available.".to_string())
    );
}

#[test]
fn inspect_exposes_road_network_distance_notes() {
    let mut game = Game::new(6, 4);
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
    let mut game = Game::new(3, 3);
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
