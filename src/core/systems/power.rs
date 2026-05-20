//! Road-network power system with plant capacity, consumer demand, and deterministic allocation.

use std::collections::HashSet;

use crate::core::entity::Entity;
use crate::core::resources::PowerStats;
use crate::core::systems::road_connectivity::{self, RoadNetwork};
use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    for consumer in world.power_consumers.values_mut() {
        consumer.powered = false;
    }

    // Power is allocated per road network so disconnected roads cannot share capacity.
    let networks = road_connectivity::discover_road_networks(world);
    let total_power_capacity: i32 = world
        .power_providers
        .values()
        .map(|provider| provider.capacity)
        .sum();
    let total_power_demand: i32 = world
        .power_consumers
        .values()
        .map(|consumer| consumer.demand)
        .sum();

    let mut total_power_supplied = 0;
    for network in &networks {
        let mut remaining_capacity = network_capacity(world, &network.roads);
        let mut consumers = consumers_adjacent_to_network(world, network);
        // Shortage behavior must be stable across runs: consume capacity by map order.
        consumers.sort_by_key(|entity| {
            world
                .positions
                .get(entity)
                .map(|position| (position.y, position.x, entity.0))
                .unwrap_or((usize::MAX, usize::MAX, entity.0))
        });

        for entity in consumers {
            let Some(consumer) = world.power_consumers.get_mut(&entity) else {
                continue;
            };
            if consumer.powered || consumer.demand > remaining_capacity {
                continue;
            }
            consumer.powered = true;
            remaining_capacity -= consumer.demand;
            total_power_supplied += consumer.demand;
        }
    }

    world.stats.power = PowerStats {
        total_power_capacity,
        total_power_demand,
        total_power_supplied,
        total_power_shortage: (total_power_demand - total_power_supplied).max(0),
    };
}

pub(crate) fn is_powered_road(world: &World, x: usize, y: usize) -> bool {
    let Some(entity) = world.grid.get(x, y) else {
        return false;
    };
    if !road_connectivity::is_road_entity(world, entity) {
        return false;
    }
    road_connectivity::discover_road_networks(world)
        .into_iter()
        .any(|network| {
            network_capacity(world, &network.roads) > 0 && network.roads.contains(&entity)
        })
}

pub(crate) fn is_power_provider_connected(world: &World, entity: Entity) -> bool {
    road_connectivity::adjacent_road_entities(world, entity)
        .next()
        .is_some()
}

fn network_capacity(world: &World, roads: &HashSet<Entity>) -> i32 {
    world
        .power_providers
        .iter()
        .filter(|(entity, _provider)| {
            road_connectivity::adjacent_road_entities(world, **entity)
                .any(|road| roads.contains(&road))
        })
        .map(|(_entity, provider)| provider.capacity)
        .sum()
}

fn consumers_adjacent_to_network(world: &World, network: &RoadNetwork) -> Vec<Entity> {
    world
        .power_consumers
        .keys()
        .filter(|entity| {
            road_connectivity::adjacent_road_entities(world, **entity)
                .any(|road| network.roads.contains(&road))
        })
        .copied()
        .collect()
}
