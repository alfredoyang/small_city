//! Round-trip serde tests for UI-facing interface-layer types.
//!
//! These tests assert that every view / event / input type the browser UI will need
//! over a JSON wire can be round-tripped losslessly (serialize → deserialize → `==`).

use small_city::interface::events::{CommandResult, GameEventView};
use small_city::interface::input::{BuildingKind, MapOverlayInput, UiCommand};
use small_city::interface::view::*;

#[test]
fn game_view_round_trips_through_json() {
    let view = sample_game_view();

    let json = serde_json::to_string(&view).expect("serialize GameView");
    let recovered: GameView = serde_json::from_str(&json).expect("deserialize GameView");

    assert_eq!(view, recovered);
}

#[test]
fn inspect_view_round_trips_through_json() {
    let view = sample_inspect_view();

    let json = serde_json::to_string(&view).expect("serialize InspectView");
    let recovered: InspectView = serde_json::from_str(&json).expect("deserialize InspectView");

    assert_eq!(view, recovered);
}

#[test]
fn ui_command_round_trips_through_json() {
    for cmd in sample_commands() {
        let json = serde_json::to_string(&cmd).unwrap();
        let recovered: UiCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, recovered);
    }
}

#[test]
fn command_result_round_trips_through_json() {
    let success = CommandResult::success(GameEventView::TurnAdvanced { turn: 42 });
    let failure = CommandResult::failure(GameEventView::BuildFailed {
        reason: "no money".into(),
    });

    for result in [success, failure] {
        let json = serde_json::to_string(&result).unwrap();
        let recovered: CommandResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, recovered);
    }
}

// ── sample values ───────────────────────────────────────────────────────────

fn sample_game_view() -> GameView {
    GameView {
        map: MapView {
            width: 20,
            height: 15,
            cells: vec![CellView {
                x: 0,
                y: 0,
                symbol: '=',
                building: Some(BuildingKind::Road),
                label: String::new(),
                buildable: false,
                population: None,
                max_population: None,
                powered: Some(true),
                power_demand: None,
                road_connected: Some(true),
                road_links: RoadLinks {
                    north: true,
                    east: false,
                    south: false,
                    west: false,
                },
                upgrade_level: None,
                job_assignments: vec![],
                local_effects: LocalEffectsView::default(),
                footprint_anchor: true,
                footprint_area: 1,
            }],
        },
        status: CityStatusView {
            money: 50_000,
            turn: 12,
            time: GameTimeView {
                total_hours: 8_640,
                year: 1,
                month: 7,
                week: 3,
                day: 5,
                hour: 14,
                label: "Year 1 - July (Week 3)".into(),
            },
            population: 200,
            citizens: 180,
            jobs: 90,
            unemployment: 20,
            pollution: 5,
            happiness: 70,
            average_citizen_happiness: Some(68),
            average_citizen_happiness_target: Some(75),
            average_citizen_money: Some(12_000),
            demand: CityDemand {
                residential: DemandLevel::High,
                commercial: DemandLevel::Medium,
                industrial: DemandLevel::Low,
            },
            power: PowerStatusView {
                total_capacity: 80,
                total_demand: 60,
                total_supplied: 60,
                total_shortage: 0,
            },
            goods: CityGoodsView {
                city_goods_produced: 30,
                goods_imported_from_outside: 15,
                goods_exported_outside: 10,
            },
        },
        build_options: vec![BuildOptionView {
            kind: BuildingKind::Residential,
            label: "Residential".into(),
            cost: 1_000,
            maintenance_cost: 50,
        }],
        travelers: vec![CitizenTravelView { x: 5, y: 3 }],
    }
}

fn sample_inspect_view() -> InspectView {
    InspectView {
        x: 1,
        y: 2,
        in_bounds: true,
        cell: Some(CellView {
            x: 1,
            y: 2,
            symbol: 'R',
            building: Some(BuildingKind::Road),
            label: String::new(),
            buildable: false,
            population: None,
            max_population: None,
            powered: Some(true),
            power_demand: None,
            road_connected: Some(true),
            road_links: RoadLinks::default(),
            upgrade_level: None,
            job_assignments: vec![],
            local_effects: LocalEffectsView {
                land_value: 50,
                pollution_pressure: -5,
                accessibility: 80,
                desirability: 42,
            },
            footprint_anchor: true,
            footprint_area: 1,
        }),
        details: Some(InspectDetailsView::Road),
        local_effects: Some(LocalEffectsView {
            land_value: 50,
            pollution_pressure: -5,
            accessibility: 80,
            desirability: 42,
        }),
        flags: vec![InspectFlag::GrowthBlockedNoJobs],
        explanations: vec!["This road is connected to the city grid.".into()],
        roster: vec![CitizenDetailView {
            age: 35,
            happiness: 70,
            money: 12_000,
            relation: CitizenRelation::LivesAt {
                region: None,
                x: 4,
                y: 7,
            },
        }],
        road_traveler_count: 2,
    }
}

fn sample_commands() -> Vec<UiCommand> {
    vec![
        UiCommand::Build {
            kind: BuildingKind::Residential,
            x: 3,
            y: 4,
        },
        UiCommand::Next,
        UiCommand::Inspect { x: 5, y: 6 },
        UiCommand::Status,
        UiCommand::View {
            overlay: MapOverlayInput::Pollution,
        },
        UiCommand::Quit,
    ]
}
