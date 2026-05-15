use std::collections::{HashSet, VecDeque};

use crate::core::entity::Entity;
use crate::core::resources::PowerStats;
use crate::core::world::World;
use crate::interface::input::BuildingKind;

#[derive(Debug, Clone)]
struct RoadNetwork {
    roads: HashSet<Entity>,
    capacity: i32,
}

pub(crate) fn run(world: &mut World) {
    for consumer in world.power_consumers.values_mut() {
        consumer.powered = false;
    }

    // Power is allocated per road network so disconnected roads cannot share capacity.
    let networks = discover_road_networks(world);
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
        let mut remaining_capacity = network.capacity;
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
    if !is_road_entity(world, entity) {
        return false;
    }
    discover_road_networks(world)
        .into_iter()
        .any(|network| network.capacity > 0 && network.roads.contains(&entity))
}

pub(crate) fn is_power_provider_connected(world: &World, entity: Entity) -> bool {
    adjacent_road_entities(world, entity).next().is_some()
}

fn discover_road_networks(world: &World) -> Vec<RoadNetwork> {
    let mut visited = HashSet::new();
    let mut road_entities = road_entities_by_position(world);
    let mut networks = Vec::new();

    for road in road_entities.drain(..) {
        if visited.contains(&road) {
            continue;
        }

        let mut roads = HashSet::new();
        let mut queue = VecDeque::from([road]);
        visited.insert(road);

        while let Some(current) = queue.pop_front() {
            roads.insert(current);
            for neighbor in adjacent_road_entities(world, current) {
                if visited.insert(neighbor) {
                    queue.push_back(neighbor);
                }
            }
        }

        let capacity = network_capacity(world, &roads);
        networks.push(RoadNetwork { roads, capacity });
    }

    networks
}

fn road_entities_by_position(world: &World) -> Vec<Entity> {
    let mut roads: Vec<_> = world
        .buildings
        .iter()
        .filter_map(|(entity, building)| (building.kind == BuildingKind::Road).then_some(*entity))
        .collect();
    roads.sort_by_key(|entity| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });
    roads
}

fn network_capacity(world: &World, roads: &HashSet<Entity>) -> i32 {
    world
        .power_providers
        .iter()
        .filter(|(entity, _provider)| {
            adjacent_road_entities(world, **entity).any(|road| roads.contains(&road))
        })
        .map(|(_entity, provider)| provider.capacity)
        .sum()
}

fn consumers_adjacent_to_network(world: &World, network: &RoadNetwork) -> Vec<Entity> {
    world
        .power_consumers
        .keys()
        .filter(|entity| {
            adjacent_road_entities(world, **entity).any(|road| network.roads.contains(&road))
        })
        .copied()
        .collect()
}

fn adjacent_road_entities(world: &World, entity: Entity) -> impl Iterator<Item = Entity> + '_ {
    adjacent_cells(world, entity).filter_map(|(x, y)| {
        world
            .grid
            .get(x, y)
            .filter(|neighbor| is_road_entity(world, *neighbor))
    })
}

fn adjacent_cells(world: &World, entity: Entity) -> impl Iterator<Item = (usize, usize)> + '_ {
    let position = world.positions.get(&entity).copied();
    [
        position.and_then(|position| position.x.checked_sub(1).map(|x| (x, position.y))),
        position.map(|position| (position.x.saturating_add(1), position.y)),
        position.and_then(|position| position.y.checked_sub(1).map(|y| (position.x, y))),
        position.map(|position| (position.x, position.y.saturating_add(1))),
    ]
    .into_iter()
    .flatten()
    .filter(|(x, y)| world.grid.contains(*x, *y))
}

fn is_road_entity(world: &World, entity: Entity) -> bool {
    world
        .buildings
        .get(&entity)
        .is_some_and(|building| building.kind == BuildingKind::Road)
}
