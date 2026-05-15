use crate::core::entity::Entity;
use crate::core::world::World;

pub(crate) fn remove_entity(world: &mut World, entity: Entity, x: usize, y: usize) {
    world.grid.clear(x, y);
    let Some(record) = world.entities.remove(&entity) else {
        remove_from_all_component_maps(world, entity);
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
}

fn remove_from_all_component_maps(world: &mut World, entity: Entity) {
    world.positions.remove(&entity);
    world.buildings.remove(&entity);
    world.populations.remove(&entity);
    world.power_providers.remove(&entity);
    world.power_consumers.remove(&entity);
    world.pollution_sources.remove(&entity);
    world.happiness_effects.remove(&entity);
}
