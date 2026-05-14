use crate::core::components::BuildingKind;
use crate::core::world::World;

pub(crate) fn run(world: &mut World) {
    let produced: i32 = world
        .pollution_sources
        .values()
        .map(|source| source.amount)
        .sum();
    let park_reduction = world
        .buildings
        .values()
        .filter(|building| building.kind == BuildingKind::Park)
        .count() as i32;

    world.stats.pollution = (produced - park_reduction).max(0);
}
