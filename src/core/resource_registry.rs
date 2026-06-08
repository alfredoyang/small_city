//! Region-local resource registry for deterministic provider/consumer allocation.
//!
//! The first registry entry is power. It registers local power providers and
//! consumers from a `World`, emits map-ordered requests, and resolves local
//! grants without mutating the world. Systems apply those grants afterward.
//!
//! Flow inside `power::run` (one tick, before downstream systems read `powered`):
//!
//! ```text
//! power::run(world)
//!   |  (1) reset every PowerConsumer { powered = false, source = None }
//!   v
//! ResourceRegistry::from_world(world)      read-only snapshot, never mutates World
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

use std::collections::HashSet;

use crate::core::components::PowerSource;
use crate::core::entity::Entity;
use crate::core::systems::road_connectivity::{self, RoadNetwork};
use crate::core::world::World;

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
}

#[derive(Debug)]
/// Region-local resource registry.
///
/// It is rebuilt from authoritative ECS state when a system needs allocation.
/// The registry owns copied IDs and amounts only; it does not expose `World`.
pub(crate) struct ResourceRegistry {
    power: PowerRegistry,
}

impl ResourceRegistry {
    pub(crate) fn from_world(world: &World) -> Self {
        Self {
            power: PowerRegistry::from_world(world),
        }
    }

    pub(crate) fn resolve_local_power(&self) -> PowerResolution {
        self.power.resolve()
    }
}

#[derive(Debug)]
struct PowerRegistry {
    networks: Vec<PowerNetworkPool>,
    requests: Vec<PowerRequest>,
    total_capacity: i32,
    total_demand: i32,
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
        PowerResolution {
            grants,
            total_capacity: self.total_capacity,
            total_demand: self.total_demand,
            total_supplied,
        }
    }
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
