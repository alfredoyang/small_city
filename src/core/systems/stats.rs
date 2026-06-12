//! City statistics refresh system for population, effective jobs, and unemployment.

use crate::core::systems::citizens;
use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    refresh_population_and_jobs(world);
}

pub(crate) fn refresh_population_and_jobs(world: &mut World) {
    citizens::sync_population_from_citizens(world);
    let population = citizens::citizen_count(world);
    let jobs = world.cached_job_counts();

    world.stats.population = population;
    world.stats.jobs = jobs.total_jobs;
    world.stats.unemployment = jobs.unemployment;
}
