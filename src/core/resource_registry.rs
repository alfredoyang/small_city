//! Region-local resource registry for deterministic provider/consumer allocation.
//!
//! The first registry entry is power. It registers local power providers and
//! consumers from a `World`, emits map-ordered requests, and resolves local
//! grants without mutating the world. Systems apply those grants afterward.
//! The jobs entry follows the same registry pattern while preserving its own
//! proximity-based matching rule.
//!
//! Flow inside `power::run` (one tick, before downstream systems read `powered`):
//!
//! ```text
//! power::run(world)
//!   |  (1) reset every PowerConsumer { powered = false, source = None }
//!   v
//! ResourceRegistry::for_power(world)       read-only snapshot, never mutates World
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
//! ResourceRegistry::for_jobs(world)
//!        |
//!        +-- Jobs entry
//!        |     register effective workplace slots:
//!        |       Commercial/Industrial only if powered + road-connected
//!        |       repeated by jobs_at_level(level), sorted by map position
//!        |
//!        |     register job seekers:
//!        |       one request per citizen, sorted by stable citizen id
//!        |
//!        |     resolve assignments:
//!        |       nearest reachable remaining job slot
//!        |       ties keep the pre-sorted slot order
//!        v
//! JobResolution
//!   assignments      -> economy::run applies Citizen.workplace
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

    pub(crate) fn local_job_slots(&self) -> &[Entity] {
        &self
            .jobs
            .as_ref()
            .expect("jobs registry entry")
            .workplace_slots
    }

    pub(crate) fn local_job_counts(world: &World) -> JobCounts {
        JobsRegistry::counts_from_world(world)
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
        PowerResolution {
            grants,
            total_capacity: self.total_capacity,
            total_demand: self.total_demand,
            total_supplied,
            remaining_capacity,
        }
    }
}

impl JobsRegistry {
    fn from_world(world: &World) -> Self {
        let workplace_slots = workplace_slots_from_world(world);
        let requests = job_requests_from_world(world);

        let (assignments, remaining_slots) =
            resolve_job_assignments(world, &requests, &workplace_slots);

        Self {
            workplace_slots,
            requests,
            assignments,
            remaining_slots,
        }
    }

    fn counts_from_world(world: &World) -> JobCounts {
        let total_jobs = workplace_slot_count(world);
        let job_seekers = world.citizens.len() as i32;
        JobCounts {
            total_jobs,
            job_seekers,
            unemployment: (job_seekers - total_jobs).max(0),
        }
    }

    fn resolve(&self) -> JobResolution {
        let total_jobs = self.workplace_slots.len() as i32;
        let job_seekers = self.requests.len() as i32;
        JobResolution {
            assignments: self.assignments.clone(),
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

    for (entity, kind, level) in workplace_entities.drain(..) {
        for _ in 0..kind.jobs_at_level(level).max(0) {
            workplace_slots.push(entity);
        }
    }

    workplace_slots
}

fn workplace_slot_count(world: &World) -> i32 {
    effective_workplace_entities(world)
        .into_iter()
        .map(|(_entity, kind, level)| kind.jobs_at_level(level).max(0))
        .sum()
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
            world.citizens.get(&citizen).map(|citizen_data| JobRequest {
                citizen,
                home: citizen_data.home,
            })
        })
        .collect()
}

fn resolve_job_assignments(
    world: &World,
    requests: &[JobRequest],
    workplace_slots: &[Entity],
) -> (Vec<JobAssignment>, i32) {
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

    (assignments, remaining_workplaces.len() as i32)
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
    use crate::core::systems::{citizens, placement, road_network_analysis};

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
    fn counts_only_jobs_entry_skips_assignment_resolution() {
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

        let counts = ResourceRegistry::local_job_counts(&world);

        assert_eq!(counts.total_jobs, 5);
        assert_eq!(counts.job_seekers, 7);
        assert_eq!(counts.unemployment, 2);
        assert!(
            world
                .citizens
                .values()
                .all(|citizen| citizen.workplace.is_none())
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

    fn power_workplace(world: &mut World, x: usize, y: usize) {
        let entity = world.grid.get(x, y).expect("workplace");
        world
            .power_consumers
            .get_mut(&entity)
            .expect("power consumer")
            .powered = true;
    }
}
