//! Private ECS world storage for entities, component maps, grid, resources, and derived state.
//!
//! A `World` is **one self-contained city's ECS instance** — the substrate every
//! `systems/` function operates on (`fn run(world: &mut World)`) and the unit that
//! is serialized on save. It is deliberately **region-agnostic**: it holds one
//! region's data but knows nothing about regions, neighbors, threads, or
//! cross-region sharing. Region identity (`RegionId`) and all cross-region
//! coordination live one layer up in `RegionState`, which owns a `World`. So the
//! name follows the ECS convention ("one simulation instance"), not "the whole
//! game" — there is one `World` per region, and a single-city game is simply a
//! one-region `RegionalGame`. Owned by exactly one worker thread at a time; moved
//! between threads, never shared.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::core::components::{
    Building, Citizen, HappinessEffect, PollutionSource, Population, Position, PowerConsumer,
    PowerProvider,
};
use crate::core::entity::Entity;
use crate::core::grid::Grid;
use crate::core::resource_registry::{
    JobCounts, JobResolution, PowerResolution, ResourceRegistryCache,
};
use crate::core::resources::{CityResources, CityStats, LocalEffectsMap};
use crate::core::systems::road_network_analysis::RoadNetworkAnalysis;
use crate::interface::input::BuildingKind;
use std::cell::RefCell;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct World {
    #[serde(rename = "next_entity_id")]
    pub next_entity: u32,
    #[serde(default)]
    pub entities: HashMap<Entity, EntityRecord>,
    pub grid: Grid,
    pub resources: CityResources,
    pub stats: CityStats,
    #[serde(default)]
    pub local_effects: LocalEffectsMap,
    #[serde(skip, default)]
    pub road_analysis: RoadNetworkAnalysis,
    #[serde(skip, default)]
    registry_cache: RefCell<ResourceRegistryCache>,
    pub positions: HashMap<Entity, Position>,
    pub buildings: HashMap<Entity, Building>,
    pub populations: HashMap<Entity, Population>,
    #[serde(default)]
    pub citizens: HashMap<Entity, Citizen>,
    pub power_providers: HashMap<Entity, PowerProvider>,
    pub power_consumers: HashMap<Entity, PowerConsumer>,
    pub pollution_sources: HashMap<Entity, PollutionSource>,
    pub happiness_effects: HashMap<Entity, HappinessEffect>,
}

/// Registry entry describing which component maps should contain data for an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct EntityRecord {
    pub kind: Option<BuildingKind>,
    pub has_position: bool,
    pub has_population: bool,
    pub has_citizen: bool,
    pub has_power_provider: bool,
    pub has_power_consumer: bool,
    pub has_pollution_source: bool,
    pub has_happiness_effect: bool,
}

impl World {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            next_entity: 0,
            entities: HashMap::new(),
            grid: Grid::new(width, height),
            resources: CityResources::default(),
            stats: CityStats::default(),
            local_effects: LocalEffectsMap::new(width, height),
            road_analysis: RoadNetworkAnalysis::default(),
            registry_cache: RefCell::default(),
            positions: HashMap::new(),
            buildings: HashMap::new(),
            populations: HashMap::new(),
            citizens: HashMap::new(),
            power_providers: HashMap::new(),
            power_consumers: HashMap::new(),
            pollution_sources: HashMap::new(),
            happiness_effects: HashMap::new(),
        }
    }

    pub fn spawn(&mut self) -> Entity {
        let entity = Entity(self.next_entity);
        self.next_entity += 1;
        self.entities.insert(entity, EntityRecord::default());
        entity
    }

    pub(crate) fn attach_position(&mut self, entity: Entity, position: Position) {
        self.positions.insert(entity, position);
        self.record_mut(entity).has_position = true;
        self.invalidate_resource_registry();
    }

    pub(crate) fn attach_building(&mut self, entity: Entity, building: Building) {
        self.buildings.insert(entity, building);
        self.record_mut(entity).kind = Some(building.kind);
        self.invalidate_resource_registry();
    }

    pub(crate) fn attach_population(&mut self, entity: Entity, population: Population) {
        self.populations.insert(entity, population);
        self.record_mut(entity).has_population = true;
        self.invalidate_jobs_registry();
    }

    pub(crate) fn attach_citizen(&mut self, entity: Entity, citizen: Citizen) {
        self.citizens.insert(entity, citizen);
        self.record_mut(entity).has_citizen = true;
        self.invalidate_jobs_registry();
    }

    pub(crate) fn attach_power_provider(&mut self, entity: Entity, provider: PowerProvider) {
        self.power_providers.insert(entity, provider);
        self.record_mut(entity).has_power_provider = true;
        self.invalidate_resource_registry();
    }

    pub(crate) fn attach_power_consumer(&mut self, entity: Entity, consumer: PowerConsumer) {
        self.power_consumers.insert(entity, consumer);
        self.record_mut(entity).has_power_consumer = true;
        self.invalidate_resource_registry();
    }

    pub(crate) fn attach_pollution_source(&mut self, entity: Entity, source: PollutionSource) {
        self.pollution_sources.insert(entity, source);
        self.record_mut(entity).has_pollution_source = true;
    }

    pub(crate) fn attach_happiness_effect(&mut self, entity: Entity, effect: HappinessEffect) {
        self.happiness_effects.insert(entity, effect);
        self.record_mut(entity).has_happiness_effect = true;
    }

    pub(crate) fn rebuild_entity_records(&mut self) {
        self.entities.clear();
        for entity in self.positions.keys().copied().collect::<Vec<_>>() {
            self.record_mut(entity).has_position = true;
        }
        for (entity, building) in self.buildings.clone() {
            self.record_mut(entity).kind = Some(building.kind);
        }
        for entity in self.populations.keys().copied().collect::<Vec<_>>() {
            self.record_mut(entity).has_population = true;
        }
        for entity in self.citizens.keys().copied().collect::<Vec<_>>() {
            self.record_mut(entity).has_citizen = true;
        }
        for entity in self.power_providers.keys().copied().collect::<Vec<_>>() {
            self.record_mut(entity).has_power_provider = true;
        }
        for entity in self.power_consumers.keys().copied().collect::<Vec<_>>() {
            self.record_mut(entity).has_power_consumer = true;
        }
        for entity in self.pollution_sources.keys().copied().collect::<Vec<_>>() {
            self.record_mut(entity).has_pollution_source = true;
        }
        for entity in self.happiness_effects.keys().copied().collect::<Vec<_>>() {
            self.record_mut(entity).has_happiness_effect = true;
        }
        self.invalidate_resource_registry();
    }

    /// Mark all registry entries dirty after topology/provider/consumer changes.
    pub(crate) fn invalidate_resource_registry(&self) {
        self.registry_cache.borrow_mut().invalidate_all();
    }

    /// Mark only job entries dirty after citizen or workplace-effect changes.
    pub(crate) fn invalidate_jobs_registry(&self) {
        self.registry_cache.borrow_mut().invalidate_jobs();
    }

    /// Return cached local power resolution, recomputing lazily when dirty.
    pub(crate) fn cached_power_resolution(&self) -> PowerResolution {
        self.registry_cache.borrow_mut().power_resolution(self)
    }

    /// Return cached local job resolution, recomputing lazily when dirty.
    pub(crate) fn cached_job_resolution(&self) -> JobResolution {
        self.registry_cache.borrow_mut().job_resolution(self)
    }

    /// Read the cached remaining workplace slots without cloning full job output.
    pub(crate) fn with_cached_remaining_job_workplaces<R>(
        &self,
        read: impl FnOnce(&[Entity]) -> R,
    ) -> R {
        self.registry_cache
            .borrow_mut()
            .with_remaining_job_workplaces(self, read)
    }

    /// Count-only job stats derived from the cached job registry.
    pub(crate) fn cached_job_counts(&self) -> JobCounts {
        let jobs = self.cached_job_resolution();
        JobCounts {
            total_jobs: jobs.total_jobs,
            job_seekers: jobs.job_seekers,
            unemployment: jobs.unemployment,
        }
    }

    #[cfg(test)]
    pub(crate) fn registry_cache_recompute_counts(&self) -> (usize, usize) {
        self.registry_cache.borrow().recompute_counts()
    }

    fn record_mut(&mut self, entity: Entity) -> &mut EntityRecord {
        self.entities.entry(entity).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::World;
    use crate::core::components::{Building, BuildingData, Citizen, Population, Position};
    use crate::interface::input::BuildingKind;

    #[test]
    fn attach_helpers_record_entity_shape() {
        let mut world = World::new(2, 2);
        let entity = world.spawn();

        world.attach_position(entity, Position { x: 1, y: 1 });
        world.attach_building(
            entity,
            Building {
                kind: BuildingKind::Residential,
                level: 1,
                data: BuildingData::None,
            },
        );
        world.attach_population(entity, Population { current: 0, max: 5 });

        let record = world.entities.get(&entity).expect("entity record");
        assert_eq!(record.kind, Some(BuildingKind::Residential));
        assert!(record.has_position);
        assert!(record.has_population);
        assert!(!record.has_citizen);
        assert!(!record.has_power_provider);
    }

    #[test]
    fn attach_helpers_record_citizen_shape() {
        let mut world = World::new(2, 2);
        let residential = world.spawn();
        let citizen = world.spawn();

        world.attach_citizen(
            citizen,
            Citizen {
                age: 0,
                home: residential,
                workplace: None,
                remote_workplace: None,
                happiness: 50,
                happiness_decay: 0,
                money: 0,
                rent_stress: 0,
            },
        );

        let record = world.entities.get(&citizen).expect("citizen record");
        assert!(record.has_citizen);
        assert!(!record.has_position);
        assert_eq!(record.kind, None);
    }

    #[test]
    fn rebuild_entity_records_recovers_component_shape() {
        let mut world = World::new(2, 2);
        let entity = world.spawn();
        world.positions.insert(entity, Position { x: 0, y: 0 });
        world.buildings.insert(
            entity,
            Building {
                kind: BuildingKind::Park,
                level: 1,
                data: BuildingData::None,
            },
        );
        world.entities.clear();

        world.rebuild_entity_records();

        let record = world.entities.get(&entity).expect("entity record");
        assert_eq!(record.kind, Some(BuildingKind::Park));
        assert!(record.has_position);
    }
}
