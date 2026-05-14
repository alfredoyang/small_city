use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    refresh_population_and_jobs(world);
}

pub(crate) fn refresh_population_and_jobs(world: &mut World) {
    let population = world
        .populations
        .values()
        .map(|population| population.current)
        .sum();
    let jobs = world
        .buildings
        .values()
        .map(|building| building.kind.jobs())
        .sum();

    world.stats.population = population;
    world.stats.jobs = jobs;
    world.stats.unemployment = (population - jobs).max(0);
}
