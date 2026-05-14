use small_city::core::game::Game;
use small_city::interface::input::BuildingKind;

#[test]
fn powered_residential_and_commercial_city_grows_over_five_ticks() {
    let mut game = Game::new(10, 10);

    assert!(game.build(0, 0, BuildingKind::PowerPlant).success);
    assert!(game.build(1, 0, BuildingKind::Residential).success);
    assert!(game.build(2, 0, BuildingKind::Commercial).success);

    let starting_view = game.view();
    let starting_money = starting_view.status.money;
    let starting_population = starting_view.status.population;

    for _ in 0..5 {
        assert!(game.tick().success);
    }

    let view = game.view();

    assert!(view.status.population > starting_population);
    assert_eq!(view.status.turn, 5);
    assert_ne!(view.status.money, starting_money);
    assert!((0..=100).contains(&view.status.happiness));

    // The UI contract stays intact after a multi-system scenario.
    assert_eq!(view.map.width, 10);
    assert_eq!(view.map.height, 10);
    assert_eq!(view.map.cells.len(), view.map.width * view.map.height);
}
