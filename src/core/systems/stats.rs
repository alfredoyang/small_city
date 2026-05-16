//! City statistics refresh system for population, effective jobs, and unemployment.

use crate::core::systems::{citizens, road_connectivity};
use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    refresh_population_and_jobs(world);
}

pub(crate) fn refresh_population_and_jobs(world: &mut World) {
    citizens::sync_population_from_citizens(world);
    let population = citizens::citizen_count(world);
    let jobs = world
        .buildings
        .iter()
        .filter(|(entity, _building)| {
            world
                .power_consumers
                .get(entity)
                .is_some_and(|consumer| consumer.powered)
                && road_connectivity::is_road_connected(world, **entity)
        })
        .map(|(_entity, building)| building.kind.jobs())
        .sum();

    world.stats.population = population;
    world.stats.jobs = jobs;
    world.stats.unemployment = (population - jobs).max(0);
}
