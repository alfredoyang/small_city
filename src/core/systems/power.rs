//! Road-network power system with plant capacity, consumer demand, and deterministic allocation.

use std::collections::HashSet;

use crate::core::entity::Entity;
use crate::core::resource_registry::ResourceRegistry;
use crate::core::resources::PowerStats;
use crate::core::systems::road_connectivity;
use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    for consumer in world.power_consumers.values_mut() {
        consumer.powered = false;
        consumer.source = None;
    }

    let resolution = ResourceRegistry::from_world(world).resolve_local_power();
    for grant in &resolution.grants {
        if let Some(consumer) = world.power_consumers.get_mut(&grant.consumer) {
            if consumer.demand != grant.amount {
                continue;
            }
            consumer.powered = true;
            consumer.source = Some(grant.source);
        }
    }

    world.stats.power = PowerStats {
        total_power_capacity: resolution.total_capacity,
        total_power_demand: resolution.total_demand,
        total_power_supplied: resolution.total_supplied,
        total_power_shortage: (resolution.total_demand - resolution.total_supplied).max(0),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::components::PowerSource;
    use crate::core::systems::placement;
    use crate::interface::input::BuildingKind;

    #[test]
    fn powered_consumers_record_local_same_network_source() {
        let mut world = World::new(6, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 0, 1, BuildingKind::Road);
        placement::place_building(&mut world, 1, 1, BuildingKind::Road);
        placement::place_building(&mut world, 1, 0, BuildingKind::Residential);

        placement::place_building(&mut world, 5, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 5, 1, BuildingKind::Road);

        run(&mut world);

        let local_provider = world.grid.get(0, 0).expect("local provider");
        let remote_provider = world.grid.get(5, 0).expect("remote provider");
        let consumer = world.grid.get(1, 0).expect("consumer");
        let power = world
            .power_consumers
            .get(&consumer)
            .expect("power consumer");

        assert!(power.powered);
        assert_eq!(power.source, Some(PowerSource::Local(local_provider)));
        assert_ne!(power.source, Some(PowerSource::Local(remote_provider)));
    }

    #[test]
    fn unpowered_consumers_have_no_power_source_after_shortage() {
        let mut world = World::new(7, 4);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        for x in 0..6 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        for x in 1..5 {
            placement::place_building(&mut world, x, 0, BuildingKind::Industrial);
        }
        placement::place_building(&mut world, 5, 0, BuildingKind::Commercial);

        run(&mut world);

        for x in 1..4 {
            let entity = world.grid.get(x, 0).expect("powered consumer");
            let consumer = world.power_consumers.get(&entity).expect("power consumer");
            assert!(consumer.powered);
            assert!(consumer.source.is_some());
        }
        for x in 4..=5 {
            let entity = world.grid.get(x, 0).expect("unpowered consumer");
            let consumer = world.power_consumers.get(&entity).expect("power consumer");
            assert!(!consumer.powered);
            assert_eq!(consumer.source, None);
        }
    }

    #[test]
    fn multiple_providers_on_one_network_keep_pooled_capacity() {
        let mut world = World::new(8, 3);
        placement::place_building(&mut world, 0, 0, BuildingKind::PowerPlant);
        placement::place_building(&mut world, 5, 0, BuildingKind::PowerPlant);
        for x in 0..=5 {
            placement::place_building(&mut world, x, 1, BuildingKind::Road);
        }
        placement::place_building(&mut world, 1, 0, BuildingKind::Industrial);
        placement::place_building(&mut world, 2, 0, BuildingKind::Industrial);
        placement::place_building(&mut world, 3, 0, BuildingKind::Industrial);

        *consumer_demand_mut(&mut world, 1, 0) = 8;
        *consumer_demand_mut(&mut world, 2, 0) = 8;
        *consumer_demand_mut(&mut world, 3, 0) = 3;

        run(&mut world);

        assert_eq!(world.stats.power.total_power_capacity, 20);
        assert_eq!(world.stats.power.total_power_demand, 19);
        assert_eq!(world.stats.power.total_power_supplied, 19);
        for x in 1..=3 {
            let entity = world.grid.get(x, 0).expect("consumer");
            let consumer = world.power_consumers.get(&entity).expect("power consumer");
            assert!(
                consumer.powered,
                "consumer at x={x} should use remaining pooled network capacity"
            );
            assert!(consumer.source.is_some());
        }
    }

    fn consumer_demand_mut(world: &mut World, x: usize, y: usize) -> &mut i32 {
        let entity = world.grid.get(x, y).expect("consumer cell");
        &mut world
            .power_consumers
            .get_mut(&entity)
            .expect("power consumer")
            .demand
    }
}
