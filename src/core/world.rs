use std::collections::HashMap;

use crate::core::components::{
    Building, HappinessEffect, PollutionSource, Population, Position, PowerConsumer, PowerProvider,
};
use crate::core::entity::Entity;
use crate::core::grid::Grid;
use crate::core::resources::{CityResources, CityStats};

#[derive(Debug)]
pub(crate) struct World {
    pub next_entity: u32,
    pub grid: Grid,
    pub resources: CityResources,
    pub stats: CityStats,
    pub positions: HashMap<Entity, Position>,
    pub buildings: HashMap<Entity, Building>,
    pub populations: HashMap<Entity, Population>,
    pub power_providers: HashMap<Entity, PowerProvider>,
    pub power_consumers: HashMap<Entity, PowerConsumer>,
    pub pollution_sources: HashMap<Entity, PollutionSource>,
    pub happiness_effects: HashMap<Entity, HappinessEffect>,
}

impl World {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            next_entity: 0,
            grid: Grid::new(width, height),
            resources: CityResources::default(),
            stats: CityStats::default(),
            positions: HashMap::new(),
            buildings: HashMap::new(),
            populations: HashMap::new(),
            power_providers: HashMap::new(),
            power_consumers: HashMap::new(),
            pollution_sources: HashMap::new(),
            happiness_effects: HashMap::new(),
        }
    }

    pub fn spawn(&mut self) -> Entity {
        let entity = Entity(self.next_entity);
        self.next_entity += 1;
        entity
    }
}
