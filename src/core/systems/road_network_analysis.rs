//! Derived road-network distances used by economy, happiness, and inspect explanations.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::core::entity::Entity;
use crate::core::systems::road_connectivity::{self, RoadNetwork};
use crate::core::world::World;
use crate::interface::input::BuildingKind;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RoadNetworkAnalysis {
    pub building_access: HashMap<Entity, RoadAccess>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RoadAccess {
    pub network_id: Option<u32>,
    pub commute_distance: Option<u32>,
    pub nearest_shop_distance: Option<u32>,
    pub goods_route_distance: Option<u32>,
    pub import_export_distance: Option<u32>,
}

pub(crate) fn run(world: &mut World) {
    world.road_analysis = analyze(world);
}

pub(crate) fn access_for(world: &World, entity: Entity) -> RoadAccess {
    world
        .road_analysis
        .building_access
        .get(&entity)
        .copied()
        .unwrap_or_default()
}

pub(crate) fn commute_penalty(distance: Option<u32>) -> i32 {
    match distance {
        Some(0..=4) => 0,
        Some(5..=8) => 1,
        Some(9..=14) => 2,
        Some(_) => 3,
        None => 0,
    }
}

pub(crate) fn shopping_happiness_modifier(distance: Option<u32>) -> i32 {
    match distance {
        Some(0..=4) => 1,
        Some(5..=10) => 0,
        Some(_) => -1,
        None => 0,
    }
}

pub(crate) fn route_margin_penalty(distance: Option<u32>) -> i32 {
    distance.map(|distance| (distance / 8) as i32).unwrap_or(0)
}

pub(crate) fn import_cost_penalty(distance: Option<u32>) -> i32 {
    distance.map(|distance| (distance / 8) as i32).unwrap_or(0)
}

pub(crate) fn distance_between_buildings(world: &World, from: Entity, to: Entity) -> Option<u32> {
    let from_access = access_for(world, from);
    let to_access = access_for(world, to);
    if from_access.network_id.is_none() || from_access.network_id != to_access.network_id {
        return None;
    }

    let networks = road_connectivity::discover_road_networks(world);
    let network = networks
        .iter()
        .find(|network| Some(network.id) == from_access.network_id)?;
    let to_roads = adjacent_roads_in_network(world, to, network);
    let distances = road_distances(world, network, &to_roads);
    let from_roads = adjacent_roads_in_network(world, from, network);
    nearest_distance(&from_roads, &distances)
}

fn analyze(world: &World) -> RoadNetworkAnalysis {
    let mut analysis = RoadNetworkAnalysis::default();
    for network in road_connectivity::discover_road_networks(world) {
        analyze_network(world, &network, &mut analysis);
    }
    analysis
}

fn analyze_network(world: &World, network: &RoadNetwork, analysis: &mut RoadNetworkAnalysis) {
    let commercial_roads = destination_roads(world, network, &[BuildingKind::Commercial]);
    let workplace_roads = destination_roads(
        world,
        network,
        &[BuildingKind::Commercial, BuildingKind::Industrial],
    );
    let industrial_roads = destination_roads(world, network, &[BuildingKind::Industrial]);
    let edge_roads = edge_roads(world, network);

    let commercial_distances = road_distances(world, network, &commercial_roads);
    let workplace_distances = road_distances(world, network, &workplace_roads);
    let industrial_distances = road_distances(world, network, &industrial_roads);
    let edge_distances = road_distances(world, network, &edge_roads);

    let mut buildings = connected_buildings(world, network);
    road_connectivity::sort_entities_by_position(world, &mut buildings);

    for building in buildings {
        let Some(kind) = world.buildings.get(&building).map(|building| building.kind) else {
            continue;
        };
        let adjacent_roads = adjacent_roads_in_network(world, building, network);
        let access = RoadAccess {
            network_id: Some(network.id),
            commute_distance: (kind == BuildingKind::Residential)
                .then(|| nearest_distance(&adjacent_roads, &workplace_distances))
                .flatten(),
            nearest_shop_distance: (kind == BuildingKind::Residential)
                .then(|| nearest_distance(&adjacent_roads, &commercial_distances))
                .flatten(),
            goods_route_distance: match kind {
                BuildingKind::Industrial => {
                    nearest_distance(&adjacent_roads, &commercial_distances)
                }
                BuildingKind::Commercial => {
                    nearest_distance(&adjacent_roads, &industrial_distances)
                }
                _ => None,
            },
            import_export_distance: matches!(
                kind,
                BuildingKind::Industrial | BuildingKind::Commercial
            )
            .then(|| nearest_distance(&adjacent_roads, &edge_distances))
            .flatten(),
        };
        analysis.building_access.insert(building, access);
    }
}

fn destination_roads(world: &World, network: &RoadNetwork, kinds: &[BuildingKind]) -> Vec<Entity> {
    let mut roads = Vec::new();
    let mut buildings: Vec<_> = world
        .buildings
        .iter()
        .filter_map(|(entity, building)| kinds.contains(&building.kind).then_some(*entity))
        .collect();
    road_connectivity::sort_entities_by_position(world, &mut buildings);

    for building in buildings {
        roads.extend(adjacent_roads_in_network(world, building, network));
    }
    roads.sort_by_key(|entity| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });
    roads.dedup();
    roads
}

fn connected_buildings(world: &World, network: &RoadNetwork) -> Vec<Entity> {
    world
        .buildings
        .keys()
        .filter(|entity| !road_connectivity::is_road_entity(world, **entity))
        .filter(|entity| {
            road_connectivity::adjacent_road_entities(world, **entity)
                .any(|road| network.roads.contains(&road))
        })
        .copied()
        .collect()
}

fn adjacent_roads_in_network(world: &World, entity: Entity, network: &RoadNetwork) -> Vec<Entity> {
    let mut roads: Vec<_> = road_connectivity::adjacent_road_entities(world, entity)
        .filter(|road| network.roads.contains(road))
        .collect();
    road_connectivity::sort_entities_by_position(world, &mut roads);
    roads
}

fn edge_roads(world: &World, network: &RoadNetwork) -> Vec<Entity> {
    let max_x = world.grid.width().saturating_sub(1);
    let max_y = world.grid.height().saturating_sub(1);
    let mut roads: Vec<_> = network
        .roads
        .iter()
        .copied()
        .filter(|road| {
            world.positions.get(road).is_some_and(|position| {
                position.x == 0 || position.y == 0 || position.x == max_x || position.y == max_y
            })
        })
        .collect();
    road_connectivity::sort_entities_by_position(world, &mut roads);
    roads
}

fn road_distances(
    world: &World,
    network: &RoadNetwork,
    sources: &[Entity],
) -> HashMap<Entity, u32> {
    let source_set: HashSet<_> = sources.iter().copied().collect();
    let mut distances = HashMap::new();
    let mut queue = VecDeque::new();

    for source in sources {
        if network.roads.contains(source) && distances.insert(*source, 0).is_none() {
            queue.push_back(*source);
        }
    }

    while let Some(current) = queue.pop_front() {
        let distance = distances.get(&current).copied().unwrap_or(0);
        let mut neighbors: Vec<_> = road_connectivity::adjacent_road_entities(world, current)
            .filter(|neighbor| network.roads.contains(neighbor))
            .filter(|neighbor| !source_set.contains(neighbor) || *neighbor != current)
            .collect();
        road_connectivity::sort_entities_by_position(world, &mut neighbors);

        for neighbor in neighbors {
            if distances.contains_key(&neighbor) {
                continue;
            }
            distances.insert(neighbor, distance + 1);
            queue.push_back(neighbor);
        }
    }

    distances
}

fn nearest_distance(roads: &[Entity], distances: &HashMap<Entity, u32>) -> Option<u32> {
    roads
        .iter()
        .filter_map(|road| distances.get(road).copied())
        .min()
}

#[cfg(test)]
mod tests {
    use super::{access_for, run};
    use crate::core::components::{Building, BuildingData, Position};
    use crate::core::world::World;
    use crate::interface::input::BuildingKind;

    #[test]
    fn analysis_finds_nearest_destinations_on_same_road_network() {
        let mut world = World::new(6, 3);
        let home = place(&mut world, 1, 0, BuildingKind::Residential);
        let commercial = place(&mut world, 4, 0, BuildingKind::Commercial);
        for x in 1..=4 {
            place(&mut world, x, 1, BuildingKind::Road);
        }

        run(&mut world);

        let home_access = access_for(&world, home);
        let commercial_access = access_for(&world, commercial);
        assert_eq!(home_access.nearest_shop_distance, Some(3));
        assert_eq!(home_access.commute_distance, Some(3));
        assert_eq!(commercial_access.import_export_distance, None);
    }

    #[test]
    fn analysis_leaves_disconnected_destinations_unreachable() {
        let mut world = World::new(6, 3);
        let home = place(&mut world, 1, 0, BuildingKind::Residential);
        place(&mut world, 4, 0, BuildingKind::Commercial);
        place(&mut world, 1, 1, BuildingKind::Road);
        place(&mut world, 4, 1, BuildingKind::Road);

        run(&mut world);

        let home_access = access_for(&world, home);
        assert_eq!(home_access.nearest_shop_distance, None);
        assert_eq!(home_access.commute_distance, None);
    }

    #[test]
    fn analysis_finds_edge_access_through_roads() {
        let mut world = World::new(5, 3);
        let industrial = place(&mut world, 3, 0, BuildingKind::Industrial);
        for x in 0..=3 {
            place(&mut world, x, 1, BuildingKind::Road);
        }

        run(&mut world);

        assert_eq!(
            access_for(&world, industrial).import_export_distance,
            Some(3)
        );
    }

    fn place(
        world: &mut World,
        x: usize,
        y: usize,
        kind: BuildingKind,
    ) -> crate::core::entity::Entity {
        let entity = world.spawn();
        world.attach_position(entity, Position { x, y });
        world.attach_building(
            entity,
            Building {
                kind,
                level: 1,
                data: BuildingData::None,
            },
        );
        assert!(world.grid.set(x, y, entity));
        entity
    }
}
