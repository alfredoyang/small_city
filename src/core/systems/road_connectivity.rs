//! Road connectivity and road-network discovery helpers shared by deterministic systems.

use std::collections::{HashSet, VecDeque};

use crate::core::entity::Entity;
use crate::core::world::World;
use crate::interface::input::BuildingKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoadNetwork {
    pub id: u32,
    pub roads: HashSet<Entity>,
}

pub(crate) fn is_road_connected(world: &World, entity: Entity) -> bool {
    adjacent_road_entities(world, entity).next().is_some()
}

pub(crate) fn discover_road_networks(world: &World) -> Vec<RoadNetwork> {
    let mut visited = HashSet::new();
    // Start from sorted road entities so network ids stay stable across runs.
    let mut road_entities = road_entities_by_position(world);
    let mut networks = Vec::new();

    for road in road_entities.drain(..) {
        if visited.contains(&road) {
            continue;
        }

        // Flood-fill one orthogonally connected road component. Each completed
        // component becomes one independent road network for power and logistics.
        let mut roads = HashSet::new();
        let mut queue = VecDeque::from([road]);
        visited.insert(road);

        while let Some(current) = queue.pop_front() {
            roads.insert(current);
            // Mark when queued, not when popped, so each road is enqueued once.
            for neighbor in adjacent_road_entities(world, current) {
                if visited.insert(neighbor) {
                    queue.push_back(neighbor);
                }
            }
        }

        networks.push(RoadNetwork {
            id: networks.len() as u32,
            roads,
        });
    }

    networks
}

pub(crate) fn road_entities_by_position(world: &World) -> Vec<Entity> {
    let mut roads: Vec<_> = world
        .buildings
        .iter()
        .filter_map(|(entity, building)| (building.kind == BuildingKind::Road).then_some(*entity))
        .collect();
    sort_entities_by_position(world, &mut roads);
    roads
}

pub(crate) fn adjacent_road_entities(
    world: &World,
    entity: Entity,
) -> impl Iterator<Item = Entity> + '_ {
    adjacent_cells(world, entity).filter_map(|(x, y)| {
        world
            .grid
            .get(x, y)
            .filter(|neighbor| is_road_entity(world, *neighbor))
    })
}

pub(crate) fn adjacent_cells(
    world: &World,
    entity: Entity,
) -> impl Iterator<Item = (usize, usize)> + '_ {
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

pub(crate) fn is_road_entity(world: &World, entity: Entity) -> bool {
    world
        .buildings
        .get(&entity)
        .is_some_and(|building| building.kind == BuildingKind::Road)
}

pub(crate) fn sort_entities_by_position(world: &World, entities: &mut [Entity]) {
    entities.sort_by_key(|entity| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });
}
