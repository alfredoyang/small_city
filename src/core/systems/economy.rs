use crate::core::systems::road_connectivity;
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
        let road_connected = road_connectivity::is_road_connected(world, *entity);

        income += match (building.kind, powered, road_connected) {
            (BuildingKind::Commercial, true, true) => 2,
            (BuildingKind::Industrial, true, true) => 3,
            _ => 0,
        };
    }

    world.resources.money += income;
}
