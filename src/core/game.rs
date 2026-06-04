//! Public Game API that owns the private ECS world and exposes UI-safe operations.

use std::fmt;
use std::fs::File;
use std::path::Path;

use crate::core::simulation::{refresh_derived_state_for_world, tick_world};
use crate::core::systems::{build, bulldoze, replace, upgrade};
use crate::core::world::World;
use crate::interface::adapter::{inspect_world, view_world, view_world_with_overlay};
use crate::interface::events::CommandResult;
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{BuildPreviewView, GameView, InspectView};

const DEFAULT_MAP_WIDTH: usize = 20;
const DEFAULT_MAP_HEIGHT: usize = 15;

#[derive(Debug)]
pub struct Game {
    world: World,
}

#[derive(Debug)]
pub enum GameError {
    Io(std::io::Error),
    SaveFormat(serde_json::Error),
}

impl fmt::Display for GameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "File error: {error}"),
            Self::SaveFormat(error) => write!(formatter, "Save file error: {error}"),
        }
    }
}

impl std::error::Error for GameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::SaveFormat(error) => Some(error),
        }
    }
}

impl From<std::io::Error> for GameError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for GameError {
    fn from(error: serde_json::Error) -> Self {
        Self::SaveFormat(error)
    }
}

impl Game {
    /// Creates a deterministic game state with a private ECS world.
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            world: World::new(width, height),
        }
    }

    /// Returns a UI-safe snapshot. Callers must render from this instead of reading World.
    pub fn view(&self) -> GameView {
        view_world(&self.world)
    }

    /// Returns a UI-safe snapshot using the requested map overlay.
    pub fn view_with_overlay(&self, overlay: MapOverlayInput) -> GameView {
        view_world_with_overlay(&self.world, overlay)
    }

    /// Applies one player build command through the core systems and returns UI-safe feedback.
    pub fn build(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        let result = build::build(&mut self.world, x, y, kind);
        // Build changes can affect derived city stats immediately, before the next turn.
        self.refresh_derived_state();
        result
    }

    /// Explains whether a build command would succeed without mutating game state.
    pub fn preview_build(&self, x: usize, y: usize, kind: BuildingKind) -> BuildPreviewView {
        build::preview_build(&self.world, x, y, kind)
    }

    /// Removes one occupied cell through the core systems and returns UI-safe feedback.
    pub fn bulldoze(&mut self, x: usize, y: usize) -> CommandResult {
        let result = bulldoze::bulldoze(&mut self.world, x, y);
        if result.success {
            self.refresh_derived_state();
        }
        result
    }

    /// Replaces one occupied cell with a new building type through the core systems.
    pub fn replace(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        let result = replace::replace(&mut self.world, x, y, kind);
        if result.success {
            self.refresh_derived_state();
        }
        result
    }

    /// Upgrades one supported occupied cell through the core systems.
    pub fn upgrade(&mut self, x: usize, y: usize) -> CommandResult {
        let result = upgrade::upgrade(&mut self.world, x, y);
        if result.success {
            self.refresh_derived_state();
        }
        result
    }

    /// Advances the simulation by one deterministic hour.
    pub fn tick(&mut self) -> CommandResult {
        tick_world(&mut self.world)
    }

    /// Returns a UI-safe view of one map coordinate without exposing ECS storage.
    pub fn inspect(&self, x: usize, y: usize) -> InspectView {
        inspect_world(&self.world, x, y)
    }

    /// Writes the complete private game state as JSON without exposing ECS storage to callers.
    pub fn save_to_file(&self, path: impl AsRef<Path>) -> Result<(), GameError> {
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, &self.world)?;
        Ok(())
    }

    /// Loads a JSON save and refreshes derived state before returning a usable Game API value.
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Game, GameError> {
        let file = File::open(path)?;
        let mut game = Self {
            world: serde_json::from_reader(file)?,
        };
        game.world.rebuild_entity_records();
        game.refresh_derived_state();
        Ok(game)
    }

    fn refresh_derived_state(&mut self) {
        refresh_derived_state_for_world(&mut self.world);
    }
}

impl Default for Game {
    fn default() -> Self {
        Self::new(DEFAULT_MAP_WIDTH, DEFAULT_MAP_HEIGHT)
    }
}

#[cfg(test)]
mod tests {
    use super::Game;
    use crate::core::systems::citizens;

    #[test]
    fn citizen_happiness_decay_happens_on_daily_boundary_not_hourly() {
        let (mut game, residential) = game_with_one_citizen();

        for _ in 0..23 {
            assert!(game.tick().success);
        }
        assert_eq!(citizen_happiness_decay(&game), 0);
        assert_eq!(
            citizens::average_happiness_for_home(&game.world, residential),
            Some(50)
        );

        assert!(game.tick().success);

        let average_happiness =
            citizens::average_happiness_for_home(&game.world, residential).expect("happiness");
        assert_eq!(citizen_happiness_decay(&game), 1);
        assert!(average_happiness < 50);
        assert!(game.view().status.happiness < 50);
    }

    fn game_with_one_citizen() -> (Game, crate::core::entity::Entity) {
        let mut game = Game::new(1, 1);
        let residential = game.world.spawn();
        citizens::spawn_for_home(&mut game.world, residential, 1);
        game.refresh_derived_state();
        (game, residential)
    }

    fn citizen_happiness_decay(game: &Game) -> i32 {
        game.world
            .citizens
            .values()
            .next()
            .expect("citizen")
            .happiness_decay
    }
}
