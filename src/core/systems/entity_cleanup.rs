//! Entity cleanup helpers that remove all known components for a deleted building or citizen.

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
    if record.has_citizen {
        world.citizens.remove(&entity);
    }
    if record.has_home {
        world.homes.remove(&entity);
    }
    if record.has_employment {
        world.employments.remove(&entity);
    }
    if record.has_citizen_happiness {
        world.citizen_happiness.remove(&entity);
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
}

fn remove_from_all_component_maps(world: &mut World, entity: Entity) {
    world.positions.remove(&entity);
    world.buildings.remove(&entity);
    world.populations.remove(&entity);
    world.citizens.remove(&entity);
    world.homes.remove(&entity);
    world.employments.remove(&entity);
    world.citizen_happiness.remove(&entity);
    world.power_providers.remove(&entity);
    world.power_consumers.remove(&entity);
    world.pollution_sources.remove(&entity);
    world.happiness_effects.remove(&entity);
    remove_citizens_for_home(world, entity);
}

fn remove_citizens_for_home(world: &mut World, residential: Entity) {
    let citizens: Vec<_> = world
        .homes
        .iter()
        .filter_map(|(citizen, home)| (home.residential == residential).then_some(*citizen))
        .collect();

    for citizen in citizens {
        world.entities.remove(&citizen);
        world.citizens.remove(&citizen);
        world.homes.remove(&citizen);
        world.employments.remove(&citizen);
        world.citizen_happiness.remove(&citizen);
    }
}
