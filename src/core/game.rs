use std::fmt;
use std::fs::File;
use std::path::Path;

use crate::core::systems::{build, economy, happiness, pollution, population, power, stats};
use crate::core::world::World;
use crate::interface::adapter::{inspect_world, view_world};
use crate::interface::events::{CommandResult, GameEventView};
use crate::interface::input::BuildingKind;
use crate::interface::view::{GameView, InspectView};

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

    /// Applies one player build command through the core systems and returns UI-safe feedback.
    pub fn build(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        let result = build::build(&mut self.world, x, y, kind);
        // Build changes can affect derived city stats immediately, before the next turn.
        stats::refresh_population_and_jobs(&mut self.world);
        pollution::run(&mut self.world);
        happiness::run(&mut self.world);
        result
    }

    /// Advances the simulation by one deterministic turn.
    pub fn tick(&mut self) -> CommandResult {
        power::run(&mut self.world);
        stats::run(&mut self.world);
        population::run(&mut self.world);
        economy::run(&mut self.world);
        stats::refresh_population_and_jobs(&mut self.world);
        pollution::run(&mut self.world);
        happiness::run(&mut self.world);
        self.world.resources.turn += 1;
        CommandResult::success(GameEventView::TurnAdvanced {
            turn: self.world.resources.turn,
        })
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
        game.refresh_derived_state();
        Ok(game)
    }

    fn refresh_derived_state(&mut self) {
        power::run(&mut self.world);
        stats::refresh_population_and_jobs(&mut self.world);
        pollution::run(&mut self.world);
        happiness::run(&mut self.world);
    }
}

impl Default for Game {
    fn default() -> Self {
        Self::new(10, 10)
    }
}
