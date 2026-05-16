//! Economy system for citizen salary, rent, shopping, taxes, and building maintenance.

use crate::core::systems::road_connectivity;
use crate::core::world::World;
use crate::interface::input::BuildingKind;

const COMMERCIAL_SHOPPER_CAPACITY: i32 = 4;
const COMMERCIAL_SALARY: i32 = 3;
const INDUSTRIAL_SALARY: i32 = 4;
const WORKPLACE_TAX_PER_WORKER: i32 = 1;
const RENT_PER_CITIZEN: i32 = 1;
const SHOPPING_COST: i32 = 1;
const COMMERCIAL_SALES_TAX_PER_PURCHASE: i32 = 1;
const SHOPPING_HAPPINESS_BONUS: i32 = 3;
const MISSED_RENT_HAPPINESS_PENALTY: i32 = 5;
const MISSED_SHOPPING_HAPPINESS_PENALTY: i32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EconomyBreakdown {
    pub salaries_paid: i32,
    pub workplace_tax: i32,
    pub rent_income: i32,
    pub commercial_sales_tax: i32,
    pub shoppers_served: i32,
    pub rent_failures: i32,
    pub maintenance_cost: i32,
    pub net: i32,
}

pub(crate) fn run(world: &mut World) -> EconomyBreakdown {
    let maintenance_cost = maintenance_cost(world);
    assign_workplaces(world);

    let mut shopping_slots = commercial_shopping_slots(world);
    let mut salaries_paid = 0;
    let mut workplace_tax = 0;
    let mut rent_income = 0;
    let mut commercial_sales_tax = 0;
    let mut shoppers_served = 0;
    let mut rent_failures = 0;

    let mut citizen_entities: Vec<_> = world.citizens.keys().copied().collect();
    citizen_entities.sort_by_key(|citizen| citizen.0);

    for citizen_entity in citizen_entities {
        let salary = world
            .citizens
            .get(&citizen_entity)
            .and_then(|citizen| citizen.workplace)
            .and_then(|workplace| salary_for_workplace(world, workplace))
            .unwrap_or(0);

        let Some(citizen) = world.citizens.get_mut(&citizen_entity) else {
            continue;
        };

        if salary > 0 {
            citizen.money += salary;
            salaries_paid += salary;
            workplace_tax += WORKPLACE_TAX_PER_WORKER;
        }

        if citizen.money >= RENT_PER_CITIZEN {
            citizen.money -= RENT_PER_CITIZEN;
            rent_income += RENT_PER_CITIZEN;
        } else {
            rent_failures += 1;
            citizen.happiness -= MISSED_RENT_HAPPINESS_PENALTY;
        }

        if citizen.money >= SHOPPING_COST && shopping_slots > 0 {
            citizen.money -= SHOPPING_COST;
            shopping_slots -= 1;
            commercial_sales_tax += COMMERCIAL_SALES_TAX_PER_PURCHASE;
            shoppers_served += 1;
            citizen.happiness += SHOPPING_HAPPINESS_BONUS;
        } else {
            citizen.happiness -= MISSED_SHOPPING_HAPPINESS_PENALTY;
        }

        citizen.happiness = citizen.happiness.clamp(0, 100);
    }

    let net = workplace_tax + rent_income + commercial_sales_tax - maintenance_cost;
    world.resources.money += net;

    EconomyBreakdown {
        salaries_paid,
        workplace_tax,
        rent_income,
        commercial_sales_tax,
        shoppers_served,
        rent_failures,
        maintenance_cost,
        net,
    }
}

fn assign_workplaces(world: &mut World) {
    let mut workplaces = workplace_slots(world);
    workplaces.sort_by_key(|entity| {
        world
            .positions
            .get(entity)
            .map(|position| (position.y, position.x, entity.0))
            .unwrap_or((usize::MAX, usize::MAX, entity.0))
    });

    let mut citizen_entities: Vec<_> = world.citizens.keys().copied().collect();
    citizen_entities.sort_by_key(|citizen| citizen.0);

    for (index, citizen_entity) in citizen_entities.into_iter().enumerate() {
        let workplace = workplaces.get(index).copied();
        if let Some(citizen) = world.citizens.get_mut(&citizen_entity) {
            citizen.workplace = workplace;
        }
    }
}

fn workplace_slots(world: &World) -> Vec<crate::core::entity::Entity> {
    let mut slots = Vec::new();
    for (entity, building) in &world.buildings {
        if !is_effective_workplace(world, *entity) {
            continue;
        }

        for _ in 0..building.kind.jobs().max(0) {
            slots.push(*entity);
        }
    }
    slots
}

fn commercial_shopping_slots(world: &World) -> i32 {
    world
        .buildings
        .iter()
        .filter(|(entity, building)| {
            building.kind == BuildingKind::Commercial && is_effective_workplace(world, **entity)
        })
        .map(|_| COMMERCIAL_SHOPPER_CAPACITY)
        .sum()
}

fn salary_for_workplace(world: &World, workplace: crate::core::entity::Entity) -> Option<i32> {
    let building = world.buildings.get(&workplace)?;
    if !is_effective_workplace(world, workplace) {
        return None;
    }

    match building.kind {
        BuildingKind::Commercial => Some(COMMERCIAL_SALARY),
        BuildingKind::Industrial => Some(INDUSTRIAL_SALARY),
        _ => None,
    }
}

fn is_effective_workplace(world: &World, entity: crate::core::entity::Entity) -> bool {
    let powered = world
        .power_consumers
        .get(&entity)
        .map(|consumer| consumer.powered)
        .unwrap_or(false);
    powered && road_connectivity::is_road_connected(world, entity)
}

fn maintenance_cost(world: &World) -> i32 {
    world
        .buildings
        .values()
        .map(|building| building.kind.maintenance_cost())
        .sum()
}
