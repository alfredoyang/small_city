use crate::core::systems::road_connectivity;
use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    let mut available_jobs = (world.stats.jobs - world.stats.population).max(0);
    let mut residential_entities: Vec<_> = world.populations.keys().copied().collect();
    // Sort by entity id so population growth is deterministic across HashMap iteration orders.
    residential_entities.sort_by_key(|entity| entity.0);

    for entity in residential_entities {
        if available_jobs <= 0 {
            break;
        }

        let powered = world
            .power_consumers
            .get(&entity)
            .map(|consumer| consumer.powered)
            .unwrap_or(false);
        if !powered {
            continue;
        }
        if !road_connectivity::is_road_connected(world, entity) {
            continue;
        }

        let Some(population) = world.populations.get_mut(&entity) else {
            continue;
        };
        if population.current < population.max {
            population.current += 1;
            available_jobs -= 1;
        }
    }
}
