//! Public Game API that owns the private ECS world and exposes UI-safe operations.

use std::fmt;
use std::fs::File;
use std::path::Path;

use crate::core::resources::{is_new_day, is_new_week};
use crate::core::systems::{
    build, bulldoze, business_growth, citizens, economy, happiness, local_effects, pollution,
    population, power, replace, road_network_analysis, stats, upgrade,
};
use crate::core::world::World;
use crate::interface::adapter::{inspect_world, view_world, view_world_with_overlay};
use crate::interface::events::{CommandResult, EconomyBreakdownView, GameEventView, MetricChange};
use crate::interface::input::{BuildingKind, MapOverlayInput};
use crate::interface::view::{BuildPreviewView, GameTimeView, GameView, InspectView};

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
        let before = TickSummarySnapshot::from_world(&self.world);
        let before_time = self.world.resources.time;
        self.world.resources.time.advance_hours(1);
        let after_time = self.world.resources.time;
        power::run(&mut self.world);
        stats::run(&mut self.world);
        local_effects::run(&mut self.world);
        if is_new_week(before_time, after_time) {
            population::run(&mut self.world);
        }
        citizens::update_happiness(&mut self.world);
        local_effects::run(&mut self.world);
        let economy = if is_new_day(before_time, after_time) {
            economy::run(&mut self.world)
        } else {
            economy::EconomyBreakdown::default()
        };
        if is_new_week(before_time, after_time) {
            business_growth::run(&mut self.world);
        }
        stats::refresh_population_and_jobs(&mut self.world);
        pollution::run(&mut self.world);
        happiness::run(&mut self.world);
        self.world.resources.turn += 1;
        let after = TickSummarySnapshot::from_world(&self.world);

        CommandResult::success(GameEventView::TickSummary {
            turn: self.world.resources.turn,
            time: game_time_view(self.world.resources.time),
            population: metric_change(before.population, after.population),
            money: metric_change(before.money, after.money),
            happiness: metric_change(before.happiness, after.happiness),
            pollution: metric_change(before.pollution, after.pollution),
            unemployment: metric_change(before.unemployment, after.unemployment),
            powered_buildings: metric_change(before.powered_buildings, after.powered_buildings),
            economy: EconomyBreakdownView {
                salaries_paid: economy.salaries_paid,
                workplace_tax: economy.workplace_tax,
                rent_income: economy.rent_income,
                commercial_sales_tax: economy.commercial_sales_tax,
                shoppers_served: economy.shoppers_served,
                local_goods_produced: economy.local_goods_produced,
                local_goods_stored: economy.local_goods_stored,
                local_goods_sold: economy.local_goods_sold,
                imported_goods_sold: economy.imported_goods_sold,
                exported_goods: economy.exported_goods,
                manufacturing_tax: economy.manufacturing_tax,
                export_tax: economy.export_tax,
                rent_failures: economy.rent_failures,
                maintenance_cost: economy.maintenance_cost,
                net: economy.net,
            },
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
        game.world.rebuild_entity_records();
        game.refresh_derived_state();
        Ok(game)
    }

    fn refresh_derived_state(&mut self) {
        power::run(&mut self.world);
        road_network_analysis::run(&mut self.world);
        stats::refresh_population_and_jobs(&mut self.world);
        pollution::run(&mut self.world);
        citizens::update_happiness(&mut self.world);
        happiness::run(&mut self.world);
        local_effects::run(&mut self.world);
    }
}

impl Default for Game {
    fn default() -> Self {
        Self::new(DEFAULT_MAP_WIDTH, DEFAULT_MAP_HEIGHT)
    }
}

#[derive(Debug, Clone, Copy)]
struct TickSummarySnapshot {
    population: i32,
    money: i32,
    happiness: i32,
    pollution: i32,
    unemployment: i32,
    powered_buildings: i32,
}

impl TickSummarySnapshot {
    fn from_world(world: &World) -> Self {
        Self {
            population: world.stats.population,
            money: world.resources.money,
            happiness: world.stats.happiness,
            pollution: world.stats.pollution,
            unemployment: world.stats.unemployment,
            powered_buildings: world
                .power_consumers
                .values()
                .filter(|consumer| consumer.powered)
                .count() as i32,
        }
    }
}

fn metric_change<T>(before: T, after: T) -> MetricChange<T> {
    MetricChange { before, after }
}

fn game_time_view(time: crate::core::resources::GameTime) -> GameTimeView {
    GameTimeView {
        total_hours: time.total_hours,
        year: time.year(),
        month: time.month(),
        week: time.week_of_month(),
        day: time.day_of_week(),
        hour: time.hour_of_day(),
        label: time.label(),
    }
}
