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
    assert_eq!(format_inspect(&inspect), "(1, 1) Empty | buildable true");
}

#[test]
fn inspect_residential_shows_powered_state_and_population() {
    let mut game = Game::new(4, 4);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    game.tick();

    let inspect = game.inspect(1, 0);

    assert_eq!(
        inspect.details,
        Some(InspectDetailsView::Residential {
            powered: true,
            population: 0,
            max_population: 5
        })
    );
    assert_eq!(
        format_inspect(&inspect),
        "(1, 0) Residential | powered true | population 0/5"
    );
}

#[test]
fn inspect_commercial_and_industrial_show_powered_state_and_jobs() {
    let mut game = Game::new(5, 5);
    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Commercial).success);
    assert!(game.build(2, 0, BuildingKind::Industrial).success);
    game.tick();

    let commercial = game.inspect(1, 0);
    let industrial = game.inspect(2, 0);

    assert_eq!(
        commercial.details,
        Some(InspectDetailsView::Commercial {
            powered: true,
            jobs: 2
        })
    );
    assert_eq!(
        industrial.details,
        Some(InspectDetailsView::Industrial {
            powered: true,
            jobs: 3
        })
    );
    assert_eq!(
        format_inspect(&commercial),
        "(1, 0) Commercial | powered true | jobs 2"
    );
    assert_eq!(
        format_inspect(&industrial),
        "(2, 0) Industrial | powered true | jobs 3"
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
        Some(InspectDetailsView::PowerPlant { power_radius: 3 })
    );
    assert_eq!(
        park.details,
        Some(InspectDetailsView::Park {
            happiness_effect: 3
        })
    );
    assert_eq!(
        format_inspect(&power_plant),
        "(0, 0) Power Plant | power radius 3"
    );
    assert_eq!(format_inspect(&park), "(1, 0) Park | happiness effect +3");
}

#[test]
fn inspect_out_of_bounds_formats_without_cell_data() {
    let game = Game::new(2, 2);
    let inspect = game.inspect(5, 5);

    assert_eq!(inspect.details, None);
    assert_eq!(format_inspect(&inspect), "(5, 5) is outside the map");
}
