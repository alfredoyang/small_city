use crate::core::entity::Entity;
use crate::core::world::World;

pub(crate) fn remove_entity(world: &mut World, entity: Entity, x: usize, y: usize) {
    world.grid.clear(x, y);
    world.positions.remove(&entity);
    world.buildings.remove(&entity);
    world.populations.remove(&entity);
    world.power_providers.remove(&entity);
    world.power_consumers.remove(&entity);
    world.pollution_sources.remove(&entity);
    world.happiness_effects.remove(&entity);
}
