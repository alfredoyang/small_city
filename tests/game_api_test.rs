use small_city::core::components::BuildingKind;
use small_city::core::game::Game;

#[test]
fn industrial_creates_pollution() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::Industrial).success);

    game.tick();

    assert_eq!(game.view().status.pollution, 2);
}

#[test]
fn park_reduces_pollution_effect() {
    let mut game = Game::new(10, 10);
    assert!(game.build(0, 0, BuildingKind::Industrial).success);
    assert!(game.build(1, 0, BuildingKind::Park).success);

    game.tick();

    assert_eq!(game.view().status.pollution, 1);
}

#[test]
fn happiness_includes_park_bonus() {
    let mut high_happiness = Game::new(10, 10);
    for x in 0..10 {
        assert!(high_happiness.build(x, 0, BuildingKind::Park).success);
    }
    high_happiness.tick();
    assert_eq!(high_happiness.view().status.happiness, 80);
}

#[test]
fn tick_advances_turn_deterministically() {
    let mut game = Game::new(10, 10);

    game.tick();
    game.tick();

    assert_eq!(game.view().status.turn, 2);
}
