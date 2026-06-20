//! Entity cleanup helpers that remove all known components for a deleted building or citizen.

use crate::core::entity::Entity;
use crate::core::world::World;

pub(crate) fn remove_entity(world: &mut World, entity: Entity, x: usize, y: usize) {
    // Clear every cell the building occupies. Multi-cell buildings store their footprint, so
    // bulldozing any one of their cells removes the whole building; fall back to (x, y) for
    // off-grid entities (citizens) or anything without a position/building. Read before removal.
    match (
        world.positions.get(&entity).copied(),
        world.buildings.get(&entity).copied(),
    ) {
        (Some(position), Some(building)) => world.grid.clear_footprint(
            position.x,
            position.y,
            // Clamp to at least 1x1 so a corrupt zero-sized footprint from a save still clears
            // the anchor cell instead of leaving a stale entity id on the grid.
            building.footprint.width.max(1) as usize,
            building.footprint.height.max(1) as usize,
        ),
        _ => {
            world.grid.clear(x, y);
        }
    }
    let Some(record) = world.entities.remove(&entity) else {
        remove_from_all_component_maps(world, entity);
        world.invalidate_resource_registry();
        return;
    };

    if record.has_position {
        world.positions.remove(&entity);
    }
    if record.kind.is_some() {
        world.buildings.remove(&entity);
    }
    if record.has_population {
        world.populations.remove(&entity);
    }
    if record.has_citizen {
        world.citizens.remove(&entity);
    }
    if record.has_power_provider {
        world.power_providers.remove(&entity);
    }
    if record.has_power_consumer {
        world.power_consumers.remove(&entity);
    }
    if record.has_pollution_source {
        world.pollution_sources.remove(&entity);
    }
    if record.has_happiness_effect {
        world.happiness_effects.remove(&entity);
    }
    remove_citizens_for_home(world, entity);
    world.invalidate_resource_registry();
}

fn remove_from_all_component_maps(world: &mut World, entity: Entity) {
    world.positions.remove(&entity);
    world.buildings.remove(&entity);
    world.populations.remove(&entity);
    world.citizens.remove(&entity);
    world.power_providers.remove(&entity);
    world.power_consumers.remove(&entity);
    world.pollution_sources.remove(&entity);
    world.happiness_effects.remove(&entity);
    remove_citizens_for_home(world, entity);
}

fn remove_citizens_for_home(world: &mut World, residential: Entity) {
    let citizens: Vec<_> = world
        .citizens
        .iter()
        .filter_map(|(entity, citizen)| (citizen.home == residential).then_some(*entity))
        .collect();

    for citizen in citizens {
        world.entities.remove(&citizen);
        world.citizens.remove(&citizen);
    }
}
