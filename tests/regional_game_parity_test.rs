//! Parity tests proving a single-region facade matches the single-city game view.

use small_city::core::game::Game;
use small_city::core::regional_game::{RegionalGame, RegionalGameView};
use small_city::core::regions::{RegionId, RegionState};
use small_city::interface::input::{BuildingKind, MapOverlayInput};
use small_city::interface::view::GameView;

const REGION_ID: RegionId = RegionId(1);

#[derive(Debug, Clone, Copy)]
enum ScriptStep {
    Build(usize, usize, BuildingKind),
    PreviewBuild(usize, usize, BuildingKind),
    Bulldoze(usize, usize),
    Replace(usize, usize, BuildingKind),
    Upgrade(usize, usize),
    Tick,
}

#[test]
fn single_region_facade_matches_game_view_after_each_script_step() {
    let mut game = Game::new(6, 5);
    let regional = RegionalGame::from_regions(vec![RegionState::new(REGION_ID, 6, 5)]).unwrap();

    assert_views_match("initial state", &game.view(), &regional.view().unwrap());

    for (index, step) in script().into_iter().enumerate() {
        apply_step_with_result_assertions(&mut game, &regional, step);

        let game_view = game.view();
        let regional_view = regional.view().unwrap();
        assert_views_match(
            &format!(
                "after step {index}: {} at turn {}",
                step.description(),
                game_view.status.turn
            ),
            &game_view,
            &regional_view,
        );
    }

    assert_inspect_views_match("after full script", &game, &regional);
}

#[test]
fn single_region_facade_overlay_views_match_game() {
    let mut game = Game::new(6, 5);
    let regional = RegionalGame::from_regions(vec![RegionState::new(REGION_ID, 6, 5)]).unwrap();

    for step in script() {
        apply_step_without_result_assertions(&mut game, &regional, step);
    }

    for overlay in overlays() {
        let game_view = game.view_with_overlay(overlay);
        let regional_view = regional.view_with_overlay(overlay).unwrap();
        assert_views_match(
            &format!("overlay {overlay:?} after full script"),
            &game_view,
            &regional_view,
        );
    }
}

fn script() -> Vec<ScriptStep> {
    let mut steps = vec![
        ScriptStep::Build(0, 1, BuildingKind::Road),
        ScriptStep::Build(1, 1, BuildingKind::Road),
        ScriptStep::Build(2, 1, BuildingKind::Road),
        ScriptStep::Build(0, 0, BuildingKind::PowerPlant),
        ScriptStep::Build(2, 0, BuildingKind::Residential),
        ScriptStep::Build(3, 0, BuildingKind::Commercial),
        ScriptStep::Build(4, 1, BuildingKind::Industrial),
        ScriptStep::Build(2, 2, BuildingKind::Park),
        ScriptStep::PreviewBuild(5, 4, BuildingKind::Commercial),
    ];
    for _ in 0..30 {
        steps.push(ScriptStep::Tick);
    }
    steps.extend([
        ScriptStep::Upgrade(0, 0),
        ScriptStep::Upgrade(2, 0),
        ScriptStep::Replace(3, 0, BuildingKind::Industrial),
        ScriptStep::Bulldoze(1, 1),
        ScriptStep::PreviewBuild(1, 1, BuildingKind::Road),
    ]);
    steps
}

fn apply_step_with_result_assertions(game: &mut Game, regional: &RegionalGame, step: ScriptStep) {
    match step {
        ScriptStep::Build(x, y, kind) => {
            assert_eq!(
                game.build(x, y, kind),
                regional.build(REGION_ID, x, y, kind).unwrap()
            );
        }
        ScriptStep::PreviewBuild(x, y, kind) => {
            assert_eq!(
                game.preview_build(x, y, kind),
                regional.preview_build(REGION_ID, x, y, kind).unwrap()
            );
        }
        ScriptStep::Bulldoze(x, y) => {
            assert_eq!(
                game.bulldoze(x, y),
                regional.bulldoze(REGION_ID, x, y).unwrap()
            );
        }
        ScriptStep::Replace(x, y, kind) => {
            assert_eq!(
                game.replace(x, y, kind),
                regional.replace(REGION_ID, x, y, kind).unwrap()
            );
        }
        ScriptStep::Upgrade(x, y) => {
            assert_eq!(
                game.upgrade(x, y),
                regional.upgrade(REGION_ID, x, y).unwrap()
            );
        }
        ScriptStep::Tick => {
            assert!(game.tick().success);
            regional.tick_region(REGION_ID).unwrap();
        }
    }
}

fn apply_step_without_result_assertions(
    game: &mut Game,
    regional: &RegionalGame,
    step: ScriptStep,
) {
    match step {
        ScriptStep::Build(x, y, kind) => {
            game.build(x, y, kind);
            regional.build(REGION_ID, x, y, kind).unwrap();
        }
        ScriptStep::PreviewBuild(x, y, kind) => {
            game.preview_build(x, y, kind);
            regional.preview_build(REGION_ID, x, y, kind).unwrap();
        }
        ScriptStep::Bulldoze(x, y) => {
            game.bulldoze(x, y);
            regional.bulldoze(REGION_ID, x, y).unwrap();
        }
        ScriptStep::Replace(x, y, kind) => {
            game.replace(x, y, kind);
            regional.replace(REGION_ID, x, y, kind).unwrap();
        }
        ScriptStep::Upgrade(x, y) => {
            game.upgrade(x, y);
            regional.upgrade(REGION_ID, x, y).unwrap();
        }
        ScriptStep::Tick => {
            game.tick();
            regional.tick_region(REGION_ID).unwrap();
        }
    }
}

fn assert_views_match(context: &str, game_view: &GameView, regional_view: &RegionalGameView) {
    assert_eq!(
        regional_view.selected_region,
        Some(REGION_ID),
        "{context}: selected region differs"
    );
    assert_eq!(
        regional_view.regions.len(),
        1,
        "{context}: regional facade should expose one region"
    );
    assert_eq!(
        regional_view.regions[0].region_id, REGION_ID,
        "{context}: snapshot region differs"
    );
    assert_eq!(
        &regional_view.regions[0].view, game_view,
        "{context}: GameView diverged"
    );
}

fn assert_inspect_views_match(context: &str, game: &Game, regional: &RegionalGame) {
    for (x, y) in [(0, 0), (1, 1), (2, 0), (4, 1), (5, 4)] {
        assert_eq!(
            regional.inspect_region(REGION_ID, x, y).unwrap(),
            game.inspect(x, y),
            "{context}: inspect view diverged at ({x}, {y})"
        );
    }
}

fn overlays() -> [MapOverlayInput; 6] {
    [
        MapOverlayInput::Normal,
        MapOverlayInput::Power,
        MapOverlayInput::Pollution,
        MapOverlayInput::Population,
        MapOverlayInput::LandValue,
        MapOverlayInput::Desirability,
    ]
}

impl ScriptStep {
    fn description(self) -> String {
        match self {
            Self::Build(x, y, kind) => format!("build {kind:?} at ({x}, {y})"),
            Self::PreviewBuild(x, y, kind) => format!("preview {kind:?} at ({x}, {y})"),
            Self::Bulldoze(x, y) => format!("bulldoze ({x}, {y})"),
            Self::Replace(x, y, kind) => format!("replace ({x}, {y}) with {kind:?}"),
            Self::Upgrade(x, y) => format!("upgrade ({x}, {y})"),
            Self::Tick => "tick".to_string(),
        }
    }
}
