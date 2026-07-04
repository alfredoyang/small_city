//! Private ECS world storage for entities, component maps, grid, resources, and derived state.
//!
//! A `World` is **one self-contained city's ECS instance** — the substrate every
//! `systems/` function operates on (`fn run(world: &mut World)`) and the unit that
//! is serialized on save. It records its owning `region_id` (a tag stamped by
//! `RegionState` at construction/load) so citizen references can be city-wide
//! (`Entity`), but it still knows nothing about neighbors, threads, or
//! cross-region coordination — those live one layer up in `RegionState`, which owns a
//! `World`. So the name follows the ECS convention ("one simulation instance"), not
//! "the whole game" — there is one `World` per region, and a single-city game is
//! simply a one-region `RegionalGame`. Owned by exactly one worker thread at a time;
//! moved between threads, never shared.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::core::components::{
    Building, Citizen, HappinessEffect, PollutionSource, Population, Position, PowerConsumer,
    PowerProvider,
};
use crate::core::entity::Entity;
use crate::core::grid::Grid;
use crate::core::regions::{RegionId, RouteExit};
use crate::core::resource_registry::{
    JobCounts, JobResolution, PowerResolution, ResourceRegistryCache,
};
use crate::core::resources::{CityResources, CityStats, LocalEffectsMap};
use crate::core::systems::road_connectivity::RoadNetwork;
use crate::core::systems::road_network_analysis::{RoadNetworkAnalysis, road_predecessors};
use crate::core::systems::route_cache::RouteCache;
use crate::interface::input::BuildingKind;
use std::cell::{Cell, RefCell};

fn default_region_id() -> RegionId {
    RegionId(0)
}

#[derive(Debug, Serialize, Deserialize)]
// TODO: consider renaming this type to `RegionWorld`. `World` follows ECS
// convention, but in multi-region mode it means one region's ECS store, not the
// whole city.
pub(crate) struct World {
    // The region that owns this world. Not serialized (region identity is owned by
    // `RegionState`); stamped at the region load/construction boundary via
    // `set_region_id`, which also tags every citizen's local home. Used to build
    // correctly-tagged `Entity`s and (from CW3 on) to resolve foreign refs.
    #[serde(skip, default = "default_region_id")]
    pub(crate) region_id: RegionId,
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
    #[serde(skip, default)]
    pub(crate) importable_remote_jobs: i32,
    #[serde(skip, default)]
    pub(crate) cross_region_goods_routes: CrossRegionGoodsRoutes,
    // P2: region-owned route cache. `came_from` trees keyed by
    // (destination, road_network_id), recomputed on miss, invalidated at the
    // placement / entity_cleanup / upgrade chokepoints. `#[serde(skip)]`
    // because the cache is derived state — a freshly-loaded world starts with
    // an empty cache and the first access recomputes.
    #[serde(skip, default)]
    route_cache: RefCell<RouteCache>,
    // P3 + P5 (R-a): one `TravelToken` per citizen *while away from home* (in the
    // region where the body physically is; idle-at-home = no token). Keyed by the
    // citizen entity (globally unique across regions). `#[serde(skip)]` because
    // it is transient display/derived state — `step_tokens` rebuilds it from the
    // daily schedule each tick, so a freshly-loaded world starts empty and
    // re-derives placement on the next tick.
    #[serde(skip, default)]
    pub(crate) tokens: HashMap<Entity, crate::core::components::TravelToken>,
    // R-a: the home region's record of residents currently away across a region
    // boundary. Inserted on cross-out (the token is placed in the neighbour),
    // removed on home-arrival (the token is back, idle-at-home, no token
    // needed). Together with `away_generation` it disambiguates a cross-region
    // away resident from an idle/new one. `#[serde(skip)]` like `tokens`.
    #[serde(skip, default)]
    pub(crate) away_residents: std::collections::HashSet<Entity>,
    // P5: crossings this region decided on this tick (moves and rollbacks),
    // drained by the regions layer, which adds border-link routing and sends
    // them. The core only produces them; it never routes.
    #[serde(skip, default)]
    pub(crate) outgoing_handoffs: Vec<crate::core::components::PendingHandoff>,
    // P5/P-c input: "to reach final target region R, walk to one of these local
    // route exits." Each candidate carries its road cell, border link, and immediate
    // next-hop region from the Layer-1 route map. Empty means no remote commuting.
    #[serde(skip, default)]
    pub(crate) remote_exit_cells: HashMap<RegionId, Vec<RouteExit>>,
    // P5: per-citizen generation of the trip currently out of region, so a stale
    // `Return` (generation mismatch) is ignored. Bumped on each outbound emit.
    #[serde(skip, default)]
    pub(crate) away_generation: HashMap<Entity, u32>,
    // DT1: marks the applied derived state (powered flags, stats, pollution,
    // local effects, happiness) out of date after a config change. Unlike the
    // registry cache above (which stores derived *resolution data* recomputed
    // lazily on read), the derived pass *writes* into `&mut World`, so it cannot
    // run behind a shared borrow; the flag lets the `&mut` step/read boundaries
    // recompute it. A `Cell` so the `&self` invalidation chokepoints can set it.
    #[serde(skip, default)]
    derived_dirty: Cell<bool>,
    // L1 routing: local road graph or border links may be stale for road-report
    // pricing. Road cost currently depends only on road presence/connectivity, not
    // road level; if level affects pricing later, road upgrades must mark this too.
    // TODO: `derived_dirty` and `road_topology_dirty` are coarse command-side
    // invalidation flags; split by subsystem if config mutation grows.
    #[serde(skip, default)]
    road_topology_dirty: Cell<bool>,
    // Event-driven plan (docs/20260703-event-driven-architecture.md), P-1:
    // marks this region's cross-region availability hints (`availability_hints`)
    // stale. Set inside the same chokepoints that already invalidate the inputs
    // hints are derived from (`invalidate_resource_registry`,
    // `invalidate_jobs_registry`, `mark_road_topology_dirty`) so both command-side
    // AND tick-internal mutations (citizen growth, business growth, applied
    // cross-region grants) flip it — unlike `derived_dirty`, which is
    // deliberately commands-only. Goods stock bypasses all three chokepoints
    // (it mutates `building.data.local_goods_stored` directly), so it gets an
    // explicit mark at its two write sites instead. Cleared only by the worker
    // after a successful publish; forced true again on load (`from_world`) so a
    // freshly loaded region republishes once.
    #[serde(skip, default)]
    hints_dirty: Cell<bool>,
    // Tunable footprint/building rules. `#[serde(skip)]` so they are not duplicated per region in
    // the save; the regional layer injects the save-stamped rules into each world (until then every
    // world deterministically gets the embedded default).
    #[serde(skip, default)]
    building_rules: crate::core::building_rules::BuildingRules,
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
            region_id: default_region_id(),
            next_entity: 0,
            entities: HashMap::new(),
            grid: Grid::new(width, height),
            resources: CityResources::default(),
            stats: CityStats::default(),
            local_effects: LocalEffectsMap::new(width, height),
            road_analysis: RoadNetworkAnalysis::default(),
            registry_cache: RefCell::default(),
            importable_remote_jobs: 0,
            cross_region_goods_routes: CrossRegionGoodsRoutes::default(),
            route_cache: RefCell::default(),
            tokens: HashMap::new(),
            away_residents: std::collections::HashSet::new(),
            outgoing_handoffs: Vec::new(),
            remote_exit_cells: HashMap::new(),
            away_generation: HashMap::new(),
            derived_dirty: Cell::new(false),
            road_topology_dirty: Cell::new(false),
            hints_dirty: Cell::new(false),
            building_rules: crate::core::building_rules::BuildingRules::default(),
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

    /// Records this world's owning region id and stamps every citizen's local home
    /// with it.
    ///
    /// Homes are always local to the owning region. The `Entity` already packs its
    /// birth region, so no stamping is needed for `home`. The citizen's `id` is the
    /// map key (its own `Entity`), which already carries the birth region.
    /// Called from the `RegionState` construction/load boundary, which knows the id.
    pub(crate) fn set_region_id(&mut self, region: RegionId) {
        self.region_id = region;
        for (entity, citizen) in self.citizens.iter_mut() {
            // Entity already carries its birth region; no stamping needed.
            // Citizen.id is the map key (its own Entity), so it's already correct.
            citizen.id = *entity;
        }
    }

    /// Tunable footprint/building rules in effect for this world.
    pub(crate) fn building_rules(&self) -> &crate::core::building_rules::BuildingRules {
        &self.building_rules
    }

    /// Injects the tunable building rules (the regional layer sets the save-stamped ruleset here so
    /// every world in a city shares it). `#[serde(skip)]` means rules are never serialized per world.
    pub(crate) fn set_building_rules(&mut self, rules: crate::core::building_rules::BuildingRules) {
        self.building_rules = rules;
    }

    pub fn spawn(&mut self) -> Entity {
        // City-wide-unique id: this region (birth) in the high bits, the local counter
        // in the low bits. region 0 worlds keep packed == local (legacy-shaped ids).
        let entity = Entity::new(self.region_id, self.next_entity);
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
        self.hints_dirty.set(true);
    }

    /// Mark only job entries dirty after citizen or workplace-effect changes.
    pub(crate) fn invalidate_jobs_registry(&self) {
        self.registry_cache.borrow_mut().invalidate_jobs();
        self.hints_dirty.set(true);
    }

    /// P2: drop every entry in the route cache. Called when a road is created
    /// or removed (the affected set isn't computable from a single road change
    /// — a new road can connect previously-disconnected areas, a removed road
    /// can disconnect them).
    pub(crate) fn clear_route_cache(&self) {
        self.route_cache.borrow_mut().clear();
    }

    /// P2: drop every entry whose key's destination is `dest`. Called when a
    /// building is removed or its footprint grows (only this destination's
    /// trees are affected — other destinations' entry cells and reachability
    /// don't change).
    pub(crate) fn evict_route_cache(&self, dest: Entity) {
        self.route_cache.borrow_mut().evict(dest);
    }

    /// P2: cached destination-rooted `came_from` tree for `dest` on `network`.
    /// Returns a `Ref` to the cached or freshly-computed tree; the tree is
    /// stored in the cache on miss and reused on subsequent calls.
    ///
    /// **Destination roots.** A non-road **building** uses its adjacent road
    /// cells (in `network`) as Dijkstra sources. A **road entity** (P5
    /// border-exit routing) uses `[dest]` as the single source.
    pub(crate) fn routes_to<'a>(
        &'a self,
        dest: Entity,
        network: &RoadNetwork,
    ) -> std::cell::Ref<'a, HashMap<Entity, Entity>> {
        use crate::core::systems::road_network_analysis::adjacent_roads_in_network;

        let key = (dest, network.id);
        let sources: Vec<Entity> = match self.buildings.get(&dest) {
            Some(building) if building.kind == BuildingKind::Road => vec![dest],
            Some(_) => adjacent_roads_in_network(self, dest, network),
            None => vec![dest],
        };
        // Compute (or reuse) the tree. `get_or_compute` returns a `&HashMap`
        // tied to the `borrow_mut` borrow, which we release before the
        // `Ref::map` below re-borrows immutably to return a `Ref` to the
        // caller. The returned `&HashMap` is intentionally discarded — its
        // lifetime ends when the `borrow_mut` goes out of scope.
        //
        // **Reentrancy invariant:** `compute` (`road_predecessors`) must
        // not touch `self.route_cache`. The `borrow_mut` is held while
        // `compute` runs, so any access to `route_cache` from inside the
        // Dijkstra would panic with `BorrowMutError`. This is fine today
        // because `road_predecessors` is a pure graph walk over the road
        // topology.
        let _ = self
            .route_cache
            .borrow_mut()
            .get_or_compute(key, || road_predecessors(self, network, &sources));
        std::cell::Ref::map(self.route_cache.borrow(), |cache| {
            cache.get(&key).expect("just inserted; key must be present")
        })
    }

    /// P2 test helper: whether `(dest, network_id)` is currently cached.
    /// Asserts selective eviction by checking key presence before and after
    /// the operation (a full clear + recompute would also yield a non-empty
    /// tree, but only a selective evict leaves the commercial's key intact).
    #[cfg(test)]
    pub(crate) fn route_cache_contains(&self, dest: Entity, network_id: u32) -> bool {
        self.route_cache.borrow().contains(&(dest, network_id))
    }

    /// P-a: walk the came_from tree rooted at `dest` (a road entity) and count
    /// the hops from `dest` to `target` (also a road entity, on the same
    /// network). Returns `None` if `target` is not reachable from `dest` (i.e.
    /// not in the tree). Used by `RegionState::road_report` to price each
    /// border-entry → border-exit crossing on the region's own road graph
    /// (Layer-2 Dijkstra distance).
    ///
    /// **Reentrancy invariant:** does NOT touch `route_cache` (calls the pure
    /// `road_predecessors_with_dist`, which builds a fresh tree without the
    /// cache). Safe to call from within another route-cache compute path.
    pub(crate) fn road_distance_to(
        &self,
        dest: Entity,
        target: Entity,
        network: &RoadNetwork,
    ) -> Option<u32> {
        use crate::core::systems::road_network_analysis::road_predecessors_with_dist;
        // `dest` is a road cell, so Dijkstra seeds from it. The returned
        // `distances` map is a fresh tree (no cache write).
        let (_tree, distances) = road_predecessors_with_dist(self, network, &[dest]);
        distances.get(&target).copied()
    }

    /// Marks the applied derived state stale after an out-of-tick config change (DT1).
    ///
    /// `derived_dirty` is set **only** by player commands (the `RegionState`
    /// command wrappers), not by `invalidate_*`. That keeps it false during a tick:
    /// tick-internal mutations (citizen growth, business auto-upgrade, an applied
    /// cross-region grant) invalidate the registry but must keep their existing
    /// one-tick lag, and a recompute-on-read mid-tick would wipe in-flight imported
    /// power. So the flag means precisely "a command changed config since the last
    /// derived recompute", which is exactly when a view/tick must recompute.
    pub(crate) fn mark_derived_dirty(&self) {
        self.derived_dirty.set(true);
    }

    /// Whether the applied derived state is stale (DT1).
    pub(crate) fn is_derived_dirty(&self) -> bool {
        self.derived_dirty.get()
    }

    /// Marks the applied derived state current after the derived pass has run.
    pub(crate) fn clear_derived_dirty(&self) {
        self.derived_dirty.set(false);
    }

    /// Marks the local road graph / border-link report stale for Layer-1 pricing.
    pub(crate) fn mark_road_topology_dirty(&self) {
        self.road_topology_dirty.set(true);
        self.hints_dirty.set(true);
    }

    pub(crate) fn is_road_topology_dirty(&self) -> bool {
        self.road_topology_dirty.get()
    }

    pub(crate) fn clear_road_topology_dirty(&self) {
        self.road_topology_dirty.set(false);
    }

    /// Marks this region's cross-region availability hints stale (P-1). Also
    /// called explicitly at the goods-stock write sites, which bypass every
    /// other chokepoint above.
    pub(crate) fn mark_hints_dirty(&self) {
        self.hints_dirty.set(true);
    }

    pub(crate) fn is_hints_dirty(&self) -> bool {
        self.hints_dirty.get()
    }

    pub(crate) fn clear_hints_dirty(&self) {
        self.hints_dirty.set(false);
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

/// Display-only cross-region road reachability published by the regional worker.
///
/// The local road analysis stays local-only. These network ids only let inspect
/// avoid saying "unreachable" when a connected neighbor has already published
/// spare city-goods supply for the same cross-region road component.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CrossRegionGoodsRoutes {
    pub supplier_networks: Vec<u32>,
}

impl CrossRegionGoodsRoutes {
    pub(crate) fn has_supplier_on(&self, network_id: Option<u32>) -> bool {
        network_id
            .map(|network_id| self.supplier_networks.binary_search(&network_id).is_ok())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::World;
    use crate::core::components::{
        Building, BuildingData, Citizen, Footprint, Morale, Population, Position,
    };
    use crate::core::entity::Entity;
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
                footprint: Footprint::single(),
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
    fn set_region_id_records_region_and_stamps_citizen_homes() {
        use crate::core::regions::RegionId;

        let mut world = World::new(2, 1);
        let residential = world.spawn();
        let citizen = world.spawn();
        // A just-deserialized citizen has Entity home (already packed with region).
        world.attach_citizen(
            citizen,
            Citizen {
                id: Entity(0), // placeholder, will be overwritten
                age: 0,
                home: residential, // Entity already packs its birth region
                workplace_assignment: None,
                morale: Morale::default(),
                money: 0,
            },
        );

        world.set_region_id(RegionId(7));

        assert_eq!(world.region_id, RegionId(7));
        // The home entity is preserved; Entity already carries its region.
        assert_eq!(world.citizens[&citizen].home, residential);
        // The city-wide id is the citizen's own entity (the map key).
        assert_eq!(world.citizens[&citizen].id, citizen);
    }

    #[test]
    fn attach_helpers_record_citizen_shape() {
        let mut world = World::new(2, 2);
        let residential = world.spawn();
        let citizen = world.spawn();

        world.attach_citizen(
            citizen,
            Citizen {
                id: citizen, // map key is the citizen's entity
                age: 0,
                home: residential,
                workplace_assignment: None,
                morale: Morale::default(),
                money: 0,
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
                footprint: Footprint::single(),
            },
        );
        world.entities.clear();

        world.rebuild_entity_records();

        let record = world.entities.get(&entity).expect("entity record");
        assert_eq!(record.kind, Some(BuildingKind::Park));
        assert!(record.has_position);
    }
}
