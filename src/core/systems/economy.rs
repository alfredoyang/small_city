use crate::core::systems::road_connectivity;
use crate::core::world::World;
use crate::interface::input::BuildingKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EconomyBreakdown {
    pub population_income: i32,
    pub commercial_income: i32,
    pub industrial_income: i32,
    pub maintenance_cost: i32,
    pub net: i32,
}

pub(crate) fn run(world: &mut World) -> EconomyBreakdown {
    let population_income: i32 = world
        .populations
        .values()
        .map(|population| population.current)
        .sum();
    let mut commercial_income = 0;
    let mut industrial_income = 0;
    let mut maintenance_cost = 0;

    for (entity, building) in world.buildings.iter() {
        maintenance_cost += building.kind.maintenance_cost();

        let powered = world
            .power_consumers
            .get(entity)
            .map(|consumer| consumer.powered)
            .unwrap_or(false);
        let road_connected = road_connectivity::is_road_connected(world, *entity);

        match (building.kind, powered, road_connected) {
            (BuildingKind::Commercial, true, true) => commercial_income += 2,
            (BuildingKind::Industrial, true, true) => industrial_income += 3,
            _ => {}
        }
    }

    let net = population_income + commercial_income + industrial_income - maintenance_cost;
    world.resources.money += net;

    EconomyBreakdown {
        population_income,
        commercial_income,
        industrial_income,
        maintenance_cost,
        net,
    }
}
