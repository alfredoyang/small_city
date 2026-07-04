//! Region-local resource registry and cache for deterministic provider/consumer allocation.
//!
//! The first registry entry is power. It registers local power providers and
//! consumers from a `World`, emits map-ordered requests, and resolves local
//! grants without mutating the world. Systems apply those grants afterward.
//! The jobs entry follows the same registry pattern while preserving its own
//! proximity-based matching rule. `World` owns a skipped `ResourceRegistryCache`
//! so systems can reuse these pure derived results until topology, citizens, or
//! workplace capacity changes.
//!
//! Flow inside `power::run` (one tick, before downstream systems read `powered`):
//!
//! ```text
//! power::run(world)
//!   |  (1) reset every PowerConsumer { powered = false, source = None }
//!   v
//! world.cached_power_resolution()          read-only snapshot, never mutates World
//!   |    networks[] : one POOLED entry per road network
//!   |    requests[] : one per consumer, sorted in map order (y, then x)
//!   v
//! registry.resolve_local_power() -> PowerResolution
//!   |    for each request, in map order:
//!   |      pick a reachable network whose pooled remaining_capacity >= demand
//!   |        -> decrement that pool, emit PowerGrant { source = Local(representative) }
//!   v
//! PowerResolution { grants[], total_capacity, total_demand, total_supplied }
//!   |  (2) apply grants: consumer.powered = true; consumer.source = Some(..)
//!   |  (3) world.stats.power = { capacity, demand, supplied, shortage }
//!   v
//! downstream systems read `powered` (population, economy, happiness, local effects)
//! ```
//!
//! The same PowerRequest/PowerGrant/PowerSource shape will carry cross-region
//! power later; only the transport changes (a request routed to a neighbor
//! returns `source = Imported { .. }`). Local stays this synchronous pass.
//!
//! Flow inside Patch R2 job registration and assignment:
//!
//! ```text
//! World (private ECS)
//!   buildings + citizens + powered flags + road_analysis
//!        |
//!        v
//! world.cached_job_resolution()
//!        |
//!        +-- Jobs entry
//!        |     register effective workplace slots:
//!        |       Commercial/Industrial only if powered + road-connected
//!        |       repeated by jobs_at_level(level), sorted by map position
//!        |
//!        |     register job seekers:
//!        |       one request per locally-unassigned citizen, sorted by stable
//!        |       citizen id; remote-assigned citizens keep their producer slot
//!        |
//!        |     resolve assignments:
//!        |       nearest reachable remaining job slot
//!        |       ties keep the pre-sorted slot order
//!        v
//! JobResolution
//!   assignments      -> economy::assign_local_jobs applies Citizen.workplace_assignment
//!   total_jobs       -> stats::refresh_population_and_jobs
//!   unemployment     -> stats::refresh_population_and_jobs
//!   remaining_slots  -> later regional spare-capacity queries
//! ```

use std::collections::HashSet;

use crate::core::components::PowerSource;
use crate::core::entity::Entity;
use crate::core::systems::road_connectivity::{self, RoadNetwork};
use crate::core::systems::road_network_analysis;
use crate::core::world::World;
use crate::interface::input::BuildingKind;

#[derive(Debug, Clone, PartialEq, Eq)]
/// One local power demand request emitted by a consuming building.
pub(crate) struct PowerRequest {
    pub consumer: Entity,
    pub demand: i32,
    adjacent_roads: Vec<Entity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// One granted local power request with its derived source.
pub(crate) struct PowerGrant {
    pub consumer: Entity,
    pub source: PowerSource,
    pub amount: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Full result of resolving local power through the registry.
pub(crate) struct PowerResolution {
    pub grants: Vec<PowerGrant>,
    pub total_capacity: i32,
    pub total_demand: i32,
    pub total_supplied: i32,
    pub remaining_capacity: i32,
    pub network_capacities: Vec<PowerNetworkCapacity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Remaining power capacity for one local road network after local grants.
pub(crate) struct PowerNetworkCapacity {
    pub road_network: u32,
    pub remaining_capacity: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// One job-seeking citizen registered for local workplace assignment.
pub(crate) struct JobRequest {
    pub citizen: Entity,
    pub home: Entity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Deterministic local workplace assignment for one citizen.
pub(crate) struct JobAssignment {
    pub citizen: Entity,
    pub workplace: Option<Entity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Full result of resolving local jobs through the registry.
pub(crate) struct JobResolution {
    pub assignments: Vec<JobAssignment>,
    pub workplace_slots: Vec<Entity>,
    pub remaining_workplaces: Vec<Entity>,
    pub total_jobs: i32,
    pub job_seekers: i32,
    pub unemployment: i32,
    pub remaining_slots: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Count-only job summary for systems that do not need assignment matching.
pub(crate) struct JobCounts {
    pub total_jobs: i32,
    pub job_seekers: i32,
    pub unemployment: i32,
}

#[derive(Debug)]
/// Region-local resource registry.
///
/// It is rebuilt from authoritative ECS state when a system needs allocation.
/// The registry owns copied IDs and amounts only; it does not expose `World`.
pub(crate) struct ResourceRegistry {
    power: Option<PowerRegistry>,
    jobs: Option<JobsRegistry>,
}

impl ResourceRegistry {
    pub(crate) fn for_power(world: &World) -> Self {
        Self {
            power: Some(PowerRegistry::from_world(world)),
            jobs: None,
        }
    }

    pub(crate) fn for_jobs(world: &World) -> Self {
        Self {
            power: None,
            jobs: Some(JobsRegistry::from_world(world)),
        }
    }

    pub(crate) fn resolve_local_power(&self) -> PowerResolution {
        self.power.as_ref().expect("power registry entry").resolve()
    }

    pub(crate) fn resolve_local_jobs(&self) -> JobResolution {
        self.jobs.as_ref().expect("jobs registry entry").resolve()
    }
}

#[derive(Debug, Default)]
/// Persistent derived registry cache owned by `World`.
///
/// # Why this cache exists
///
/// A `PowerResolution` / `JobResolution` is **not** raw data the ECS can hand
/// back with a lookup -- it is the output of a non-trivial computation over many
/// entities: power flood-fills road tiles into networks and allocates capacity;
/// jobs gate effective workplaces on power and road connectivity, then assign
/// each citizen to its nearest slot. The ECS stores the *inputs*; this result has
/// to be *computed*.
///
/// That computation is read-heavy but change-light:
///
/// ```text
///   reads per tick (many)                inputs change (rarely)
///   --------------------                 ----------------------
///   power::run, stats, economy,          build / bulldoze / replace / upgrade,
///   discovery hints, regional spare,     business auto-upgrade, citizen
///   per cross-region export request      growth/removal, imported power grant
///        |                                      |
///        v                                      v
///   recompute every read?  -- wasteful    mark dirty here (invalidate_*)
///        \____________ cache _____________/
///                       |
///   compute once on change, hand back the stored result until the next change.
/// ```
///
/// So the win is **compute-on-change instead of compute-per-read** (a plain hourly
/// tick changes none of the inputs, yet several systems still need the result).
/// An earlier step in this plan that merely stopped recomputing jobs when only
/// power was needed already cut a scenario suite ~7.3s -> ~1.9s; this extends that
/// across ticks.
///
/// The cost is the invalidation surface: every mutation that changes a registry
/// input must mark the matching entry dirty, and a *missed* invalidation is a
/// silent determinism bug -- hence the parity-guard test asserting the cached
/// result equals a fresh recompute. The cache stores only owned ECS ids and
/// derived values, is `#[serde(skip)]` (never persisted), and is rebuilt lazily
/// from authoritative state on the next read after `invalidate_*`.
pub(crate) struct ResourceRegistryCache {
    power: Option<PowerResolution>,
    jobs: Option<JobResolution>,
    power_dirty: bool,
    jobs_dirty: bool,
    #[cfg(test)]
    power_recomputes: usize,
    #[cfg(test)]
    jobs_recomputes: usize,
}

impl ResourceRegistryCache {
    pub(crate) fn invalidate_all(&mut self) {
        self.power_dirty = true;
        self.jobs_dirty = true;
    }

    pub(crate) fn invalidate_jobs(&mut self) {
        self.jobs_dirty = true;
    }

    pub(crate) fn power_resolution(&mut self, world: &World) -> PowerResolution {
        if self.power_dirty || self.power.is_none() {
            self.power = Some(ResourceRegistry::for_power(world).resolve_local_power());
            self.power_dirty = false;
            #[cfg(test)]
            {
                self.power_recomputes += 1;
            }
        }
        self.power.as_ref().expect("power registry cache").clone()
    }

    pub(crate) fn job_resolution(&mut self, world: &World) -> JobResolution {
        self.ensure_jobs(world).clone()
    }

    pub(crate) fn with_remaining_job_workplaces<R>(
        &mut self,
        world: &World,
        read: impl FnOnce(&[Entity]) -> R,
    ) -> R {
        let jobs = self.ensure_jobs(world);
        read(&jobs.remaining_workplaces)
    }

    fn ensure_jobs(&mut self, world: &World) -> &JobResolution {
        if self.jobs_dirty || self.jobs.is_none() {
            self.jobs = Some(ResourceRegistry::for_jobs(world).resolve_local_jobs());
            self.jobs_dirty = false;
            #[cfg(test)]
            {
                self.jobs_recomputes += 1;
            }
        }

        self.jobs.as_ref().expect("jobs registry cache")
    }

    #[cfg(test)]
    pub(crate) fn recompute_counts(&self) -> (usize, usize) {
        (self.power_recomputes, self.jobs_recomputes)
    }
}

#[derive(Debug)]
struct PowerRegistry {
    networks: Vec<PowerNetworkPool>,
    requests: Vec<PowerRequest>,
    total_capacity: i32,
    total_demand: i32,
}

#[derive(Debug)]
struct JobsRegistry {
    workplace_slots: Vec<Entity>,
    requests: Vec<JobRequest>,
    assignments: Vec<JobAssignment>,
    remaining_workplaces: Vec<Entity>,
    remaining_slots: i32,
}

/// Pooled per-network power capacity (the R1 parity invariant).
///
/// ```text
/// road network with two plants (cap 10 each):
///   +----+----+----+----+
/// y0| T  | R  | C  | T  |   T = plant(10)   R = demand 1   C = demand 2
///   +----+----+----+----+
/// y1| == | == | == | == |   all roads connected -> ONE network
///   +----+----+----+----+
///   remaining_capacity (pool) = 10 + 10 = 20
///   representative_provider   = plant @ (0,0)   (first by map order)
///
///   a consumer's demand is checked against the POOL, never blocked because a
///   single plant is short. (Per-provider slots were the rejected design.)
/// ```
#[derive(Debug, Clone)]
struct PowerNetworkPool {
    road_network: u32,
    roads: HashSet<Entity>,
    remaining_capacity: i32,
    representative_provider: Option<Entity>,
}

impl PowerRegistry {
    fn from_world(world: &World) -> Self {
        let networks = road_connectivity::discover_road_networks(world)
            .into_iter()
            .map(|network| PowerNetworkPool::from_network(world, network))
            .collect::<Vec<_>>();

        let mut consumers = world.power_consumers.keys().copied().collect::<Vec<_>>();
        road_connectivity::sort_entities_by_position(world, &mut consumers);
        let requests = consumers
            .into_iter()
            .filter_map(|consumer| {
                world
                    .power_consumers
                    .get(&consumer)
                    .map(|power_consumer| PowerRequest {
                        consumer,
                        demand: power_consumer.demand,
                        adjacent_roads: road_connectivity::adjacent_road_entities(world, consumer)
                            .collect(),
                    })
            })
            .collect::<Vec<_>>();

        Self {
            networks,
            requests,
            total_capacity: world
                .power_providers
                .values()
                .map(|provider| provider.capacity)
                .sum(),
            total_demand: world
                .power_consumers
                .values()
                .map(|consumer| consumer.demand)
                .sum(),
        }
    }

    fn resolve(&self) -> PowerResolution {
        let mut networks = self.networks.clone();
        let mut grants = Vec::new();

        for request in &self.requests {
            let Some(grant) = grant_request(request, &mut networks) else {
                continue;
            };
            grants.push(grant);
        }

        let total_supplied = grants.iter().map(|grant| grant.amount).sum();
        let remaining_capacity = networks
            .iter()
            .map(|network| network.remaining_capacity)
            .sum();
        let network_capacities = networks
            .iter()
            .map(|network| PowerNetworkCapacity {
                road_network: network.road_network,
                remaining_capacity: network.remaining_capacity,
            })
            .collect();
        PowerResolution {
            grants,
            total_capacity: self.total_capacity,
            total_demand: self.total_demand,
            total_supplied,
            remaining_capacity,
            network_capacities,
        }
    }
}

impl JobsRegistry {
    fn from_world(world: &World) -> Self {
        let workplace_slots = workplace_slots_from_world(world);
        let requests = job_requests_from_world(world);

        let (assignments, remaining_workplaces) =
            resolve_job_assignments(world, &requests, &workplace_slots);
        let remaining_slots = remaining_workplaces.len() as i32;

        Self {
            workplace_slots,
            requests,
            assignments,
            remaining_workplaces,
            remaining_slots,
        }
    }

    fn resolve(&self) -> JobResolution {
        let total_jobs = self.workplace_slots.len() as i32;
        let job_seekers = self.requests.len() as i32;
        JobResolution {
            assignments: self.assignments.clone(),
            workplace_slots: self.workplace_slots.clone(),
            remaining_workplaces: self.remaining_workplaces.clone(),
            total_jobs,
            job_seekers,
            unemployment: (job_seekers - total_jobs).max(0),
            remaining_slots: self.remaining_slots,
        }
    }
}

fn workplace_slots_from_world(world: &World) -> Vec<Entity> {
    let mut workplace_slots = Vec::new();
    let mut workplace_entities = effective_workplace_entities(world);

    for (entity, kind, _level) in workplace_entities.drain(..) {
        // Job capacity scales with footprint area through the shared capacity_for source.
        let area = world
            .buildings
            .get(&entity)
            .map(|building| building.footprint.area())
            .unwrap_or(1);
        for _ in 0..crate::core::building_stats::capacity_for(kind, area).max(0) {
            workplace_slots.push(entity);
        }
    }

    workplace_slots
}

fn effective_workplace_entities(world: &World) -> Vec<(Entity, BuildingKind, u8)> {
    let mut workplace_entities = world
        .buildings
        .iter()
        .filter_map(|(entity, building)| {
            is_effective_workplace(world, *entity).then_some((
                *entity,
                building.kind,
                building.level,
            ))
        })
        .collect::<Vec<_>>();
    workplace_entities.sort_by_key(|(entity, _kind, _level)| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });
    workplace_entities
}

fn job_requests_from_world(world: &World) -> Vec<JobRequest> {
    let mut citizen_entities = world.citizens.keys().copied().collect::<Vec<_>>();
    citizen_entities.sort_by_key(|citizen| citizen.0);
    citizen_entities
        .into_iter()
        .filter_map(|citizen| {
            let citizen_data = world.citizens.get(&citizen)?;
            // A citizen already holding a remote job (workplace in another region) is
            // not seeking local work.
            if citizen_data
                .workplace_assignment
                .is_some_and(|assignment| assignment.workplace.region() != world.region_id)
            {
                return None;
            }
            Some(JobRequest {
                citizen,
                // Home is always local to this region; it's already the local id.
                home: citizen_data.home,
            })
        })
        .collect()
}

fn resolve_job_assignments(
    world: &World,
    requests: &[JobRequest],
    workplace_slots: &[Entity],
) -> (Vec<JobAssignment>, Vec<Entity>) {
    let mut remaining_workplaces = workplace_slots.to_vec();
    let mut assignments = Vec::new();

    for request in requests {
        let workplace_index = nearest_slot_index(world, request.home, &remaining_workplaces);
        let workplace = workplace_index.map(|index| remaining_workplaces.remove(index));
        assignments.push(JobAssignment {
            citizen: request.citizen,
            workplace,
        });
    }

    (assignments, remaining_workplaces)
}

fn nearest_slot_index(world: &World, from: Entity, slots: &[Entity]) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            road_network_analysis::distance_between_buildings(world, from, *slot)
                .map(|distance| (index, distance))
        })
        .min_by_key(|(index, distance)| (*distance, *index))
        .map(|(index, _distance)| index)
}

fn is_effective_workplace(world: &World, entity: Entity) -> bool {
    let Some(building) = world.buildings.get(&entity) else {
        return false;
    };
    if !matches!(
        building.kind,
        BuildingKind::Commercial | BuildingKind::Industrial
    ) {
        return false;
    }

    let powered = world
        .power_consumers
        .get(&entity)
        .is_some_and(|consumer| consumer.powered);
    powered && road_connectivity::is_road_connected(world, entity)
}

impl PowerNetworkPool {
    fn from_network(world: &World, network: RoadNetwork) -> Self {
        let mut provider_entities = world
            .power_providers
            .keys()
            .filter(|provider| {
                road_connectivity::adjacent_road_entities(world, **provider)
                    .any(|road| network.roads.contains(&road))
            })
            .copied()
            .collect::<Vec<_>>();
        road_connectivity::sort_entities_by_position(world, &mut provider_entities);

        let remaining_capacity = provider_entities
            .iter()
            .filter_map(|entity| world.power_providers.get(entity))
            .map(|provider| provider.capacity)
            .sum();

        Self {
            road_network: network.id,
            roads: network.roads,
            remaining_capacity,
            // Local power capacity is pooled per road network. The source is a
            // deterministic representative for provenance, not per-plant accounting.
            representative_provider: provider_entities.first().copied(),
        }
    }

    fn can_reach_request(&self, request: &PowerRequest) -> bool {
        request
            .adjacent_roads
            .iter()
            .any(|road| self.roads.contains(road))
    }
}

fn grant_request(request: &PowerRequest, networks: &mut [PowerNetworkPool]) -> Option<PowerGrant> {
    for network in networks {
        if !network.can_reach_request(request) {
            continue;
        }

        if network.remaining_capacity < request.demand {
            continue;
        }

        let Some(provider) = network.representative_provider else {
            continue;
        };

        network.remaining_capacity -= request.demand;
        return Some(PowerGrant {
            consumer: request.consumer,
            source: PowerSource::Local(provider),
            amount: request.demand,
        });
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::components::{BuildingData, BusinessFinance, PowerSource};
    use crate::core::regions::RegionId;
    use crate::core::simulation::{clear_imported_power, imported_power_grants, tick_world};
    use crate::core::systems::{
        business_growth, citizens, placement, power, road_network_analysis,
    };

    #[test]
    fn jobs_entry_reports_totals_unemployment_and_remaining_slots() {
        let mut world = World::new(5, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        placement::place_building(&mut world, 2, 0, BuildingKind::Industrial);
        for x in 0..=2 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        power_workplace(&mut world, 1, 0);
        power_workplace(&mut world, 2, 0);
        let home = world.grid.get(0, 0).expect("home");
        citizens::spawn_for_home(&mut world, home, 4);
        road_network_analysis::run(&mut world);

        let jobs = ResourceRegistry::for_jobs(&world).resolve_local_jobs();

        assert_eq!(jobs.total_jobs, 5);
        assert_eq!(jobs.job_seekers, 4);
        assert_eq!(jobs.unemployment, 0);
        assert_eq!(jobs.remaining_slots, 1);
    }

    #[test]
    fn cached_job_counts_report_totals_without_mutating_assignments() {
        let mut world = World::new(5, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        placement::place_building(&mut world, 2, 0, BuildingKind::Industrial);
        for x in 0..=2 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        power_workplace(&mut world, 1, 0);
        power_workplace(&mut world, 2, 0);
        let home = world.grid.get(0, 0).expect("home");
        citizens::spawn_for_home(&mut world, home, 7);

        let counts = world.cached_job_counts();

        assert_eq!(counts.total_jobs, 5);
        assert_eq!(counts.job_seekers, 7);
        assert_eq!(counts.unemployment, 2);
        assert!(
            world
                .citizens
                .values()
                .all(|citizen| citizen.workplace_assignment.is_none())
        );
    }

    #[test]
    fn jobs_entry_assigns_nearest_reachable_workplace_slots() {
        let mut world = World::new(7, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 2, 0, BuildingKind::Commercial);
        placement::place_building(&mut world, 5, 0, BuildingKind::Industrial);
        for x in 0..=5 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        power_workplace(&mut world, 2, 0);
        power_workplace(&mut world, 5, 0);
        let home = world.grid.get(0, 0).expect("home");
        citizens::spawn_for_home(&mut world, home, 3);
        road_network_analysis::run(&mut world);

        let commercial = world.grid.get(2, 0).expect("commercial");
        let industrial = world.grid.get(5, 0).expect("industrial");
        let jobs = ResourceRegistry::for_jobs(&world).resolve_local_jobs();
        let assigned = jobs
            .assignments
            .iter()
            .map(|assignment| assignment.workplace)
            .collect::<Vec<_>>();

        assert_eq!(
            assigned,
            vec![Some(commercial), Some(commercial), Some(industrial)]
        );
    }

    #[test]
    fn cached_power_resolution_reuses_until_build_invalidation() {
        let mut world = World::new(5, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 0, 1, BuildingKind::Road);
        placement::place_building(&mut world, 1, 1, BuildingKind::Road);
        placement::place_building(&mut world, 1, 0, BuildingKind::Residential);

        assert_eq!(world.cached_power_resolution().total_capacity, 10);
        assert_eq!(world.cached_power_resolution().total_capacity, 10);
        assert_eq!(world.registry_cache_recompute_counts().0, 1);

        placement::place_building(&mut world, 4, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 4, 1, BuildingKind::Road);

        assert_eq!(world.cached_power_resolution().total_capacity, 20);
        assert_eq!(world.registry_cache_recompute_counts().0, 2);
    }

    #[test]
    fn cached_job_resolution_reuses_until_citizen_invalidation() {
        let mut world = World::new(5, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        for x in 0..=1 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        power_workplace(&mut world, 1, 0);
        road_network_analysis::run(&mut world);

        assert_eq!(world.cached_job_resolution().job_seekers, 0);
        assert_eq!(world.cached_job_resolution().job_seekers, 0);
        assert_eq!(world.registry_cache_recompute_counts().1, 1);

        let home = world.grid.get(0, 0).expect("home");
        citizens::spawn_for_home(&mut world, home, 1);

        assert_eq!(world.cached_job_resolution().job_seekers, 1);
        assert_eq!(world.registry_cache_recompute_counts().1, 2);
    }

    #[test]
    fn cached_remaining_job_workplaces_reuses_job_cache_without_full_resolution_read() {
        let mut world = World::new(5, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        placement::place_building(&mut world, 2, 0, BuildingKind::Industrial);
        for x in 0..=2 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        power_workplace(&mut world, 1, 0);
        power_workplace(&mut world, 2, 0);
        let home = world.grid.get(0, 0).expect("home");
        citizens::spawn_for_home(&mut world, home, 4);
        road_network_analysis::run(&mut world);

        let first = world.with_cached_remaining_job_workplaces(|slots| slots.len());
        let second = world.with_cached_remaining_job_workplaces(|slots| slots.len());

        assert_eq!(first, 1);
        assert_eq!(second, 1);
        assert_eq!(world.registry_cache_recompute_counts().1, 1);

        citizens::spawn_for_home(&mut world, home, 1);

        let after_citizen_growth = world.with_cached_remaining_job_workplaces(|slots| slots.len());

        assert_eq!(after_citizen_growth, 0);
        assert_eq!(world.registry_cache_recompute_counts().1, 2);
    }

    #[test]
    fn cached_registry_matches_forced_recompute_script() {
        let mut cached = parity_world();
        let mut forced = parity_world();
        assert_worlds_match("initial build", &cached, &forced);

        for _ in 0..24 {
            assert!(tick_world(&mut cached).success);
            forced.invalidate_resource_registry();
            assert!(tick_world(&mut forced).success);
        }
        assert_worlds_match("after population growth", &cached, &forced);
        assert!(cached.stats.population > 0);

        let cached_commercial = cached.grid.get(2, 0).expect("cached commercial");
        let forced_commercial = forced.grid.get(2, 0).expect("forced commercial");
        set_business_finance(&mut cached, cached_commercial, 10, 3);
        set_business_finance(&mut forced, forced_commercial, 10, 3);
        let cached_upgrade = business_growth::run(&mut cached);
        forced.invalidate_resource_registry();
        let forced_upgrade = business_growth::run(&mut forced);
        assert_eq!(cached_upgrade, forced_upgrade);
        assert_worlds_match("after business auto-upgrade", &cached, &forced);

        cached = roundtrip_world(cached);
        forced = roundtrip_world(forced);
        assert_worlds_match("after save/load", &cached, &forced);

        for _ in 0..24 {
            assert!(tick_world(&mut cached).success);
            forced.invalidate_resource_registry();
            assert!(tick_world(&mut forced).success);
        }
        assert_worlds_match("after post-load ticks", &cached, &forced);
    }

    #[test]
    fn cached_jobs_rebuild_when_imported_power_is_lost() {
        let mut world = World::new(3, 2);
        placement::place_building(&mut world, 0, 0, BuildingKind::Commercial);
        placement::place_building(&mut world, 0, 1, BuildingKind::Road);
        let commercial = world.grid.get(0, 0).expect("commercial");
        let consumer = world
            .power_consumers
            .get_mut(&commercial)
            .expect("power consumer");
        consumer.powered = true;
        consumer.source = Some(PowerSource::Imported {
            source_region: RegionId(2),
        });
        road_network_analysis::run(&mut world);

        assert_eq!(world.cached_job_resolution().total_jobs, 2);

        // Event-driven plan, P-3: `power::run` diff-applies and now KEEPS an
        // existing `Imported` source when no local grant covers the consumer
        // (there is no power plant anywhere in this world), instead of
        // unconditionally clearing every consumer first. Losing an import is
        // now `clear_imported_power`'s job (called by a dirty reconcile
        // before re-requesting), not something `power::run` does on its own
        // — exercise that real mechanism so this test still proves the cache
        // invalidates and rebuilds in response to an actual power-state loss.
        let imported = imported_power_grants(&world);
        clear_imported_power(&mut world, &imported);
        power::run(&mut world);

        assert_eq!(world.cached_job_resolution().total_jobs, 0);
        assert_cached_matches_forced(&world);
    }

    #[test]
    fn cached_jobs_rebuild_after_business_auto_upgrade() {
        let mut world = World::new(4, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        for x in 0..=1 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        let residential = world.grid.get(0, 0).expect("residential");
        let commercial = world.grid.get(1, 0).expect("commercial");
        citizens::spawn_for_home(&mut world, residential, 1);
        power_workplace(&mut world, 1, 0);
        road_network_analysis::run(&mut world);
        set_business_finance(&mut world, commercial, 10, 3);

        assert_eq!(world.cached_job_resolution().total_jobs, 2);
        let summary = business_growth::run(&mut world);

        assert_eq!(summary.commercial_upgrades, 1);
        // Upgrading grows the commercial footprint to 2 cells, so job capacity is area-based
        // capacity_for(Commercial, 2) = 6 (was a flat +1 under the old level-only model).
        assert_eq!(world.cached_job_resolution().total_jobs, 6);
        assert_cached_matches_forced(&world);
    }

    #[test]
    fn cached_registry_rebuilds_after_save_load() {
        let mut world = World::new(4, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 1, 0, BuildingKind::Commercial);
        for x in 0..=1 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        road_network_analysis::run(&mut world);
        power::run(&mut world);
        assert_cached_matches_forced(&world);

        let encoded = serde_json::to_vec(&world).expect("serialize world");
        let loaded: World = serde_json::from_slice(&encoded).expect("deserialize world");

        assert_cached_matches_forced(&loaded);
    }

    fn power_workplace(world: &mut World, x: usize, y: usize) {
        let entity = world.grid.get(x, y).expect("workplace");
        world
            .power_consumers
            .get_mut(&entity)
            .expect("power consumer")
            .powered = true;
    }

    fn assert_cached_matches_forced(world: &World) {
        let cached_power = world.cached_power_resolution();
        let forced_power = ResourceRegistry::for_power(world).resolve_local_power();
        assert_eq!(cached_power, forced_power);
        let cached_jobs = world.cached_job_resolution();
        let forced_jobs = ResourceRegistry::for_jobs(world).resolve_local_jobs();
        assert_eq!(cached_jobs, forced_jobs);
    }

    fn set_business_finance(
        world: &mut World,
        entity: Entity,
        business_cash: i32,
        last_period_profit: i32,
    ) {
        let building = world.buildings.get_mut(&entity).expect("business building");
        match &mut building.data {
            BuildingData::Commercial { business, .. } | BuildingData::Industrial { business } => {
                *business = BusinessFinance {
                    business_cash,
                    last_period_profit,
                    days_profitable: 1,
                    lifetime_profit: business_cash,
                    ..BusinessFinance::default()
                };
            }
            BuildingData::None => panic!("not a business building"),
        }
    }

    fn parity_world() -> World {
        let mut world = World::new(6, 3);
        // tick_world simulates as RegionId(1); stamp it before spawning so entity
        // birth regions match the tick region (Entity packs its birth region).
        world.set_region_id(RegionId(1));
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 1, 0, BuildingKind::Residential);
        placement::place_building(&mut world, 2, 0, BuildingKind::Commercial);
        placement::place_building(&mut world, 4, 0, BuildingKind::Industrial);
        for x in 0..=4 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        road_network_analysis::run(&mut world);
        power::run(&mut world);
        world
    }

    fn roundtrip_world(world: World) -> World {
        let encoded = serde_json::to_vec(&world).expect("serialize world");
        let mut loaded: World = serde_json::from_slice(&encoded).expect("deserialize world");
        loaded.set_region_id(RegionId(1));
        loaded.rebuild_entity_records();
        road_network_analysis::run(&mut loaded);
        power::run(&mut loaded);
        loaded.invalidate_resource_registry();
        loaded
    }

    fn assert_worlds_match(label: &str, cached: &World, forced: &World) {
        assert_cached_matches_forced(cached);
        forced.invalidate_resource_registry();
        assert_cached_matches_forced(forced);

        assert_eq!(cached.stats, forced.stats, "{label}: stats diverged");
        assert_eq!(
            powered_state(cached),
            powered_state(forced),
            "{label}: powered flags diverged"
        );
        assert_eq!(
            cached.cached_job_resolution().assignments,
            forced.cached_job_resolution().assignments,
            "{label}: job assignments diverged"
        );

        let stats_before = cached.stats.clone();
        assert_cached_matches_forced(cached);
        assert_eq!(
            cached.stats, stats_before,
            "{label}: registry reads mutated stats"
        );
    }

    fn powered_state(world: &World) -> Vec<(Entity, bool, Option<PowerSource>)> {
        let mut powered = world
            .power_consumers
            .iter()
            .map(|(entity, consumer)| (*entity, consumer.powered, consumer.source))
            .collect::<Vec<_>>();
        powered.sort_by_key(|(entity, _powered, _source)| entity.0);
        powered
    }
}
