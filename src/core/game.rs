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

impl Game {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            world: World::new(width, height),
        }
    }

    pub fn view(&self) -> GameView {
        view_world(&self.world)
    }

    pub fn build(&mut self, x: usize, y: usize, kind: BuildingKind) -> CommandResult {
        let result = build::build(&mut self.world, x, y, kind);
        stats::refresh_population_and_jobs(&mut self.world);
        pollution::run(&mut self.world);
        happiness::run(&mut self.world);
        result
    }

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

    pub fn inspect(&self, x: usize, y: usize) -> InspectView {
        inspect_world(&self.world, x, y)
    }
}

impl Default for Game {
    fn default() -> Self {
        Self::new(10, 10)
    }
}
