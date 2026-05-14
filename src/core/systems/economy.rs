use crate::core::world::World;
use crate::interface::input::BuildingKind;

pub(crate) fn run(world: &mut World) {
    let citizens: i32 = world
        .populations
        .values()
        .map(|population| population.current)
        .sum();
    let mut income = citizens;

    for (entity, building) in world.buildings.iter() {
        let powered = world
            .power_consumers
            .get(entity)
            .map(|consumer| consumer.powered)
            .unwrap_or(false);

        income += match (building.kind, powered) {
            (BuildingKind::Commercial, true) => 2,
            (BuildingKind::Industrial, true) => 3,
            _ => 0,
        };
    }

    world.resources.money += income;
}
